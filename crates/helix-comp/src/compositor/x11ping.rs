//! `_NET_WM_PING` for X11 apps (helix-desktop-spec `contracts/wayland/README.md`,
//! "Unresponsive apps"). Smithay's window manager does not pass the answers
//! on, so Helix keeps a connection of its own to Xwayland: it sends the
//! pings and a thread watches the root window for the answers.

use std::sync::Arc;

use smithay::reexports::calloop::channel::Sender;
use smithay::reexports::x11rb::connection::Connection;
use smithay::reexports::x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConnectionExt, EventMask, Window,
};
use smithay::reexports::x11rb::protocol::Event;
use smithay::reexports::x11rb::rust_connection::RustConnection;

pub struct X11Ping {
    conn: Arc<RustConnection>,
    wm_protocols: Atom,
    net_wm_ping: Atom,
}

impl X11Ping {
    /// Connect to Xwayland `:display`; answers (their window) go to `answers`.
    pub fn start(display: u32, answers: Sender<Window>) -> Result<Self, String> {
        let (conn, screen) = RustConnection::connect(Some(&format!(":{display}"))).map_err(|e| e.to_string())?;
        let root = conn.setup().roots[screen].root;
        let atom = |name: &str| -> Result<Atom, String> {
            Ok(conn.intern_atom(false, name.as_bytes()).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?.atom)
        };
        let (wm_protocols, net_wm_ping) = (atom("WM_PROTOCOLS")?, atom("_NET_WM_PING")?);
        // Answers are sent to the root window with the substructure masks.
        conn.change_window_attributes(root, &ChangeWindowAttributesAux::new().event_mask(EventMask::SUBSTRUCTURE_NOTIFY))
            .map_err(|e| e.to_string())?
            .check()
            .map_err(|e| e.to_string())?;
        let conn = Arc::new(conn);
        let reader = conn.clone();
        std::thread::Builder::new()
            .name("helix-x11-ping".into())
            .spawn(move || {
                while let Ok(event) = reader.wait_for_event() {
                    if let Event::ClientMessage(msg) = event {
                        let data = msg.data.as_data32();
                        if msg.type_ == wm_protocols && data[0] == net_wm_ping && answers.send(data[2]).is_err() {
                            break;
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { conn, wm_protocols, net_wm_ping })
    }

    /// Ping `window` if it takes part (`WM_PROTOCOLS` lists `_NET_WM_PING`).
    pub fn ping(&self, window: Window) -> bool {
        let listed = self
            .conn
            .get_property(false, window, self.wm_protocols, AtomEnum::ATOM, 0, 32)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().map(|mut v| v.any(|a| a == self.net_wm_ping)))
            .unwrap_or(false);
        if !listed {
            return false;
        }
        let event = ClientMessageEvent::new(32, window, self.wm_protocols, [self.net_wm_ping, 0, window, 0, 0]);
        let sent = self.conn.send_event(false, window, EventMask::NO_EVENT, event).is_ok();
        let _ = self.conn.flush();
        sent
    }

    /// Force Quit: close the app's connection to the display.
    pub fn kill(&self, window: Window) {
        let _ = self.conn.kill_client(window);
        let _ = self.conn.flush();
    }
}
