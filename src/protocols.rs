//! Vendored KDE plasma Wayland protocol bindings for the in-place
//! virtual-output mode change (experiment D).
//!
//! The pinned `wayland-protocols-plasma` crate (0.3.x) bundles XML
//! snapshots that predate `kde_output_management_v2` v18 — the revision
//! that adds `create_mode_list` / `set_custom_modes` (custom modes,
//! KWin >= 6.7 compositor versions 21+). This module generates client
//! bindings from the upstream protocol XMLs directly, mirroring the
//! upstream crate's own generation layout so `Dispatch` impls look
//! identical to the zkde-screencast code beside them.
//!
//! XMLs are vendored from KDE/plasma-wayland-protocols
//! (`src/protocols/kde-output-device-v2.xml`, v25, and
//! `src/protocols/kde-output-management-v2.xml`, v22), both licensed
//! MIT-CMU (see their SPDX headers).

#![allow(clippy::all)]

macro_rules! wayland_protocol {
    ($path:expr, [$($imports:path),*]) => {
        pub use self::generated::client;

        mod generated {
            #![allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables)]
            #![allow(non_upper_case_globals, non_snake_case, unused_imports)]
            #![allow(missing_docs)]

            pub mod client {
                //! Client-side API of this protocol
                use wayland_client;
                use wayland_client::protocol::*;
                $(use $imports::{client::*};)*

                pub mod __interfaces {
                    use wayland_client::protocol::__interfaces::*;
                    $(use $imports::{client::__interfaces::*};)*
                    wayland_scanner::generate_interfaces!($path);
                }
                use self::__interfaces::*;

                wayland_scanner::generate_client_code!($path);
            }
        }
    };
}

pub mod output_device {
    pub mod v2 {
        wayland_protocol!(
            "protocols/kde-output-device-v2.xml",
            []
        );
    }
}

pub mod output_management {
    pub mod v2 {
        wayland_protocol!(
            "protocols/kde-output-management-v2.xml",
            [crate::protocols::output_device::v2]
        );
    }
}
