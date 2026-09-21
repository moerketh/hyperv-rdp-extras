//! KWin zkde-screencast virtual-output machinery (KDE Plasma 6+).
//!
//! Creates a KWin **virtual output** at an arbitrary resolution and streams
//! it via the private `zkde_screencast_unstable_v1` protocol — the same
//! protocol xdg-desktop-portal-kde wraps. One request creates the output
//! AND its PipeWire stream: no portal session, no consent dialog, no source
//! picker. Capture size == desktop size, so no scaling or coordinate
//! remapping is needed between the stream and an encoder.
//!
//! Also provides the **output layout guard**: while a virtual-output session
//! is live, the physical (DRM) outputs are disabled so the virtual output
//! becomes the primary at (0,0) — the layout that makes the session fully
//! interactive (panel and windows relocate onto it). The guard re-enables
//! the physical outputs on drop, physical first (the machine must never be
//! left without a display).
//!
//! This module is transport/protocol agnostic about the consumer: it owns
//! the Wayland thread, the stream lifecycle, and the layout; the caller
//! owns input injection and session plumbing.
//!
//! Wayland plumbing lives on a dedicated thread (the connection is not
//! async); commands flow in via `std::sync::mpsc`, results back via tokio
//! oneshot.
//!
//! Origin: fork-authored code from moerketh/lamco-rdp-server, absent from
//! upstream, rewritten against crate-local types. See `PROVENANCE.md`.

use std::{
    os::fd::AsFd as _,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use anyhow::{Context as _, Result};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

/// Configuration for the virtual-output machinery: the name KWin will
/// assign/connect (`Virtual-<name>` in kscreen) and the full kscreen
/// connector name to exclude from physical-output management.
///
/// The exclusion used by the kscreen parser is EXACT-match against
/// `kscreen_name` (see [`parse_enabled_physical_outputs`]): a DRM virtual
/// connector (e.g. Hyper-V's `Virtual-1`) is itself a physical output that
/// the layout guard must manage, so the match may never be a
/// `"Virtual-"` prefix.
#[derive(Debug, Clone)]
pub struct VirtualOutputConfig {
    /// The name passed to `stream_virtual_output`; KWin lists the output
    /// as `Virtual-{name}` in kscreen.
    pub name: String,
    /// The full kscreen connector name to exclude from physical-output
    /// management. Must equal `Virtual-{name}` for the exclusion to hit.
    pub kscreen_name: String,
}

impl VirtualOutputConfig {
    /// Build a config from the output name; derives the kscreen connector
    /// name (`Virtual-{name}`) the way KWin does.
    #[must_use]
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            kscreen_name: format!("Virtual-{name}"),
        }
    }
}

impl Default for VirtualOutputConfig {
    /// The neutral default identity (`Virtual-rdp`). The fork passes its
    /// own name (`lamco`) explicitly — see that repo's strategy shell.
    fn default() -> Self {
        Self::new("rdp")
    }
}

/// How long to wait for a virtual-output stream's `created` reply before
/// declaring the create wedged (KWin not answering) and giving up.
pub const STREAM_CREATE_TIMEOUT: Duration = Duration::from_secs(10);

    /// Settle window before a swapped-out stream's close is sent. The
    /// close removes the old virtual output; plasmashell binds the
    /// replacement's wl_registry global asynchronously, and a removal
    /// processed before that bind sends Qt to its placeholder screen
    /// (never re-latches; field-observed during resize churn). Mirrors the
    /// layout guard's engage settle window.
    pub const SETTLE_CLOSE_MS: Duration = Duration::from_millis(750);
/// Commands sent to the Wayland connection thread.
enum WlCommand {
    /// Create a virtual output at the given size; replies with the PipeWire
    /// node id.
    CreateStream {
        width: i32,
        height: i32,
        reply: tokio::sync::oneshot::Sender<Result<u32, String>>,
    },
    /// Close the current stream (destroys the virtual output server-side).
    Close,
}

// ============================================================================
// Stream request state machine
// ============================================================================

/// Stream-request outcome for the caller of
/// [`VirtualOutputManager::recreate_stream`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamOutcome {
    /// KWin created the virtual output; `node` is the PipeWire node id to
    /// bind for capture.
    Created { node: u32 },
    /// KWin refused the create (its `failed` event).
    Failed { reason: String },
    /// The compositor closed the stream before it produced anything usable.
    Closed,
}

/// First-conclusive-event bookkeeping for a `stream_virtual_output`
/// request.
///
/// Invariants (unit-tested below):
/// 1. The FIRST conclusive event decides the outcome; later events are
///    ignored (a `Closed` following a `Failed` must not double-deliver).
/// 2. A new request ([`Self::reset`]) re-arms the machine.
#[derive(Debug, Default)]
pub struct StreamRequestMachine {
    /// A conclusive event has already been delivered for this request.
    done: bool,
}

impl StreamRequestMachine {
    /// A machine armed for a fresh request.
    #[must_use]
    pub fn new() -> Self {
        Self { done: false }
    }

    /// Re-arm for a fresh request (new `stream_virtual_output` call).
    pub fn reset(&mut self) {
        self.done = false;
    }

    /// Apply a conclusive or non-conclusive stream event: returns the
    /// outcome to deliver if this event is the FIRST conclusive one, else
    /// `None`.
    fn transition(&mut self, outcome: Option<StreamOutcome>) -> Option<StreamOutcome> {
        if self.done {
            return None;
        }
        if let Some(outcome) = outcome {
            self.done = true;
            return Some(outcome);
        }
        None
    }
}

// ============================================================================
// Virtual output manager (Wayland thread owner)
// ============================================================================

/// Thread-side state for the Wayland connection.
struct WlState {
    /// Sender side for commands; `None` once the thread has exited.
    tx: Option<std::sync::mpsc::Sender<WlCommand>>,
}

/// Manages the KWin virtual output: owns the Wayland connection thread,
/// creates the output at requested sizes, and destroys it on release.
///
/// The thread implements **create-before-close**: when replacing a live
/// stream (resize), the previous stream's proxy is kept alive while the
/// replacement is being created, and destroyed only after the replacement's
/// `created` event. This guarantees the enabled-output set never empties
/// mid-session — a zero-outputs window sends plasmashell to its
/// placeholder screen (field-observed: it does not re-latch; black
/// screen). If the replacement fails, the previous stream is restored as
/// the active one.
pub struct VirtualOutputManager {
    wl: RwLock<WlState>,
    /// Output identity (name + kscreen exclusion), passed to the thread.
    config: VirtualOutputConfig,
    /// Set when the Wayland thread has died (compositor gone); the next
    /// create will rebuild it.
    wl_dead: AtomicBool,
}

