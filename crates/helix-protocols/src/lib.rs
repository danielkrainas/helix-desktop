//! Helix-private Wayland protocols.
//!
//! - `helix_keybindings_v1` (`protocols/helix-keybindings-v1.xml`): global
//!   key bindings requested by the shell, delivered by the compositor.
//!
//! Enable the `server` feature in the compositor and `client` in the shell.

#![allow(non_upper_case_globals, non_camel_case_types, unused_imports, missing_docs)]

/// The protocol's interface descriptions, shared by both sides.
#[cfg(any(feature = "server", feature = "client"))]
pub mod __interfaces {
    wayland_scanner::generate_interfaces!("protocols/helix-keybindings-v1.xml");
}

#[cfg(feature = "server")]
pub mod server {
    use crate::__interfaces::*;
    use wayland_server;
    wayland_scanner::generate_server_code!("protocols/helix-keybindings-v1.xml");
}

#[cfg(feature = "client")]
pub mod client {
    use crate::__interfaces::*;
    use wayland_client;
    wayland_scanner::generate_client_code!("protocols/helix-keybindings-v1.xml");
}
