//! The magnifier on screen (helix-desktop-spec
//! `contracts/gsettings/a11y-magnifier.md`): every monitor's elements
//! scaled up and moved so the part around the pointer shows.

use std::collections::HashMap;

use helix_comp::magnifier::{self, Settings};
use smithay::backend::renderer::element::solid::{SolidColorBuffer, SolidColorRenderElement};
use smithay::backend::renderer::element::utils::{Relocate, RelocateRenderElement, RescaleRenderElement};
use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::gles::{GlesError, GlesFrame, GlesRenderer, GlesTexProgram, Uniform, UniformName, UniformType};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::utils::{Buffer, Scale, Transform};
use smithay::output::Output;
use smithay::utils::{Logical, Physical, Point, Rectangle};

use super::udev::OutputElements;
use super::Helix;

smithay::backend::renderer::element::render_elements! {
    pub ScreenElement<=GlesRenderer>;
    Plain = OutputElements,
    Zoomed = RelocateRenderElement<RescaleRenderElement<OutputElements>>,
    Cross = SolidColorRenderElement,
    /// An element drawn through the color effects.
    Effect = Effected,
}

/// An element whose textures are drawn with the color effect shader.
pub struct Effected {
    inner: Box<ScreenElement>,
    program: GlesTexProgram,
    uniforms: Vec<Uniform<'static>>,
}

impl Element for Effected {
    fn id(&self) -> &Id {
        self.inner.id()
    }
    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }
    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }
    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        self.inner.location(scale)
    }
    fn transform(&self) -> Transform {
        self.inner.transform()
    }
    fn damage_since(&self, scale: Scale<f64>, commit: Option<CommitCounter>) -> DamageSet<i32, Physical> {
        self.inner.damage_since(scale, commit)
    }
    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        self.inner.opaque_regions(scale)
    }
    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }
    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl RenderElement<GlesRenderer> for Effected {
    fn draw(&self, frame: &mut GlesFrame<'_, '_>, src: Rectangle<f64, Buffer>, dst: Rectangle<i32, Physical>, damage: &[Rectangle<i32, Physical>], opaque_regions: &[Rectangle<i32, Physical>]) -> Result<(), GlesError> {
        frame.override_default_tex_program(self.program.clone(), self.uniforms.clone());
        let result = RenderElement::<GlesRenderer>::draw(&*self.inner, frame, src, dst, damage, opaque_regions);
        frame.clear_tex_program_override();
        result
    }

    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        // Never scanned out directly: it must go through the shader.
        None
    }
}

/// The magnifier's color effects (helix-desktop-spec `a11y-magnifier.md`).
const EFFECT_SHADER: &str = r#"#version 100
//_DEFINES_
#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif
precision mediump float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif
uniform float alpha;
varying vec2 v_coords;
uniform float invert;
uniform float saturation;
uniform vec3 contrast;
uniform vec3 brightness;
void main() {
    vec4 color = texture2D(tex, v_coords);
#if defined(NO_ALPHA)
    color = vec4(color.rgb, 1.0);
#endif
    vec3 c = color.a > 0.0 ? color.rgb / color.a : color.rgb;
    if (invert > 0.5) {
        float mx = max(c.r, max(c.g, c.b));
        float mn = min(c.r, min(c.g, c.b));
        c = c + (1.0 - mx - mn);
    }
    float l = dot(c, vec3(0.2126, 0.7152, 0.0722));
    c = mix(vec3(l), c, saturation);
    c = clamp((c - 0.5) * (1.0 + contrast) + 0.5 + brightness, 0.0, 1.0);
    gl_FragColor = vec4(c * color.a, color.a) * alpha;
}
"#;

/// The gap around the pointer with `cross-hairs-clip`, in logical pixels
/// before magnification.
const CLIP_GAP: f64 = 16.0;

#[derive(Default)]
pub struct ZoomState {
    pub settings: Settings,
    /// The shown part's top-left corner per monitor (by name).
    views: HashMap<String, (f64, f64)>,
    cross: [SolidColorBuffer; 4],
    /// The effect shader, once compiled.
    program: Option<GlesTexProgram>,
    /// Everything drawn without color (screen time limits).
    pub grayscale: bool,
}

impl Helix {
    pub fn apply_magnifier_settings(&mut self) {
        let settings = magnifier::read();
        if settings != self.zoom.settings {
            tracing::info!(enabled = settings.enabled, factor = settings.factor, tracking = ?settings.tracking, effects = settings.effects(), "magnifier settings");
            if !settings.active() {
                self.zoom.views.clear();
            }
            self.zoom.settings = settings;
            self.redraw_fully();
            self.render_all();
        }
    }
}

/// `elements` (front to back) of `output` at `geo`, magnified when the
/// magnifier is on.
pub fn wrap(zoom: &mut ZoomState, renderer: &mut GlesRenderer, output: &Output, geo: Rectangle<i32, Logical>, pointer: Point<f64, Logical>, elements: Vec<OutputElements>) -> Vec<ScreenElement> {
    let list = magnify(zoom, output, geo, pointer, elements);
    if !zoom.settings.effects() && !zoom.grayscale {
        return list;
    }
    match effect_program(zoom, renderer) {
        Ok(program) => effect(zoom, program, list),
        Err(err) => {
            tracing::warn!(err, "cannot apply the magnifier's color effects");
            list
        }
    }
}

/// The effect shader (compiled the first time).
fn effect_program(zoom: &mut ZoomState, renderer: &mut GlesRenderer) -> Result<GlesTexProgram, String> {
    if zoom.program.is_none() {
        let uniforms = [
            UniformName::new("invert", UniformType::_1f),
            UniformName::new("saturation", UniformType::_1f),
            UniformName::new("contrast", UniformType::_3f),
            UniformName::new("brightness", UniformType::_3f),
        ];
        zoom.program = Some(renderer.compile_custom_texture_shader(EFFECT_SHADER, &uniforms).map_err(|e| format!("{e:?}"))?);
    }
    zoom.program.clone().ok_or_else(|| "no shader".into())
}

/// Draw every element of `list` through the effect shader.
fn effect(zoom: &ZoomState, program: GlesTexProgram, list: Vec<ScreenElement>) -> Vec<ScreenElement> {
    let s = &zoom.settings;
    // The magnifier's effects only while it is on; grayscale on top.
    let (invert, saturation, contrast, brightness) = if s.effects() { (s.invert, s.saturation, s.contrast, s.brightness) } else { (false, 1.0, [0.0; 3], [0.0; 3]) };
    let uniforms = vec![
        Uniform::new("invert", if invert { 1.0f32 } else { 0.0 }),
        Uniform::new("saturation", if zoom.grayscale { 0.0 } else { saturation }),
        Uniform::new("contrast", contrast),
        Uniform::new("brightness", brightness),
    ];
    list.into_iter()
        .map(|e| ScreenElement::Effect(Effected { inner: Box::new(e), program: program.clone(), uniforms: uniforms.clone() }))
        .collect()
}

/// The elements, magnified when the magnifier is.
fn magnify(zoom: &mut ZoomState, output: &Output, geo: Rectangle<i32, Logical>, pointer: Point<f64, Logical>, elements: Vec<OutputElements>) -> Vec<ScreenElement> {
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
