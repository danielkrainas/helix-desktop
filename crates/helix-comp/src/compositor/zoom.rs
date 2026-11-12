//! The magnifier on screen (helix-desktop-spec
//! `contracts/gsettings/a11y-magnifier.md`): every monitor's elements
//! scaled up and moved so the part around the pointer shows.

use std::collections::HashMap;

use helix_comp::magnifier::{self, Settings};
use smithay::backend::renderer::element::solid::{SolidColorBuffer, SolidColorRenderElement};
use smithay::backend::renderer::element::utils::{Relocate, RelocateRenderElement, RescaleRenderElement};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::output::Output;
use smithay::utils::{Logical, Physical, Point, Rectangle};

use super::udev::OutputElements;
use super::Helix;

smithay::backend::renderer::element::render_elements! {
    pub ScreenElement<=GlesRenderer>;
    Plain = OutputElements,
    Zoomed = RelocateRenderElement<RescaleRenderElement<OutputElements>>,
    Cross = SolidColorRenderElement,
}

/// The gap around the pointer with `cross-hairs-clip`, in logical pixels
/// before magnification.
const CLIP_GAP: f64 = 16.0;

#[derive(Default)]
pub struct ZoomState {
    pub settings: Settings,
    /// The shown part's top-left corner per monitor (by name).
    views: HashMap<String, (f64, f64)>,
    cross: [SolidColorBuffer; 4],
}

impl Helix {
    pub fn apply_magnifier_settings(&mut self) {
        let settings = magnifier::read();
        if settings != self.zoom.settings {
            tracing::info!(enabled = settings.enabled, factor = settings.factor, tracking = ?settings.tracking, "magnifier settings");
            if !settings.active() {
                self.zoom.views.clear();
            }
            self.zoom.settings = settings;
            self.render_all();
        }
    }
}

/// `elements` (front to back) of `output` at `geo`, magnified when the
/// magnifier is on.
pub fn wrap(zoom: &mut ZoomState, output: &Output, geo: Rectangle<i32, Logical>, pointer: Point<f64, Logical>, elements: Vec<OutputElements>) -> Vec<ScreenElement> {
    if !zoom.settings.active() {
        return elements.into_iter().map(ScreenElement::Plain).collect();
    }
    let s = &zoom.settings;
    let scale = output.current_scale().fractional_scale();
    let m = s.factor;
    let size = (f64::from(geo.size.w), f64::from(geo.size.h));
    let on_here = geo.to_f64().contains(pointer);
    let relative = (pointer.x - f64::from(geo.loc.x), pointer.y - f64::from(geo.loc.y));
    let name = output.name();
    let view = magnifier::view(s, size, zoom.views.get(&name).copied(), on_here.then_some(relative));
    zoom.views.insert(name, view);
    let offset: Point<i32, Physical> = (-(view.0 * scale * m).round() as i32, -(view.1 * scale * m).round() as i32).into();
    let mut out = Vec::with_capacity(elements.len() + 4);
    if s.cross_hairs && on_here {
        let at = ((relative.0 - view.0) * m * scale, (relative.1 - view.1) * m * scale);
        let (len, thick) = (f64::from(s.cross_length), f64::from(s.cross_thickness));
        let gap = if s.cross_clip { CLIP_GAP * m * scale } else { 0.0 };
        // Horizontal then vertical, each in two halves (meeting without a gap).
        let rects = [
            (at.0 - len / 2.0, at.1 - thick / 2.0, len / 2.0 - gap, thick),
            (at.0 + gap, at.1 - thick / 2.0, len / 2.0 - gap, thick),
            (at.0 - thick / 2.0, at.1 - len / 2.0, thick, len / 2.0 - gap),
            (at.0 - thick / 2.0, at.1 + gap, thick, len / 2.0 - gap),
        ];
        for (buffer, (x, y, w, h)) in zoom.cross.iter_mut().zip(rects) {
            if w < 1.0 || h < 1.0 {
                continue;
            }
            buffer.update((w.round() as i32, h.round() as i32), s.cross_color);
            out.push(ScreenElement::Cross(SolidColorRenderElement::from_buffer(buffer, (x.round() as i32, y.round() as i32), 1.0, 1.0, Kind::Unspecified)));
        }
    }
    out.extend(elements.into_iter().map(|e| {
        ScreenElement::Zoomed(RelocateRenderElement::from_element(RescaleRenderElement::from_element(e, (0, 0).into(), m), offset, Relocate::Relative))
    }));
    out
}
