//! Transparent pointer-shape PDU fields, derived from the MS-RDPBCGR
//! color-pointer specification — for taking pointer ownership on clients
//! that ignore `HidePointer` (SYSPTR_NULL) and fall back to their local
//! arrow (measured on Hyper-V vmconnect, ).
//!
//! A **transparent color-pointer shape** makes the client swap its local
//! cursor for one that renders nothing: with every AND-mask bit set and
//! every XOR byte zero, each covered pixel keeps its screen content
//! (AND=1 → "screen pixel", XOR=0 → "leave unchanged"). This is the
//! xrdp-proven mechanism for clients that treat a null-pointer system
//! pointer as "no server cursor" rather than "draw nothing".
//!
//! The [`TransparentPointer`] produced here carries the fields of the
//! wire structures defined by MS-RDPBCGR §2.2.9.1.1.4
//! (TS_COLORPOINTERATTRIBUTE, used by both the Fast-Path
//! TS_FP_POINTERPOINTERATTRIBUTE and the Color Pointer Update): cache
//! index, hotspot, dimensions, and the two masks. This crate deliberately
//! does **not** depend on any RDP protocol crate; encode against your
//! protocol implementation at the call site. Mask rows are laid out
//! exactly as the wire format requires (see [`TransparentPointer`]).
//!
//! Re-send periodically rather than once: client pointer state can be
//! reset after the initial send (EGFX ResetGraphics, resize-driven
//! Deactivate/Reactivate, capability re-exchange). The PDU is small and
//! idempotent. [`ResendCounter`] implements the cadence: send immediately
//! on first call, then every `interval`-th frame.

/// Classic monochrome-and-color cursor size every client accepts; also the
/// size the color-pointer XOR mask supports without the capability-gated
/// New Pointer Update path.
pub const TRANSPARENT_POINTER_SIZE: u16 = 32;

/// Fields of a fully transparent color-pointer shape, per MS-RDPBCGR.
///
/// Wire layout notes (per spec):
///
/// - `cache_index`: TS_POINTERPOINTERATTRIBUTE cacheIndex (2 bytes) —
///   clients cache pointer shapes; index 0 with a full re-send is always
///   safe.
/// - `hot_x` / `hot_y`: hotspot in pixels relative to the top-left.
/// - `width` / `height`: shape dimensions in pixels.
/// - `and_mask`: monochrome AND mask, **1 bit per pixel**, MSB-first
///   within each byte, each row padded to a 2-byte boundary. Bit set
///   (1) = pixel keeps the screen content.
/// - `xor_mask`: color XOR mask at **24 bpp (3 bytes/pixel)** —
///   TS_COLORPOINTERATTRIBUTE fixes this at 24 bpp; 32 bpp would only be
///   legal via New Pointer Update's `xorBpp`, which is capability-gated
///   — each row padded to a 2-byte boundary. Zero bytes leave the
///   (AND-preserved) screen pixels unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransparentPointer {
    /// Cache slot the client should store the shape under.
    pub cache_index: u16,
    /// Hotspot X, pixels from the shape's left edge.
    pub hot_x: u16,
    /// Hotspot Y, pixels from the shape's top edge.
    pub hot_y: u16,
    /// Shape width in pixels.
    pub width: u16,
    /// Shape height in pixels.
    pub height: u16,
    /// AND mask: 1 bit/pixel, rows padded to 2 bytes; all bits set here.
    pub and_mask: Vec<u8>,
    /// XOR mask: 24 bpp, rows padded to 2 bytes; all zeros here.
    pub xor_mask: Vec<u8>,
}

impl TransparentPointer {
    /// Build the canonical transparent shape: `32×32`, all-opaque AND
    /// mask, all-zero 24-bpp XOR mask, hotspot at the top-left.
    ///
    /// `32×32` needs no row padding (4 bytes AND/row, 96 bytes XOR/row —
    /// both multiples of 2), and the color-pointer path is not gated on
    /// the New Pointer capability the way RGBAPointer is.
    #[must_use]
    pub fn new_32x32() -> Self {
        let width = TRANSPARENT_POINTER_SIZE;
        let height = TRANSPARENT_POINTER_SIZE;

        // AND mask: one bit per pixel, 32 px per row = 32 bits = 4 bytes
        // (no 2-byte padding needed at this width). All bits set: every
        // pixel is declared a "screen" pixel.
        let and_mask = vec![0xFF; usize::from(width) * 4];

        // XOR mask: 24 bpp = 3 bytes/pixel × 32 px = 96 bytes/row (no
        // padding). All zero: screen pixels are left unchanged, so the
        // swapped-in cursor renders nothing.
        let xor_mask = vec![0x00; usize::from(width) * usize::from(height) * 3];

        Self {
            cache_index: 0,
            hot_x: 0,
            hot_y: 0,
            width,
            height,
            and_mask,
            xor_mask,
        }
    }

