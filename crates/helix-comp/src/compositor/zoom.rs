//! The magnifier on screen (helix-desktop-spec
//! `contracts/gsettings/a11y-magnifier.md`): every monitor's elements
//! scaled up and moved so the part around the pointer shows.

use std::collections::HashMap;
use std::rc::Rc;

use helix_comp::magnifier::{self, Settings, Tracking};
use smithay::backend::renderer::element::solid::{SolidColorBuffer, SolidColorRenderElement};
use smithay::backend::renderer::element::utils::{CropRenderElement, Relocate, RelocateRenderElement, RescaleRenderElement};
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
    /// An element shown twice (as it is, and in a lens or half view).
    Twice = Shared,
    Lensed = CropRenderElement<RelocateRenderElement<RescaleRenderElement<Shared>>>,
    /// The whole frame converted for an HDR monitor.
    Hdr = smithay::backend::renderer::gles::element::TextureShaderElement,
}

/// One element drawn twice: the copies share it, each with its own id (so
/// damage is followed for each).
pub struct Shared {
    inner: Rc<OutputElements>,
    id: Id,
}

impl Element for Shared {
    fn id(&self) -> &Id {
        &self.id
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

impl RenderElement<GlesRenderer> for Shared {
    fn draw(&self, frame: &mut GlesFrame<'_, '_>, src: Rectangle<f64, Buffer>, dst: Rectangle<i32, Physical>, damage: &[Rectangle<i32, Physical>], opaque_regions: &[Rectangle<i32, Physical>]) -> Result<(), GlesError> {
        RenderElement::<GlesRenderer>::draw(&*self.inner, frame, src, dst, damage, opaque_regions)
    }

    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
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
    /// A point followed instead of the pointer (focus or caret, layout
    /// coordinates) with its tracking, until the pointer moves.
    target: Option<(Point<f64, Logical>, Tracking)>,
    last_pointer: Option<Point<f64, Logical>>,
    /// Ids of the magnified copies, per monitor (by the element's id).
    aliases: HashMap<String, HashMap<Id, Id>>,
    /// A partial view's background and frame.
    frame: [SolidColorBuffer; 5],
    /// Whether focus and caret are being followed (told to the D-Bus side).
    following: bool,
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
            self.zoom.target = None;
            self.zoom.aliases.clear();
            self.redraw_fully();
            self.render_all();
        }
        let follow = self.zoom.settings.follows_focus();
        if follow != self.zoom.following {
            self.zoom.following = follow;
            self.send(helix_comp::Event::FocusTracking(follow));
        }
    }

