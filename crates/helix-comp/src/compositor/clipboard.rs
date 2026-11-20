//! Who set the clipboard, and the shell's reads and writes of it for Agent
//! Access (helixdesktop/spec `contracts/agent-access/README.md`,
//! "Clipboard methods"; `helix_screenshot_v1` since version 7).
//!
//! Every change of the clipboard selection bumps a generation and records
//! its source: the Wayland client that set it (its app id, from a window of
//! that client, and its process), an X11 app (the focused X11 window's, if
//! any, when it took the selection), the shell, an agent (`copy_data`) or a
//! remote desktop session. The shell reads the selection only by naming
//! the generation it was told about, so what it reads is always what it
//! checked, never a newer selection.

use std::os::fd::OwnedFd;
use std::sync::Arc;

use helix_protocols::server::helix_clipboard_info_v1::{self, HelixClipboardInfoV1, Source as Wire};
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};
use smithay::wayland::selection::SelectionTarget;

use super::{ClientState, Helix, ServerSelection};

/// Where the clipboard selection came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Kind {
    #[default]
    None,
    App,
    X11,
    Shell,
    Agent,
    /// A remote desktop session (its id).
    Remote(u32),
}

impl Kind {
    fn wire(self) -> Wire {
        match self {
            Kind::None => Wire::None,
            Kind::App => Wire::App,
            Kind::X11 => Wire::X11,
            Kind::Shell => Wire::Shell,
            Kind::Agent => Wire::Agent,
            Kind::Remote(_) => Wire::Remote,
        }
    }
}

/// The clipboard's source: its kind, and the app and process when known.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Source {
    pub kind: Kind,
    pub app_id: String,
    pub pid: u32,
}

impl Source {
    pub fn of(kind: Kind) -> Source {
        Source { kind, ..Source::default() }
    }
}

/// The current selection's generation and source.
#[derive(Debug, Default)]
pub struct ClipboardState {
    pub generation: u32,
    pub source: Source,
}

/// The most `copy_data` reads (as the shell's Agent Access allows).
const COPY_MAX: u64 = 16 << 20;

/// Whether `mime` is one of the text types `copy_text` offers text as.
fn is_plain_text(mime: &str) -> bool {
    matches!(mime, "text/plain;charset=utf-8" | "text/plain")
}

impl Helix {
    /// The clipboard changed: a new generation, from `source`.
    pub fn note_clipboard_source(&mut self, source: Source) {
        self.clipboard.generation = self.clipboard.generation.wrapping_add(1);
        tracing::debug!(generation = self.clipboard.generation, kind = ?source.kind, app = %source.app_id, "clipboard source");
        self.clipboard.source = source;
    }

    /// Who `client` is: the shell, or an app (the app id of one of its
    /// windows, the focused one first, and its process).
    pub fn clipboard_source_of(&self, client: Option<&Client>) -> Source {
        let Some(client) = client else { return Source::of(Kind::App) };
        if client.get_data::<ClientState>().is_some_and(|c| c.privileged) {
            return Source::of(Kind::Shell);
        }
        let pid = client.get_credentials(&self.display_handle).map_or(0, |c| c.pid.max(0) as u32);
        let of_client = |w: &smithay::desktop::Window| {
            w.x11_surface().is_none() && smithay::wayland::seat::WaylandFocus::wl_surface(w).and_then(|s| s.client()).is_some_and(|c| c == *client)
        };
        let focused = self.focused_window().filter(|w| of_client(w));
        let window = focused.or_else(|| self.space.elements().chain(self.minimized.iter()).chain(self.workspaces.parked()).find(|w| of_client(w)).cloned());
        let app_id = window.map(|w| super::window_list::describe(&w).0).unwrap_or_default();
        Source { kind: Kind::App, app_id, pid }
    }

    /// An X11 app took the clipboard: the focused X11 window's app and
    /// process, if an X11 window has the focus.
    pub fn x11_clipboard_source(&self) -> Source {
        let window = self.focused_window().filter(|w| w.x11_surface().is_some());
        let (app_id, pid) = window
            .map(|w| (super::window_list::describe(&w).0, w.x11_surface().and_then(|x| x.pid()).unwrap_or(0)))
            .unwrap_or_default();
        Source { kind: Kind::X11, app_id, pid }
    }

    /// `copy_data`: take the clipboard with an agent's data of type `mime`.
    pub fn copy_data(&mut self, mime: String, fd: OwnedFd) {
        use std::io::Read;
        let mut data = Vec::new();
        if let Err(err) = std::fs::File::from(fd).take(COPY_MAX + 1).read_to_end(&mut data) {
            tracing::warn!(%err, "cannot read the data to copy");
            return;
        }
        if data.len() as u64 > COPY_MAX || mime.is_empty() || mime.chars().any(char::is_control) {
            tracing::warn!(len = data.len(), "not copying: too large, or no type");
            return;
        }
        let (types, selection) = if is_plain_text(&mime) {
            if std::str::from_utf8(&data).is_err() {
                tracing::warn!("not copying: not UTF-8 text");
                return;
            }
            let types: Vec<String> = super::capture::TEXT_MIMES.iter().map(|m| (*m).to_owned()).collect();
            (types, ServerSelection::Text(Arc::new(data)))
        } else {
            (vec![mime.clone()], ServerSelection::Data { mime_type: mime.clone(), data: Arc::new(data) })
        };
        smithay::wayland::selection::data_device::set_data_device_selection(&self.display_handle, &self.seat, types.clone(), selection);
        if let Some(xwm) = self.xwm.as_mut() {
            if let Err(err) = xwm.new_selection(SelectionTarget::Clipboard, Some(types.clone())) {
                tracing::warn!(?err, "cannot offer an agent's copy to X11");
            }
        }
        self.clipboard_changed(types, Source::of(Kind::Agent));
        tracing::info!("an agent's data copied to the clipboard");
    }

    /// `get_clipboard`: tell `info` what the clipboard offers and who set it.
    pub fn describe_clipboard(&self, info: &HelixClipboardInfoV1) {
        let offered = &self.remote_share_offered();
        let source = if offered.is_empty() { Source::of(Kind::None) } else { self.clipboard.source.clone() };
        for mime in offered {
            info.offer(mime.clone());
        }
        info.done(self.clipboard.generation, source.kind.wire(), source.app_id, source.pid);
    }

    /// `receive_clipboard`: write the selection of `generation` as `mime`
    /// into `fd`; nothing (`fd` closed) when the selection has changed
    /// since or does not offer `mime`.
    pub fn receive_clipboard(&mut self, generation: u32, mime: String, fd: OwnedFd) {
        if generation != self.clipboard.generation || !self.remote_share_offered().contains(&mime) {
            tracing::info!("clipboard changed before the shell read it; nothing sent");
            return;
        }
        if let Err(err) = self.read_clipboard(mime, fd) {
            tracing::info!(?err, "cannot read the clipboard for the shell");
        }
    }
}

impl Dispatch<HelixClipboardInfoV1, ()> for Helix {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &HelixClipboardInfoV1,
        _request: helix_clipboard_info_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_types() {
        assert!(is_plain_text("text/plain;charset=utf-8") && is_plain_text("text/plain"));
        assert!(!is_plain_text("text/html") && !is_plain_text("image/png") && !is_plain_text("UTF8_STRING"));
        assert_eq!(Kind::Agent.wire(), Wire::Agent);
        assert_eq!(Source::of(Kind::Shell), Source { kind: Kind::Shell, app_id: String::new(), pid: 0 });
    }
}
