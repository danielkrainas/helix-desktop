//! Frame timing for clients: `wp_presentation` feedback, `wp_fifo_v1`
//! barriers and `wp_commit_timer_v1` deadlines.

use std::time::Duration;

use smithay::desktop::{layer_map_for_output, Space, Window};
use smithay::desktop::utils::OutputPresentationFeedback;
use smithay::output::Output;
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{Monotonic, Time};
use smithay::wayland::commit_timing::CommitTimerBarrierStateUserData;
use smithay::wayland::compositor::{with_surface_tree_downward, TraversalAction};
use smithay::wayland::fifo::FifoBarrierCachedState;

use super::{ClientState, Helix};

/// How long a frame of `output` lasts.
pub fn refresh_of(output: &Output) -> Duration {
    let mhz = output.current_mode().map(|m| m.refresh).unwrap_or(60_000).max(1);
    Duration::from_micros(1_000_000_000 / mhz as u64)
}

impl Helix {
    /// The root surfaces shown on `output` this frame: its windows (those
    /// it is the first monitor of) and its layer surfaces.
    fn surfaces_on(&self, output: &Output) -> Vec<WlSurface> {
        let mut out: Vec<WlSurface> = self
            .space
            .elements()
            .filter(|w| self.space.outputs_for_element(w).first() == Some(output))
            .filter_map(super::wm::surface_of)
            .collect();
        out.extend(layer_map_for_output(output).layers().map(|l| l.wl_surface().clone()));
        if let Some(lock) = self.lock.surface_for(output) {
            out.push(lock.wl_surface().clone());
        }
        out
    }

}

/// Collect the presentation feedback of what is drawn on `output` now.
pub fn presentation_feedback(space: &Space<Window>, output: &Output) -> OutputPresentationFeedback {
        let mut feedback = OutputPresentationFeedback::new(output);
        for window in space.elements() {
            if space.outputs_for_element(window).first() != Some(output) {
                continue;
            }
            window.take_presentation_feedback(
                &mut feedback,
                |_, _| Some(output.clone()),
                |_, _| wp_presentation_feedback::Kind::Vsync,
            );
        }
        for layer in layer_map_for_output(output).layers() {
            layer.take_presentation_feedback(
                &mut feedback,
                |_, _| Some(output.clone()),
                |_, _| wp_presentation_feedback::Kind::Vsync,
            );
        }
        feedback
}

impl Helix {
    /// A frame of `output` was shown: release the commits that waited for
    /// it (fifo barriers, commit timers up to the next frame).
    pub fn frame_shown(&mut self, output: &Output) {
        let next = Time::<Monotonic>::from(Duration::from(self.clock.now()) + refresh_of(output));
        let roots = self.surfaces_on(output);
        self.release_barriers(&roots, Some(next));
    }

    /// Release the barriers of windows nobody sees (minimized, on another
    /// workspace), so they are not stuck.
    pub fn release_hidden_barriers(&mut self) {
        let roots: Vec<WlSurface> = self
            .minimized
            .iter()
            .chain(self.workspaces.parked())
            .filter_map(super::wm::surface_of)
            .collect();
        let now = self.clock.now();
        self.release_barriers(&roots, Some(now));
    }

    fn release_barriers(&mut self, roots: &[WlSurface], until: Option<Time<Monotonic>>) {
        let mut cleared: Vec<smithay::reexports::wayland_server::Client> = Vec::new();
        for root in roots {
            with_surface_tree_downward(
                root,
                (),
                |_, _, _| TraversalAction::DoChildren(()),
                |surface, states, _| {
                    let mut any = false;
                    if let Some(barrier) = states.cached_state.get::<FifoBarrierCachedState>().current().barrier.take() {
                        barrier.signal();
                        any = true;
                    }
                    if let (Some(until), Some(timers)) = (until, states.data_map.get::<CommitTimerBarrierStateUserData>()) {
                        if timers.lock().unwrap().signal_until(until) {
                            any = true;
                        }
                    }
                    if any {
                        if let Some(client) = surface.client() {
                            if !cleared.contains(&client) {
                                cleared.push(client);
                            }
                        }
                    }
                },
                |_, _, _| true,
            );
        }
        let dh = self.display_handle.clone();
        for client in cleared {
            if let Some(data) = client.get_data::<ClientState>() {
                data.compositor_state.blocker_cleared(self, &dh);
            } else if let Some(data) = client.get_data::<smithay::xwayland::XWaylandClientData>() {
                data.compositor_state.blocker_cleared(self, &dh);
            }
        }
    }
}
