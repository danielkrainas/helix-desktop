//! Window-management actions the shell asks for (window menu entries and
//! `org.gnome.desktop.wm.keybindings` keys; helix-desktop-spec
//! `contracts/wayland/README.md`, "Window menu and window keys"), and the
//! window menu event.

use helix_protocols::server::helix_window_list_v1::{Action, MenuState};
use smithay::desktop::Window;
use smithay::input::pointer::{Focus, GrabStartData};
use smithay::output::Output;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{Logical, Point, Rectangle, SERIAL_COUNTER};

use super::frames::frame_top;
use super::grabs::{Edges, MoveGrab, ResizeGrab, ResizeTracking};
use super::wm::{is_override_redirect, Takeover};
use super::Helix;

/// A direction to another monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// The nearest of `others` that lies entirely `dir` of `from`.
pub fn neighbor(
    from: Rectangle<i32, Logical>,
    others: &[Rectangle<i32, Logical>],
    dir: Direction,
) -> Option<Rectangle<i32, Logical>> {
    let center = |r: &Rectangle<i32, Logical>| (r.loc.x + r.size.w / 2, r.loc.y + r.size.h / 2);
    let (cx, cy) = center(&from);
    others
        .iter()
        .filter(|o| **o != from)
        .filter(|o| match dir {
            Direction::Left => o.loc.x + o.size.w <= from.loc.x,
            Direction::Right => o.loc.x >= from.loc.x + from.size.w,
            Direction::Up => o.loc.y + o.size.h <= from.loc.y,
            Direction::Down => o.loc.y >= from.loc.y + from.size.h,
        })
        .min_by_key(|o| {
            let (ox, oy) = center(o);
            i64::from(ox - cx).pow(2) + i64::from(oy - cy).pow(2)
        })
        .copied()
}

/// Where `size` (with a frame `top` above it) goes in `area` for a
/// placement action: the window's own location.
pub fn placed(action: Action, area: Rectangle<i32, Logical>, size: (i32, i32), top: i32, current: Point<i32, Logical>) -> Point<i32, Logical> {
    let (w, h) = (size.0, size.1 + top);
    let left = area.loc.x;
    let right = area.loc.x + area.size.w - w;
    let center_x = area.loc.x + (area.size.w - w) / 2;
    let upper = area.loc.y;
    let lower = area.loc.y + area.size.h - h;
    let center_y = area.loc.y + (area.size.h - h) / 2;
    let (x, y) = match action {
        Action::MoveToCenter => (center_x, center_y),
        Action::MoveToSideN => (current.x, upper),
        Action::MoveToSideS => (current.x, lower),
        Action::MoveToSideE => (right, current.y - top),
        Action::MoveToSideW => (left, current.y - top),
        Action::MoveToCornerNe => (right, upper),
        Action::MoveToCornerNw => (left, upper),
        Action::MoveToCornerSe => (right, lower),
        Action::MoveToCornerSw => (left, lower),
        _ => (current.x, current.y - top),
    };
    (x, y + top).into()
}

impl Helix {
    /// Run `action` on the shell's window `id` (0: the focused window).
    pub fn window_action(&mut self, id: u32, action: Action) {
        let window = if id == 0 { self.focused_window() } else { self.window_by_list_id(id) };
        let Some(window) = window.filter(|w| !is_override_redirect(w)) else {
            if action == Action::ShowDesktop {
                self.toggle_show_desktop();
            }
            return;
        };
        tracing::debug!(?action, "window action");
        match action {
            Action::Close => close_window(&window),
            Action::Minimize => self.minimize(&window),
            Action::Maximize => self.maximize(&window),
            Action::Unmaximize => self.restore_window(&window),
            Action::ToggleMaximize => {
                if self.is_taken_over(&window, Takeover::Maximized) {
                    self.restore_window(&window);
                } else {
                    self.maximize(&window);
                }
            }
            Action::MaximizeHorizontally => self.toggle_maximize_direction(&window, true),
            Action::MaximizeVertically => self.toggle_maximize_direction(&window, false),
            Action::ToggleFullscreen => {
                if self.is_taken_over(&window, Takeover::Fullscreen) {
                    self.restore_window(&window);
                } else {
                    self.fullscreen(&window, None);
                }
            }
            Action::ToggleAbove => self.toggle_above(&window),
            Action::BeginMove => self.begin_follow(window, false),
            Action::BeginResize => self.begin_follow(window, true),
            Action::ShowMenu => {
                let top = frame_top(&window);
                if let Some(loc) = self.space.element_location(&window) {
                    self.send_window_menu(&window, (loc.x, loc.y - top).into());
                }
            }
            Action::MoveToMonitorLeft => self.move_to_monitor(&window, Direction::Left),
            Action::MoveToMonitorRight => self.move_to_monitor(&window, Direction::Right),
            Action::MoveToMonitorUp => self.move_to_monitor(&window, Direction::Up),
            Action::MoveToMonitorDown => self.move_to_monitor(&window, Direction::Down),
            Action::MoveToCenter
            | Action::MoveToSideN
            | Action::MoveToSideS
            | Action::MoveToSideE
            | Action::MoveToSideW
            | Action::MoveToCornerNe
            | Action::MoveToCornerNw
            | Action::MoveToCornerSe
            | Action::MoveToCornerSw => self.place_window(&window, action),
            Action::Raise => self.focus_window(&window),
            Action::Lower => self.lower_window(&window),
            Action::RaiseOrLower => {
                if self.top_window().as_ref() == Some(&window) {
                    self.lower_window(&window);
                } else {
                    self.focus_window(&window);
                }
            }
            Action::ShowDesktop => self.toggle_show_desktop(),
            Action::ToggleTiledLeft => self.toggle_tiled(&window, Takeover::TiledLeft),
            Action::ToggleTiledRight => self.toggle_tiled(&window, Takeover::TiledRight),
            _ => {}
        }
        self.sync_window_list();
    }

