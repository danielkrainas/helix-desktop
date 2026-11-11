//! The scale each surface should draw at (helix-desktop-spec
//! `org.gnome.Mutter.DisplayConfig.md`, "Supported scales"): the exact
//! scale of its monitor through the fractional-scale protocol, and the next
//! whole scale as the `wl_surface` preferred buffer scale.

use smithay::desktop::layer_map_for_output;
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::compositor::{send_surface_state, SurfaceData};
use smithay::wayland::fractional_scale::with_fractional_scale;

use super::Helix;

/// Tell one surface the scale and transform of `output`.
fn prefer(surface: &WlSurface, states: &SurfaceData, output: &Output) {
    let scale = output.current_scale().fractional_scale();
    with_fractional_scale(states, |fractional| fractional.set_preferred_scale(scale));
    send_surface_state(surface, states, scale.ceil() as i32, output.current_transform());
}

impl Helix {
    /// Bring every surface's preferred scale in line with the monitor it is
    /// (mostly) on. Only changes are sent to clients.
    pub fn update_surface_scales(&mut self) {
        let windows: Vec<_> = self.space.elements().cloned().collect();
        for window in windows {
            if let Some(output) = self.output_of(&window) {
                window.with_surfaces(|surface, states| prefer(surface, states, &output));
            }
        }
        let outputs: Vec<Output> = self.space.outputs().cloned().collect();
        for output in &outputs {
            for layer in layer_map_for_output(output).layers() {
                layer.with_surfaces(|surface, states| prefer(surface, states, output));
            }
            if let Some(lock) = self.lock.surface_for(output) {
                smithay::wayland::compositor::with_surface_tree_downward(
                    lock.wl_surface(),
                    (),
                    |_, _, _| smithay::wayland::compositor::TraversalAction::DoChildren(()),
                    |surface, states, _| prefer(surface, states, output),
                    |_, _, _| true,
                );
            }
        }
    }

    /// A surface just asked for its fractional scale: until it is placed,
    /// the monitor new windows open on (the pointer's).
    pub fn initial_surface_scale(&self) -> f64 {
        self.pointer_output().map_or(1.0, |o| o.current_scale().fractional_scale())
    }
}
