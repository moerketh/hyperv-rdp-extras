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

## Usage

```toml
[dependencies]
hyperv-rdp-extras = { git = "https://github.com/moerketh/hyperv-rdp-extras", rev = "<pinned>" }
```

Both modules are on by default; `default-features = false` +
`features = ["geometry"]` drops the tokio dependency entirely.

## License

MIT — see [LICENSE](LICENSE). The origin fork is BUSL-1.1; the modules in
this crate contain no expression from that repository (see
[PROVENANCE.md](PROVENANCE.md)).