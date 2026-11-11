//! Edge tiling (helix-desktop-spec `contracts/wayland/README.md`,
//! "Tiling"): while a window is dragged with `edge-tiling` on, touching a
//! monitor's left, right or top edge previews the half (or the whole work
//! area) it would fill; releasing there tiles (or maximizes) it.

use smithay::desktop::Window;
use smithay::utils::{Logical, Point, Rectangle};

use super::wm::{half, Takeover};
use super::Helix;

/// How close (logical pixels) the pointer must be to an edge.
const EDGE: f64 = 2.0;
/// Accent blue, mostly transparent (premultiplied).
const PREVIEW: [f32; 4] = [0.21 * 0.3, 0.52 * 0.3, 0.89 * 0.3, 0.3];

impl Helix {
    /// The takeover (and the area it would fill) for a drag at `pos`.
    fn edge_target(&self, pos: Point<f64, Logical>) -> Option<(Takeover, Rectangle<i32, Logical>)> {
        let output = self.space.output_under(pos).next()?.clone();
        let geo = self.space.output_geometry(&output)?;
        let area = self.usable_area_of(&output)?;
        let beyond = |p: Point<f64, Logical>| self.space.output_under(p).next().is_some();
        let (x0, x1) = (f64::from(geo.loc.x), f64::from(geo.loc.x + geo.size.w));
        let y0 = f64::from(geo.loc.y);
        if pos.x < x0 + EDGE && !beyond((x0 - 3.0, pos.y).into()) {
            Some((Takeover::TiledLeft, half(area, Takeover::TiledLeft)))
        } else if pos.x >= x1 - EDGE && !beyond((x1 + 3.0, pos.y).into()) {
            Some((Takeover::TiledRight, half(area, Takeover::TiledRight)))
        } else if pos.y < y0 + EDGE && !beyond((pos.x, y0 - 3.0).into()) {
            Some((Takeover::Maximized, area))
        } else {
            None
        }
    }

    /// The pointer dragging a window moved to `pos`.
    pub fn update_tile_preview(&mut self, pos: Point<f64, Logical>) {
        let target = if self.wm_settings.edge_tiling { self.edge_target(pos) } else { None };
        self.tile_target = target.map(|(kind, _)| kind);
        self.frames.set_preview(target.map(|(_, area)| area), PREVIEW);
    }

    /// The drag of `window` ended: tile or maximize it if a preview was up.
    pub fn finish_tile_preview(&mut self, window: &Window) {
        let target = self.tile_target.take();
        self.frames.set_preview(None, PREVIEW);
        match target {
            Some(Takeover::Maximized) => self.maximize(window),
            Some(side) => self.toggle_tiled(window, side),
            None => {}
        }
    }
}
