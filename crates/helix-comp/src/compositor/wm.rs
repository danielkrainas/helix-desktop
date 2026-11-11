//! Window management policy: focus, cycling, maximize, fullscreen,
//! minimize and close — for both Wayland (xdg) and X11 (Xwayland) windows.

use smithay::desktop::{layer_map_for_output, Window};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Rectangle, SERIAL_COUNTER};
use smithay::wayland::seat::WaylandFocus;

use super::Helix;

/// Why a window's geometry was taken over, so it can be restored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Takeover {
    Maximized,
    Fullscreen,
}

/// The surface that receives keyboard focus for a window (xdg toplevel or
/// the Xwayland window's surface once associated).
pub fn surface_of(window: &Window) -> Option<WlSurface> {
    window.wl_surface().map(|s| s.into_owned())
}

/// X11 menus, tooltips and the like: positioned by the app, never focused.
pub fn is_override_redirect(window: &Window) -> bool {
    window.x11_surface().is_some_and(|x| x.is_override_redirect())
}

impl Helix {
    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.space
            .elements()
            .chain(self.minimized.iter())
            .find(|w| surface_of(w).as_ref() == Some(surface))
            .cloned()
    }

    /// Managed (non-override-redirect) mapped windows, bottom to top.
    fn managed(&self) -> impl Iterator<Item = &Window> {
        self.space.elements().filter(|w| !is_override_redirect(w))
    }

    /// Topmost managed window.
    pub fn top_window(&self) -> Option<Window> {
        self.managed().last().cloned()
    }

    /// Raise `window`, give it keyboard focus, and mark only it activated.
    pub fn focus_window(&mut self, window: &Window) {
        if is_override_redirect(window) {
            return;
        }
        if let Some(pos) = self.minimized.iter().position(|w| w == window) {
            let window = self.minimized.remove(pos);
            let location = self.minimized_at.remove(pos);
            self.space.map_element(window, location, false);
        }
        self.space.raise_element(window, true);
        if let (Some(xwm), Some(x11)) = (self.xwm.as_mut(), window.x11_surface()) {
            let _ = xwm.raise_window(x11);
        }
        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.set_focus(self, surface_of(window), SERIAL_COUNTER.next_serial());
        let managed: Vec<Window> = self.managed().cloned().collect();
        for w in &managed {
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
            if is_override_redirect(w) {
                continue;
            }
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
            .managed()
            .chain(self.minimized.iter())
            .find(|w| Some(*w) != top.as_ref())
            .cloned();
        if let Some(window) = next {
            self.focus_window(&window);
        }
    }

    /// Ask the focused window to close (Alt+F4).
    pub fn close_focused(&mut self) {
        let Some(window) = self.top_window() else { return };
        if let Some(t) = window.toplevel() {
            t.send_close();
        } else if let Some(x11) = window.x11_surface() {
            let _ = x11.close();
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
        if is_override_redirect(window) {
            return;
        }
        if !self.restore.iter().any(|(w, _, _)| w == window) {
            let location = self.space.element_location(window).unwrap_or_default();
            let size = window.geometry().size;
            self.restore.push((window.clone(), Rectangle::new(location, size), kind));
        }
        if let Some(toplevel) = window.toplevel() {
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
        } else if let Some(x11) = window.x11_surface() {
            let _ = x11.set_maximized(kind == Takeover::Maximized);
            let _ = x11.set_fullscreen(kind == Takeover::Fullscreen);
            let _ = x11.configure(area);
        }
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
        } else if let Some(x11) = window.x11_surface() {
            let _ = x11.set_maximized(false);
            let _ = x11.set_fullscreen(false);
            let _ = x11.configure(geometry);
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

    /// Keep windows inside the work area after it changes (e.g. a panel
    /// appeared): move windows out from under reserved zones and resize
    /// maximized ones to the new area.
    pub fn constrain_to_work_area(&mut self) {
        let Some(area) = self.usable_area() else { return };
        if self.work_area == Some(area) {
            return;
        }
        self.work_area = Some(area);
        let windows: Vec<Window> = self.managed().cloned().collect();
        for window in windows {
            if self.is_taken_over(&window, Takeover::Fullscreen) {
                continue;
            }
            if self.is_taken_over(&window, Takeover::Maximized) {
                self.take_over(&window, Takeover::Maximized, area);
                continue;
            }
            let Some(location) = self.space.element_location(&window) else { continue };
            let fixed = (location.x.max(area.loc.x), location.y.max(area.loc.y)).into();
            if fixed != location {
                self.space.map_element(window.clone(), fixed, false);
                if let Some(x11) = window.x11_surface() {
                    let _ = x11.configure(Rectangle::new(fixed, x11.geometry().size));
                }
            }
        }
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

    /// Forget a window that is gone (destroyed or unmapped by its client)
    /// and focus whatever is on top now if it had focus.
    pub fn forget_window(&mut self, window: &Window) {
        if let Some(pos) = self.minimized.iter().position(|w| w == window) {
            self.minimized.remove(pos);
            self.minimized_at.remove(pos);
        }
        self.restore.retain(|(w, _, _)| w != window);
        if self.resize.as_ref().is_some_and(|r| &r.window == window) {
            self.resize = None;
        }
        let had_focus = {
            let keyboard = self.seat.get_keyboard().unwrap();
            let focus = keyboard.current_focus();
            focus.is_none() || focus == surface_of(window)
        };
        self.space.unmap_elem(window);
        if had_focus {
            match self.top_window() {
                Some(top) => self.focus_window(&top),
                None => self.unfocus_all(),
            }
        }
    }

    /// An xdg toplevel was destroyed.
    pub fn window_gone(&mut self, surface: &WlSurface) {
        match self.window_for_surface(surface) {
            Some(window) => self.forget_window(&window),
            None => {
                if let Some(top) = self.top_window() {
                    self.focus_window(&top);
                }
            }
        }
    }

    /// While (and just after) a top/left-edge resize of an xdg window, keep
    /// the opposite edge fixed once the client commits its new size.
    pub fn track_resize_commit(&mut self, surface: &WlSurface) {
        let Some(tracking) = self.resize.clone() else { return };
        if tracking.window.toplevel().is_none() || surface_of(&tracking.window).as_ref() != Some(surface) {
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