    /// Total wire size of the two masks, in bytes (with the spec's
    /// 2-byte-per-row padding already applied by [`new_32x32`]).
    #[must_use]
    pub fn mask_bytes(&self) -> usize {
        self.and_mask.len() + self.xor_mask.len()
    }
}

/// Frame counter gating the periodic transparent-shape re-send.
///
/// The first call after construction returns `true` (the shape goes out
/// immediately on mode entry); afterwards a re-send fires every
/// `interval`-th call. Interior-mutable (shared atomic) so a frame loop
/// can tick it without a mutex, and `Clone` so clones share one counter.
#[derive(Debug, Clone, Default)]
pub struct ResendCounter {
    frames: std::sync::Arc<std::sync::atomic::AtomicU32>,
    interval: u32,
}

impl ResendCounter {
    /// A counter that re-sends every `interval` processed frames (after
    /// the immediate first send). Panics if `interval` is zero.
    #[must_use]
    pub fn new(interval: u32) -> Self {
        assert!(interval > 0, "re-send interval must be > 0");
        Self {
            frames: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
            interval,
        }
    }

    /// Whether this frame should carry a periodic re-send. Ticks the
    /// counter on every call. The FIRST call returns `true` (the shape
    /// goes out immediately on mode entry); afterwards a re-send fires
    /// every `interval`-th call.
    pub fn should_send(&self) -> bool {
        let n = self
            .frames
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        n == 0 || n.is_multiple_of(self.interval)
    }

    /// The configured interval (for diagnostics).
    pub fn interval(&self) -> u32 {
        self.interval
    }
}

/// Decides when a server-observed pointer position may be sent to the
/// client as a Pointer Position update (MS-RDPBCGR §2.2.9.1.1.4.2).
///
/// A client handles that PDU by **moving its local pointer** there. When
/// the position comes from compositor cursor metadata, most positions are
/// echoes of the client's own mouse input, a few frames late; forwarding
/// them drags the local pointer back to where it was 50-100 ms earlier
/// while the user moves it (measured on Hyper-V vmconnect against GNOME's
/// metadata cursor: a lagging trail of pointers). Only a position the
/// guest produced on its own — an application warp — belongs on the wire.
///
/// The filter suppresses a position while client input is recent (within
/// `quiet`) or when it lies within `tolerance` pixels (per axis) of where
/// the client last put — or was last sent — the pointer, so coordinate
/// rounding between the client and stream spaces never reads as a warp.
#[derive(Debug, Clone)]
pub struct PointerEchoFilter {
    quiet: std::time::Duration,
    tolerance: u16,
    /// Where the client's local pointer is believed to be, and when the
    /// client last moved it (`None` for a position this filter sent).
    client_pointer: Option<((u16, u16), Option<std::time::Instant>)>,
}

impl Default for PointerEchoFilter {
    fn default() -> Self {
        Self::new(Self::DEFAULT_QUIET, Self::DEFAULT_TOLERANCE)
    }
}

impl PointerEchoFilter {
    /// Long enough for the compositor's trailing cursor samples after the
    /// last client move to arrive (a few frames plus pipeline latency).
    pub const DEFAULT_QUIET: std::time::Duration = std::time::Duration::from_millis(500);
    /// Per-axis slack for client/stream coordinate rounding (e.g. a client
    /// at 1920x1200 against a 1920x1080 capture).
    pub const DEFAULT_TOLERANCE: u16 = 4;

    /// A filter with the given quiet period and per-axis tolerance.
    #[must_use]
    pub fn new(quiet: std::time::Duration, tolerance: u16) -> Self {
        Self {
            quiet,
            tolerance,
            client_pointer: None,
        }
    }

    /// Record an absolute pointer move the client sent.
    pub fn note_client_move(&mut self, position: (u16, u16), now: std::time::Instant) {
        self.client_pointer = Some((position, Some(now)));
    }

