//! # hyperv-rdp-extras
//!
//! Standalone, MIT-licensed modules originating from the
//! [moerketh/lamco-rdp-server](https://github.com/moerketh/lamco-rdp-server)
//! Hyper-V fork. Each module was authored for the fork, is absent from the
//! upstream repository, and is re-typed here against crate-local abstractions
//! so that nothing here derives from any upstream expression. See
//! [`PROVENANCE.md`](../PROVENANCE.md) for the per-module audit trail.
//!
//! Modules:
//!
//! - [`geometry`] — damage-region arithmetic: an axis-aligned [`Region`]
//!   type, exact rectangle subtraction with a fragmentation cap, overlap
//!   merging, and MS-RDPEGFX macroblock-grid snapping. Pure `std`.
//! - [`transport`] — [`HandshakeDeadlineStream`](transport::HandshakeDeadlineStream):
//!   a wrapper that drops a freshly accepted connection whose peer never
//!   sends a first byte within a deadline, so a silent client cannot park a
//!   serial accept loop and black out every listener. Generic over any
//!   tokio `AsyncRead + AsyncWrite` stream.
//!
//! Feature flags: `geometry` (pure std) and `transport` (tokio + tracing)
//! are both on by default; each can be turned off independently.

#![forbid(unsafe_code)]

#[cfg(feature = "geometry")]
pub mod geometry;
#[cfg(feature = "transport")]
pub mod transport;
