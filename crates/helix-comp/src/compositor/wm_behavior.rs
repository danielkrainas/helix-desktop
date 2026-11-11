//! Window-management settings in action (helix-desktop-spec
//! `contracts/wayland/README.md`, "Window-management settings"): focus
//! following the pointer, auto-raise, modifier clicks, where new windows
//! and dialogs go, bells and locating the pointer.

use std::time::{Duration, Instant};

use helix_comp::wm_settings::FocusMode;
use smithay::desktop::Window;
use smithay::input::keyboard::xkb;
use smithay::input::pointer::{Focus, GrabStartData};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, SERIAL_COUNTER};

use super::frames::{client_rect, frame_top};
use super::grabs::{Edges, MoveGrab, ResizeGrab, ResizeTracking};
use super::input::{BTN_LEFT, BTN_MIDDLE, BTN_RIGHT};
use super::wm::{is_override_redirect, Takeover};
use super::Helix;

/// How long the pointer must rest before focus follows it
/// (`focus-change-on-pointer-rest`).
const REST: Duration = Duration::from_millis(150);

/// State behind the settings.
#[derive(Default)]
pub struct WmState {
    /// Waiting for the pointer to rest over this window.
    pending_focus: Option<(Window, Instant)>,
    /// Raise this window then (`auto-raise`).
    raise_at: Option<(Window, Instant)>,
    /// New Wayland windows waiting for their first buffer to be placed.
    pub unplaced: Vec<Window>,
    /// Modal dialogs attached to their parent: (dialog, parent, offset).
    attached: Vec<(Window, Window, Point<i32, Logical>)>,
    /// Surfaces marked modal (`xdg_dialog_v1`).
    pub modal: Vec<WlSurface>,
    /// An action for a key that was not kept from the client.
    pub pending_action: Option<super::shortcuts::Action>,
}

/// Which corner a modifier resize at `pos` moves: the window quarter the
/// pointer is in.
pub fn quadrant_edges(rect: Rectangle<i32, Logical>, pos: Point<f64, Logical>) -> u32 {
    let left = pos.x < f64::from(rect.loc.x) + f64::from(rect.size.w) / 2.0;
    let top = pos.y < f64::from(rect.loc.y) + f64::from(rect.size.h) / 2.0;
    (if top { 1 } else { 2 }) | (if left { 4 } else { 8 })
}

/// Where a new window of `size` (a title bar `top` above it) opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    Keep,
    At(Point<i32, Logical>),
    Maximize,
}

/// Placement of a new window without a parent.
pub fn place_toplevel(area: Rectangle<i32, Logical>, size: (i32, i32), top: i32, center: bool, auto_maximize: bool) -> Placement {
    if auto_maximize && size.0 >= area.size.w && size.1 + top >= area.size.h {
        return Placement::Maximize;
    }
    if center {
        let x = area.loc.x + (area.size.w - size.0) / 2;
        let y = area.loc.y + ((area.size.h - size.1 - top) / 2).max(0) + top;
        return Placement::At((x, y).into());
    }
    Placement::Keep
}

/// Placement of a dialog over `parent` (its own area): centered, or at the
/// parent's top edge when attached.
pub fn place_dialog(parent: Rectangle<i32, Logical>, size: (i32, i32), top: i32, attached: bool) -> Point<i32, Logical> {
    let x = parent.loc.x + (parent.size.w - size.0) / 2;
    let y = if attached { parent.loc.y + top } else { parent.loc.y + (parent.size.h - size.1) / 2 };
    (x, y).into()
}

