//! Smaller Wayland protocols (helix-desktop-spec `contracts/wayland/README.md`,
//! "More Wayland protocols"): idle inhibition, pointer constraints and
//! relative motion, keyboard shortcut inhibition, foreign toplevels,
//! pointer gestures and Xwayland keyboard grabs.

use smithay::desktop::{layer_map_for_output, WindowSurfaceType};
use smithay::input::pointer::{PointerHandle, RelativeMotionEvent};
use smithay::input::Seat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{Logical, Point, SERIAL_COUNTER};
use smithay::wayland::compositor::get_parent;
use smithay::wayland::idle_inhibit::IdleInhibitHandler;
use smithay::wayland::keyboard_shortcuts_inhibit::{
    KeyboardShortcutsInhibitHandler, KeyboardShortcutsInhibitState, KeyboardShortcutsInhibitor,
};
use smithay::wayland::pointer_constraints::{with_pointer_constraint, PointerConstraint, PointerConstraintsHandler};
use smithay::wayland::xdg_foreign::{XdgForeignHandler, XdgForeignState};
use smithay::wayland::xwayland_keyboard_grab::{XWaylandKeyboardGrab, XWaylandKeyboardGrabHandler};

use super::focus::KeyboardFocusTarget;
use super::Helix;

/// The globals of these protocols, kept alive.
pub type Globals = (
    smithay::wayland::idle_inhibit::IdleInhibitManagerState,
    smithay::wayland::relative_pointer::RelativePointerManagerState,
    smithay::wayland::pointer_constraints::PointerConstraintsState,
    smithay::wayland::pointer_gestures::PointerGesturesState,
    smithay::wayland::presentation::PresentationState,
    smithay::wayland::single_pixel_buffer::SinglePixelBufferState,
    smithay::wayland::fifo::FifoManagerState,
    smithay::wayland::commit_timing::CommitTimingManagerState,
    smithay::wayland::xwayland_keyboard_grab::XWaylandKeyboardGrabState,
);

pub fn globals(dh: &smithay::reexports::wayland_server::DisplayHandle) -> Globals {
    use smithay::utils::{Clock, Monotonic};
    (
        smithay::wayland::idle_inhibit::IdleInhibitManagerState::new::<Helix>(dh),
        smithay::wayland::relative_pointer::RelativePointerManagerState::new::<Helix>(dh),
        smithay::wayland::pointer_constraints::PointerConstraintsState::new::<Helix>(dh),
        smithay::wayland::pointer_gestures::PointerGesturesState::new::<Helix>(dh),
        // Frame times are CLOCK_MONOTONIC (`Helix::clock`).
        smithay::wayland::presentation::PresentationState::new::<Helix>(dh, Clock::<Monotonic>::new().id() as u32),
        smithay::wayland::single_pixel_buffer::SinglePixelBufferState::new::<Helix>(dh),
        smithay::wayland::fifo::FifoManagerState::new::<Helix>(dh),
        smithay::wayland::commit_timing::CommitTimingManagerState::new::<Helix>(dh),
        smithay::wayland::xwayland_keyboard_grab::XWaylandKeyboardGrabState::new::<Helix>(dh),
    )
}

/// A shortcut inhibitor and the user's answer for it.
#[derive(Debug)]
struct ShortcutInhibit {
    inhibitor: KeyboardShortcutsInhibitor,
    /// `None` while the shell has not answered.
    allowed: Option<bool>,
}

#[derive(Debug, Default)]
pub struct ProtocolState {
    /// Surfaces with an idle inhibitor.
    idle_inhibitors: Vec<WlSurface>,
    /// What the IdleMonitor was last told.
    idle_inhibited: bool,
    /// The host pointer position last seen (nested), for relative motion.
    last_host_pos: Option<Point<f64, Logical>>,
    shortcut_inhibits: Vec<ShortcutInhibit>,
    /// The surface restore-shortcuts suspended, until it loses focus.
    suspended: Option<WlSurface>,
    /// The X11 window holding an Xwayland keyboard grab.
    xwayland_grab: Option<WlSurface>,
    /// A 3/4-finger swipe Helix keeps: how far it went.
    swipe: Option<Point<f64, Logical>>,
}

