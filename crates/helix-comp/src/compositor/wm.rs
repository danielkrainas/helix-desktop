//! Window management policy: focus, cycling, maximize, fullscreen,
//! minimize and close.

use smithay::desktop::{layer_map_for_output, Window};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Rectangle, SERIAL_COUNTER};

use super::Helix;

/// Why a window's geometry was taken over, so it can be restored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Takeover {
    Maximized,
    Fullscreen,
}

impl Helix {
    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.space
            .elements()
            .chain(self.minimized.iter())
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
            .cloned()
    }

    /// Topmost mapped window.
    pub fn top_window(&self) -> Option<Window> {
        self.space.elements().last().cloned()
    }

    /// Raise `window`, give it keyboard focus, and mark only it activated.
    pub fn focus_window(&mut self, window: &Window) {
        if let Some(pos) = self.minimized.iter().position(|w| w == window) {
            let window = self.minimized.remove(pos);
            let location = self.minimized_at.remove(pos);
            self.space.map_element(window, location, false);
        }
        self.space.raise_element(window, true);
        let surface = window.toplevel().map(|t| t.wl_surface().clone());
        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.set_focus(self, surface, SERIAL_COUNTER.next_serial());
        for w in self.space.elements() {
            w.set_activated(w == window);
            if let Some(t) = w.toplevel() {
                t.send_pending_configure();
            }
        }
    }

    /// Clear keyboard focus and deactivate every window.
    pub fn unfocus_all(&mut self) {
        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.set_focus(self, Option::<WlSurface>::None, SERIAL_COUNTER.next_serial());
        for w in self.space.elements() {
            w.set_activated(false);
            if let Some(t) = w.toplevel() {
                t.send_pending_configure();
            }
        }
    }

    /// Focus the next window in the cycle (Alt+Tab): the bottom-most mapped
    /// window, so repeated presses walk through all of them, then minimized
    /// ones.
    pub fn cycle_windows(&mut self) {
        let top = self.top_window();
        let next = self
            .space
            .elements()
            .chain(self.minimized.iter())
            .find(|w| Some(*w) != top.as_ref())
            .cloned();
        if let Some(window) = next {
            self.focus_window(&window);
        }
    }

    /// Ask the focused window to close (Alt+F4).
    pub fn close_focused(&mut self) {
        if let Some(t) = self.top_window().and_then(|w| w.toplevel().cloned()) {
            t.send_close();
        }
    }

    /// The area windows may cover: the output minus panels' reserved zones.
    fn usable_area(&self) -> Option<Rectangle<i32, Logical>> {
        let output = self.space.outputs().next()?;
        let output_geo = self.space.output_geometry(output)?;
        let mut zone = layer_map_for_output(output).non_exclusive_zone();
        zone.loc += output_geo.loc;
        Some(zone)
    }

    fn output_area(&self) -> Option<Rectangle<i32, Logical>> {
        let output = self.space.outputs().next()?;
        self.space.output_geometry(output)
    }

    fn take_over(&mut self, window: &Window, kind: Takeover, area: Rectangle<i32, Logical>) {
        let Some(toplevel) = window.toplevel() else { return };
        if !self.restore.iter().any(|(w, _, _)| w == window) {
            let location = self.space.element_location(window).unwrap_or_default();
            let size = window.geometry().size;
            self.restore.push((window.clone(), Rectangle::new(location, size), kind));
        }
        toplevel.with_pending_state(|state| {
            let (set, unset) = match kind {
                Takeover::Maximized => (xdg_toplevel::State::Maximized, xdg_toplevel::State::Fullscreen),
                Takeover::Fullscreen => (xdg_toplevel::State::Fullscreen, xdg_toplevel::State::Maximized),
            };
            state.states.set(set);
            state.states.unset(unset);
            state.size = Some(area.size);
        });
        toplevel.send_pending_configure();
        self.space.map_element(window.clone(), area.loc, true);
    }

    /// Undo maximize/fullscreen, restoring the previous geometry.
    pub fn restore_window(&mut self, window: &Window) {
        let Some(pos) = self.restore.iter().position(|(w, _, _)| w == window) else {
            return;
        };
        let (_, geometry, _) = self.restore.remove(pos);
        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| {
                state.states.unset(xdg_toplevel::State::Maximized);
                state.states.unset(xdg_toplevel::State::Fullscreen);
                state.size = Some(geometry.size);
            });
            toplevel.send_pending_configure();
        }
        self.space.map_element(window.clone(), geometry.loc, true);
    }

    pub fn maximize(&mut self, window: &Window) {
        if let Some(area) = self.usable_area() {
            self.take_over(window, Takeover::Maximized, area);
        }
    }

    pub fn fullscreen(&mut self, window: &Window) {
        if let Some(area) = self.output_area() {
            self.take_over(window, Takeover::Fullscreen, area);
        }
    }

    pub fn is_taken_over(&self, window: &Window, kind: Takeover) -> bool {
        self.restore.iter().any(|(w, _, k)| w == window && *k == kind)
    }

    /// Toggle maximize on the focused window (Super+Up / Super+Down).
    pub fn toggle_maximize_focused(&mut self) {
        if let Some(window) = self.top_window() {
            if self.is_taken_over(&window, Takeover::Maximized) {
                self.restore_window(&window);
            } else {
                self.maximize(&window);
            }
        }
    }

    /// Hide a window until it is cycled back to.
    pub fn minimize(&mut self, window: &Window) {
        let location = self.space.element_location(window).unwrap_or_default();
        self.space.unmap_elem(window);
        self.minimized.push(window.clone());
        self.minimized_at.push(location);
        match self.top_window() {
            Some(top) => self.focus_window(&top),
            None => self.unfocus_all(),
        }
    }

    /// Forget a destroyed window and focus whatever is on top now.
    pub fn window_gone(&mut self, surface: &WlSurface) {
        let is_gone = |w: &Window| w.toplevel().is_some_and(|t| t.wl_surface() == surface);
        if let Some(pos) = self.minimized.iter().position(is_gone) {
            self.minimized.remove(pos);
            self.minimized_at.remove(pos);
        }
        self.restore.retain(|(w, _, _)| !is_gone(w));
        if self.resize.as_ref().is_some_and(|r| is_gone(&r.window)) {
            self.resize = None;
        }
        let mapped = self.space.elements().find(|w| is_gone(w)).cloned();
        if let Some(window) = mapped {
            self.space.unmap_elem(&window);
        }
        match self.top_window() {
            Some(top) => self.focus_window(&top),
            None => self.unfocus_all(),
        }
    }

    /// While (and just after) a top/left-edge resize, keep the opposite edge
    /// fixed once the client commits its new size.
    pub fn track_resize_commit(&mut self, surface: &WlSurface) {
        let Some(tracking) = self.resize.clone() else { return };
        if !tracking.window.toplevel().is_some_and(|t| t.wl_surface() == surface) {
            return;
        }
        let size = tracking.window.geometry().size;
        let mut location = tracking.initial.loc;
        if tracking.edges.left() {
            location.x += tracking.initial.size.w - size.w;
        }
        if tracking.edges.top() {
            location.y += tracking.initial.size.h - size.h;
        }
        if tracking.edges.left() || tracking.edges.top() {
            self.space.map_element(tracking.window.clone(), location, false);
        }
        if tracking.finished {
            self.resize = None;
        }
    }
}