    /// The focused object or the caret moved (relative to the focused
    /// window): the magnifier follows it, as its tracking says.
    pub fn track_point(&mut self, point: helix_comp::focus_tracking::TrackPoint) {
        use helix_comp::focus_tracking::Kind;
        let s = &self.zoom.settings;
        let tracking = match point.kind {
            Kind::Focus => s.focus_tracking,
            Kind::Caret | Kind::CaretArea => s.caret_tracking,
        };
        if !s.active() || tracking == Tracking::None {
            return;
        }
        let Some(window) = self.focused_window() else { return };
        let Some(at) = self.space.element_location(&window) else { return };
        // The window's own coordinates start at its surface, before any
        // shadow it draws.
        let origin = at - window.geometry().loc;
        let (x, y) = point.point();
        tracing::debug!(?point, origin = ?origin, "magnifier follows focus or caret");
        self.zoom.target = Some(((f64::from(origin.x) + x, f64::from(origin.y) + y).into(), tracking));
        self.render_all();
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
    // The pointer moved: it leads again (over focus and caret).
    if zoom.last_pointer != Some(pointer) {
        if zoom.last_pointer.is_some() {
            zoom.target = None;
        }
        zoom.last_pointer = Some(pointer);
    }
    let s = zoom.settings.clone();
    let scale = output.current_scale().fractional_scale();
    let m = s.factor;
    let size = (f64::from(geo.size.w), f64::from(geo.size.h));
    let area = geo.to_f64();
    let local = |p: Point<f64, Logical>| (p.x - area.loc.x, p.y - area.loc.y);
    let on_here = area.contains(pointer);
    let relative = local(pointer);
    let name = output.name();
    let viewport = magnifier::viewport(&s, size, on_here.then_some(relative));
    let shown = (viewport.2 / m, viewport.3 / m);
    let previous = zoom.views.get(&name).copied();
    let view = if s.lens {
        // A lens shows what is around the pointer.
        magnifier::view_of(&s, Tracking::Centered, size, shown, Some(previous.unwrap_or_default()), on_here.then_some(relative))
    } else {
        match zoom.target {
            Some((p, tracking)) if area.contains(p) => magnifier::view_of(&s, tracking, size, shown, previous, Some(local(p))),
            // Followed elsewhere: this monitor's view stays.
            Some(_) => magnifier::view_of(&s, s.tracking, size, shown, previous, None),
            None => magnifier::view_of(&s, s.tracking, size, shown, previous, on_here.then_some(relative)),
        }
    };
    zoom.views.insert(name.clone(), view);
    let px = |v: f64| (v * scale).round() as i32;
    let offset: Point<i32, Physical> = (px(viewport.0 - view.0 * m), px(viewport.1 - view.1 * m)).into();
    let clip = Rectangle::<i32, Physical>::new((px(viewport.0), px(viewport.1)).into(), (px(viewport.2), px(viewport.3)).into());
    let mut out = Vec::with_capacity(elements.len() * 2 + 9);
    if s.cross_hairs && on_here {
        let at = (viewport.0 * scale + (relative.0 - view.0) * m * scale, viewport.1 * scale + (relative.1 - view.1) * m * scale);
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
            // Within the magnified view.
            let r = Rectangle::<i32, Physical>::new((x.round() as i32, y.round() as i32).into(), (w.round() as i32, h.round() as i32).into());
            let Some(r) = r.intersection(clip) else { continue };
            if r.size.w < 1 || r.size.h < 1 {
                continue;
            }
            buffer.update((r.size.w, r.size.h), s.cross_color);
            out.push(ScreenElement::Cross(SolidColorRenderElement::from_buffer(buffer, r.loc, 1.0, 1.0, Kind::Unspecified)));
        }
    }
    if !s.partial() {
        out.extend(elements.into_iter().map(|e| {
            ScreenElement::Zoomed(RelocateRenderElement::from_element(RescaleRenderElement::from_element(e, (0, 0).into(), m), offset, Relocate::Relative))
        }));
        return out;
    }
    // Part of the monitor: a framed view over the unmagnified screen.
    let (x, y, w, h) = (clip.loc.x, clip.loc.y, clip.size.w, clip.size.h);
    let frame_color = [0.55, 0.55, 0.6, 1.0];
    let frames = [(x, y, w, 2), (x, y + h - 2, w, 2), (x, y, 2, h), (x + w - 2, y, 2, h)];
    for (buffer, (fx, fy, fw, fh)) in zoom.frame.iter_mut().zip(frames) {
        buffer.update((fw.max(1), fh.max(1)), frame_color);
        out.push(ScreenElement::Cross(SolidColorRenderElement::from_buffer(buffer, (fx, fy), 1.0, 1.0, Kind::Unspecified)));
    }
    let shared: Vec<Rc<OutputElements>> = elements.into_iter().map(Rc::new).collect();
    let old = zoom.aliases.remove(&name).unwrap_or_default();
    let mut aliases = HashMap::with_capacity(shared.len());
    for e in &shared {
        let id = e.id().clone();
        let alias = old.get(&id).cloned().unwrap_or_else(Id::new);
        aliases.insert(id, alias.clone());
        let copy = Shared { inner: e.clone(), id: alias };
        let moved = RelocateRenderElement::from_element(RescaleRenderElement::from_element(copy, (0, 0).into(), m), offset, Relocate::Relative);
        if let Some(cropped) = CropRenderElement::from_element(moved, scale, clip) {
            out.push(ScreenElement::Lensed(cropped));
        }
    }
    zoom.aliases.insert(name, aliases);
    // Behind the magnified view, so the screen does not show through.
    let back = &mut zoom.frame[4];
    back.update((w.max(1), h.max(1)), [0.0, 0.0, 0.0, 1.0]);
    out.push(ScreenElement::Cross(SolidColorRenderElement::from_buffer(back, (x, y), 1.0, 1.0, Kind::Unspecified)));
    out.extend(shared.into_iter().map(|e| {
        let id = e.id().clone();
        ScreenElement::Twice(Shared { inner: e, id })
    }));
    out
}
