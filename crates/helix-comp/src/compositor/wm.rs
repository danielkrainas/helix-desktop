//! Window management policy: focus, cycling, maximize, fullscreen,
//! minimize and close — for both Wayland (xdg) and X11 (Xwayland) windows.

use smithay::desktop::{layer_map_for_output, Window};
use smithay::output::Output;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, SERIAL_COUNTER};
use smithay::wayland::seat::WaylandFocus;

use super::Helix;

/// Configuring a toplevel only once it has made its initial commit.
///
/// xdg-shell has the compositor answer the client's initial (bufferless)
/// commit with the first configure, after the client set its app id,
/// parent, maximized or fullscreen state. Changes made to a new window
/// before then (focus, maximize, bounds) stay pending and go out together
/// in that first configure (`handle_xdg_commit`).
pub(super) trait ConfigureWhenReady {
    /// Send the pending state if it changed and the first configure was
    /// already sent; otherwise keep it for the first configure.
    fn configure_pending(&self);
}

impl ConfigureWhenReady for smithay::wayland::shell::xdg::ToplevelSurface {
    fn configure_pending(&self) {
        if self.is_initial_configure_sent() {
            self.send_pending_configure();
        }
    }
}

/// Top-layer shell surfaces that stay shown over a full-screen window:
/// the on-screen keyboard (typing into the full-screen app needs it) and
/// the dock with its reveal edge, which the shell hides itself in full
/// screen unless `autohide-in-fullscreen` lets the pointer reveal it.
const SHOWN_IN_FULLSCREEN: &[&str] = &["helix-osk", "helix-dock", "helix-dock-edge"];

/// Whether `layer` is hidden on an output in full screen: top-layer
/// surfaces are (the panel, notification banners, other clients' bars),
/// except [`SHOWN_IN_FULLSCREEN`]. Overlay surfaces (OSDs, menus, the
/// overview, critical banners) stay above the full-screen window.
pub fn hidden_in_fullscreen(layer: smithay::wayland::shell::wlr_layer::Layer, namespace: &str) -> bool {
    layer == smithay::wayland::shell::wlr_layer::Layer::Top && !SHOWN_IN_FULLSCREEN.contains(&namespace)
}

/// Why a window's geometry was taken over, so it can be restored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Takeover {
    Maximized,
    Fullscreen,
    /// The left or right half of the work area (`toggle-tiled-*`).
    TiledLeft,
    TiledRight,
}