impl Helix {
    /// Settings changed (or start): follow them.
    pub fn apply_wm_settings(&mut self) {
        let settings = helix_comp::wm_settings::read();
        let sym = |name: &String| {
            let sym = xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS);
            (sym.raw() != 0).then_some(sym)
        };
        let overlay = settings.overlay_keys.iter().filter_map(sym).collect();
        let locate = settings.locate_pointer_key.as_ref().and_then(sym);
        self.shortcuts.set_special_keys(overlay, locate);
        if settings != self.wm_settings {
            tracing::info!(?settings.focus_mode, "window-management settings");
        }
        self.cursors.set_theme(&settings.cursor_theme, settings.cursor_size);
        let luminance_changed = settings.output_luminance != self.wm_settings.output_luminance;
        self.wm_settings = settings;
        if luminance_changed {
            self.apply_luminance();
        }
    }

    /// Focus the window the pointer is over, as `focus-mode` says.
    pub fn focus_follows_pointer(&mut self, pos: Point<f64, Logical>) {
        let mode = self.wm_settings.focus_mode;
        if mode == FocusMode::Click || self.lock.locked || self.seat.get_pointer().unwrap().is_grabbed() || self.over_panel(pos) {
            return;
        }
        let under = self.space.element_under(pos).map(|(w, _)| w.clone()).filter(|w| !is_override_redirect(w));
        let focused = self.focused_window();
        match under {
            Some(window) if focused.as_ref() != Some(&window) => {
                if self.wm_settings.focus_on_rest {
                    self.wm.pending_focus = Some((window, Instant::now()));
                } else {
                    self.focus_by_pointer(&window);
                }
            }
            Some(_) => self.wm.pending_focus = None,
            None => {
                self.wm.pending_focus = None;
                if mode == FocusMode::Mouse && focused.is_some() && self.frame_under(pos).is_none() {
                    self.unfocus_all();
                }
            }
        }
    }

    fn focus_by_pointer(&mut self, window: &Window) {
        self.focus_window_with(window, false);
        if self.wm_settings.auto_raise {
            let delay = Duration::from_millis(u64::from(self.wm_settings.auto_raise_delay_ms));
            self.wm.raise_at = Some((window.clone(), Instant::now() + delay));
        }
    }

    /// A modifier click (`mouse-button-modifier`) on `window`: move, resize
    /// or the window menu. True when it was one.
    pub fn modifier_click(&mut self, window: &Window, button: u32, pos: Point<f64, Logical>) -> bool {
        let Some(want) = self.wm_settings.modifier else { return false };
        let held = self.seat.get_keyboard().map(|k| k.modifier_state());
        let Some(held) = held else { return false };
        let ok = (!want.logo || held.logo) && (!want.alt || held.alt) && (!want.ctrl || held.ctrl) && (!want.shift || held.shift);
        if !ok || is_override_redirect(window) {
            return false;
        }
        let (resize_button, menu_button) =
            if self.wm_settings.resize_with_right_button { (BTN_RIGHT, BTN_MIDDLE) } else { (BTN_MIDDLE, BTN_RIGHT) };
        let pointer = self.seat.get_pointer().unwrap();
        let start_data = GrabStartData { focus: None, button, location: pos };
        let serial = SERIAL_COUNTER.next_serial();
        self.focus_window(window);
        if button == BTN_LEFT {
            self.restore_window(window);
            let initial_location = self.space.element_location(window).unwrap_or_default();
            let grab = MoveGrab { start_data, window: window.clone(), initial_location, follow: false, unmaximize: false };
            pointer.set_grab(self, grab, serial, Focus::Clear);
        } else if button == resize_button {
            if self.is_taken_over(window, Takeover::Maximized) || self.is_taken_over(window, Takeover::Fullscreen) {
                return true;
            }
            let Some(initial) = client_rect(&self.space, window) else { return true };
            let edges = Edges::from_bits(quadrant_edges(initial, pos));
            self.resize = Some(ResizeTracking { window: window.clone(), edges, initial, finished: false });
            let grab = ResizeGrab { start_data, window: window.clone(), edges, initial, last_size: initial.size, follow: false };
            pointer.set_grab(self, grab, serial, Focus::Clear);
        } else if button == menu_button {
            self.send_window_menu(window, pos.to_i32_round());
        } else {
            return false;
        }
        true
    }

    /// Show a new window: focused and on top, or (`focus-new-windows`
    /// `strict`, while another window has focus) just below the focused one.
    pub fn map_new_window(&mut self, window: &Window, location: Point<i32, Logical>) {
        let focused = self.focused_window();
        self.space.map_element(window.clone(), location, true);
        match focused.filter(|_| self.wm_settings.strict_new_windows) {
            Some(focused) => {
                self.space.raise_element(&focused, false);
                self.restack_above();
            }
            None => self.focus_window(window),
        }
    }

    /// The window `window` is a dialog of, if any.
    fn parent_of(&self, window: &Window) -> Option<Window> {
        if let Some(toplevel) = window.toplevel() {
            let parent = toplevel.parent()?;
            return self.window_for_surface(&parent);
        }
        let transient_for = window.x11_surface()?.is_transient_for()?;
        self.space.elements().find(|w| w.x11_surface().is_some_and(|x| x.window_id() == transient_for)).cloned()
    }

    /// A new window has its size: put it where the settings say.
    pub fn place_new_window(&mut self, window: &Window) {
        self.wm.unplaced.retain(|w| w != window);
        let size = window.geometry().size;
        let top = frame_top(window);
        if let Some(parent) = self.parent_of(window) {
            let Some(parent_rect) = client_rect(&self.space, &parent) else { return };
            let modal = window.toplevel().is_some_and(|t| self.wm.modal.contains(t.wl_surface()));
            let attach = modal && self.wm_settings.attach_modal_dialogs;
            let mut at = place_dialog(parent_rect, (size.w, size.h), top, attach);
            if let Some(area) = self.output_of(&parent).and_then(|o| self.usable_area_of(&o)) {
                at.x = at.x.clamp(area.loc.x, (area.loc.x + area.size.w - size.w).max(area.loc.x));
                at.y = at.y.clamp(area.loc.y + top, (area.loc.y + area.size.h - size.h).max(area.loc.y + top));
            }
            self.place_at(window, at);
            if attach {
                self.wm.attached.push((window.clone(), parent, at - parent_rect.loc));
            }
            return;
        }
        let Some(area) = self.pointer_output().and_then(|o| self.usable_area_of(&o)) else { return };
        match place_toplevel(area, (size.w, size.h), top, self.wm_settings.center_new_windows, self.wm_settings.auto_maximize) {
            Placement::Keep => {}
            Placement::At(at) => self.place_at(window, at),
            Placement::Maximize => self.maximize(window),
        }
    }

    fn place_at(&mut self, window: &Window, at: Point<i32, Logical>) {
        self.space.map_element(window.clone(), at, false);
        self.restack_above();
        if let Some(x11) = window.x11_surface() {
            let _ = x11.configure(Rectangle::new(at, window.geometry().size));
        }
    }

    /// `parent` moved to `location`: its attached dialogs follow.
    pub fn move_attached(&mut self, parent: &Window, location: Point<i32, Logical>) {
        self.wm.attached.retain(|(d, p, _)| self.space.elements().any(|e| e == d) && self.space.elements().any(|e| e == p));
        let children: Vec<(Window, Point<i32, Logical>)> =
            self.wm.attached.iter().filter(|(_, p, _)| p == parent).map(|(d, _, o)| (d.clone(), *o)).collect();
        for (dialog, offset) in children {
            self.place_at(&dialog, location + offset);
        }
    }

    /// An app rang the bell (`xdg_system_bell_v1`; `surface` if it said
    /// which).
    pub fn ring_bell(&mut self, surface: Option<&WlSurface>) {
        let window = surface.and_then(|s| self.window_for_surface(s)).or_else(|| self.focused_window());
        if self.wm_settings.audible_bell {
            helix_settings::sound::play(&helix_settings::sound::Sound::Name("bell".into()));
        }
        if !self.wm_settings.visual_bell {
            return;
        }
        let area = if self.wm_settings.fullscreen_flash {
            window.as_ref().and_then(|w| self.output_of(w)).or_else(|| self.pointer_output()).and_then(|o| self.space.output_geometry(&o))
        } else {
            window.as_ref().and_then(|w| {
                let rect = client_rect(&self.space, w)?;
                let top = frame_top(w);
                Some(Rectangle::new((rect.loc.x, rect.loc.y - top).into(), (rect.size.w, rect.size.h + top).into()))
            })
        };
        if let Some(area) = area {
            self.frames.flash(area);
            self.render_all();
        }
    }

    /// Show where the pointer is.
    pub fn locate_pointer(&mut self) {
        let pos = self.seat.get_pointer().unwrap().current_location();
        self.frames.locate(pos);
        self.render_all();
    }

    /// Every 50 ms: rest focus, auto-raise, animations.
    pub fn wm_tick(&mut self) {
        let now = Instant::now();
        if let Some((window, since)) = self.wm.pending_focus.clone() {
            if now.duration_since(since) >= REST {
                self.wm.pending_focus = None;
                let pos = self.seat.get_pointer().unwrap().current_location();
                let still = self.space.element_under(pos).is_some_and(|(w, _)| *w == window);
                if still {
                    self.focus_by_pointer(&window);
                }
            }
        }
        if let Some((window, at)) = self.wm.raise_at.clone() {
            if now >= at {
                self.wm.raise_at = None;
                if self.focused_window().as_ref() == Some(&window) {
                    self.space.raise_element(&window, false);
                    self.restack_above();
                }
            }
        }
        self.alive_tick(now);
        self.idle_inhibit_tick();
        self.release_hidden_barriers();
        self.remote_share_tick();
        if self.frames.animate(now) {
            self.render_all();
        }
    }

    /// Run an action from a key the client also got.
    pub fn run_pending_action(&mut self, time: u32) {
        if let Some(action) = self.wm.pending_action.take() {
            self.run_shortcut(action, time);
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
    fn quadrants() {
        let rect = r(0, 0, 100, 100);
        assert_eq!(quadrant_edges(rect, (10.0, 10.0).into()), 5, "top-left");
        assert_eq!(quadrant_edges(rect, (90.0, 90.0).into()), 10, "bottom-right");
        assert_eq!(quadrant_edges(rect, (90.0, 10.0).into()), 9);
    }

    #[test]
    fn new_windows() {
        let area = r(0, 32, 1000, 768);
        assert_eq!(place_toplevel(area, (400, 300), 0, false, true), Placement::Keep);
        assert_eq!(place_toplevel(area, (400, 300), 0, true, true), Placement::At((300, 266).into()));
        assert_eq!(place_toplevel(area, (400, 300), 34, true, true), Placement::At((300, 32 + 217 + 34).into()));
        assert_eq!(place_toplevel(area, (1000, 768), 0, true, true), Placement::Maximize);
        assert_eq!(place_toplevel(area, (1000, 768), 0, true, false), Placement::At((0, 32).into()));
        let parent = r(100, 100, 600, 400);
        assert_eq!(place_dialog(parent, (200, 100), 0, false), (300, 250).into());
        assert_eq!(place_dialog(parent, (200, 100), 0, true), (300, 100).into());
    }
}
