//! Helix-private Wayland protocols.
//!
//! - `helix_keybindings_v1` (`protocols/helix-keybindings-v1.xml`): global
//!   key bindings requested by the shell, delivered by the compositor.
//! - `helix_window_list_v1` (`protocols/helix-window-list-v1.xml`): open
//!   windows and focus for the shell's window switcher; activation.
//! - `helix_screenshot_v1` (`protocols/helix-screenshot-v1.xml`): screen
//!   captures written as PNG images, for the shell's screenshot service.
//! - `gtk_shell1` (`protocols/gtk-shell.xml`): GNOME's own protocol, as
//!   mutter offers it; GTK 3 says through it which dialogs are modal.
//!
//! Enable the `server` feature in the compositor and `client` in the shell.

#![allow(non_upper_case_globals, non_camel_case_types, unused_imports, missing_docs)]

/// Each protocol's interface descriptions, shared by both sides.
#[cfg(any(feature = "server", feature = "client"))]
pub mod __keybindings {
    wayland_scanner::generate_interfaces!("protocols/helix-keybindings-v1.xml");
}

#[cfg(any(feature = "server", feature = "client"))]
pub mod __window_list {
    wayland_scanner::generate_interfaces!("protocols/helix-window-list-v1.xml");
}

#[cfg(any(feature = "server", feature = "client"))]
pub mod __screenshot {
    wayland_scanner::generate_interfaces!("protocols/helix-screenshot-v1.xml");
}

#[cfg(any(feature = "server", feature = "client"))]
pub mod __gtk_shell {
    // It refers to wl_surface and wl_seat.
    #[cfg(feature = "server")]
    use wayland_server::protocol::__interfaces::*;
    #[cfg(all(feature = "client", not(feature = "server")))]
    use wayland_client::protocol::__interfaces::*;
    wayland_scanner::generate_interfaces!("protocols/gtk-shell.xml");
}

/// GNOME's `gtk_shell1` (not Helix-private: GTK binds it).
#[cfg(feature = "server")]
pub mod gtk_shell_server {
    use crate::__gtk_shell::*;
    use wayland_server;
    use wayland_server::protocol::*;
    wayland_scanner::generate_server_code!("protocols/gtk-shell.xml");
}

/// GNOME's `gtk_shell1`, client side (tests).
#[cfg(feature = "client")]
pub mod gtk_shell_client {
    use crate::__gtk_shell::*;
    use wayland_client;
    use wayland_client::protocol::*;
    wayland_scanner::generate_client_code!("protocols/gtk-shell.xml");
}

#[cfg(feature = "server")]
pub mod server {
    pub mod keybindings {
        use crate::__keybindings::*;
        use wayland_server;
        wayland_scanner::generate_server_code!("protocols/helix-keybindings-v1.xml");
    }
    pub mod window_list {
        use crate::__window_list::*;
        use wayland_server;
        wayland_scanner::generate_server_code!("protocols/helix-window-list-v1.xml");
    }
    pub mod screenshot {
        use crate::__screenshot::*;
        use wayland_server;
        wayland_scanner::generate_server_code!("protocols/helix-screenshot-v1.xml");
    }
    pub use keybindings::*;
    pub use screenshot::*;
    pub use window_list::*;
}

#[cfg(feature = "client")]
pub mod client {
    pub mod keybindings {
        use crate::__keybindings::*;
        use wayland_client;
        wayland_scanner::generate_client_code!("protocols/helix-keybindings-v1.xml");
    }
    pub mod window_list {
        use crate::__window_list::*;
        use wayland_client;
        wayland_scanner::generate_client_code!("protocols/helix-window-list-v1.xml");
    }
    pub mod screenshot {
        use crate::__screenshot::*;
        use wayland_client;
        wayland_scanner::generate_client_code!("protocols/helix-screenshot-v1.xml");
    }
    pub use keybindings::*;
    pub use screenshot::*;
    pub use window_list::*;
}