/// The `side` half of `area`.
pub fn half(area: Rectangle<i32, Logical>, side: Takeover) -> Rectangle<i32, Logical> {
    let left = area.size.w / 2;
    match side {
        Takeover::TiledRight => Rectangle::new((area.loc.x + left, area.loc.y).into(), (area.size.w - left, area.size.h).into()),
        _ => Rectangle::new(area.loc, (left, area.size.h).into()),
    }
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

/// Where a window taken over at `before` (its own area, without a title
/// bar Helix draws) goes when a drag at `pointer` restores it to `size`:
/// under the pointer, which keeps its fraction of the width, and its
/// distance below the top (on a title bar, Helix's above the area or the
/// client's own at its top) unless the restored window is not that tall,
/// then its fraction of the height. Only the restored size counts (the
/// one asked of the client, not the size it still has), so where the
/// window was before it was maximized never does.
pub fn anchored(before: Rectangle<i32, Logical>, size: smithay::utils::Size<i32, Logical>, pointer: Point<f64, Logical>) -> Point<i32, Logical> {
    let fraction = |at: f64, start: i32, len: i32| ((at - f64::from(start)) / f64::from(len.max(1))).clamp(0.0, 1.0);
    let x = pointer.x - fraction(pointer.x, before.loc.x, before.size.w) * f64::from(size.w);
    let down = pointer.y - f64::from(before.loc.y);
    let y = if down < f64::from(size.h) {
        f64::from(before.loc.y)
    } else {
        pointer.y - fraction(pointer.y, before.loc.y, before.size.h) * f64::from(size.h)
    };
    (x.round() as i32, y.round() as i32).into()
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

    /// Managed windows, shown or hidden (minimized, on another workspace).
    fn all_managed(&self) -> Vec<Window> {
        self.space
            .elements()
            .chain(self.minimized.iter())
            .chain(self.workspaces.parked())
            .filter(|w| !is_override_redirect(w))
            .cloned()
            .collect()
    }

    /// Topmost managed window.
    pub fn top_window(&self) -> Option<Window> {
        self.managed().last().cloned()
    }

    /// Raise `window`, give it keyboard focus, and mark only it activated.
    pub fn focus_window(&mut self, window: &Window) {
        self.focus_window_with(window, true);
    }

    /// Give `window` keyboard focus, raising it or not (focus following
    /// the pointer, `raise-on-click` false).
    pub fn focus_window_with(&mut self, window: &Window, raise: bool) {
        // Nothing gets focus while the session is locked.
        if is_override_redirect(window) || self.lock.locked {
            return;
        }
        // A window with a modal dialog: the dialog gets focus (mutter's
        // meta_window_focus), the window itself shown and raised below it.
        let dialog = self.modal_dialog_of(window);
        let window = match &dialog {
            Some(dialog) => {
                tracing::debug!("focus goes to the window's modal dialog");
                self.show_for_focus(window);
                if raise {
                    self.space.raise_element(window, false);
                }
                dialog
            }
            None => window,
        };
        self.show_for_focus(window);
        if raise {
            self.space.raise_element(window, true);
            self.restack_above();
            if let (Some(xwm), Some(x11)) = (self.xwm.as_mut(), window.x11_surface()) {
                let _ = xwm.raise_window(x11);
            }
        }
        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.set_focus(self, super::focus::focus_target(window), SERIAL_COUNTER.next_serial());
        self.set_attention(window, false);
        self.check_alive(window);
        // Hidden windows (minimized, on another workspace) too: one that
        // kept "activated" while hidden would get no configure when it is
        // shown and focused again, and Chromium-based apps that minimized
        // themselves wait for one before drawing again (a still picture
        // that takes no input).
        for w in &self.all_managed() {
            w.set_activated(w == window);
            if let Some(t) = w.toplevel() {
                t.configure_pending();
            }
        }
    }

    /// Before `window` gets focus: its workspace shown, and itself if it
    /// was minimized.
    fn show_for_focus(&mut self, window: &Window) {
        self.workspace_focus(window);
        if let Some(pos) = self.minimized.iter().position(|w| w == window) {
            let window = self.minimized.remove(pos);
            let location = self.minimized_at.remove(pos);
            self.animations.window_opened(self.wm_settings.motion, &window);
            self.space.map_element(window, location, false);
        }
    }

    /// Clear keyboard focus and deactivate every window.
    pub fn unfocus_all(&mut self) {
        // While locked the lock surface keeps the keyboard.
        if self.lock.locked {
            return;
        }
        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.set_focus(self, Option::<super::focus::KeyboardFocusTarget>::None, SERIAL_COUNTER.next_serial());
        for w in &self.all_managed() {
            w.set_activated(false);
            if let Some(t) = w.toplevel() {
                t.configure_pending();
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
        match self.restore.iter_mut().find(|(w, _, _)| w == window) {
            Some(entry) => entry.2 = kind,
            None => {
                let location = self.space.element_location(window).unwrap_or_default();
                let size = window.geometry().size;
                self.restore.push((window.clone(), Rectangle::new(location, size), kind));
            }
        }
        if let Some(toplevel) = window.toplevel() {
            use xdg_toplevel::State;
            toplevel.with_pending_state(|state| {
                for s in [State::Maximized, State::Fullscreen, State::TiledLeft, State::TiledRight, State::TiledTop, State::TiledBottom] {
                    state.states.unset(s);
                }
                let set: &[State] = match kind {
                    Takeover::Maximized => &[State::Maximized],
                    Takeover::Fullscreen => &[State::Fullscreen],
                    Takeover::TiledLeft => &[State::TiledLeft, State::TiledTop, State::TiledBottom],
                    Takeover::TiledRight => &[State::TiledRight, State::TiledTop, State::TiledBottom],
                };
                for s in set {
                    state.states.set(*s);
                }
                state.size = Some(area.size);
            });
            toplevel.configure_pending();
        } else if let Some(x11) = window.x11_surface() {
            let _ = x11.set_maximized(kind == Takeover::Maximized);
            let _ = x11.set_fullscreen(kind == Takeover::Fullscreen);
            let _ = x11.configure(area);
        }
        self.animate_resize(window, area);
        self.space.map_element(window.clone(), area.loc, true);
        self.restack_above();
    }

    /// Grow or shrink `window` from where it is to `to` (client area).
    fn animate_resize(&mut self, window: &Window, to: Rectangle<i32, Logical>) {
        if let Some(location) = self.space.element_location(window) {
            let from = Rectangle::new(location, window.geometry().size);
            self.animations.window_resized(self.wm_settings.motion, window, from, to);
        }
    }

    /// Undo maximize/fullscreen, restoring the previous geometry.
    pub fn restore_window(&mut self, window: &Window) {
        self.restore_window_at(window, None);
    }

    /// Whether `window`'s geometry is taken over (maximized, tiled or
    /// full screen): dragging it restores it.
    pub fn is_any_taken_over(&self, window: &Window) -> bool {
        self.restore.iter().any(|(w, _, _)| w == window)
    }

    /// [`Self::restore_window`], with `grabbed` (a pointer position on the
    /// window, dragging it): the restored window goes under the pointer
    /// instead of back where it was ([`anchored`]).
    pub fn restore_window_at(&mut self, window: &Window, grabbed: Option<Point<f64, Logical>>) {
        let Some(pos) = self.restore.iter().position(|(w, _, _)| w == window) else {
            return;
        };
        let (_, mut geometry, _) = self.restore.remove(pos);
        if let (Some(pointer), Some(location)) = (grabbed, self.space.element_location(window)) {
            let before = Rectangle::new(location, window.geometry().size);
            geometry.loc = anchored(before, geometry.size, pointer);
        }
        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| {
                use xdg_toplevel::State;
                for s in [State::Maximized, State::Fullscreen, State::TiledLeft, State::TiledRight, State::TiledTop, State::TiledBottom] {
                    state.states.unset(s);
                }
                state.size = Some(geometry.size);
            });
            toplevel.configure_pending();
        } else if let Some(x11) = window.x11_surface() {
            let _ = x11.set_maximized(false);
            let _ = x11.set_fullscreen(false);
            let _ = x11.configure(geometry);
        }
        self.animate_resize(window, geometry);
        self.space.map_element(window.clone(), geometry.loc, true);
        self.restack_above();
    }

    /// Tile `window` to `side` (`TiledLeft`/`TiledRight`) of its work
    /// area; again restores it.
    pub fn toggle_tiled(&mut self, window: &Window, side: Takeover) {
        if self.is_taken_over(window, side) {
            self.restore_window(window);
            return;
        }
        if let Some(area) = self.usable_area(window) {
            self.take_over(window, side, below_frame(window, half(area, side)));
        }
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

    /// Whether `output` is in full screen: the topmost managed window on
    /// it is full-screen (its own dialogs above it don't count). Shell
    /// surfaces on the top layer are then hidden there
    /// ([`hidden_in_fullscreen`]); the overlay layer stays above.
    pub fn in_fullscreen(&self, output: &Output) -> bool {
        let fullscreen = |w: &Window| self.is_taken_over(w, Takeover::Fullscreen);
        for window in self.managed().collect::<Vec<_>>().into_iter().rev() {
            if self.output_of(window).as_ref() != Some(output) {
                continue;
            }
            if fullscreen(window) {
                return true;
            }
            let parent = window.toplevel().and_then(|t| t.parent()).and_then(|p| self.window_for_surface(&p));
            if parent.is_some_and(|p| fullscreen(&p)) {
                continue;
            }
            return false;
        }
        false
    }

    /// The outputs in full screen ([`Self::in_fullscreen`]).
    pub fn fullscreen_outputs(&self) -> Vec<Output> {
        self.space.outputs().filter(|o| self.in_fullscreen(o)).cloned().collect()
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
            // configure_bounds follows the work area (taking over below
            // sends the configure itself).
            if let Some(toplevel) = window.toplevel() {
                let changed = toplevel.with_pending_state(|s| s.bounds.replace(area.size) != Some(area.size));
                if changed && toplevel.is_initial_configure_sent() && !self.restore.iter().any(|(w, _, _)| *w == window) {
                    toplevel.configure_pending();
                }
            }
            if self.is_taken_over(&window, Takeover::Fullscreen) {
                self.take_over(&window, Takeover::Fullscreen, full);
                continue;
            }
            if self.is_taken_over(&window, Takeover::Maximized) {
                self.take_over(&window, Takeover::Maximized, below_frame(&window, area));
                continue;
            }
            if let Some(side) = [Takeover::TiledLeft, Takeover::TiledRight].into_iter().find(|s| self.is_taken_over(&window, *s)) {
                self.take_over(&window, side, below_frame(&window, half(area, side)));
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
        // Towards the dock: the bottom middle of the window's monitor.
        let target = self.output_of(window).and_then(|o| self.space.output_geometry(&o)).map_or(Point::default(), |g| {
            Point::from((f64::from(g.loc.x) + f64::from(g.size.w) / 2.0, f64::from(g.loc.y + g.size.h)))
        });
        self.window_leaves(window, helix_comp::animation::Kind::Minimize, target, 0);
        self.space.unmap_elem(window);
        self.minimized.push(window.clone());
        self.minimized_at.push(location);
        match self.top_window() {
            Some(top) => self.focus_window(&top),
            None => self.unfocus_all(),
        }
    }

    /// Draw `window` animating away as `kind` (if it is shown).
    pub fn window_leaves(&mut self, window: &Window, kind: helix_comp::animation::Kind, target: Point<f64, Logical>, direction: i32) {
        let (Some(location), Some(surface)) = (self.space.element_location(window), surface_of(window)) else { return };
        let geometry = window.geometry();
        let rect = Rectangle::new(location, geometry.size).to_f64();
        let origin = (location - geometry.loc).to_f64();
        self.animations.leave(self.wm_settings.motion, &surface, rect, origin, kind, target, direction, false);
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
            focus.is_none() || focus == super::focus::focus_target(window)
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
            Some(window) => {
                self.window_leaves(&window, helix_comp::animation::Kind::Close, Point::default(), 0);
                self.forget_window(&window)
            }
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

    #[test]
    fn restored_under_the_pointer() {
        let maximized = r(1280, 32, 1280, 768);
        let size = (400, 300).into();
        // On the title bar: as far below the top, the same fraction across.
        assert_eq!(anchored(maximized, size, (1280.0 + 960.0, 50.0).into()), (1280 + 960 - 300, 32).into());
        // Helix's own title bar is above the window's area.
        assert_eq!(anchored(maximized, size, (1280.0 + 640.0, 10.0).into()), (1280 + 640 - 200, 32).into());
        // Lower than the restored window is tall: the fraction of the height.
        assert_eq!(anchored(maximized, size, (1280.0 + 640.0, 32.0 + 384.0).into()), (1280 + 640 - 200, 32 + 384 - 150).into());
    }
}
