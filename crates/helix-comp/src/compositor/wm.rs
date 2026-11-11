//! Window management policy: focus, cycling, maximize, fullscreen,
//! minimize and close — for both Wayland (xdg) and X11 (Xwayland) windows.

use smithay::desktop::{layer_map_for_output, Window};
use smithay::output::Output;
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

/// The leftmost (then topmost) of `outputs`.
pub fn leftmost<'a, T>(outputs: &[(&'a T, Rectangle<i32, Logical>)]) -> Option<&'a T> {
    outputs.iter().min_by_key(|(_, g)| (g.loc.x, g.loc.y)).map(|(o, _)| *o)
}

/// The one of `outputs` that `rect` covers most, if it touches any.
pub fn most_covered<'a, T>(rect: Rectangle<i32, Logical>, outputs: &[(&'a T, Rectangle<i32, Logical>)]) -> Option<&'a T> {
    outputs
        .iter()
        .filter_map(|(o, g)| g.intersection(rect).map(|i| (i.size.w * i.size.h, *o)))
        .max_by_key(|(area, _)| *area)
        .map(|(_, o)| o)
}

/// Whether a window at `rect` touches none of the monitors.
/// `area` less the title bar `window` has above it.
pub fn below_frame(window: &Window, area: Rectangle<i32, Logical>) -> Rectangle<i32, Logical> {
    let top = super::frames::frame_top(window);
    Rectangle::new((area.loc.x, area.loc.y + top).into(), (area.size.w, area.size.h - top).into())
}

pub fn is_lost(rect: Rectangle<i32, Logical>, outputs: &[Rectangle<i32, Logical>]) -> bool {
    !outputs.iter().any(|o| o.overlaps(rect))
}

impl Helix {
    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.space
            .elements()
            .chain(self.minimized.iter())
            .chain(self.workspaces.parked())
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
        // Nothing gets focus while the session is locked.
        if is_override_redirect(window) || self.lock.locked {
            return;
        }
        self.workspace_focus(window);
        if let Some(pos) = self.minimized.iter().position(|w| w == window) {
            let window = self.minimized.remove(pos);
            let location = self.minimized_at.remove(pos);
            self.space.map_element(window, location, false);
        }
        self.space.raise_element(window, true);
        self.restack_above();
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
        // While locked the lock surface keeps the keyboard.
        if self.lock.locked {
            return;
        }
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

    /// The primary monitor: the configured one if it is on, else the
    /// leftmost (then topmost) one.
    pub fn primary(&self) -> Option<Output> {
        let configured = self
            .primary_output
            .as_deref()
            .and_then(|name| self.space.outputs().find(|o| o.name() == name));
        configured.cloned().or_else(|| {
            let outputs: Vec<(&Output, Rectangle<i32, Logical>)> =
                self.space.outputs().filter_map(|o| Some((o, self.space.output_geometry(o)?))).collect();
            leftmost(&outputs).cloned()
        })
    }

    /// The monitor under the pointer, else the primary one.
    pub fn pointer_output(&self) -> Option<Output> {
        let pos = self.seat.get_pointer().map(|p| p.current_location()).unwrap_or_default();
        self.space.output_under(pos).next().cloned().or_else(|| self.primary())
    }

    /// The monitor `window` covers most, else the one under the pointer.
    pub fn output_of(&self, window: &Window) -> Option<Output> {
        let outputs: Vec<(&Output, Rectangle<i32, Logical>)> =
            self.space.outputs().filter_map(|o| Some((o, self.space.output_geometry(o)?))).collect();
        let best = self.space.element_geometry(window).and_then(|g| most_covered(g, &outputs)).cloned();
        best.or_else(|| self.pointer_output())
    }

    /// The area windows may cover on `output`: it minus panels' reserved
    /// zones.
    pub fn usable_area_of(&self, output: &Output) -> Option<Rectangle<i32, Logical>> {
        let output_geo = self.space.output_geometry(output)?;
        let mut zone = layer_map_for_output(output).non_exclusive_zone();
        zone.loc += output_geo.loc;
        Some(zone)
    }

    fn usable_area(&self, window: &Window) -> Option<Rectangle<i32, Logical>> {
        self.usable_area_of(&self.output_of(window)?)
    }

    fn output_area(&self, window: &Window) -> Option<Rectangle<i32, Logical>> {
        self.space.output_geometry(&self.output_of(window)?)
    }

    pub(super) fn take_over(&mut self, window: &Window, kind: Takeover, area: Rectangle<i32, Logical>) {
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
        self.restack_above();
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
        self.restack_above();
    }

    pub fn maximize(&mut self, window: &Window) {
        if let Some(area) = self.maximized_area(window) {
            self.take_over(window, Takeover::Maximized, area);
        }
    }

    /// The area a maximized `window` fills: the work area of its monitor,
    /// less its title bar.
    pub(super) fn maximized_area(&self, window: &Window) -> Option<Rectangle<i32, Logical>> {
        self.usable_area(window).map(|area| below_frame(window, area))
    }