    /// Tell the shell to show `window`'s menu at `at` (layout coordinates).
    pub fn send_window_menu(&mut self, window: &Window, at: Point<i32, Logical>) {
        self.sync_window_list();
        let Some(id) = self.window_list.ids.iter().find(|(w, _)| w == window).map(|(_, id)| *id) else { return };
        let mut state = MenuState::empty();
        if self.is_taken_over(window, Takeover::Maximized) {
            state |= MenuState::Maximized;
        }
        if self.is_taken_over(window, Takeover::Fullscreen) {
            state |= MenuState::Fullscreen;
        }
        if self.above.contains(window) {
            state |= MenuState::Above;
        }
        if self.workspaces.is_sticky(window) {
            state |= MenuState::OnAllWorkspaces;
        }
        if let Some(current) = self.output_of(window).and_then(|o| self.space.output_geometry(&o)) {
            let others: Vec<_> = self.space.outputs().filter_map(|o| self.space.output_geometry(o)).collect();
            for (dir, bit) in [
                (Direction::Left, MenuState::MonitorLeft),
                (Direction::Right, MenuState::MonitorRight),
                (Direction::Up, MenuState::MonitorUp),
                (Direction::Down, MenuState::MonitorDown),
            ] {
                if neighbor(current, &others, dir).is_some() {
                    state |= bit;
                }
            }
        }
        for listener in self.window_list.listeners.iter().filter(|l| l.version() >= 4) {
            listener.menu(id, at.x, at.y, state);
        }
    }

    /// Keep Always on Top windows above the rest (after something was
    /// raised).
    pub fn restack_above(&mut self) {
        let above: Vec<Window> = self.space.elements().filter(|w| self.above.contains(w)).cloned().collect();
        for window in above {
            self.space.raise_element(&window, false);
        }
    }

    fn toggle_above(&mut self, window: &Window) {
        if let Some(pos) = self.above.iter().position(|w| w == window) {
            self.above.remove(pos);
        } else {
            self.above.push(window.clone());
            self.restack_above();
        }
    }

    /// Move (or resize from the bottom-right corner) with the pointer, no
    /// button held: a click ends it, Escape undoes it.
    fn begin_follow(&mut self, window: Window, resize: bool) {
        if self.is_taken_over(&window, Takeover::Maximized) || self.is_taken_over(&window, Takeover::Fullscreen) {
            return;
        }
        let Some(location) = self.space.element_location(&window) else { return };
        self.focus_window(&window);
        let pointer = self.seat.get_pointer().unwrap();
        let start_data = GrabStartData { focus: None, button: 0, location: pointer.current_location() };
        let initial = Rectangle::new(location, window.geometry().size);
        self.follow = Some((window.clone(), initial, resize));
        let serial = SERIAL_COUNTER.next_serial();
        if resize {
            let edges = Edges::from_bits(2 | 8);
            self.resize = Some(ResizeTracking { window: window.clone(), edges, initial, finished: false });
            pointer.set_grab(self, ResizeGrab { start_data, window, edges, initial, last_size: initial.size, follow: true }, serial, Focus::Clear);
        } else {
            pointer.set_grab(self, MoveGrab { start_data, window, initial_location: location, follow: true, unmaximize: false }, serial, Focus::Clear);
        }
    }

