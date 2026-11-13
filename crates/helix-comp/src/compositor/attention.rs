//! Windows wanting attention (helix-desktop-spec
//! `contracts/wayland/README.md`, "Windows wanting attention"): an
//! activation Helix does not grant, an X11 urgency hint or
//! `_NET_WM_STATE_DEMANDS_ATTENTION` marks the window until it gets focus;
//! the shell is told (`helix_window_list_v1.attention`).

use smithay::desktop::Window;
use smithay::reexports::wayland_server::Resource;
use smithay::xwayland::X11Surface;

use super::Helix;

/// `_NET_WM_STATE` client message actions.
pub const STATE_REMOVE: u32 = 0;
pub const STATE_ADD: u32 = 1;
pub const STATE_TOGGLE: u32 = 2;

impl Helix {
    /// Whether `window` has keyboard focus.
    fn has_focus(&self, window: &Window) -> bool {
        let focus = self.seat.get_keyboard().and_then(|k| k.current_focus());
        focus.and_then(|f| self.window_for_focus(&f)).is_some_and(|w| w == *window)
    }

    /// Mark `window` as wanting attention (or not). A focused window never
    /// wants it.
    pub fn set_attention(&mut self, window: &Window, wanting: bool) {
        let wanting = wanting && !self.has_focus(window);
        let had = self.attention.contains(window);
        if had == wanting {
            return;
        }
        if wanting {
            self.attention.push(window.clone());
        } else {
            self.attention.retain(|w| w != window);
        }
        tracing::debug!(wanting, "window attention");
        self.sync_window_list();
        let Some(id) = self.window_list.ids.iter().find(|(w, _)| w == window).map(|(_, id)| *id) else { return };
        for listener in self.window_list.listeners.iter().filter(|l| l.version() >= 9) {
            listener.attention(id, u32::from(wanting));
        }
    }

    /// Forget windows that are gone.
    pub fn prune_attention(&mut self) {
        let alive: Vec<Window> = self.window_list.ids.iter().map(|(w, _)| w.clone()).collect();
        self.attention.retain(|w| alive.contains(w));
    }

    /// The window of X11 window `id`.
    fn x11_by_id(&self, id: u32) -> Option<Window> {
        self.window_list.ids.iter().map(|(w, _)| w).find(|w| w.x11_surface().is_some_and(|x| x.window_id() == id)).cloned()
    }

    /// An X11 window's `WM_HINTS` changed: its urgency flag.
    pub fn x11_hints_changed(&mut self, surface: &X11Surface) {
        let urgent = surface.hints().is_some_and(|h| h.urgent);
        if let Some(window) = self.x11_by_id(surface.window_id()) {
            self.set_attention(&window, urgent);
        }
    }

    /// `_NET_WM_STATE` asks to add, remove or toggle
    /// `_NET_WM_STATE_DEMANDS_ATTENTION` on X11 window `id`.
    pub fn x11_demands_attention(&mut self, id: u32, action: u32) {
        let Some(window) = self.x11_by_id(id) else { return };
        let now = self.attention.contains(&window);
        let wanting = match action {
            STATE_REMOVE => false,
            STATE_ADD => true,
            STATE_TOGGLE => !now,
            _ => return,
        };
        self.set_attention(&window, wanting);
    }
}
