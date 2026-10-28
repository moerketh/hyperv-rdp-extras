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
//! - [`clipboard`] — MS-RDPECLIP helpers: Format List deduplication and
//!   paced re-requests of early client failures, host→guest file-paste
//!   planning (a FileGroupDescriptorW listing planned into a sanitized
//!   relative tree, top-level entries flagged for the URI list), the
//!   guest→host walk that expands a copied local directory into its
//!   entries, and the pending-request matcher that pairs a remote file
//!   list with the local paste it answers. Pure `std`.
//! - [`cursor`] — spec-derived transparent pointer-shape fields
//!   (MS-RDPBCGR TS_COLORPOINTERATTRIBUTE) plus a re-send cadence counter,
//!   for taking pointer ownership on clients that ignore `HidePointer`.
//! - [`session`] — KWin zkde-screencast virtual-output machinery (KDE
//!   Plasma 6+): the Wayland thread with create-before-close stream
//!   lifecycle, and the physical-output layout guard.
//!
//! Feature flags: `geometry` (pure std) and `transport` (tokio + tracing)
//! are on by default; `clipboard`, `cursor` and `kwin-virtual` are opt-in.

#![forbid(unsafe_code)]

#[cfg(feature = "clipboard")]
pub mod clipboard;
#[cfg(feature = "cursor")]
pub mod cursor;
#[cfg(feature = "geometry")]
pub mod geometry;
#[cfg(feature = "kwin-virtual")]
pub mod protocols;
#[cfg(feature = "kwin-virtual")]
pub mod session;
#[cfg(feature = "transport")]
pub mod transport;