    /// Fullscreen on `on` (the client's choice), else the window's monitor.
    pub fn fullscreen(&mut self, window: &Window, on: Option<Output>) {
        let area = match on {
            Some(output) => self.space.output_geometry(&output),
            None => self.output_area(window),
        };
        if let Some(area) = area {
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
        // (output, usable area, full area)
        type Areas = Vec<(String, Rectangle<i32, Logical>, Rectangle<i32, Logical>)>;
        let areas: Areas = self
            .space
            .outputs()
            .filter_map(|o| Some((o.name(), self.usable_area_of(o)?, self.space.output_geometry(o)?)))
            .collect();
        let key: Vec<(String, Rectangle<i32, Logical>)> = areas.iter().map(|(n, a, _)| (n.clone(), *a)).collect();
        if self.work_areas == key {
            return;
        }
        self.work_areas = key;
        let windows: Vec<Window> = self.managed().cloned().collect();
        for window in windows {
            let Some(output) = self.output_of(&window) else { continue };
            let Some((area, full)) = areas.iter().find(|(n, _, _)| *n == output.name()).map(|(_, a, f)| (*a, *f)) else { continue };
            if self.is_taken_over(&window, Takeover::Fullscreen) {
                self.take_over(&window, Takeover::Fullscreen, full);
                continue;
            }
            if self.is_taken_over(&window, Takeover::Maximized) {
                self.take_over(&window, Takeover::Maximized, below_frame(&window, area));
                continue;
            }
            let Some(location) = self.space.element_location(&window) else { continue };
            let top = super::frames::frame_top(&window);
            let fixed = (location.x.max(area.loc.x), location.y.max(area.loc.y + top)).into();
            if fixed != location {
                self.space.map_element(window.clone(), fixed, false);
                if let Some(x11) = window.x11_surface() {
                    let _ = x11.configure(Rectangle::new(fixed, x11.geometry().size));
                }
            }
        }
    }

    /// Monitors changed: move windows that no longer touch any monitor onto
    /// the primary one, and fit windows to the new work areas.
    pub fn rescue_windows(&mut self) {
        let Some(primary) = self.primary() else { return };
        let Some(area) = self.usable_area_of(&primary) else { return };
        let outputs: Vec<Rectangle<i32, Logical>> =
            self.space.outputs().filter_map(|o| self.space.output_geometry(o)).collect();
        let lost: Vec<Window> = self
            .managed()
            .filter(|w| self.space.element_geometry(w).is_none_or(|g| is_lost(g, &outputs)))
            .cloned()
            .collect();
        for (i, window) in lost.into_iter().enumerate() {
            let offset = 32 * (i as i32 % 10);
            let location = (area.loc.x + offset, area.loc.y + offset + super::frames::frame_top(&window)).into();
            tracing::info!(location = ?location, "moving a window off a monitor that went away");
            if let Some(pos) = self.restore.iter().position(|(w, _, _)| *w == window) {
                // Restore to the new place, keep the takeover.
                self.restore[pos].1.loc = location;
            }
            self.space.map_element(window.clone(), location, false);
            if let Some(x11) = window.x11_surface() {
                let _ = x11.configure(Rectangle::new(location, x11.geometry().size));
            }
        }
        // Takeovers follow their (new) monitor; work areas are rechecked.
        self.work_areas.clear();
        self.constrain_to_work_area();
    }

    /// Close the layer surfaces of a monitor that went away, so their
    /// clients make new ones.
    pub fn close_layers(&mut self, output: &Output) {
        let layers: Vec<_> = layer_map_for_output(output).layers().cloned().collect();
        for layer in layers {
            tracing::info!(namespace = layer.namespace(), "closing a layer surface of a monitor that went away");
            layer.layer_surface().send_close();
            layer_map_for_output(output).unmap_layer(&layer);
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
        self.frames.forget(window);
        self.above.retain(|w| w != window);
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
        self.workspace_forget(window);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    #[test]
    fn monitor_choice() {
        let (left, right) = ("DP-1", "HDMI-1");
        let outputs = [(&right, r(1920, 0, 1280, 1024)), (&left, r(0, 0, 1920, 1080))];
        assert_eq!(leftmost(&outputs), Some(&left));
        // Mostly on the right monitor.
        assert_eq!(most_covered(r(1800, 100, 800, 600), &outputs), Some(&right));
        assert_eq!(most_covered(r(1500, 100, 800, 600), &outputs), Some(&left));
        assert_eq!(most_covered(r(4000, 0, 100, 100), &outputs), None);
        let rects = [r(0, 0, 1920, 1080)];
        assert!(is_lost(r(1950, 10, 300, 300), &rects), "on the monitor that went away");
        assert!(!is_lost(r(1900, 10, 300, 300), &rects), "still partly visible");
    }
}
