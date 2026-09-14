# hyperv-rdp-extras

Standalone, MIT-licensed modules originating from the
[moerketh/lamco-rdp-server](https://github.com/moerketh/lamco-rdp-server)
Hyper-V fork. Each module was authored for the fork, is absent from the
upstream repository, and is re-typed here against crate-local abstractions.
See [PROVENANCE.md](PROVENANCE.md) for the per-module audit trail.

## Modules

### `geometry` — damage-region arithmetic (pure `std`)

- [`Region`](src/geometry.rs) — axis-aligned exclusive-LTRB rectangle
- [`subtract_regions`](src/geometry.rs) — exact rectangle subtraction with
  a fragmentation cap; used by the damage-calibration probe so compositor
  hint misses are never lost
- [`DebtAccumulator`](src/geometry.rs) — skip-streak debt tracking:
  replace-not-extend absorb, merged, hard-capped by bounding union

### `transport` — first-byte deadline (tokio)

- [`HandshakeDeadlineStream`](src/transport.rs) — wraps any
  `AsyncRead + AsyncWrite + Unpin` stream so a freshly accepted connection
  whose peer never speaks is dropped after a deadline instead of parking a
  serial accept loop and blacking out every listener. Transport-agnostic:
  TCP, AF_VSOCK, Unix, TLS, WebSocket.

### `cursor` — transparent pointer shape (opt-in)

- [`TransparentPointer`](src/cursor.rs) — the MS-RDPBCGR
  TS_COLORPOINTERATTRIBUTE fields of a fully transparent color-pointer
  shape (all-opaque AND mask, all-zero 24-bpp XOR mask), for taking
  pointer ownership on clients that ignore `HidePointer` (e.g. Hyper-V
  vmconnect); plus [`ResendCounter`](src/cursor.rs) for the
  immediate-first-send / periodic re-send cadence.

### `session` — KWin virtual output (opt-in)

- [`VirtualOutputManager`](src/session.rs) — creates a KWin virtual
  output at an arbitrary resolution via the private zkde-screencast
  protocol and streams it (dialog-free, elastic resize), with a
  create-before-close stream lifecycle that never empties the enabled-
  output set mid-swap
- [`OutputLayoutGuard`](src/session.rs) — disables the physical (DRM)
  outputs for the session so the virtual output becomes primary, and
  re-enables them on drop, physical first
- The virtual-output identity is **configurable**
  ([`VirtualOutputConfig`](src/session.rs)): the crate's default is the
  neutral `rdp` (kscreen `Virtual-rdp`); callers with their own output
  name pass it explicitly (`with_config` / `engage_with`)

## Usage

```toml
[dependencies]
hyperv-rdp-extras = { git = "https://github.com/moerketh/hyperv-rdp-extras", rev = "<pinned>" }
```

`geometry` and `transport` are on by default; `cursor` and `kwin-virtual`
are opt-in (`features = ["cursor", "kwin-virtual"]`). With
`default-features = false` + `features = ["geometry"]` the crate has no
dependencies beyond `std`.

## License

MIT — see [LICENSE](LICENSE). The origin fork is BUSL-1.1; the modules in
this crate contain no expression from that repository (see
[PROVENANCE.md](PROVENANCE.md)).