impl VirtualOutputManager {
    /// A manager using the neutral default output identity (`rdp` →
    /// `Virtual-rdp` in kscreen).
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(VirtualOutputConfig::default())
    }

    /// A manager for a custom virtual-output identity.
    #[must_use]
    pub fn with_config(config: VirtualOutputConfig) -> Self {
        Self {
            wl: RwLock::new(WlState { tx: None }),
            config,
            wl_dead: AtomicBool::new(true),
        }
    }

    /// Ensure the Wayland thread exists, creating it if needed.
    async fn ensure_wl_thread(&self) -> Result<std::sync::mpsc::Sender<WlCommand>> {
        if self.wl_dead.load(Ordering::Acquire) {
            let mut guard = self.wl.write().await;
            if guard.tx.is_none() {
                let (tx, rx) = std::sync::mpsc::channel::<WlCommand>();
                let config = self.config.clone();
                std::thread::Builder::new()
                    .name("kwin-zkde-screencast".into())
                    .spawn(move || wayland_thread(rx, config))
                    .context("Failed to spawn zkde-screencast thread")?;
                guard.tx = Some(tx);
                self.wl_dead.store(false, Ordering::Release);
                info!("[kwin-virtual] Wayland thread started");
            }
        }
        self.wl
            .read()
            .await
            .tx
            .clone()
            .ok_or_else(|| anyhow::anyhow!("zkde-screencast thread unavailable"))
    }

    /// (Re-)create the virtual output stream at the given size, returning
    /// the PipeWire node id to bind for capture.
    ///
    /// Ensures the fresh output is ENABLED — after EVERY create, not just
    /// the first (a resize recreate can be born disabled exactly like the
    /// initial one; a persisted `kscreen-doctor` disable of our virtual
    /// output makes every later output be born disabled). A disabled
    /// output never gets rendered into: the screencast buffers stay
    /// untouched (all-zero, alpha 0x00) — the black-screen signature.
    /// Enable is idempotent.
    pub async fn recreate_stream(&self, width: u16, height: u16) -> Result<u32> {
        let tx = self.ensure_wl_thread().await?;

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        tx.send(WlCommand::CreateStream {
            width: width as i32,
            height: height as i32,
            reply: reply_tx,
        })
        .map_err(|_| anyhow::anyhow!("zkde-screencast thread exited"))?;

        let node_id = tokio::time::timeout(STREAM_CREATE_TIMEOUT, reply_rx)
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for zkde stream creation"))?
            .map_err(|_| anyhow::anyhow!("zkde stream reply dropped"))?
            .map_err(|e| anyhow::anyhow!("zkde stream creation failed: {e}"))?;

        let output_name = &self.config.name;
        let kscreen_name = self.config.kscreen_name.clone();
        info!(
            "[kwin-virtual] virtual output '{output_name}' @ {width}x{height} streaming on node {node_id}"
        );
        let enable_kscreen = kscreen_name.clone();
        let enabled = tokio::task::spawn_blocking(move || enable_output(&enable_kscreen))
            .await
            .unwrap_or(false);
        if enabled {
            info!("[kwin-virtual] virtual output '{output_name}' enabled");
        } else {
            warn!(
                "[kwin-virtual] virtual output '{output_name}' could NOT be enabled — \
                 session may show a black screen"
            );
        }
        // NORMALIZE TO ORIGIN after every create — but DELAYED past the
        // retiring output's settle-close. Two reasons:
        // 1. KWin >= 6.7 parks a new virtual output beside the stale
        //    geometry of other outputs — including the DISABLED physical
        //    one AND the RETIRING virtual one on a resize (create-before-
        //    close keeps it alive for SETTLE_CLOSE_MS). An immediate move
        //    to (0,0) would overlap the retiring output at (0,0); kscreen
        //    may refuse or revert overlapping positions. After the close,
        //    the new output is the only one left and the move is clean.
        // 2. KWin < 6.7 already normalized; the check is a cheap no-op.
        // Layered with the guard's engage normalization and the fork's
        // blank-capture heal; failures only log.
        let norm_kscreen = kscreen_name.clone();
        tokio::spawn(async move {
            // PROACTIVE CONTAINMENT REATTACH, twice:
            //
            // Every output recreate gives the replacement a fresh screen
            // id; the desktop containment stays glued to the DEAD one and
            // plasmashell re-latches only after its async output-bind —
            // on Plasma 6.3/6.7 measured as: either nothing adopts the
            // new screen (panel-less wedge) or Plasma spawns a REPLACEMENT
            // containment with DEFAULT wallpaper (the churned-Parrot
            // "generic KDE background"). Re-adopting the ORIGINAL
            // containment early — before Plasma manufactures a
            // duplicate — keeps the user's desktop (wallpaper, panel,
            // icon positions) and cuts the visible relayout from ~20s
            // (settle-gated fault heal) to ~2s.
            //
            // Two passes: t1 = SETTLE_CLOSE_MS (750ms, right as the
            // retiring output closes) catches the fastest re-creates;
            // t2 = +3.3s catches slow binds (KWin 6.7 observed adopting
            // up to ~3s after close). The adoption rule inside skips
            // when screen 0 is already occupied, so a healthy layout is
            // never disturbed — at most one gdbus round-trip is wasted.
            for delay_ms in [
                SETTLE_CLOSE_MS.as_millis() as u64,
                SETTLE_CLOSE_MS.as_millis() as u64 + 2_500,
            ] {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                let adopted = tokio::task::spawn_blocking(
                    reattach_plasmashell_containments,
                )
                .await
                .unwrap_or(0);
                info!(
                    adopted,
                    delay_ms,
                    "[kwin-virtual] proactive containment reattach pass complete"
                );
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            let name = norm_kscreen;
            let norm = tokio::task::spawn_blocking(move || {
                normalize_virtual_output_origin(&name)
            })
            .await
            .unwrap_or(None);
            if norm != Some((0, 0)) {
                warn!(
                    "[kwin-virtual] virtual output not verified at (0,0) after create settle ({norm:?}) — continuing"
                );
            }
        });
        Ok(node_id)
    }

    /// Close the current stream — KWin destroys the virtual output on
    /// stream close.
    pub async fn close_stream(&self) {
        if let Some(tx) = self.wl.read().await.tx.as_ref() {
            let _ = tx.send(WlCommand::Close);
        }
    }
}

