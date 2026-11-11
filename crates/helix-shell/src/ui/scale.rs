//! Drawing shell surfaces at their monitor's scale (helix-desktop-spec
//! `org.gnome.Mutter.DisplayConfig.md`, "Supported scales"): the compositor
//! tells each surface its exact scale (`wp_fractional_scale_v1`); buffers
//! are drawn at that many pixels per logical unit and shown at their
//! logical size through a viewport (`wp_viewporter`).

use smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface;
use smithay_client_toolkit::reexports::client::{Connection, Dispatch, Proxy, QueueHandle};
use smithay_client_toolkit::reexports::protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use smithay_client_toolkit::reexports::protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};

use super::Ui;

/// Which surface a fractional-scale object belongs to.
pub(super) struct ScaleOf(WlSurface);

struct Tracked {
    surface: WlSurface,
    fractional: Option<WpFractionalScaleV1>,
    viewport: WpViewport,
    scale: f64,
}

impl Drop for Tracked {
    fn drop(&mut self) {
        if let Some(fractional) = self.fractional.take() {
            fractional.destroy();
        }
        self.viewport.destroy();
    }
}

/// The scale of each shell surface.
#[derive(Default)]
pub(super) struct Scales {
    fractional: Option<WpFractionalScaleManagerV1>,
    viewporter: Option<WpViewporter>,
    surfaces: Vec<Tracked>,
}

impl Scales {
    pub fn new(fractional: Option<WpFractionalScaleManagerV1>, viewporter: Option<WpViewporter>) -> Self {
        Self { fractional, viewporter, surfaces: Vec::new() }
    }

    /// The scale to draw `surface` at (1 until the compositor says).
    pub fn of(&self, surface: &WlSurface) -> f64 {
        self.surfaces.iter().find(|t| &t.surface == surface).map_or(1.0, |t| t.scale)
    }

    /// Before attaching a buffer drawn at [`Self::of`]: show it at `w`×`h`
    /// logical units and damage all of it. Starts following the surface's
    /// scale the first time.
    pub fn prepare(&mut self, qh: &QueueHandle<Ui>, surface: &WlSurface, w: i32, h: i32) {
        self.surfaces.retain(|t| t.surface.is_alive());
        if !self.surfaces.iter().any(|t| &t.surface == surface) {
            if let Some(viewporter) = &self.viewporter {
                let viewport = viewporter.get_viewport(surface, qh, ());
                let fractional = self.fractional.as_ref().map(|m| m.get_fractional_scale(surface, qh, ScaleOf(surface.clone())));
                self.surfaces.push(Tracked { surface: surface.clone(), fractional, viewport, scale: 1.0 });
            }
        }
        if let Some(tracked) = self.surfaces.iter().find(|t| &t.surface == surface) {
            tracked.viewport.set_destination(w, h);
        }
        surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
    }

    /// The compositor's preferred scale for a surface; true if it changed.
    fn set(&mut self, surface: &WlSurface, scale: f64) -> bool {
        match self.surfaces.iter_mut().find(|t| &t.surface == surface) {
            Some(tracked) if tracked.scale != scale => {
                tracked.scale = scale;
                true
            }
            _ => false,
        }
    }
}

impl Dispatch<WpFractionalScaleV1, ScaleOf> for Ui {
    fn event(state: &mut Self, _: &WpFractionalScaleV1, event: wp_fractional_scale_v1::Event, data: &ScaleOf, _: &Connection, _: &QueueHandle<Self>) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            // In 120ths.
            let scale = f64::from(scale.max(1)) / 120.0;
            if state.scales.set(&data.0, scale) {
                tracing::debug!(scale, "shell surface scale");
                state.redraw_all();
            }
        }
    }
}

impl Dispatch<WpFractionalScaleManagerV1, ()> for Ui {
    fn event(_: &mut Self, _: &WpFractionalScaleManagerV1, _: <WpFractionalScaleManagerV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WpViewporter, ()> for Ui {
    fn event(_: &mut Self, _: &WpViewporter, _: <WpViewporter as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WpViewport, ()> for Ui {
    fn event(_: &mut Self, _: &WpViewport, _: <WpViewport as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