/// Fingers a swipe needs to be Helix's.
const SWIPE_FINGERS: std::ops::RangeInclusive<u32> = 3..=4;
/// How far (logical pixels) a swipe goes sideways to switch workspace.
const SWIPE_DISTANCE: f64 = 100.0;

/// The workspace a finished swipe of `moved` leads to from `active` (of
/// `count`): fingers moving left show the next one.
pub fn swipe_target(active: usize, count: usize, moved: Point<f64, Logical>) -> Option<usize> {
    if moved.x.abs() < SWIPE_DISTANCE || moved.x.abs() <= moved.y.abs() {
        return None;
    }
    let target = if moved.x < 0.0 { active.checked_add(1)? } else { active.checked_sub(1)? };
    (target < count).then_some(target)
}

fn root_of(surface: &WlSurface) -> WlSurface {
    let mut root = surface.clone();
    while let Some(parent) = get_parent(&root) {
        root = parent;
    }
    root
}

impl Helix {
    // ---- Idle inhibition --------------------------------------------------

    /// Is `surface` shown: its window mapped (on the active workspace, not
    /// minimized), or a shell or lock surface?
    fn surface_shown(&self, surface: &WlSurface) -> bool {
        let root = root_of(surface);
        if self.space.elements().any(|w| super::wm::surface_of(w).as_ref() == Some(&root)) {
            return true;
        }
        if self.lock.locked {
            return self.space.outputs().any(|o| self.lock.surface_for(o).is_some_and(|l| l.wl_surface() == &root));
        }
        self.space
            .outputs()
            .any(|o| layer_map_for_output(o).layer_for_surface(&root, WindowSurfaceType::TOPLEVEL).is_some())
    }

    /// Every tick: tell the IdleMonitor whether a shown surface inhibits
    /// idling.
    pub fn idle_inhibit_tick(&mut self) {
        self.protocols.idle_inhibitors.retain(|s| s.is_alive());
        let inhibited = self.protocols.idle_inhibitors.iter().any(|s| self.surface_shown(s));
        if inhibited != self.protocols.idle_inhibited {
            self.protocols.idle_inhibited = inhibited;
            self.send(helix_comp::Event::IdleInhibited(inhibited));
        }
    }

    // ---- Pointer constraints and relative motion ---------------------------

    /// The active constraint under the pointer: `Some(None)` locked,
    /// `Some(Some(..))` confined to the surface (at origin) and region.
    fn active_constraint(&self, pointer: &PointerHandle<Self>) -> Option<Constraint> {
        let pos = pointer.current_location();
        let focus = pointer.current_focus()?;
        let (_, origin) = self.surface_under(pos).filter(|(s, _)| *s == focus)?;
        let mut out = None;
        with_pointer_constraint(&focus, pointer, |c| {
            if let Some(c) = c.filter(|c| c.is_active()) {
                out = Some(match &*c {
                    PointerConstraint::Locked(_) => Constraint::Locked,
                    PointerConstraint::Confined(confined) => Constraint::Confined {
                        surface: focus.clone(),
                        origin,
                        region: confined.region().cloned(),
                    },
                });
            }
        });
        out
    }

    /// Where the pointer may go on its way from `current` to `target`.
    fn confined_target(&self, surface: &WlSurface, origin: Point<f64, Logical>, region: Option<&smithay::wayland::compositor::RegionAttributes>, current: Point<f64, Logical>, target: Point<f64, Logical>) -> Point<f64, Logical> {
        let ok = |p: Point<f64, Logical>| {
            self.surface_under(p).is_some_and(|(s, _)| s == *surface)
                && region.is_none_or(|r| r.contains((p - origin).to_i32_floor()))
        };
        [target, (target.x, current.y).into(), (current.x, target.y).into()]
            .into_iter()
            .find(|p| ok(*p))
            .unwrap_or(current)
    }

