//! Interactive move and resize pointer grabs (started by xdg_toplevel
//! move/resize requests). Modeled on Smithay's `smallvil` example (MIT).

use smithay::desktop::Window;
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
    GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
    GestureSwipeUpdateEvent, GrabStartData, MotionEvent, PointerGrab, PointerInnerHandle,
    RelativeMotionEvent,
};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::SurfaceCachedState;

use super::Helix;

const BTN_LEFT: u32 = 0x110;

/// Forward the grab methods that need no special handling.
macro_rules! forward_passthrough {
    () => {
        fn relative_motion(
            &mut self,
            data: &mut Helix,
            handle: &mut PointerInnerHandle<'_, Helix>,
            focus: Option<(WlSurface, Point<f64, Logical>)>,
            event: &RelativeMotionEvent,
        ) {
            handle.relative_motion(data, focus, event);
        }
        fn axis(&mut self, data: &mut Helix, handle: &mut PointerInnerHandle<'_, Helix>, details: AxisFrame) {
            handle.axis(data, details)
        }
        fn frame(&mut self, data: &mut Helix, handle: &mut PointerInnerHandle<'_, Helix>) {
            handle.frame(data);
        }
        fn gesture_swipe_begin(&mut self, data: &mut Helix, handle: &mut PointerInnerHandle<'_, Helix>, event: &GestureSwipeBeginEvent) {
            handle.gesture_swipe_begin(data, event)
        }
        fn gesture_swipe_update(&mut self, data: &mut Helix, handle: &mut PointerInnerHandle<'_, Helix>, event: &GestureSwipeUpdateEvent) {
            handle.gesture_swipe_update(data, event)
        }
        fn gesture_swipe_end(&mut self, data: &mut Helix, handle: &mut PointerInnerHandle<'_, Helix>, event: &GestureSwipeEndEvent) {
            handle.gesture_swipe_end(data, event)
        }
        fn gesture_pinch_begin(&mut self, data: &mut Helix, handle: &mut PointerInnerHandle<'_, Helix>, event: &GesturePinchBeginEvent) {
            handle.gesture_pinch_begin(data, event)
        }
        fn gesture_pinch_update(&mut self, data: &mut Helix, handle: &mut PointerInnerHandle<'_, Helix>, event: &GesturePinchUpdateEvent) {
            handle.gesture_pinch_update(data, event)
        }
        fn gesture_pinch_end(&mut self, data: &mut Helix, handle: &mut PointerInnerHandle<'_, Helix>, event: &GesturePinchEndEvent) {
            handle.gesture_pinch_end(data, event)
        }
        fn gesture_hold_begin(&mut self, data: &mut Helix, handle: &mut PointerInnerHandle<'_, Helix>, event: &GestureHoldBeginEvent) {
            handle.gesture_hold_begin(data, event)
        }
        fn gesture_hold_end(&mut self, data: &mut Helix, handle: &mut PointerInnerHandle<'_, Helix>, event: &GestureHoldEndEvent) {
            handle.gesture_hold_end(data, event)
        }
        fn start_data(&self) -> &GrabStartData<Helix> {
            &self.start_data
        }
        fn unset(&mut self, _data: &mut Helix) {}
    };
}

/// Drags a window with the pointer.
pub struct MoveGrab {
    pub start_data: GrabStartData<Helix>,
    pub window: Window,
    pub initial_location: Point<i32, Logical>,
    /// Following the pointer with no button held (window menu Move): a
    /// click ends it.
    pub follow: bool,
    /// The window is maximized: restore it once the pointer has moved.
    pub unmaximize: bool,
}

impl PointerGrab<Helix> for MoveGrab {
    fn motion(
        &mut self,
        data: &mut Helix,
        handle: &mut PointerInnerHandle<'_, Helix>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
        let mut delta = event.location - self.start_data.location;
        if self.unmaximize {
            if delta.x.abs() + delta.y.abs() < 8.0 {
                return;
            }
            self.unmaximize = false;
            data.restore_under_pointer(&self.window, self.start_data.location);
            self.initial_location = data.space.element_location(&self.window).unwrap_or(self.initial_location);
            delta = event.location - self.start_data.location;
        }
        let location = (self.initial_location.to_f64() + delta).to_i32_round();
        data.space.map_element(self.window.clone(), location, true);
        data.restack_above();
        data.update_tile_preview(event.location);
        // X11 apps need to know where they are (for menus and input).
        if let Some(x11) = self.window.x11_surface() {
            let _ = x11.configure(Rectangle::new(location, x11.geometry().size));
        }
    }