    /// Whether `position` (server-observed, client coordinates) should be
    /// sent. Returning `true` records it as the client's new pointer
    /// position, so the same position is not sent twice.
    pub fn should_send(&mut self, position: (u16, u16), now: std::time::Instant) -> bool {
        if let Some((known, moved_at)) = self.client_pointer {
            if moved_at.is_some_and(|t| now.saturating_duration_since(t) < self.quiet) {
                return false;
            }
            if known.0.abs_diff(position.0) <= self.tolerance
                && known.1.abs_diff(position.1) <= self.tolerance
            {
                return false;
            }
        }
        self.client_pointer = Some((position, None));
        true
    }

    /// Forget the client pointer state (a new client connected).
    pub fn reset(&mut self) {
        self.client_pointer = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_fields_follow_the_spec() {
        let p = TransparentPointer::new_32x32();
        assert_eq!(p.width, 32);
        assert_eq!(p.height, 32);
        assert_eq!(p.cache_index, 0);
        assert_eq!(p.hot_x, 0);
        assert_eq!(p.hot_y, 0);
    }

    #[test]
    fn and_mask_is_one_bit_per_pixel_all_opaque() {
        let p = TransparentPointer::new_32x32();
        // 32 px/row → 4 bytes/row, no padding; 32 rows.
        assert_eq!(p.and_mask.len(), 32 * 4);
        assert!(p.and_mask.iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn xor_mask_is_24bpp_all_zero() {
        let p = TransparentPointer::new_32x32();
        // 3 bytes/px × 32 px = 96 bytes/row, no padding; 32 rows.
        assert_eq!(p.xor_mask.len(), 32 * 32 * 3);
        assert!(p.xor_mask.iter().all(|&b| b == 0x00));
    }

    #[test]
    fn mask_bytes_matches_field_lengths() {
        let p = TransparentPointer::new_32x32();
        assert_eq!(p.mask_bytes(), 32 * 4 + 32 * 32 * 3);
    }

    #[test]
    fn first_send_is_immediate_then_periodic() {
        let c = ResendCounter::new(60);
        // Frame 0: immediate send.
        assert!(c.should_send());
        // Frames 1..59: no send.
        for _ in 1..60 {
            assert!(!c.should_send(), "must not send inside the interval");
        }
        // Frame 60: periodic re-send.
        assert!(c.should_send());
        // Frames 61..119: no send; frame 120: re-send.
        for _ in 61..120 {
            assert!(!c.should_send());
        }
        assert!(c.should_send());
    }

    #[test]
    fn counter_clones_share_state() {
        let a = ResendCounter::new(3);
        let b = a.clone();
        assert!(a.should_send()); // frame 0 (immediate)
        assert!(!b.should_send()); // frame 1
        assert!(!a.should_send()); // frame 2
        assert!(b.should_send()); // frame 3 → periodic
    }

    #[test]
    #[should_panic(expected = "interval must be > 0")]
    fn zero_interval_is_rejected() {
        let _ = ResendCounter::new(0);
    }

    use std::time::{Duration, Instant};

    #[test]
    fn echo_filter_suppresses_positions_while_the_client_moves() {
        let mut f = PointerEchoFilter::default();
        let t0 = Instant::now();
        f.note_client_move((500, 500), t0);
        // Trailing compositor samples of the client's own motion.
        assert!(!f.should_send((480, 490), t0 + Duration::from_millis(40)));
        assert!(!f.should_send((300, 300), t0 + Duration::from_millis(499)));
    }

    #[test]
    fn echo_filter_suppresses_the_settled_echo_within_tolerance() {
        let mut f = PointerEchoFilter::default();
        let t0 = Instant::now();
        f.note_client_move((500, 500), t0);
        let later = t0 + Duration::from_secs(1);
        assert!(!f.should_send((503, 497), later), "rounding is not a warp");
    }

    #[test]
    fn echo_filter_sends_an_application_warp_once() {
        let mut f = PointerEchoFilter::default();
        let t0 = Instant::now();
        f.note_client_move((500, 500), t0);
        let later = t0 + Duration::from_secs(1);
        assert!(f.should_send((100, 900), later), "guest-side warp");
        assert!(
            !f.should_send((100, 900), later + Duration::from_millis(16)),
            "not re-sent"
        );
        assert!(
            f.should_send((800, 100), later + Duration::from_millis(32)),
            "next warp"
        );
    }

    #[test]
    fn echo_filter_sends_the_first_position_and_after_reset() {
        let mut f = PointerEchoFilter::default();
        let t0 = Instant::now();
        assert!(f.should_send((10, 10), t0), "initial sync");
        assert!(!f.should_send((10, 10), t0));
        f.reset();
        assert!(f.should_send((10, 10), t0), "new client gets a sync");
    }
}
