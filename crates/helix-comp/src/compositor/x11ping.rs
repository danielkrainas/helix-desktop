//! `_NET_WM_PING` and `_NET_WM_ICON` for X11 apps (helix-desktop-spec
//! `contracts/wayland/README.md`, "Unresponsive apps" and "Window
//! switching"). Smithay's window manager does not pass these on, so Helix
//! keeps a connection of its own to Xwayland: it sends the pings, reads
//! icons, and a thread watches for ping answers and icon changes.

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
    net_wm_icon: Atom,
}

/// What the watching thread reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum X11Note {
    /// A window answered a ping.
    Pong(Window),
    /// A window's `_NET_WM_ICON` changed.
    IconChanged(Window),
}

/// An icon: premultiplied ARGB8888 (native byte order) and its size.
pub type X11Icon = (Vec<u8>, u32);

/// The best image of a `_NET_WM_ICON` value (width, height, then
/// width×height straight-alpha ARGB pixels, repeated): the largest square
/// one, else the largest, centered on a transparent square.
pub fn pick_icon(values: &[u32]) -> Option<X11Icon> {
    let mut images: Vec<(usize, usize, &[u32])> = Vec::new();
    let mut rest = values;
    while let [w, h, tail @ ..] = rest {
        let (w, h) = (*w as usize, *h as usize);
        let n = w.checked_mul(h)?;
        if n == 0 || tail.len() < n {
            break;
        }
        if w <= 512 && h <= 512 {
            images.push((w, h, &tail[..n]));
        }
        rest = &tail[n..];
    }
    let (w, h, pixels) = images
        .iter()
        .filter(|(w, h, _)| w == h)
        .max_by_key(|(w, _, _)| *w)
        .or_else(|| images.iter().max_by_key(|(w, h, _)| w * h))
        .copied()?;
    let size = w.max(h);
    let (dx, dy) = ((size - w) / 2, (size - h) / 2);
    let mut out = vec![0u8; size * size * 4];
    for y in 0..h {
        for x in 0..w {
            let p = pixels[y * w + x];
            let a = p >> 24;
            let m = |c: u32| (c * a + 127) / 255;
            let premul = (a << 24) | (m((p >> 16) & 0xff) << 16) | (m((p >> 8) & 0xff) << 8) | m(p & 0xff);
            let at = ((y + dy) * size + x + dx) * 4;
            out[at..at + 4].copy_from_slice(&premul.to_ne_bytes());
        }
    }
    Some((out, size as u32))
}

impl X11Ping {
    /// Connect to Xwayland `:display`; answers (their window) go to `answers`.
    pub fn start(display: u32, answers: Sender<X11Note>) -> Result<Self, String> {
        let (conn, screen) = RustConnection::connect(Some(&format!(":{display}"))).map_err(|e| e.to_string())?;
        let root = conn.setup().roots[screen].root;
        let atom = |name: &str| -> Result<Atom, String> {
            Ok(conn.intern_atom(false, name.as_bytes()).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?.atom)
        };
        let (wm_protocols, net_wm_ping, net_wm_icon) = (atom("WM_PROTOCOLS")?, atom("_NET_WM_PING")?, atom("_NET_WM_ICON")?);
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
                    let note = match event {
                        Event::ClientMessage(msg) => {
                            let data = msg.data.as_data32();
                            (msg.type_ == wm_protocols && data[0] == net_wm_ping).then_some(X11Note::Pong(data[2]))
                        }
                        Event::PropertyNotify(p) if p.atom == net_wm_icon => Some(X11Note::IconChanged(p.window)),
                        _ => None,
                    };
                    if let Some(note) = note {
                        if answers.send(note).is_err() {
                            break;
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { conn, wm_protocols, net_wm_ping, net_wm_icon })
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

    /// Follow `window`'s icon changes, and read its icon now.
    pub fn watch_icon(&self, window: Window) -> Option<X11Icon> {
        let _ = self.conn.change_window_attributes(window, &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE));
        let _ = self.conn.flush();
        self.icon(window)
    }

    /// `window`'s icon, if it has a usable one.
    pub fn icon(&self, window: Window) -> Option<X11Icon> {
        let reply = self
            .conn
            .get_property(false, window, self.net_wm_icon, AtomEnum::CARDINAL, 0, 4 * 1024 * 1024)
            .ok()?
            .reply()
            .ok()?;
        let values: Vec<u32> = reply.value32()?.collect();
        pick_icon(&values)
    }

    /// Force Quit: close the app's connection to the display.
    pub fn kill(&self, window: Window) {
        let _ = self.conn.kill_client(window);
        let _ = self.conn.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn largest_square_icon() {
        // A 2x1 (not square), a 1x1 and a 2x2.
        let mut v = vec![2, 1, 0xFFFF0000, 0xFFFF0000, 1, 1, 0xFF00FF00, 2, 2];
        v.extend([0x80FFFFFF, 0xFF0000FF, 0, 0xFF000000]);
        let (pixels, size) = pick_icon(&v).unwrap();
        assert_eq!(size, 2);
        // Half-transparent white premultiplied: 0x80808080.
        assert_eq!(u32::from_ne_bytes(pixels[0..4].try_into().unwrap()), 0x80808080);
        assert_eq!(u32::from_ne_bytes(pixels[4..8].try_into().unwrap()), 0xFF0000FF);
        assert!(pick_icon(&[3, 3, 1]).is_none(), "truncated");
        // Only a 2x3 image: centered on a 3x3 square (columns of padding
        // are whole pixels, so it starts at column 0).
        let (pixels, size) = pick_icon(&[2, 3, 0xFF0000FF, 0xFF00FF00, 0xFF0000FF, 0xFF00FF00, 0xFF0000FF, 0xFF00FF00]).unwrap();
        assert_eq!(size, 3);
        assert_eq!(u32::from_ne_bytes(pixels[0..4].try_into().unwrap()), 0xFF0000FF);
        assert_eq!(u32::from_ne_bytes(pixels[8..12].try_into().unwrap()), 0, "padding on the right");
        // A square one wins over a larger non-square one.
        let mut v = vec![4, 2];
        v.extend([0xFFFFFFFF; 8]);
        v.extend([1, 1, 0xFF000000]);
        assert_eq!(pick_icon(&v).unwrap().1, 1);
        assert!(pick_icon(&[]).is_none());
    }
}
