# PROVENANCE

This document records, for every module in this crate, where the code
originated, who authored it, and the evidence that it does not derive from
the expression of any third party. It exists so the crate's MIT license
rests on an auditable record rather than an assertion.

**Repository of origin:** [moerketh/lamco-rdp-server](https://github.com/moerketh/lamco-rdp-server)
(the "fork"), a fork of [lamco-admin/lamco-rdp-server](https://github.com/lamco-admin/lamco-rdp-server)
("upstream"), which is licensed BUSL-1.1 by Lamco Development LLC.

**Method:** for each module, distinctive strings from the module were
searched against the upstream repository (GitHub code search, plus file
inspection) to establish upstream absence. Modules below are re-typed
against crate-local types (`Region` instead of the fork's `DamageRegion`,
generic `S: AsyncRead + AsyncWrite + Unpin` instead of the fork's
`AsyncRdpStream`), so no upstream file text is included verbatim.

---

## `src/transport.rs` — HandshakeDeadlineStream

| | |
|---|---|
| Fork source | `src/transport/handshake_deadline.rs` (whole file, fork-authored) |
| Purpose | Drops a freshly accepted connection whose peer never sends a first byte within a deadline, so a silent client cannot park a serial accept loop and black out every listener |
| Upstream-absence searches | `handshake_deadline` (0 hits), `HandshakeDeadlineStream` (0 hits), `DEFAULT_HANDSHAKE_DEADLINE` (0 hits) — performed during extraction |
| Re-typing | The bound `S: AsyncRdpStream` (a fork-local trait) became `S: AsyncRead + AsyncWrite + Unpin` (tokio std traits). The `assert!(!deadline.is_zero())` guard and `into_inner()` are additions made in this crate. Everything else is the fork's own expression, whose module doc and comments are preserved with light edits |
| Dependencies | tokio, tracing — no upstream code |

## `src/geometry.rs` — Region arithmetic

| | |
|---|---|
| Fork source | `subtract_regions`: `src/server/pipeline_decisions.rs` (fork commit lineage; function fork-authored). `DebtAccumulator` (fork name `DamageAccumulator`): same file, fork-authored. `Region`: crate-local re-typing of the fork's `DamageRegion` shape (`x/y/width/height` — dictated by function, not protectable expression). `merge_regions` / `is_adjacent`: **re-authored fresh in this crate** (see note) |
| Purpose | Damage-region subtraction for the calibration probe (missed compositor-hint areas must still be sent); skip-streak debt tracking with replace-not-extend and a bounding-union cap |
| Upstream-absence searches | `subtract_regions` (0 hits), `DamageAccumulator` (0 hits) — performed during extraction |
| Re-typing | `DamageRegion` → crate-local `Region`; `crate::damage::merge_regions` call replaced by a crate-local merge |
| Note on `merge_regions` | Upstream *does* contain a `merge_regions` in its `src/damage/mod.rs`. The fork uses upstream's version. Because copying it would import upstream expression, this crate ships a **fresh implementation** written for this crate (`is_adjacent` + single-pass union to fixed point), documented here deliberately. Its behavior matches what the accumulator needs (merge_distance = 0 collapses touching/overlapping rects) and it carries its own tests (`merge_regions_collapses_touching_rects`, `merge_regions_respects_merge_distance`). Any future contributor: do NOT copy the upstream implementation into this crate |
| Deliberately excluded | The macroblock-snap logic (`damage_regions_to_avc420` in fork `src/server/egfx_sender.rs`): upstream now contains its own expression of macroblock snapping (verified during extraction), so this is not fork-only and stays out of the crate |
| Dependencies | std only |

## `src/cursor.rs` — transparent pointer-shape fields + re-send counter

| | |
|---|---|
| Fork source | The transparent-shape construction block in fork `src/server/display_handler.rs` (~:4606, Painted branch of `process_cursor_update`) + `PaintedShapeCounter` (:117-135) — fork-authored (measured on vmconnect , xrdp-parity approach) |
| Purpose | Pointer ownership on clients that ignore `HidePointer`/SYSPTR_NULL: an all-opaque AND + all-zero XOR color-pointer shape makes the client render nothing |
| Upstream-absence searches | `TS_COLORPOINTERATTRIBUTE` (0 hits in upstream code; their CHANGELOG documents a *different* cursor direction — server-driven shapes from metadata, not transparent-shape suppression), `PAINTED_SHAPE_INTERVAL`, `PaintedShapeCounter`, `painted_shape_counter` (0 hits) — performed during extraction |
| Re-typing | RE-AUTHORED from the MS-RDPBCGR spec (TS_COLORPOINTERATTRIBUTE §2.2.9.1.1.4): the crate module describes the PDU field-by-field and constructs the masks itself; it does not inline the fork's `ColorPointer` construction — the fork adapts the fields into its protocol crate's type at its call site. The wire-format constants (24-bpp XOR, bit-per-pixel AND, 2-byte row padding notes) come from the spec, not from any repository's expression |
| Dependencies | std + tracing — no protocol crates |

## `src/session.rs` — KWin zkde-screencast virtual-output machinery

| | |
|---|---|
| Fork source | `src/session/strategies/kwin_virtual.rs` (whole file, fork-authored: strategy added in the fork's 1.4.4-hyperv.1 lineage; the Wayland thread, create-before-close state machine, OutputLayoutGuard, kscreen parsers and the field-measured comments are fork work) |
| Purpose | Create a KWin virtual output at an arbitrary resolution via the private zkde protocol and stream it (dialog-free, elastic resize); manage the physical-output layout around the session |
| Upstream-absence searches | `zkde-screencast` (0 hits), `stream_virtual_output` (0 hits), `KwinVirtualStrategy` / `kwin-virtual` (0 hits), `OutputLayoutGuard` (0 hits), `parse_enabled_physical_outputs` (0 hits) — performed during extraction |
| Re-typing | REWRITE per plan (Claude review pt 4): the fork file implements upstream's `SessionHandle` and composes `LibeiStrategy` (upstream machinery) — those stay fork-side. The crate module owns the genuinely self-contained machinery: the Wayland thread (registry bind, poll loop, create-before-close swap with `retiring`/`pending` lifecycle), `StreamRequestMachine` (re-expressed over a crate-local `StreamOutcome` enum instead of protocol event types, making it unit-testable off-compositor), `VirtualOutputManager` (thread owner minus the fork's StreamInfo/libei coupling), `OutputLayoutGuard` + kscreen parsers/helpers (verbatim-logic port of fork-authored code). The fork's strategy shell (libei composition, `SessionHandle` impl, `clipboard_source`/`build_clipboard`) remains in the fork and calls into this module |
| Dependencies | wayland-client + wayland-protocols-plasma + nix(poll) + tokio + tracing + anyhow |

---

## Modules deliberately NOT extracted (non-exhaustive, for future maintainers)

- **`x264_encoder.rs` + `x264_shim.c`** — the shim's `#include <x264.h>` is
  GPLv2+; the ABI gate derives its symbol/soname from the header's
  `X264_BUILD`, so the include cannot be dropped. An MIT crate must not
  carry GPL-derived compilation units. If this is ever extracted, it needs
  its own repository under GPL.
- **In-place fix set** (dead-peer `try_send` wedge fix, clipboard
  `broadcast_server_event` fan-out, EGFX HALT latch, D-Bus connection
  tracking, probe-union send set, damage distrust tuning, DMA-BUF bounds
  helper, auto-select Painted flip) — these are edits inside upstream-owned
  files and ride the fork's BUSL license.
- **vsock transport, socket activation, ESM plain-RDP dual-server,
  `pipeline_decisions.rs` core (compute_timestamp_ms, suppress gating,
  codec resolution, stress IDR, compositor trust)** — already present in
  the upstream tree (their 1.4.4); not ours to extract.

## Verification procedure for future changes

1. Before adding any module: search the upstream repo for its distinctive
   strings; record the queries and hit counts here.
2. Never copy upstream file text. Re-type against crate-local types; when a
   shared algorithm is needed, write a fresh implementation and note it
   here (the `merge_regions` precedent).
3. Keep this file current with every module change.