    /// Escape during Move/Resize: put the window back.
    pub fn cancel_follow(&mut self) {
        let Some((window, initial, resize)) = self.follow.take() else { return };
        let pointer = self.seat.get_pointer().unwrap();
        pointer.unset_grab(self, SERIAL_COUNTER.next_serial(), 0);
        if resize {
            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|state| {
                    state.states.unset(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Resizing);
                    state.size = Some(initial.size);
                });
                toplevel.send_pending_configure();
            }
        }
        self.space.map_element(window.clone(), initial.loc, true);
        if let Some(x11) = window.x11_surface() {
            let _ = x11.configure(initial);
        }
    }

    fn move_window_to(&mut self, window: &Window, location: Point<i32, Logical>) {
        self.space.map_element(window.clone(), location, false);
        if let Some(x11) = window.x11_surface() {
            let _ = x11.configure(Rectangle::new(location, window.geometry().size));
        }
    }

    fn move_to_monitor(&mut self, window: &Window, dir: Direction) {
        let Some(from) = self.output_of(window) else { return };
        let outputs: Vec<(Output, Rectangle<i32, Logical>)> =
            self.space.outputs().filter_map(|o| Some((o.clone(), self.space.output_geometry(o)?))).collect();
        let Some(from_geo) = self.space.output_geometry(&from) else { return };
        let geos: Vec<_> = outputs.iter().map(|(_, g)| *g).collect();
        let Some(target_geo) = neighbor(from_geo, &geos, dir) else { return };
        let Some(target) = outputs.iter().find(|(_, g)| *g == target_geo).map(|(o, _)| o.clone()) else { return };
        let (Some(from_area), Some(to_area)) = (self.usable_area_of(&from), self.usable_area_of(&target)) else { return };
        let shift = |p: Point<i32, Logical>, size: (i32, i32)| -> Point<i32, Logical> {
            let x = (to_area.loc.x + p.x - from_area.loc.x).clamp(to_area.loc.x, (to_area.loc.x + to_area.size.w - size.0).max(to_area.loc.x));
            let y = (to_area.loc.y + p.y - from_area.loc.y).clamp(to_area.loc.y, (to_area.loc.y + to_area.size.h - size.1).max(to_area.loc.y));
            (x, y).into()
        };
        // Taken-over windows come back to the same place on the new monitor.
        if let Some(entry) = self.restore.iter_mut().find(|(w, _, _)| w == window) {
            let size = (entry.1.size.w, entry.1.size.h);
            entry.1.loc = shift(entry.1.loc, size);
        }
        if self.is_taken_over(window, Takeover::Fullscreen) {
            self.move_window_to(window, target_geo.loc);
            self.fullscreen(window, Some(target));
        } else if self.is_taken_over(window, Takeover::Maximized) {
            self.move_window_to(window, to_area.loc);
            self.maximize(window);
        } else if let Some(location) = self.space.element_location(window) {
            let size = window.geometry().size;
            let top = frame_top(window);
            let moved = shift((location.x, location.y - top).into(), (size.w, size.h + top));
            self.move_window_to(window, (moved.x, moved.y + top).into());
        }
        self.focus_window(window);
    }

    fn place_window(&mut self, window: &Window, action: Action) {
        if self.is_taken_over(window, Takeover::Maximized) || self.is_taken_over(window, Takeover::Fullscreen) {
            return;
        }
        let (Some(area), Some(location)) = (self.output_of(window).and_then(|o| self.usable_area_of(&o)), self.space.element_location(window)) else {
            return;
        };
        let size = window.geometry().size;
        let to = placed(action, area, (size.w, size.h), frame_top(window), location);
        self.move_window_to(window, to);
    }

    /// Minimize the workspace's windows, or bring them back.
    fn toggle_show_desktop(&mut self) {
        if let Some(hidden) = self.shown_desktop.take() {
            if hidden.iter().all(|w| self.minimized.contains(w)) {
                for window in hidden.iter() {
                    self.focus_window(window);
                }
                return;
            }
        }
        let visible: Vec<Window> = self.space.elements().filter(|w| !is_override_redirect(w)).cloned().collect();
        for window in &visible {
            self.minimize(window);
        }
        self.shown_desktop = Some(visible);
    }
}

/// Ask a window to close, as its own close button would.
pub fn close_window(window: &Window) {
    if let Some(toplevel) = window.toplevel() {
        toplevel.send_close();
    } else if let Some(x11) = window.x11_surface() {
        let _ = x11.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    #[test]
    fn neighbors() {
        let a = r(0, 0, 1920, 1080);
        let b = r(1920, 0, 1280, 1024);
        let c = r(0, 1080, 1920, 1080);
        let all = [a, b, c];
        assert_eq!(neighbor(a, &all, Direction::Right), Some(b));
        assert_eq!(neighbor(a, &all, Direction::Down), Some(c));
        assert_eq!(neighbor(a, &all, Direction::Left), None);
        assert_eq!(neighbor(b, &all, Direction::Left), Some(a), "the nearer of a and c");
    }

    #[test]
    fn placement() {
        let area = r(0, 32, 1000, 768);
        let at = Point::from((100, 200));
        assert_eq!(placed(Action::MoveToCenter, area, (400, 300), 0, at), (300, 266).into());
        assert_eq!(placed(Action::MoveToCornerSe, area, (400, 300), 0, at), (600, 500).into());
        assert_eq!(placed(Action::MoveToSideN, area, (400, 300), 34, at), (100, 66).into(), "below the title bar");
        assert_eq!(placed(Action::MoveToSideW, area, (400, 300), 0, at), (0, 200).into());
    }
}
