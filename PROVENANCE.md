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