    /// The pointer moved by `delta` (a mouse, a touchpad, a remote).
    /// `utime` is the event time in microseconds.
    pub(super) fn pointer_moved_by(&mut self, delta: Point<f64, Logical>, unaccel: Point<f64, Logical>, utime: u64, time: u32) {
        let pointer = self.seat.get_pointer().unwrap();
        let current = pointer.current_location();
        let target = self.clamp_to_outputs(current + delta);
        self.pointer_moved(current, target, delta, unaccel, utime, time);
    }

    /// The host pointer is at `pos` (nested): relative motion is the
    /// host's.
    pub(super) fn pointer_moved_to_host(&mut self, pos: Point<f64, Logical>, utime: u64, time: u32) {
        let pointer = self.seat.get_pointer().unwrap();
        let current = pointer.current_location();
        let delta = self.protocols.last_host_pos.map_or_else(|| pos - current, |last| pos - last);
        self.protocols.last_host_pos = Some(pos);
        self.pointer_moved(current, pos, delta, delta, utime, time);
    }

    fn pointer_moved(&mut self, current: Point<f64, Logical>, target: Point<f64, Logical>, delta: Point<f64, Logical>, unaccel: Point<f64, Logical>, utime: u64, time: u32) {
        let pointer = self.seat.get_pointer().unwrap();
        let constraint = if pointer.is_grabbed() { None } else { self.active_constraint(&pointer) };
        if delta != Point::default() || unaccel != Point::default() {
            let under = self.surface_under(current);
            pointer.relative_motion(self, under, &RelativeMotionEvent { delta, delta_unaccel: unaccel, utime });
        }
        let target = match constraint {
            Some(Constraint::Locked) => {
                pointer.frame(self);
                return;
            }
            Some(Constraint::Confined { surface, origin, region }) => {
                self.confined_target(&surface, origin, region.as_ref(), current, target)
            }
            None => target,
        };
        self.pointer_to(target, time);
        self.activate_constraint_under();
    }

    /// Activate the constraint of the surface under the pointer when the
    /// pointer is in its region.
    pub(super) fn activate_constraint_under(&mut self) {
        let pointer = self.seat.get_pointer().unwrap();
        let pos = pointer.current_location();
        let Some(focus) = pointer.current_focus() else { return };
        let Some((_, origin)) = self.surface_under(pos).filter(|(s, _)| *s == focus) else { return };
        with_pointer_constraint(&focus, &pointer, |c| {
            if let Some(c) = c {
                if !c.is_active() && c.region().is_none_or(|r| r.contains((pos - origin).to_i32_floor())) {
                    c.activate();
                }
            }
        });
    }

    // ---- Gestures -----------------------------------------------------------

    pub(super) fn gesture_swipe_begin(&mut self, fingers: u32, time: u32) {
        if SWIPE_FINGERS.contains(&fingers) {
            self.protocols.swipe = Some(Point::default());
            return;
        }
        self.protocols.swipe = None;
        let pointer = self.seat.get_pointer().unwrap();
        let e = smithay::input::pointer::GestureSwipeBeginEvent { serial: SERIAL_COUNTER.next_serial(), time, fingers };
        pointer.gesture_swipe_begin(self, &e);
    }

    pub(super) fn gesture_swipe_update(&mut self, delta: Point<f64, Logical>, time: u32) {
        if let Some(moved) = self.protocols.swipe.as_mut() {
            *moved += delta;
            return;
        }
        let pointer = self.seat.get_pointer().unwrap();
        pointer.gesture_swipe_update(self, &smithay::input::pointer::GestureSwipeUpdateEvent { time, delta });
    }

    pub(super) fn gesture_swipe_end(&mut self, cancelled: bool, time: u32) {
        if let Some(moved) = self.protocols.swipe.take() {
            let layout = self.workspaces.layout;
            if !cancelled && !self.lock.locked {
                if let Some(target) = swipe_target(layout.active, layout.count, moved) {
                    tracing::debug!(target, "workspace swipe");
                    self.switch_workspace(target);
                }
            }
            return;
        }
        let pointer = self.seat.get_pointer().unwrap();
        let e = smithay::input::pointer::GestureSwipeEndEvent { serial: SERIAL_COUNTER.next_serial(), time, cancelled };
        pointer.gesture_swipe_end(self, &e);
    }

