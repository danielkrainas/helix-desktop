//! Extensions' popovers (helix-desktop-spec `contracts/extensions/README.md`,
//! "Popovers"): layer surfaces with the namespace
//! `helix-extension-popover`. One at a time, at most 480 × 640, closed by
//! a click outside, and none while the session is locked.

use smithay::desktop::layer_map_for_output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point};

use super::Helix;

pub const NAMESPACE: &str = "helix-extension-popover";
pub const MAX: (i32, i32) = (480, 640);

/// Whether a popover asking for `size` is too big.
pub fn too_big(size: (i32, i32)) -> bool {
    size.0 > MAX.0 || size.1 > MAX.1
}

impl Helix {
    /// Close the popovers, but `keep`.
    pub fn close_popovers(&mut self, keep: Option<&WlSurface>) {
        for output in self.space.outputs().cloned().collect::<Vec<_>>() {
            let map = layer_map_for_output(&output);
            for layer in map.layers() {
                if layer.namespace() == NAMESPACE && Some(layer.wl_surface()) != keep {
                    tracing::info!("closing an extension popover");
                    layer.layer_surface().send_close();
                }
            }
        }
    }

    /// A new layer surface: a popover replaces the one shown (and none
    /// shows while locked). False if it must not be mapped.
    pub fn popover_opened(&mut self, namespace: &str, surface: &WlSurface) -> bool {
        if namespace != NAMESPACE {
            return true;
        }
        if self.lock.locked {
            return false;
        }
        self.close_popovers(Some(surface));
        true
    }

    /// A popover's commit: too big ones are closed.
    pub fn popover_commit(&mut self, surface: &WlSurface) {
        for output in self.space.outputs().cloned().collect::<Vec<_>>() {
            let map = layer_map_for_output(&output);
            let Some(layer) = map.layer_for_surface(surface, smithay::desktop::WindowSurfaceType::TOPLEVEL) else { continue };
            if layer.namespace() != NAMESPACE {
                return;
            }
            let size = layer.cached_state().size;
            if too_big((size.w, size.h)) {
                tracing::warn!(w = size.w, h = size.h, "extension popover too big; closed");
                layer.layer_surface().send_close();
            }
            return;
        }
    }

    /// A button press at `pos`: popovers it is not in close.
    pub fn popover_press(&mut self, pos: Point<f64, Logical>) {
        let mut inside: Option<WlSurface> = None;
        let mut any = false;
        for output in self.space.outputs().cloned().collect::<Vec<_>>() {
            let origin = self.space.output_geometry(&output).map(|g| g.loc).unwrap_or_default();
            let map = layer_map_for_output(&output);
            for layer in map.layers().filter(|l| l.namespace() == NAMESPACE) {
                any = true;
                if let Some(geo) = map.layer_geometry(layer) {
                    let local = pos - origin.to_f64();
                    if geo.to_f64().contains(local) {
                        inside = Some(layer.wl_surface().clone());
                    }
                }
            }
        }
        if any {
            self.close_popovers(inside.as_ref());
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sizes() {
        assert!(!super::too_big((480, 640)));
        assert!(super::too_big((481, 10)) && super::too_big((10, 641)));
    }
}
