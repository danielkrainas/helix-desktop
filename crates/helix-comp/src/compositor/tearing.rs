//! Tearing for full-screen games (`wp_tearing_control_v1`; helix-desktop-spec
//! `contracts/wayland/README.md`, "Tearing"): the facts about a monitor's
//! next frame that `helix_comp::tearing_control::may_tear` decides on.
//! Only the DRM backend acts on them; the headless and nested backends
//! keep the hint and always wait for vblank (or have none).

use helix_comp::tearing_control::{current_hint, Frame, Hint, TearingCachedState, TearingControl};
use smithay::desktop::{layer_map_for_output, Window};
use smithay::input::pointer::CursorImageStatus;
use smithay::output::Output;
use smithay::reexports::wayland_protocols::wp::tearing_control::v1::server::wp_tearing_control_manager_v1::WpTearingControlManagerV1;
use smithay::reexports::wayland_protocols::wp::tearing_control::v1::server::wp_tearing_control_v1::WpTearingControlV1;
use smithay::reexports::wayland_server::{delegate_dispatch, delegate_global_dispatch};
use smithay::wayland::compositor::{with_surface_tree_downward, TraversalAction};
use smithay::wayland::shell::wlr_layer::Layer;

use super::Helix;

delegate_global_dispatch!(Helix: [WpTearingControlManagerV1: ()] => TearingControl);
delegate_dispatch!(Helix: [WpTearingControlManagerV1: ()] => TearingControl);
delegate_dispatch!(Helix: [WpTearingControlV1: helix_comp::tearing_control::ControlData] => TearingControl);

/// Whether any surface of `window` (its subsurfaces too) asks for `async`.
fn window_wants_async(window: &Window) -> bool {
    let Some(toplevel) = window.toplevel() else {
        // X11 windows: Xwayland sets the hint on its own surface.
        return window.x11_surface().and_then(|x| x.wl_surface()).is_some_and(|s| current_hint(&s) == Hint::Async);
    };
    // (From the `states` given: `with_states` again in here deadlocks.)
    let wants = std::cell::Cell::new(false);
    with_surface_tree_downward(
        toplevel.wl_surface(),
        (),
        |_, _, _| TraversalAction::DoChildren(()),
        |_, states, _| {
            if states.cached_state.get::<TearingCachedState>().current().hint == Hint::Async {
                wants.set(true);
            }
        },
        |_, _, _| !wants.get(),
    );
    wants.get()
}

impl Helix {
    /// The facts for `output`'s next frame (all but the GPU's support).
    pub(super) fn tearing_frame(&self, output: &Output) -> Frame {
        let focused = self.focused_window();
        // The topmost window on the monitor: full screen and focused.
        let top = self.space.elements().rev().find(|w| self.output_of(w).as_ref() == Some(output)).cloned();
        let fullscreen_focused = self.in_fullscreen(output) && top.is_some() && top == focused && top.as_ref().is_some_and(|w| self.is_taken_over(w, super::wm::Takeover::Fullscreen));
        let wants_async = fullscreen_focused && top.as_ref().is_some_and(window_wants_async);
        let map = layer_map_for_output(output);
        // Shown top-layer surfaces (most are hidden in full screen) and
        // any overlay-layer surface are drawn over the game.
        let over = map.layers().any(|l| l.layer() == Layer::Overlay || (l.layer() == Layer::Top && !super::wm::hidden_in_fullscreen(Layer::Top, l.namespace())));
        let pointer = self.seat.get_pointer().map(|p| p.current_location());
        let on_output = pointer.is_some_and(|p| self.space.output_geometry(output).is_some_and(|g| g.to_f64().contains(p)));
        Frame {
            allowed: self.wm_settings.allow_tearing,
            supported: false,
            fullscreen_focused,
            wants_async,
            covered: over || self.lock.locked || self.zoom.settings.active(),
            cursor_shown: on_output && !matches!(self.cursor_status, CursorImageStatus::Hidden),
            animating: self.animations.active(),
        }
    }
}