    // ---- Keyboard shortcut inhibition --------------------------------------

    /// The shell answered whether window `id` may get all shortcuts.
    pub fn answer_shortcuts_inhibit(&mut self, id: u32, allowed: bool) {
        let Some(window) = self.window_by_list_id(id) else { return };
        let window_surface = super::wm::surface_of(&window);
        for entry in self.protocols.shortcut_inhibits.iter_mut() {
            if entry.allowed.is_none() && Some(root_of(entry.inhibitor.wl_surface())) == window_surface {
                entry.allowed = Some(allowed);
            }
        }
        self.update_shortcut_inhibit(self.focused_surface());
    }

    /// Activate the inhibitor of the focused surface (when allowed and not
    /// suspended); deactivate the others. `focused` is the keyboard focus
    /// (passed in: during a focus change the keyboard is locked).
    pub fn update_shortcut_inhibit(&mut self, focused: Option<WlSurface>) {
        if self.protocols.suspended.as_ref().is_some_and(|s| Some(s) != focused.as_ref() || !s.is_alive()) {
            self.protocols.suspended = None;
        }
        self.protocols.shortcut_inhibits.retain(|e| e.inhibitor.wl_surface().is_alive());
        for entry in &self.protocols.shortcut_inhibits {
            let surface = entry.inhibitor.wl_surface();
            let want = entry.allowed == Some(true)
                && focused.as_ref() == Some(surface)
                && self.protocols.suspended.as_ref() != Some(surface);
            if want && !entry.inhibitor.is_active() {
                entry.inhibitor.activate();
            } else if !want && entry.inhibitor.is_active() {
                entry.inhibitor.inactivate();
            }
        }
    }

    /// Do Helix's and the shell's shortcuts go to the focused app now?
    pub fn shortcuts_inhibited(&self) -> bool {
        use smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitorSeat;
        self.seat.keyboard_shortcuts_inhibited() || self.xwayland_grab_active()
    }

    fn xwayland_grab_active(&self) -> bool {
        self.protocols.xwayland_grab.as_ref().is_some_and(|s| s.is_alive())
            && self.seat.get_keyboard().is_some_and(|k| k.is_grabbed())
    }

    /// Is this key restore-shortcuts?
    pub fn is_restore_shortcuts(&self, mods: super::shortcuts::Mods, sym: smithay::input::keyboard::Keysym) -> bool {
        self.wm_settings.restore_shortcuts.iter().any(|(m, name)| {
            m.alt == mods.alt
                && m.logo == mods.logo
                && m.ctrl == mods.ctrl
                && m.shift == mods.shift
                && super::keybindings::key_from_name(name) == Some(super::shortcuts::Key::Sym(sym))
        })
    }

    /// restore-shortcuts: give the shortcuts back to Helix and the shell.
    pub fn restore_shortcuts(&mut self) {
        if self.xwayland_grab_active() {
            if let Some(keyboard) = self.seat.get_keyboard() {
                keyboard.unset_grab(self);
            }
        }
        self.protocols.xwayland_grab = None;
        self.protocols.suspended = self.focused_surface();
        self.update_shortcut_inhibit(self.focused_surface());
    }
}

/// An active pointer constraint.
enum Constraint {
    Locked,
    Confined {
        surface: WlSurface,
        origin: Point<f64, Logical>,
        region: Option<smithay::wayland::compositor::RegionAttributes>,
    },
}

impl IdleInhibitHandler for Helix {
    fn inhibit(&mut self, surface: WlSurface) {
        self.protocols.idle_inhibitors.push(surface);
        self.idle_inhibit_tick();
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        if let Some(i) = self.protocols.idle_inhibitors.iter().position(|s| *s == surface) {
            self.protocols.idle_inhibitors.remove(i);
        }
        self.idle_inhibit_tick();
    }
}

impl PointerConstraintsHandler for Helix {
    fn new_constraint(&mut self, _surface: &WlSurface, _pointer: &PointerHandle<Self>) {
        self.activate_constraint_under();
    }