    fn button(&mut self, data: &mut Helix, handle: &mut PointerInnerHandle<'_, Helix>, event: &ButtonEvent) {
        if self.follow {
            if event.state == smithay::backend::input::ButtonState::Pressed {
                data.follow = None;
                handle.unset_grab(self, data, event.serial, event.time, true);
                data.finish_tile_preview(&self.window);
            }
            return;
        }
        handle.button(data, event);
        if !handle.current_pressed().contains(&BTN_LEFT) {
            data.finish_tile_preview(&self.window);
            tracing::debug!(
                start = ?self.start_data.location,
                initial = ?self.initial_location,
                end = ?data.space.element_location(&self.window),
                "move finished"
            );
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    forward_passthrough!();
}

/// Which edges are being dragged (xdg_toplevel resize edge bits).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edges(u32);

impl Edges {
    /// From xdg_toplevel resize edge bits (top 1, bottom 2, left 4, right 8).
    pub fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    const TOP: u32 = 1;
    const BOTTOM: u32 = 2;
    const LEFT: u32 = 4;
    const RIGHT: u32 = 8;

    pub fn top(self) -> bool {
        self.0 & Self::TOP != 0
    }
    pub fn bottom(self) -> bool {
        self.0 & Self::BOTTOM != 0
    }
    pub fn left(self) -> bool {
        self.0 & Self::LEFT != 0
    }
    pub fn right(self) -> bool {
        self.0 & Self::RIGHT != 0
    }
}

impl From<xdg_toplevel::ResizeEdge> for Edges {
    fn from(edge: xdg_toplevel::ResizeEdge) -> Self {
        Self(edge as u32)
    }
}

/// An interactive resize in progress. Kept on the compositor state until
/// the client's final commit, so dragging a top or left edge can keep the
/// opposite edge in place once the new size is known.
#[derive(Debug, Clone)]
pub struct ResizeTracking {
    pub window: Window,
    pub edges: Edges,
    pub initial: Rectangle<i32, Logical>,
    pub finished: bool,
}

/// Resizes a window with the pointer.
pub struct ResizeGrab {
    pub start_data: GrabStartData<Helix>,
    pub window: Window,
    pub edges: Edges,
    pub initial: Rectangle<i32, Logical>,
    pub last_size: Size<i32, Logical>,
    /// Following the pointer with no button held: a click ends it.
    pub follow: bool,
}

/// New size for a drag of `delta` on `edges`, clamped to the client's
/// min/max size (0 meaning unbounded).
pub fn resized(
    initial: Size<i32, Logical>,
    edges: Edges,
    delta: Point<f64, Logical>,
    min: Size<i32, Logical>,
    max: Size<i32, Logical>,
) -> Size<i32, Logical> {
    let mut w = initial.w;
    let mut h = initial.h;
    if edges.left() {
        w = (f64::from(initial.w) - delta.x) as i32;
    } else if edges.right() {
        w = (f64::from(initial.w) + delta.x) as i32;
    }
    if edges.top() {
        h = (f64::from(initial.h) - delta.y) as i32;
    } else if edges.bottom() {
        h = (f64::from(initial.h) + delta.y) as i32;
    }
    let clamp = |v: i32, lo: i32, hi: i32| v.max(lo.max(1)).min(if hi == 0 { i32::MAX } else { hi });
    (clamp(w, min.w, max.w), clamp(h, min.h, max.h)).into()
}

impl PointerGrab<Helix> for ResizeGrab {
    fn motion(
        &mut self,
        data: &mut Helix,
        handle: &mut PointerInnerHandle<'_, Helix>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
        let delta = event.location - self.start_data.location;
        if let Some(x11) = self.window.x11_surface() {
            // X11: apply the new geometry directly, keeping the opposite
            // edge in place.
            let (min, max) = (
                x11.min_size().unwrap_or_default(),
                x11.max_size().unwrap_or_default(),
            );
            self.last_size = resized(self.initial.size, self.edges, delta, min, max);
            let mut location = self.initial.loc;
            if self.edges.left() {
                location.x += self.initial.size.w - self.last_size.w;
            }
            if self.edges.top() {
                location.y += self.initial.size.h - self.last_size.h;
            }
            let _ = x11.configure(Rectangle::new(location, self.last_size));
            data.space.map_element(self.window.clone(), location, true);
            data.restack_above();
            return;
        }
        let Some(toplevel) = self.window.toplevel() else { return };
        let (min, max) = with_states(toplevel.wl_surface(), |states| {
            let mut cached = states.cached_state.get::<SurfaceCachedState>();
            let current = cached.current();
            (current.min_size, current.max_size)
        });
        self.last_size = resized(self.initial.size, self.edges, delta, min, max);
        toplevel.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Resizing);
            state.size = Some(self.last_size);
        });
        toplevel.send_pending_configure();
    }

    fn button(&mut self, data: &mut Helix, handle: &mut PointerInnerHandle<'_, Helix>, event: &ButtonEvent) {
        if self.follow {
            if event.state != smithay::backend::input::ButtonState::Pressed {
                return;
            }
            data.follow = None;
        } else {
            handle.button(data, event);
            if handle.current_pressed().contains(&BTN_LEFT) {
                return;
            }
        }
        handle.unset_grab(self, data, event.serial, event.time, true);
        if let Some(toplevel) = self.window.toplevel() {
            toplevel.with_pending_state(|state| {
                state.states.unset(xdg_toplevel::State::Resizing);
                state.size = Some(self.last_size);
            });
            toplevel.send_pending_configure();
        }
        if let Some(tracking) = data.resize.as_mut() {
            tracking.finished = true;
        }
    }

    forward_passthrough!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_math() {
        let initial: Size<i32, Logical> = (400, 300).into();
        let none: Size<i32, Logical> = (0, 0).into();
        let br = Edges(Edges::BOTTOM | Edges::RIGHT);
        assert_eq!(resized(initial, br, (50.0, 20.0).into(), none, none), (450, 320).into());
        let tl = Edges(Edges::TOP | Edges::LEFT);
        assert_eq!(resized(initial, tl, (50.0, 20.0).into(), none, none), (350, 280).into());
        // Clamped to the client's minimum and maximum.
        let min: Size<i32, Logical> = (380, 0).into();
        let max: Size<i32, Logical> = (0, 310).into();
        assert_eq!(resized(initial, br, (-100.0, 100.0).into(), min, max), (380, 310).into());
        // Never below 1x1.
        assert_eq!(resized(initial, br, (-1000.0, -1000.0).into(), none, none), (1, 1).into());
    }
}