impl Default for VirtualOutputManager {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Wayland thread
// ============================================================================

fn wayland_thread(rx: std::sync::mpsc::Receiver<WlCommand>, config: VirtualOutputConfig) {
    use wayland_client::{Connection, Dispatch, QueueHandle, protocol::wl_registry};

    use wayland_protocols_plasma::screencast::v1::client::{
        zkde_screencast_stream_unstable_v1::Event as StreamEvent,
        zkde_screencast_stream_unstable_v1::ZkdeScreencastStreamUnstableV1,
        zkde_screencast_unstable_v1::{Event as ManagerEvent, Pointer, ZkdeScreencastUnstableV1},
    };

    /// Pending request: the stream proxy + where to send the result. The
    /// reply is consumed on the FIRST conclusive event; the proxy itself
    /// is RETAINED here after conclusion so a later Close command can
    /// destroy the stream (KWin keeps the virtual output alive until the
    /// stream object is destroyed — dropping the proxy too early leaves a
    /// zombie output that blocks every subsequent create).
    type PendingStream = (
        ZkdeScreencastStreamUnstableV1,
        Option<tokio::sync::oneshot::Sender<Result<u32, String>>>,
    );

    /// Per-thread dispatch state.
    struct State {
        screencast: Option<ZkdeScreencastUnstableV1>,
        /// Pending request: see [`PendingStream`] for the retention
        /// contract.
        pending: Option<PendingStream>,
        /// The PREVIOUS stream's proxy, kept alive while its replacement
        /// is being created (create-before-close).
        retiring: Option<ZkdeScreencastStreamUnstableV1>,
        /// A retiring proxy parked for deferred close: (proxy, deadline).
        /// The close is what removes the old virtual output; doing it the
        /// instant the replacement's `created` event arrives still races
        /// plasmashell's ASYNC bind of the new output's wl_registry global
        /// — if the removal is processed first, Qt sees zero outputs,
        /// creates its placeholder screen, and never re-latches (uniform
        /// capture; field-observed during resize churn). Parking the close
        /// for SETTLE_CLOSE_MS lets clients bind first, exactly like the
        /// layout guard's engage settle.
        closing: Option<(
            ZkdeScreencastStreamUnstableV1,
            std::time::Instant,
        )>,
        /// Stream request state machine (conclusive-event bookkeeping).
        stream_sm: StreamRequestMachine,
    }

    impl Dispatch<wl_registry::WlRegistry, ()> for State {
        fn event(
            state: &mut Self,
            registry: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            _: &(),
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            if let wl_registry::Event::Global {
                name,
                interface,
                version,
            } = event
                && interface == "zkde_screencast_unstable_v1"
            {
                // KWin advertises version 6; the plasma bindings (XML
                // v4) cap us at 4 — bind min(server, 4).
                let bind_version = version.min(4);
                let screencast =
                    registry.bind::<ZkdeScreencastUnstableV1, _, State>(name, bind_version, qh, ());
                state.screencast = Some(screencast);
                info!(
                    "[kwin-virtual] bound zkde_screencast_unstable_v1 (global v{version}, bound v{bind_version})"
                );
            }
        }
    }

    impl Dispatch<ZkdeScreencastUnstableV1, ()> for State {
        fn event(
            _: &mut Self,
            _: &ZkdeScreencastUnstableV1,
            _: ManagerEvent,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            // The manager object has no events.
        }
    }

    impl Dispatch<ZkdeScreencastStreamUnstableV1, ()> for State {
        fn event(
            state: &mut Self,
            _stream: &ZkdeScreencastStreamUnstableV1,
            event: StreamEvent,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            // Pure transition, then deliver: the state machine decides
            // whether the event concludes the pending request (first
            // conclusive event wins). Consume ONLY the reply — the stream
            // PROXY is retained in state.pending so a later Close command
            // can destroy the stream (KWin removes the virtual output only
            // on stream destruction; dropping the proxy early leaves a
            // zombie output that wedges all reconnects).
            let outcome = match event {
                StreamEvent::Created { node } => Some(StreamOutcome::Created { node }),
                StreamEvent::Failed { error } => Some(StreamOutcome::Failed { reason: error }),
                StreamEvent::Closed => Some(StreamOutcome::Closed),
                _ => None,
            };
            if let Some(outcome) = state.stream_sm.transition(outcome) {
                // Deliver the reply, then finalize the swap: the retiring
                // stream is destroyed only now (replacement live) or handed
                // back (replacement failed). The reply slot is CONSUMED
                // (taken) on delivery — the oneshot is single-use and the
                // proxy must stay behind for a later Close.
                if let Some((_, reply_slot)) = state.pending.as_mut()
                    && let Some(tx) = reply_slot.take()
                {
                    let _ = tx.send(outcome_to_reply(outcome.clone()));
                }
                match outcome {
                    StreamOutcome::Created { .. } => {
                        // Replacement is live. Destroying the previous stream
                        // NOW would remove the old virtual output before
                        // plasmashell has (asynchronously) bound the new
                        // one's registry global — a zero-outputs window that
                        // sends Qt to its placeholder screen, which then
                        // never re-latches (field-observed as a uniform
                        // capture after resize churn). Park the close for
                        // SETTLE_CLOSE_MS instead; the poll loop completes
                        // it once the settle deadline passes.
                        if let Some(old) = state.retiring.take() {
                            state.closing =
                                Some((old, std::time::Instant::now() + SETTLE_CLOSE_MS));
                            info!(
                                "[kwin-virtual] swap complete — previous stream close parked for settle"
                            );
                        }
                    }
                    StreamOutcome::Failed { .. } | StreamOutcome::Closed => {
                        // Replacement failed: hand the previous stream back
                        // as the active one — its output never died, so the
                        // session keeps working and the caller's fallback
                        // stays truthful. The failed proxy is simply
                        // dropped — no output was ever created, so nothing
                        // lingers server-side. (`Closed` for a PENDING
                        // request likewise never created an output.)
                        if let Some(old) = state.retiring.take() {
                            state.pending = Some((old, None));
                            info!("[kwin-virtual] replacement failed — previous stream restored");
                        }
                    }
                }
            }
        }
    }

    impl Dispatch<wayland_client::protocol::wl_display::WlDisplay, ()> for State {
        fn event(
            _: &mut Self,
            _: &wayland_client::protocol::wl_display::WlDisplay,
            _: wayland_client::protocol::wl_display::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    /// Consume a pending request's reply slot (if any) with an error, on a
    /// fatal thread condition (socket failure, dispatch failure). The
    /// proxy is dropped along with it — the thread is exiting.
    fn reply_error(state: &mut State, msg: String) {
        if let Some((_, reply_slot)) = state.pending.take()
            && let Some(tx) = reply_slot
        {
            let _ = tx.send(Err(msg));
        }
    }

    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => {
            error!("[kwin-virtual] cannot connect to Wayland: {e}");
            // Reply with errors until the channel drains, then exit.
            while let Ok(cmd) = rx.recv() {
                if let WlCommand::CreateStream { reply, .. } = cmd {
                    let _ = reply.send(Err(format!("wayland connection failed: {e}")));
                }
            }
            return;
        }
    };

    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();
    let display = conn.display();
    let _registry = display.get_registry(&qh, ());

    let mut state = State {
        screencast: None,
        pending: None,
        retiring: None,
        closing: None,
        stream_sm: StreamRequestMachine::new(),
    };

    // Initial roundtrip: binds the zkde global (if advertised).
    if let Err(e) = event_queue.roundtrip(&mut state) {
        error!("[kwin-virtual] initial roundtrip failed: {e}");
    }

    if state.screencast.is_none() {
        warn!(
            "[kwin-virtual] zkde_screencast_unstable_v1 not advertised — \
             is this KWin? (The global appears once the screencast plugin \
             has loaded; it may also be absent on non-KDE compositors.)"
        );
    }

    loop {
        // Poll the Wayland socket with a short timeout so queued commands
        // are picked up promptly. blocking_dispatch only wakes on
        // COMPOSITOR events, and an idle desktop (fresh virtual output
        // showing nothing) produces none — commands would then sit
        // unprocessed until a long timeout, wedging resize on connect and
        // every reconnect. std mpsc has no pollable fd, so the command
        // side is a 50ms poll timeout; ≤50ms command latency is fine (the
        // `created` reply is what gates callers, not this loop).
        let mut poll_fds = [nix::poll::PollFd::new(
            conn.as_fd(),
            nix::poll::PollFlags::POLLIN,
        )];
        let rc = match nix::poll::poll(&mut poll_fds, 50u16) {
            Ok(n) => n,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => {
                error!("[kwin-virtual] poll failed: {e} — thread exiting");
                reply_error(&mut state, format!("poll failed: {e}"));
                return;
            }
        };

        // Wayland socket ready: read + dispatch compositor events.
        if rc > 0
            && poll_fds[0]
                .revents()
                .is_some_and(|f| f.contains(nix::poll::PollFlags::POLLIN))
        {
            if let Some(guard) = event_queue.prepare_read() {
                match guard.read() {
                    Ok(_) => {}
                    Err(e) => {
                        error!("[kwin-virtual] wayland socket read failed: {e} — thread exiting");
                        reply_error(&mut state, format!("wayland read failed: {e}"));
                        return;
                    }
                }
            }
            if let Err(e) = event_queue.dispatch_pending(&mut state) {
                error!("[kwin-virtual] dispatch failed: {e} — thread exiting");
                reply_error(&mut state, format!("dispatch failed: {e}"));
                return;
            }
        }

        // Drain queued commands (arriving while parked or during the poll
        // window — try_recv is cheap, and this also covers the case where
        // the channel filled between the poll and now).
        loop {
            match rx.try_recv() {
                Ok(WlCommand::CreateStream {
                    width,
                    height,
                    reply,
                }) => {
                    let Some(screencast) = state.screencast.as_ref() else {
                        let _ = reply.send(Err("zkde_screencast global not bound".into()));
                        continue;
                    };
                    // CREATE-BEFORE-CLOSE: the previous stream (if any)
                    // moves to `retiring` — kept alive, NOT destroyed yet —
                    // so the enabled-output set never empties while the
                    // replacement is being created (see `retiring`). It is
                    // destroyed only after the replacement's `created`
                    // event, or restored if the replacement fails.
                    //
                    // An abandoned swap (a create whose reply timed out
                    // against a wedged KWin, followed by another create) is
                    // cleaned up here: destroy the orphaned previous proxy
                    // so it cannot linger as a zombie output.
                    if let Some(orphan) = state.retiring.take() {
                        warn!(
                            "[kwin-virtual] destroying orphaned stream left by an abandoned swap"
                        );
                        orphan.close();
                    }
                    if let Some((prev, _)) = state.pending.take() {
                        state.retiring = Some(prev);
                    }
                    state.stream_sm.reset();
                    // Pointer mode argument: measured on KWin
                    // 6.3.6 — this argument does NOT control
                    // whether KWin paints the cursor into the virtual
                    // output's frames. Requesting Metadata still yielded a
                    // composited cursor (two pointers with the client's
                    // arrow); requesting Hidden did too. It also does not
                    // yield SPA_META_Cursor: with Metadata requested, cursor
                    // meta was absent on every frame of a whole live session
                    // while the consumer provably requested SPA_META_Cursor
                    // (the capture crate requests it unconditionally).
                    // Embedded (=2) is kept as the declared intent — it
                    // matches the observed behaviour (composited cursor,
                    // context-aware shapes) — but the value passed here is
                    // not the lever it appears to be. The client-side arrow
                    // is suppressed by a transparent pointer shape at the
                    // RDP layer instead.
                    let stream = screencast.stream_virtual_output(
                        config.name.clone(),
                        width,
                        height,
                        // scale: 1.0 — clients express size in physical
                        // pixels; no compositor-side scaling wanted.
                        1.0,
                        u32::from(Pointer::Embedded),
                        &qh,
                        (),
                    );
                    state.pending = Some((stream, Some(reply)));
                    if let Err(e) = conn.flush() {
                        warn!("[kwin-virtual] flush failed: {e}");
                    }
                }
                Ok(WlCommand::Close) => {
                    // The ONLY place the stream is destroyed — this is
                    // what makes KWin remove the virtual output (the proxy
                    // was retained past the concluded request for exactly
                    // this call). A mid-swap retirement goes too: release
                    // means NO virtual output may survive — including one
                    // still parked for settle (its deadline is void once
                    // the session releases).
                    let mut destroyed = false;
                    if let Some((stream, _)) = state.pending.take() {
                        stream.close();
                        destroyed = true;
                    }
                    if let Some(old) = state.retiring.take() {
                        old.close();
                        destroyed = true;
                    }
                    if let Some((parked, _)) = state.closing.take() {
                        parked.close();
                        destroyed = true;
                    }
                    if destroyed {
                        state.stream_sm.reset();
                        if let Err(e) = conn.flush() {
                            warn!("[kwin-virtual] flush failed: {e}");
                        }
                        info!("[kwin-virtual] stream destroyed on Close command");
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // Caller dropped the channel — exit thread.
                    info!("[kwin-virtual] command channel closed, thread exiting");
                    return;
                }
            }
        }

        // Complete any settle-parked close: the replacement output has
        // had SETTLE_CLOSE_MS to be bound by clients; removing the old one
        // is now a plain output change instead of a zero-outputs window.
        // take_due_settle_close keeps the proxy parked until the deadline
        // (a bare take() whose guard fails would drop the proxy without
        // close(), leaking the old virtual output as a live zombie).
        if let Some(parked) = take_due_settle_close(&mut state.closing) {
            parked.close();
            info!("[kwin-virtual] settled — previous stream destroyed after swap");
        }

        // Always flush before the next poll iteration so requests reach
        // KWin.
        if let Err(e) = conn.flush() {
            warn!("[kwin-virtual] flush failed: {e}");
        }
    }
}

/// Translate a conclusive outcome into the oneshot reply form.
fn outcome_to_reply(outcome: StreamOutcome) -> Result<u32, String> {
    match outcome {
        StreamOutcome::Created { node } => Ok(node),
        StreamOutcome::Failed { reason } => Err(reason),
        StreamOutcome::Closed => Err("stream closed by compositor".to_string()),
    }
}

/// Due-check for a settle-parked close: returns the parked item ONLY when
/// its deadline has passed (removing it from the slot); before the deadline
/// the item STAYS PARKED. Extracted from the poll loop so the bookkeeping
/// is unit-testable: the original inline form
/// (`if let Some(x) = closing.take() && now >= deadline`) consumed the
/// proxy on EVERY not-yet-due poll iteration and silently dropped it when
/// the guard failed — each swap leaked its old virtual output as a live
/// zombie (panel theft, off-origin geometry, one zombie per resize;
/// field-observed). The regression tests pin both behaviors.
fn take_due_settle_close<T>(closing: &mut Option<(T, std::time::Instant)>) -> Option<T> {
    let (_, deadline) = closing.as_ref()?;
    if std::time::Instant::now() >= *deadline {
        closing.take().map(|(item, _)| item)
    } else {
        None
    }
}

// ============================================================================
// Output layout guard (kscreen-doctor management)
// ============================================================================

/// Output-management for a virtual-output session: disable the physical
/// (DRM) outputs so the virtual output becomes the primary at (0,0) — the
/// layout that makes the session fully interactive (panel+windows
/// relocate; pointer coordinate chain closes). Re-enables them on drop,
/// physical FIRST (never leave the host blind).
///
/// `kscreen-doctor` is invoked in blocking threads; both directions are
/// best-effort — if it fails, the session still works (the virtual output
/// is usable as a secondary screen, just with the panel elsewhere).
pub struct OutputLayoutGuard {
    /// Connector names that were disabled by this guard.
    disabled: Vec<String>,
    /// The virtual output's kscreen name (exclusion for verification in
    /// Drop — the same name the guard engaged with).
    exclude: String,
}

impl OutputLayoutGuard {
    /// Snapshot enabled non-virtual outputs, then disable them, using the
    /// neutral default output identity.
    ///
    /// Callers must have created the virtual output FIRST so a disable
    /// leaves a valid layout: KWin refuses to disable the ONLY enabled
    /// output ("Disabling all outputs through configuration changes is
    /// not allowed"), which bounces back silently. A disable that bounced
    /// is retried once after the compositor settles.
    pub async fn engage() -> Self {
        Self::engage_with(VirtualOutputConfig::default()).await
    }

    /// [`engage`](Self::engage) with an explicit virtual-output identity:
    /// the guard's kscreen exclusion matches the caller's config (the
    /// manager must be creating outputs under the SAME identity, or the
    /// guard would manage its own output as a physical one).
    pub async fn engage_with(config: VirtualOutputConfig) -> Self {
        // The exclusion name is what the closures need; clone it per
        // spawn_blocking so `config` itself is not moved.
        let exclude0 = config.kscreen_name.clone();
        let exclude1 = config.kscreen_name.clone();
        let mut names =
            tokio::task::spawn_blocking(move || list_enabled_physical_outputs(&exclude0))
                .await
                .unwrap_or_default();
        if names.is_empty() {
            // Already headless (or kscreen unusable). Adopt the
            // connected-but-DISABLED physical outputs anyway: Drop must
            // re-enable the console at release even when the guard engaged
            // a machine that was already blind. Without this, engage
            // snapshotted nothing, Drop restored nothing, and a single
            // headless entry wedged forever (field-observed: every later
            // session streamed a dead shell after its teardown).
            let adopted = tokio::task::spawn_blocking(move || {
                list_recoverable_physical_outputs(&exclude1)
            })
            .await
            .unwrap_or_default();
            if adopted.is_empty() {
                warn!(
                    "[kwin-virtual] no enabled or recoverable physical outputs — console restore at release will be impossible"
                );
                return Self {
                    disabled: Vec::new(),
                    exclude: config.kscreen_name.clone(),
                };
            }
            warn!(
                "[kwin-virtual] layout already headless — adopting {} connected-but-disabled physical output(s) for restore at release",
                adopted.len()
            );
            // They are ALREADY disabled: nothing to disable now, but Drop's
            // re-enable must cover them.
            return Self {
                disabled: adopted,
                exclude: config.kscreen_name.clone(),
            };
        }
        // SETTLE BEFORE DISABLING. The virtual output was created and
        // enabled moments ago; KWin announces it to Wayland clients via a
        // wl_registry global event, but clients bind the new output
        // ASYNCHRONOUSLY. Plasmashell processes pending registry events in
        // its event loop — if the physical output's removal (our kscreen
        // disable, which lands as an immediate wl_output global removal)
        // reaches plasmashell's queue BEFORE it has bound the new virtual
        // output, Qt sees zero outputs and falls to its placeholder screen
        // ("qt.qpa.wayland: There are no outputs - creating placeholder
        // screen") — and once in placeholder mode plasmashell NEVER
        // re-latches onto the later-appearing output (field-observed
        // 2026-09-04, mechanism confirmed 2026-09-16: the desktop keeps
        // rendering into the placeholder while the virtual output scans
        // out an empty desktop — frames flow, zero damage, black client).
        // A settle window here lets plasmashell bind the virtual output
        // first, so the subsequent disable is a plain output change.
        tokio::time::sleep(Duration::from_millis(750)).await;
        info!(
            "[kwin-virtual] disabling physical output(s) for session: [{}]",
            names.join(", ")
        );
        for name in names.iter() {
            let n = name.clone();
            let _ = tokio::task::spawn_blocking(move || disable_output(&n))
                .await
                .unwrap_or(false);
        }
        // VERIFY the disables stuck. KWin/kscreen can silently refuse —
        // notably disabling the only enabled output, which is reverted
        // immediately (the physical output comes back enabled at (0,0) and
        // the captured virtual output stays an empty secondary: black
        // screen). If a disable still bounced, retry once after the
        // compositor settles.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let exclude = config.kscreen_name.clone();
        let still_enabled =
            tokio::task::spawn_blocking(move || list_enabled_physical_outputs(&exclude))
                .await
                .unwrap_or_default();
        let bounced: Vec<String> = names
            .iter()
            .filter(|n| still_enabled.contains(n))
            .cloned()
            .collect();
        if !bounced.is_empty() {
            warn!(
                "[kwin-virtual] disable bounced for [{}] — retrying once after settle",
                bounced.join(", ")
            );
            for name in bounced.iter() {
                let n = name.clone();
                let _ = tokio::task::spawn_blocking(move || disable_output(&n))
                    .await
                    .unwrap_or(false);
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
            let exclude = config.kscreen_name.clone();
            let final_enabled =
                tokio::task::spawn_blocking(move || list_enabled_physical_outputs(&exclude))
                    .await
                    .unwrap_or_default();
            let stuck: Vec<String> = names
                .iter()
                .filter(|n| final_enabled.contains(n))
                .cloned()
                .collect();
            if !stuck.is_empty() {
                warn!(
                    "[kwin-virtual] outputs still enabled after retry: [{}] — continuing (panel may be elsewhere)",
                    stuck.join(", ")
                );
            }
            // Only bookkeep the ones that actually disabled.
            names.retain(|n| !final_enabled.contains(n));
        }
        // NORMALIZE the virtual output to (0,0) AFTER the physical
        // disables: with the physical output's stale geometry vacated,
        // KWin >= 6.7 may still have the virtual output parked beside
        // the disabled output's old extent (it does not re-normalize;
        // 6.3 did). plasmashell maps its desktop containment by screen
        // — an off-origin virtual output gets no desktop rendered into
        // it: uniformly blank capture on a healthy frame pipeline
        // (measured live, Plasma 6.7.4). Verified-move with one retry;
        // a failure logs and continues (the fork's blank-capture heal
        // remains as backstop).
        let norm_name = config.kscreen_name.clone();
        let norm = tokio::task::spawn_blocking(move || {
            normalize_virtual_output_origin(&norm_name)
        })
        .await
        .unwrap_or(None);
        if norm != Some((0, 0)) {
            warn!(
                "[kwin-virtual] virtual output not verified at (0,0) after engage ({norm:?})"
            );
        }
        Self {
            disabled: names,
            exclude: config.kscreen_name,
        }
    }

    /// The connector names this guard disabled (for diagnostics).
    pub fn disabled_outputs(&self) -> &[String] {
        &self.disabled
    }

    /// Restore the physical outputs NOW and settle — the caller-facing
    /// counterpart of Drop for the release path.
    ///
    /// [`release_after_client`](super) must call this BEFORE closing the
    /// zkde stream: the close destroys the virtual output, and KWin-side
    /// the physical outputs must not only be re-enabled but BOUND again by
    /// clients before that removal lands. Plasmashell binds a
    /// re-announced wl_output asynchronously; destroying the only other
    /// output in the same breath races that bind — Qt sees zero outputs,
    /// creates its placeholder screen, and (KWin 6.7-era shells) never
    /// re-latches (field-observed: every reconnect after a
    /// resolution-change disconnect rendered a dead shell). The settle
    /// window mirrors the engage settle.
    ///
    /// Idempotent: after the first call the guard holds nothing, and a
    /// later Drop is a no-op.
    pub async fn finish(&mut self) {
        let names = std::mem::take(&mut self.disabled);
        if names.is_empty() {
            return;
        }
        restore_physical_outputs(&names, &self.exclude).await;
        tokio::time::sleep(Duration::from_millis(750)).await;
    }
}

impl Drop for OutputLayoutGuard {
    fn drop(&mut self) {
        // Physical FIRST — the machine must never be left without a
        // display if the virtual output died first.
        let names = std::mem::take(&mut self.disabled);
        let exclude = std::mem::take(&mut self.exclude);
        if names.is_empty() {
            return;
        }
        restore_physical_outputs_blocking(&names, &exclude);
    }
}

/// Verified restore (enable, settle, verify, retry once, truthful logs) —
/// the async entry for [`OutputLayoutGuard::finish`]. A bounced enable is
/// not cosmetic: it leaves the machine headless and plasmashell falls to
/// its placeholder screen, NEVER re-latching onto later outputs
/// (field-observed: a re-enable that raced KWin's teardown of the
/// just-destroyed virtual output bounced silently; the machine stayed
/// headless and every subsequent session was a black screen with frames
/// flowing).
async fn restore_physical_outputs(names: &[String], exclude: &str) {
    let names0 = names.to_vec();
    let exclude0 = exclude.to_string();
    tokio::task::spawn_blocking(move || restore_physical_outputs_blocking(&names0, &exclude0))
        .await
        .ok();
}

/// Blocking core shared by Drop and [`restore_physical_outputs`].
fn restore_physical_outputs_blocking(names: &[String], exclude: &str) {
    for name in names {
        enable_output(name);
    }
    std::thread::sleep(Duration::from_millis(300));
    let still_disabled: Vec<String> = {
        let enabled_now = list_enabled_physical_outputs(exclude);
        names
            .iter()
            .filter(|n| !enabled_now.contains(n))
            .cloned()
            .collect()
    };
    if !still_disabled.is_empty() {
        warn!(
            "[kwin-virtual] re-enable bounced for [{}] — retrying once after settle",
            still_disabled.join(", ")
        );
        for name in &still_disabled {
            enable_output(name);
        }
        std::thread::sleep(Duration::from_millis(300));
        let enabled_final = list_enabled_physical_outputs(exclude);
        let stuck: Vec<String> = names
            .iter()
            .filter(|n| !enabled_final.contains(n))
            .cloned()
            .collect();
        if !stuck.is_empty() {
            error!(
                "[kwin-virtual] physical output(s) STILL disabled after retry: [{}] — the machine may be headless and plasmashell may be stuck on its placeholder screen",
                stuck.join(", ")
            );
        }
        let restored: Vec<String> = names
            .iter()
            .filter(|n| enabled_final.contains(n))
            .cloned()
            .collect();
        if !restored.is_empty() {
            info!(
                "[kwin-virtual] physical output(s) re-enabled: [{}]",
                restored.join(", ")
            );
        }
    } else {
        info!(
            "[kwin-virtual] physical output(s) re-enabled: [{}]",
            names.join(", ")
        );
    }
}

/// Parse `kscreen-doctor -o` output: names of ENABLED outputs that are not
/// our virtual one. Best-effort — returns empty on any failure.
///
/// NOTE on naming: the exclusion is EXACT (`exclude_kscreen_name` — the
/// full kscreen name of our zkde-created output, e.g.
/// `Virtual-{name}`). It must NOT be a "Virtual-" prefix match: a DRM
/// virtual connector (e.g. Hyper-V's) may itself be named `Virtual-1`,
/// and that IS a physical output this guard must manage (disabling it is
/// the whole point — panel relocation + origin placement). A prefix
/// exclusion would skip the DRM output entirely and leave a two-screen
/// layout that breaks pointer coordinate mapping.
fn list_enabled_physical_outputs(exclude_kscreen_name: &str) -> Vec<String> {
    let out = match std::process::Command::new("kscreen-doctor")
        .arg("-o")
        .stdin(std::process::Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        Ok(o) => {
            // Non-zero exit: the compositor may be mid-restart or kscreen
            // not ready. Log it — an empty list here would otherwise look
            // identical to "already headless" in the journal.
            warn!(
                "[kwin-virtual] kscreen-doctor -o failed (exit {:?}): {}",
                o.status.code(),
                String::from_utf8_lossy(&o.stderr).trim()
            );
            return Vec::new();
        }
        Err(e) => {
            warn!("[kwin-virtual] cannot run kscreen-doctor: {e}");
            return Vec::new();
        }
    };

    let names = parse_enabled_physical_outputs(&out, exclude_kscreen_name);
    if names.is_empty() {
        // Either genuinely headless, or kscreen reported nothing usable.
        // The first 400 chars make the difference diagnosable in the log.
        warn!(
            "[kwin-virtual] kscreen-doctor listed no enabled physical outputs. Raw output head: {:?}",
            out.chars().take(400).collect::<String>()
        );
    }
    names
}

/// Pure parser: given `kscreen-doctor -o` text, return enabled output
/// names excluding `exclude_kscreen_name` (our own virtual output's full
/// kscreen connector name).
///
/// The exclusion is EXACT-match (see [`VirtualOutputConfig`]): a DRM
/// virtual connector (e.g. Hyper-V's `Virtual-1`) is itself a physical
/// output the layout guard must manage, so the match may never be a
/// `"Virtual-"` prefix.
///
/// The text form is a sequence of blocks:
///
/// ```text
/// Output: 1 Virtual-1
///     enabled
///     connected
///     priority 1
///     ...
/// ```
///
/// Blocks open with an "Output: N \<name\>" line; a bare "enabled" line
/// marks the block's output enabled. Only enabled, non-virtual names are
/// collected, in order.
pub fn parse_enabled_physical_outputs(
    kscreen_text: &str,
    exclude_kscreen_name: &str,
) -> Vec<String> {
    parse_physical_outputs(kscreen_text, exclude_kscreen_name, false)
}

/// Pure parser: enabled AND connected-but-disabled physical outputs.
/// Used by the layout guard when it engages an already-headless layout:
/// adopting the disabled physical outputs lets Drop re-enable them at
/// release, so "engaged headless" cannot permanently wedge the console
/// (the pre-fix behavior: engage found nothing ENABLED to snapshot, Drop
/// had nothing to restore, and the machine stayed blind forever).
pub fn parse_recoverable_physical_outputs(
    kscreen_text: &str,
    exclude_kscreen_name: &str,
) -> Vec<String> {
    parse_physical_outputs(kscreen_text, exclude_kscreen_name, true)
}

fn parse_physical_outputs(
    kscreen_text: &str,
    exclude_kscreen_name: &str,
    include_disabled: bool,
) -> Vec<String> {
    // kscreen-doctor colorizes its output unconditionally (even piped), so
    // ANSI escape sequences sit between the marker words and the values
    // (e.g. "\u{1b}[01;32mOutput: \u{1b}[0;0m1 Virtual-1"). Strip them
    // BEFORE matching, or every comparison misses.
    let stripped = strip_ansi(kscreen_text);

    let mut result = Vec::new();
    let mut current_name: Option<String> = None;
    let mut current_enabled = false;
    let mut current_connected = false;
    for line in stripped.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Output: ") {
            // Flush previous block: enabled always qualifies; disabled
            // only in the recoverable variant AND only when the connector
            // is still connected (an absent display can't be re-enabled
            // into anything useful).
            let qualifies = current_enabled
                || (include_disabled && current_connected && !current_enabled);
            if let (Some(name), true) = (&current_name, qualifies)
                && name != exclude_kscreen_name
            {
                result.push(name.clone());
            }
            // "1 Virtual-1" -> name is everything after the index.
            //
            // KWin >= 6.7 appends the output's UUID ("1 Virtual-1 <uuid>"),
            // so the connector name is only the FIRST token after the index
            // — anything after it is metadata, never part of the name. Older
            // KWin (< 6.7) has no further tokens, so the token-split
            // degenerates to the same value.
            current_name = rest
                .split_once(' ')
                .and_then(|(_, n)| n.split(' ').next())
                .map(str::to_string);
            current_enabled = false;
            current_connected = false;
        } else if line == "enabled" {
            current_enabled = true;
        } else if line == "disabled" {
            current_enabled = false;
        } else if line == "connected" {
            current_connected = true;
        }
    }
    // Flush the final block (no trailing "Output:" line).
    let qualifies =
        current_enabled || (include_disabled && current_connected && !current_enabled);
    if let (Some(name), true) = (&current_name, qualifies) && name != exclude_kscreen_name
    {
        result.push(name.clone());
    }
    result
}

/// Remove ANSI escape sequences (ESC [ ... final-byte). kscreen-doctor
/// emits color codes even when stdout is a pipe. A bare ESC not followed
/// by `[` drops only the ESC itself; the following character survives.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // Only a CSI sequence (ESC '[') is consumed as a unit; anything
        // else after a bare ESC keeps its next character (peek, not
        // consume).
        if chars.clone().next() == Some('[') {
            chars.next(); // consume the '['
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
    }
    out
}

/// Like [`list_enabled_physical_outputs`] but ALSO returns
/// connected-but-disabled outputs (see
/// [`parse_recoverable_physical_outputs`]).
fn list_recoverable_physical_outputs(exclude_kscreen_name: &str) -> Vec<String> {
    let out = match std::process::Command::new("kscreen-doctor")
        .arg("-o")
        .stdin(std::process::Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        Ok(o) => {
            warn!(
                "[kwin-virtual] kscreen-doctor -o failed (exit {:?}): {}",
                o.status.code(),
                String::from_utf8_lossy(&o.stderr).trim()
            );
            return Vec::new();
        }
        Err(e) => {
            warn!("[kwin-virtual] cannot run kscreen-doctor: {e}");
            return Vec::new();
        }
    };
    parse_recoverable_physical_outputs(&out, exclude_kscreen_name)
}

/// Disable a kscreen output by connector name (blocking; returns success).
pub fn disable_output(name: &str) -> bool {
    std::process::Command::new("kscreen-doctor")
        .arg(format!("output.{name}.disable"))
        .stdin(std::process::Stdio::null())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A single output's kscreen geometry: position + mode size, and whether
/// kscreen reports it enabled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputGeometry {
    /// Connector name (first token after the index; KWin >= 6.7 appends a
    /// UUID which is NOT part of the name).
    pub name: String,
    /// Enabled per kscreen.
    pub enabled: bool,
    /// Output position in the compositor's global layout space.
    pub x: i32,
    pub y: i32,
    /// Mode size in pixels.
    pub width: i32,
    pub height: i32,
}

/// Pure parser: extract every output's geometry from `kscreen-doctor -o`
/// text (ANSI-stripped first). Blocks look like:
///
/// ```text
/// Output: 1 Virtual-lamco be96cb63-...
///     enabled
///     connected
///     ...
///     Geometry: 1920,0 1366x768
/// ```
///
/// KWin < 6.7 has no UUID token; the parser takes the geometry line as a
/// whole so both formats parse identically. Outputs without a Geometry
/// line keep a zeroed geometry (they stay listed — callers match by name
/// and never mistake them for ours).
pub fn parse_output_geometries(kscreen_text: &str) -> Vec<OutputGeometry> {
    let stripped = strip_ansi(kscreen_text);
    let mut result = Vec::new();
    let mut current: Option<OutputGeometry> = None;
    for line in stripped.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Output: ") {
            if let Some(done) = current.take() {
                result.push(done);
            }
            // "1 Virtual-lamco <uuid>" -> name is the token after the
            // index only (UUID is metadata, never part of the name).
            let name = rest
                .split_once(' ')
                .and_then(|(_, n)| n.split(' ').next())
                .unwrap_or_default()
                .to_string();
            current = Some(OutputGeometry {
                name,
                enabled: false,
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            });
        } else if let Some(geo) = current.as_mut() {
            if line == "enabled" {
                geo.enabled = true;
            } else if let Some(rest) = line.strip_prefix("Geometry: ") {
                // "1920,0 1366x768" -> ((1920, 0), (1366, 768)).
                let mut parts = rest.split_whitespace();
                let pos = parts.next().unwrap_or_default();
                let size = parts.next().unwrap_or_default();
                let (x, y) = pos
                    .split_once(',')
                    .map(|(x, y)| {
                        (
                            x.trim().parse::<i32>().unwrap_or(0),
                            y.trim().parse::<i32>().unwrap_or(0),
                        )
                    })
                    .unwrap_or((0, 0));
                let (w, h) = size
                    .split_once('x')
                    .map(|(w, h)| {
                        (
                            w.trim().parse::<i32>().unwrap_or(0),
                            h.trim().parse::<i32>().unwrap_or(0),
                        )
                    })
                    .unwrap_or((0, 0));
                geo.x = x;
                geo.y = y;
                geo.width = w;
                geo.height = h;
            }
        }
    }
    if let Some(done) = current.take() {
        result.push(done);
    }
    result
}