    fn cursor_position_hint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>, location: Point<f64, Logical>) {
        let locked = with_pointer_constraint(surface, pointer, |c| {
            c.is_some_and(|c| c.is_active() && matches!(&*c, PointerConstraint::Locked(_)))
        });
        if !locked {
            return;
        }
        let pos = pointer.current_location();
        if let Some((_, origin)) = self.surface_under(pos).filter(|(s, _)| s == surface) {
            pointer.set_location(origin + location);
        }
    }
}

impl KeyboardShortcutsInhibitHandler for Helix {
    fn keyboard_shortcuts_inhibit_state(&mut self) -> &mut KeyboardShortcutsInhibitState {
        &mut self.keyboard_shortcuts_inhibit_state
    }

    fn new_inhibitor(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        let root = root_of(inhibitor.wl_surface());
        self.sync_window_list();
        let id = self
            .window_for_surface(&root)
            .and_then(|w| self.window_list.ids.iter().find(|(known, _)| *known == w).map(|(_, id)| *id));
        let listeners: Vec<_> = self.window_list.listeners.iter().filter(|l| l.version() >= 6).cloned().collect();
        // Only windows the shell can ask about; nobody to ask: denied.
        let allowed = match id {
            Some(id) if !listeners.is_empty() => {
                for l in &listeners {
                    l.shortcuts_inhibit(id);
                }
                None
            }
            _ => Some(false),
        };
        self.protocols.shortcut_inhibits.push(ShortcutInhibit { inhibitor, allowed });
        self.update_shortcut_inhibit(self.focused_surface());
    }

    fn inhibitor_destroyed(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        let surface = inhibitor.wl_surface().clone();
        self.protocols.shortcut_inhibits.retain(|e| e.inhibitor.wl_surface() != &surface);
    }
}

impl XdgForeignHandler for Helix {
    fn xdg_foreign_state(&mut self) -> &mut XdgForeignState {
        &mut self.xdg_foreign_state
    }
}

impl XWaylandKeyboardGrabHandler for Helix {
    fn grab(&mut self, surface: WlSurface, seat: Seat<Self>, grab: XWaylandKeyboardGrab<Self>) {
        if !self.wm_settings.xwayland_allow_grabs {
            return;
        }
        let Some(x11) = self.window_for_surface(&surface).and_then(|w| w.x11_surface().cloned()) else { return };
        if !helix_comp::wm_settings::grab_allowed(&self.wm_settings.xwayland_grab_rules, &x11.instance(), &x11.class()) {
            tracing::info!("an X11 app may not grab the keyboard");
            return;
        }
        if let Some(keyboard) = seat.get_keyboard() {
            keyboard.set_grab(self, grab, SERIAL_COUNTER.next_serial());
            self.protocols.xwayland_grab = Some(surface);
        }
    }

    fn keyboard_focus_for_xsurface(&self, surface: &WlSurface) -> Option<KeyboardFocusTarget> {
        let window = self.window_for_surface(surface)?;
        window.x11_surface()?;
        super::focus::focus_target(&window)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swipes() {
        let p = |x: f64, y: f64| Point::<f64, Logical>::from((x, y));
        assert_eq!(swipe_target(1, 4, p(-150.0, 20.0)), Some(2), "left: next");
        assert_eq!(swipe_target(1, 4, p(150.0, 20.0)), Some(0), "right: previous");
        assert_eq!(swipe_target(0, 4, p(150.0, 0.0)), None, "none before the first");
        assert_eq!(swipe_target(3, 4, p(-150.0, 0.0)), None, "none past the last");
        assert_eq!(swipe_target(1, 4, p(-90.0, 0.0)), None, "too short");
        assert_eq!(swipe_target(1, 4, p(-150.0, 160.0)), None, "mostly vertical");
    }
}

smithay::delegate_idle_inhibit!(Helix);
smithay::delegate_relative_pointer!(Helix);
smithay::delegate_pointer_constraints!(Helix);
smithay::delegate_pointer_gestures!(Helix);
smithay::delegate_keyboard_shortcuts_inhibit!(Helix);
smithay::delegate_xdg_foreign!(Helix);
smithay::delegate_xwayland_keyboard_grab!(Helix);