/// List every output's geometry (blocking; empty on any kscreen failure).
fn list_output_geometries() -> Vec<OutputGeometry> {
    let out = match std::process::Command::new("kscreen-doctor")
        .arg("-o")
        .stdin(std::process::Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => return Vec::new(),
    };
    parse_output_geometries(&out)
}

/// Move an output to a position via kscreen-doctor (blocking). Returns
/// success of the COMMAND, not whether the position stuck — pair with
/// [`list_output_geometries`] for verification.
fn position_output(name: &str, x: i32, y: i32) -> bool {
    std::process::Command::new("kscreen-doctor")
        .arg(format!("output.{name}.position.{x},{y}"))
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Ensure our virtual output is the leftmost/topmost screen: position it
/// at (0,0) and VERIFY the move stuck, retrying once. Returns the final
/// verified position.
///
/// WHY this exists: KWin >= 6.7 positions a newly created virtual output
/// to avoid overlapping other outputs in the layout — including DISABLED
/// ones, whose stale geometry still occupies layout space. With the
/// physical output disabled by the layout guard, the virtual output can
/// end up parked at e.g. (1920,0), never normalized to the origin. KWin
/// 6.3 auto-normalized; 6.7 does not. plasmashell then maps its desktop
/// containment to the wrong screen and renders nothing — the capture is
/// genuinely black while the frame pipeline logs healthy (measured live
/// on Plasma 6.7.4: virtual output at (1920,0), client black at
/// 327/330 frames acked).
///
/// On KWin versions that already normalize (6.3 et al.) the position
/// command is a no-op move to where the output already sits — harmless.
///
/// Returns `None` when the output cannot be found or the position cannot
/// be verified after retry — callers treat that as best-effort failure
/// (log, continue; the heal path may still recover).
fn normalize_virtual_output_origin(kscreen_name: &str) -> Option<(i32, i32)> {
    let read = || {
        list_output_geometries()
            .into_iter()
            .find(|g| g.name == kscreen_name && g.enabled)
    };
    let attempt = |retry_pending: bool| -> Option<(i32, i32)> {
        let geo = read()?;
        if geo.x == 0 && geo.y == 0 {
            // Already normalized (KWin did it, or a previous call) — no
            // command needed.
            return Some((0, 0));
        }
        info!(
            "[kwin-virtual] virtual output '{kscreen_name}' parked at {},{} — moving to (0,0) (KWin >= 6.7 keeps stale side-by-side positions; 6.3 normalized)",
            geo.x, geo.y
        );
        if !position_output(kscreen_name, 0, 0) {
            return None;
        }
        // Give KWin a beat to apply the move before verifying.
        std::thread::sleep(Duration::from_millis(300));
        let after = read()?;
        if after.x == 0 && after.y == 0 {
            info!("[kwin-virtual] virtual output '{kscreen_name}' normalized to (0,0)");
            Some((0, 0))
        } else if retry_pending {
            None // caller retries once
        } else {
            warn!(
                "[kwin-virtual] virtual output '{kscreen_name}' position move did not stick ({},{} after retry) — continuing best-effort",
                after.x, after.y
            );
            Some((after.x, after.y))
        }
    };
    attempt(true).or_else(|| attempt(false))
}

/// Restart plasmashell (blocking; returns whether a restart was issued
/// and the process came back).
///
/// WHY: a plasmashell that has fallen to its placeholder screen (zero
/// bound wl_outputs at its registry-event level) never re-latches onto
/// later outputs — the desktop keeps rendering into the placeholder
/// while the virtual output scans out an empty desktop. A restart is
/// the only observed way to force a clean re-bind of every output
/// (field-observed 2026-09-18: after moving the virtual output to the
/// origin the capture stayed uniformly blank until plasmashell was
/// restarted, then the desktop appeared within seconds).
///
/// Mechanism ladder (portable across Plasma launches):
/// 1. `plasma-plasmashell.service` ACTIVE → `systemctl --user restart`
///    (Plasma's own supervision respawns it; also correct for
///    `--no-respawn` units).
/// 2. Otherwise (unit inactive/dead, process dbus-launched via kstart):
///    graceful `kquitapp6`, wait out exit, then `kstart` to relaunch
///    inside the session. A hung shell gets TERM'd after the grace
///    window.
fn restart_plasmashell() -> bool {
    use std::process::Command;
    let run = |cmd: &str, args: &[&str]| {
        Command::new(cmd)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    let plasmashell_alive = || run("pgrep", &["-x", "plasmashell"]);

    if run("systemctl", &["--user", "is-active", "--quiet", "plasma-plasmashell.service"]) {
        info!("[kwin-virtual] restarting plasmashell via systemd unit");
        if run("systemctl", &["--user", "restart", "plasma-plasmashell.service"]) {
            std::thread::sleep(Duration::from_millis(1500));
            return plasmashell_alive();
        }
        return false;
    }

    info!("[kwin-virtual] restarting plasmashell via session (kquitapp6 + kstart)");
    run("kquitapp6", &["plasmashell"]);
    // Grace window for a clean exit (up to 5s).
    for _ in 0..10 {
        if !plasmashell_alive() {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    if plasmashell_alive() {
        // Hung shell: force TERM, wait again.
        let _ = Command::new("pkill")
            .args(["-TERM", "-x", "plasmashell"])
            .stdin(std::process::Stdio::null())
            .output();
        for _ in 0..6 {
            if !plasmashell_alive() {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    // Relaunch inside the session.
    run("kstart", &["plasmashell"]);
    // Wait for it to come up (up to 8s).
    for _ in 0..16 {
        if plasmashell_alive() {
            info!("[kwin-virtual] plasmashell relaunched");
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    warn!("[kwin-virtual] plasmashell did not come back after relaunch");
    false
}

/// Reattach orphaned plasmashell desktop containments to the live screen
/// (blocking; returns how many containments were adopted).
///
/// WHY: when the virtual output is destroyed and recreated (elastic
/// resize), Plasma assigns the NEW output a fresh screen id — but the
/// desktop containment stays glued to the OLD id, which no longer
/// exists: `desktops()` then reports `screen: -1` for it. The result is
/// the "windows floating on black, no panel" wedge: plasmashell runs,
/// windows render, but wallpaper+panel never attach to the captured
/// output. Verified live on Plasma 6.3.6 (Parrot): reassigning the
/// orphaned containment via the shell scripting API restored the full
/// desktop IMMEDIATELY mid-session (taskbar included) at sizes that had
/// failed 10+ consecutive runs — no plasmashell restart needed (and a
/// restart alone does NOT fix 6.3; the stale mapping survives it).
///
/// ADOPTION RULE: an orphan is adopted ONLY when no desktop containment
/// currently sits on screen 0. When one does, Plasma has already spun
/// up a replacement (with DEFAULT wallpaper — the churned-Parrot
/// "generic KDE background") and blindly adopting the orphan would
/// put two containments on one screen; Plasma then evicts one
/// arbitrarily and can bounce them per session (measured: a forced
/// swap reverted at the next session). The duplicate-containment case
/// is prevented instead — by the proactive reattach after every
/// create (see `recreate_stream`), which re-adopts the original before
/// Plasma ever creates a replacement.
fn reattach_plasmashell_containments() -> u32 {
    let script = "var d=desktops();var occupied=false;var best=-1;\
                  for(var i=0;i<d.length;i++){\
                  if(d[i].screen===0){occupied=true}\
                  if(d[i].screen<0&&(best<0||d[i].id<best)){best=d[i].id}}\
                  var n=0;\
                  if(!occupied&&best>=0){\
                  for(var j=0;j<d.length;j++){\
                  if(d[j].id===best){d[j].screen=0;n=1}}}\
                  print(n)";
    let out = match std::process::Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.kde.plasmashell",
            "--object-path",
            "/PlasmaShell",
            "--method",
            "org.kde.PlasmaShell.evaluateScript",
            script,
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            warn!(
                "[kwin-virtual] plasmashell evaluateScript failed (exit {:?}): {}",
                o.status.code(),
                err.lines().last().unwrap_or("").trim()
            );
            return 0;
        }
        Err(e) => {
            // gdbus missing or bus unreachable — the restart escalation
            // remains as the fallback.
            warn!("[kwin-virtual] could not run gdbus for containment reattach: {e}");
            return 0;
        }
    };
    // Output looks like `(0,)' for a print of a number. A failed parse
    // means the script returned something unexpected — treat as 0 but
    // LOG the raw value: the adoption decision is otherwise a black
    // box from the outside (measured: k31 runs showed no line at all
    // and the reason was indeterminable).
    let raw = out.trim();
    let n = raw
        .trim_start_matches('(')
        .trim_end_matches(')')
        .trim()
        .trim_matches(',')
        .trim()
        .parse::<u32>()
        .unwrap_or_else(|_| {
            warn!(
                raw,
                "[kwin-virtual] containment reattach script returned unparseable output"
            );
            0
        });
    if n > 0 {
        info!(
            "[kwin-virtual] reattached {n} orphaned plasmashell containment(s) to the live screen"
        );
    } else {
        // Distinguish the benign skip from the suspicious one:
        // occupied = a containment already owns screen 0 (healthy or
        // duplicate-spawned); no orphan = nothing to adopt.
        let probe = "var d=desktops();var occ=-1;var orph=-1;\
                     for(var i=0;i<d.length;i++){\
                     if(d[i].screen===0){occ=d[i].id}\
                     if(d[i].screen<0){orph=d[i].id}}\
                     print(occ+'+'+orph)";
        let state = std::process::Command::new("gdbus")
            .args([
                "call",
                "--session",
                "--dest",
                "org.kde.plasmashell",
                "--object-path",
                "/PlasmaShell",
                "--method",
                "org.kde.PlasmaShell.evaluateScript",
                probe,
            ])
            .stdin(std::process::Stdio::null())
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        tracing::debug!(
            "[kwin-virtual] containment reattach adopted nothing (state: {})",
            state.trim()
        );
    }
    n
}

/// One-shot compositor-side layout heal for the kwin-virtual strategy:
/// normalize the virtual output to the origin, reattach orphaned desktop
/// containments, and (escalation only) restart plasmashell. Returns
/// `true` when the origin normalization verified (0,0).
///
/// `escalate_restart` is set by the caller on REPEAT heals (the first
/// heal is the surgical path: origin + containment reattach, which
/// fixes both observed fault families — 6.3's orphaned containment and
/// 6.7's wedged-shell states — without disrupting the desktop). A
/// restart is a 40s sledgehammer that does not even restore 6.3's
/// containment mapping, so it is reserved for faults the surgical path
/// could not fix.
pub async fn heal_output_layout(kscreen_name: &str, escalate_restart: bool) -> bool {
    let name = kscreen_name.to_string();
    let normalized = tokio::task::spawn_blocking(move || {
        normalize_virtual_output_origin(&name)
    })
    .await
    .unwrap_or(None);
    let normalized = match normalized {
        Some(pos) => pos,
        None => {
            warn!(
                "[kwin-virtual] layout heal: could not verify '{kscreen_name}' at origin — continuing with containment reattach"
            );
            (0, 0)
        }
    };
    // Surgical containment reattach FIRST — instant, no disruption, and
    // the only thing that fixes Plasma 6.3's orphaned containment.
    let reattached = tokio::task::spawn_blocking(reattach_plasmashell_containments)
        .await
        .unwrap_or(0);
    let restarted = if escalate_restart {
        tokio::task::spawn_blocking(restart_plasmashell)
            .await
            .unwrap_or(false)
    } else {
        false
    };
    info!(
        x = normalized.0,
        y = normalized.1,
        reattached,
        restarted,
        "[kwin-virtual] layout heal complete"
    );
    normalized == (0, 0)
}

/// Enable a kscreen output by connector name (blocking; returns success).
pub fn enable_output(name: &str) -> bool {
    match std::process::Command::new("kscreen-doctor")
        .arg(format!("output.{name}.enable"))
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
    {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            // kscreen-doctor prints its locale nag to stdout; the actual
            // error is the last stderr line.
            let err = String::from_utf8_lossy(&o.stderr);
            let err = err.lines().last().unwrap_or("").trim();
            warn!("[kwin-virtual] kscreen-doctor failed to enable '{name}': {err}");
            false
        }
        Err(e) => {
            warn!("[kwin-virtual] could not run kscreen-doctor: {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // === Geometry parsing (origin normalization) ===

    #[test]
    fn parses_geometries_with_uuid_and_without() {
        // KWin >= 6.7 appends the output UUID after the name; 6.3 does
        // not. Both must yield the same parsed geometry.
        let text_67 = "\
Output: 1 Virtual-lamco be96cb63-6649-4f1d-b22e-fce849119fe3
    enabled
    connected
    priority 0
    Modes: 1:1024x768@60
    Geometry: 1920,0 1366x768
Output: 2 Virtual-1 892482c9-0b93-4a86-b5ea-d1ddce3cb43b
    disabled
    Geometry: 0,0 1920x1080
";
        let text_63 = text_67
            .replace(" be96cb63-6649-4f1d-b22e-fce849119fe3", "")
            .replace(" 892482c9-0b93-4a86-b5ea-d1ddce3cb43b", "");
        let expected = vec![
            OutputGeometry {
                name: "Virtual-lamco".into(),
                enabled: true,
                x: 1920,
                y: 0,
                width: 1366,
                height: 768,
            },
            OutputGeometry {
                name: "Virtual-1".into(),
                enabled: false,
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
        ];
        assert_eq!(parse_output_geometries(&text_67), expected);
        assert_eq!(parse_output_geometries(&text_63), expected);
    }

    #[test]
    fn geometry_parses_with_ansi_noise() {
        // kscreen-doctor colorizes unconditionally; escapes sit between
        // markers and values.
        let text = "\u{1b}[01;32mOutput: \u{1b}[0;0m1 Virtual-lamco\n\t\u{1b}[01;32menabled\u{1b}[0;0m\n\t\u{1b}[01;34mGeometry: \u{1b}[0;0m 1920,0 1366x768\n";
        let geos = parse_output_geometries(text);
        assert_eq!(geos.len(), 1);
        assert_eq!(geos[0].name, "Virtual-lamco");
        assert!(geos[0].enabled);
        assert_eq!((geos[0].x, geos[0].y), (1920, 0));
        assert_eq!((geos[0].width, geos[0].height), (1366, 768));
    }

    #[test]
    fn geometry_missing_outputs_and_negative_positions() {
        // No Geometry line (not yet enumerated): output skipped, others
        // unaffected. Negative positions must parse (outputs CAN sit at
        // negative coordinates in multi-monitor layouts).
        let text = "\
Output: 1 DP-1
    enabled
Output: 2 Virtual-lamco
    enabled
    Geometry: -1920,0 1366x768
";
        let geos = parse_output_geometries(text);
        // DP-1 IS included (zeroed geometry) — skipping entries would
        // hide real outputs from the origin check; the find() callers
        // match by name+enabled and never see it as ours.
        assert_eq!(geos.len(), 2);
        assert_eq!(geos[0].name, "DP-1");
        assert_eq!((geos[0].x, geos[0].y, geos[0].width), (0, 0, 0));
        assert_eq!(geos[1].name, "Virtual-lamco");
        assert_eq!((geos[1].x, geos[1].y), (-1920, 0));
    }

    // === Stream request machine invariants ===

    #[test]
    fn first_conclusive_event_wins() {
        let mut sm = StreamRequestMachine::new();
        assert_eq!(
            sm.transition(Some(StreamOutcome::Created { node: 7 })),
            Some(StreamOutcome::Created { node: 7 })
        );
        // A Closed following the Created must not double-deliver.
        assert_eq!(sm.transition(Some(StreamOutcome::Closed)), None);
        assert_eq!(
            sm.transition(Some(StreamOutcome::Failed {
                reason: "late".into()
            })),
            None
        );
    }

    #[test]
    fn failed_then_closed_does_not_double_deliver() {
        let mut sm = StreamRequestMachine::new();
        assert_eq!(
            sm.transition(Some(StreamOutcome::Failed {
                reason: "nope".into()
            })),
            Some(StreamOutcome::Failed {
                reason: "nope".into()
            })
        );
        assert_eq!(sm.transition(Some(StreamOutcome::Closed)), None);
    }

    #[test]
    fn reset_re_arms_the_machine() {
        let mut sm = StreamRequestMachine::new();
        assert!(sm.transition(Some(StreamOutcome::Closed)).is_some());
        sm.reset();
        assert_eq!(
            sm.transition(Some(StreamOutcome::Created { node: 1 })),
            Some(StreamOutcome::Created { node: 1 })
        );
    }

    #[test]
    fn non_conclusive_events_pass_through() {
        let mut sm = StreamRequestMachine::new();
        assert_eq!(sm.transition(None), None);
        assert_eq!(sm.transition(None), None);
        // Machine still armed.
        assert_eq!(
            sm.transition(Some(StreamOutcome::Created { node: 3 })),
            Some(StreamOutcome::Created { node: 3 })
        );
    }

    // === kscreen-doctor parsing ===

    #[test]
    fn parses_enabled_outputs_in_order() {
        let text = "\
Output: 1 Virtual-1
    enabled
    connected
    priority 1
Output: 2 HDMI-A-1
    disabled
Output: 3 DP-1
    enabled
";
        assert_eq!(
            parse_enabled_physical_outputs(text, "Virtual-lamco"),
            vec!["Virtual-1".to_string(), "DP-1".to_string()]
        );
    }

    #[test]
    fn excludes_our_virtual_output_exactly() {
        let text = "\
Output: 1 Virtual-lamco
    enabled
Output: 2 Virtual-1
    enabled
";
        // Exact-name exclusion: our virtual output is skipped, but the
        // DRM-named Virtual-1 (a real physical output here) is kept.
        assert_eq!(
            parse_enabled_physical_outputs(text, "Virtual-lamco"),
            vec!["Virtual-1".to_string()]
        );
    }

    #[test]
    fn exclusion_name_is_parameterized() {
        // A custom output identity: the exclusion follows the configured
        // kscreen name, and a foreign Virtual-* output is treated as
        // physical (it is not ours).
        let text = "\
Output: 1 Virtual-mine
    enabled
Output: 2 Virtual-lamco
    enabled
";
        assert_eq!(
            parse_enabled_physical_outputs(text, "Virtual-mine"),
            vec!["Virtual-lamco".to_string()]
        );
        // The config derives the kscreen name from the output name.
        let cfg = VirtualOutputConfig::new("mine");
        assert_eq!(cfg.name, "mine");
        assert_eq!(cfg.kscreen_name, "Virtual-mine");
        // The neutral default is rdp → Virtual-rdp (the fork passes its
        // own name explicitly; the default must stay identity-free).
        let dflt = VirtualOutputConfig::default();
        assert_eq!(dflt.name, "rdp");
        assert_eq!(dflt.kscreen_name, "Virtual-rdp");
    }

    #[test]
    fn strips_ansi_before_matching() {
        // kscreen-doctor colorizes unconditionally, even piped.
        let text =
            "\u{1b}[01;32mOutput: \u{1b}[0;0m1 HDMI-A-1\n    \u{1b}[01;32menabled\u{1b}[0m\n";
        assert_eq!(
            parse_enabled_physical_outputs(text, "Virtual-lamco"),
            vec!["HDMI-A-1".to_string()]
        );
    }

    #[test]
    fn final_block_without_trailing_marker_is_flushed() {
        let text = "Output: 1 eDP-1\n    enabled\n";
        assert_eq!(
            parse_enabled_physical_outputs(text, "Virtual-lamco"),
            vec!["eDP-1".to_string()]
        );
    }

    #[test]
    fn empty_text_yields_no_outputs() {
        assert!(parse_enabled_physical_outputs("", "Virtual-lamco").is_empty());
    }

    #[test]
    fn strip_ansi_removes_csi_sequences() {
        assert_eq!(strip_ansi("\u{1b}[01;32mhi\u{1b}[0m"), "hi");
        assert_eq!(strip_ansi("plain"), "plain");
        // ESC without '[' is dropped.
        assert_eq!(strip_ansi("\u{1b}x"), "x");
    }

    // === Guard bookkeeping shape (no kscreen-doctor in CI) ===

    #[tokio::test]
    async fn engage_on_headless_host_is_noop() {
        // No kscreen-doctor binary (or no compositor) in the test
        // environment: engage() must degrade to an empty guard, not panic.
        let guard = OutputLayoutGuard::engage().await;
        assert!(guard.disabled_outputs().is_empty());
    }

    #[tokio::test]
    async fn manager_new_starts_thread_lazily() {
        // Construction must not require a Wayland connection (the thread is
        // created on demand at first recreate_stream).
        let m = VirtualOutputManager::new();
        // close_stream on a fresh manager is a no-op (no thread yet).
        m.close_stream().await;
    }

    // === Settle-parked close bookkeeping ===

    #[test]
    fn settle_close_stays_parked_until_deadline() {
        // REGRESSION: the original inline form consumed the proxy on every
        // not-yet-due poll iteration (`if let Some(x) = take() && due`) and
        // dropped it when the guard failed — one leaked virtual output per
        // swap (zombie outputs stole the panel and shifted geometry).
        let mut closing = Some(("old-stream", std::time::Instant::now() + SETTLE_CLOSE_MS));
        // Simulate many fast poll passes before the deadline.
        for _ in 0..10 {
            assert!(take_due_settle_close(&mut closing).is_none());
            // The item must still be parked, not dropped.
            assert!(closing.is_some(), "parked close was dropped before its deadline");
            assert_eq!(closing.as_ref().map(|(s, _)| *s), Some("old-stream"));
        }
    }

    #[test]
    fn settle_close_returns_once_when_due() {
        let mut closing = Some(("old-stream", std::time::Instant::now() - Duration::from_millis(1)));
        assert_eq!(take_due_settle_close(&mut closing), Some("old-stream"));
        // Consumed exactly once; the slot is empty afterwards.
        assert!(closing.is_none());
        assert!(take_due_settle_close(&mut closing).is_none());
    }

    #[test]
    fn settle_close_empty_slot_is_noop() {
        let mut closing: Option<(u32, std::time::Instant)> = None;
        assert!(take_due_settle_close(&mut closing).is_none());
        assert!(closing.is_none());
    }

    #[test]
    fn settle_window_mirrors_engage_settle() {
        // The teardown settle exists for the same reason as the engage
        // settle (clients bind wl_output globals asynchronously); the two
        // windows are deliberately equal. If either changes deliberately,
        // change both — and update this pin.
        assert_eq!(SETTLE_CLOSE_MS, Duration::from_millis(750));
    }

    // === Recoverable-output parsing (headless-adoption support) ===

    #[test]
    fn recoverable_includes_disabled_but_connected() {
        // A headless layout: the physical output is connected yet disabled
        // (e.g. a previous wedge). The guard must be able to adopt it so
        // Drop can re-enable the console.
        let text = "\
Output: 1 Virtual-1
    disabled
    connected
Output: 2 Virtual-lamco
    enabled
";
        assert_eq!(
            parse_recoverable_physical_outputs(text, "Virtual-lamco"),
            vec!["Virtual-1".to_string()]
        );
    }

    #[test]
    fn recoverable_still_includes_enabled_outputs() {
        let text = "\
Output: 1 Virtual-1
    enabled
    connected
Output: 2 HDMI-A-1
    disabled
    connected
";
        assert_eq!(
            parse_recoverable_physical_outputs(text, "Virtual-lamco"),
            vec!["Virtual-1".to_string(), "HDMI-A-1".to_string()]
        );
    }

    #[test]
    fn recoverable_excludes_disconnected_outputs() {
        // A disconnected connector cannot be re-enabled into anything
        // useful; adopting it would make Drop's verified-restore log noise.
        let text = "\
Output: 1 HDMI-A-1
    disabled
Output: 2 Virtual-1
    disabled
    connected
";
        assert_eq!(
            parse_recoverable_physical_outputs(text, "Virtual-lamco"),
            vec!["Virtual-1".to_string()]
        );
    }

    #[test]
    fn recoverable_excludes_our_virtual_output() {
        let text = "\
Output: 1 Virtual-lamco
    disabled
    connected
Output: 2 Virtual-1
    disabled
    connected
";
        assert_eq!(
            parse_recoverable_physical_outputs(text, "Virtual-lamco"),
            vec!["Virtual-1".to_string()]
        );
    }

    #[test]
    fn enabled_parser_never_returns_disabled_outputs() {
        // The classic parser must remain enabled-only: the recoverable
        // variant is exclusively for the headless-adoption path.
        let text = "\
Output: 1 Virtual-1
    disabled
    connected
Output: 2 HDMI-A-1
    enabled
";
        assert_eq!(
            parse_enabled_physical_outputs(text, "Virtual-lamco"),
            vec!["HDMI-A-1".to_string()]
        );
    }
}
