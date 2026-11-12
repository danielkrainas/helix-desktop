//! Color-managed windows (helix-desktop-spec `contracts/wayland/README.md`,
//! "Color management"): a window with a surface whose image description
//! needs converting is drawn from its surfaces' textures, those surfaces
//! through the conversion shader.

use std::cell::RefCell;

use helix_comp::colorimetry::{self, Conversion};
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::element::{Id, Kind};
use smithay::backend::renderer::gles::element::TextureShaderElement;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexProgram, GlesTexture, Uniform, UniformName, UniformType, UniformValue};
use smithay::backend::renderer::utils::RendererSurfaceStateUserData;
use smithay::backend::renderer::{ContextId, Renderer};
use smithay::desktop::Window;
use smithay::wayland::seat::WaylandFocus;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Physical, Point};
use smithay::wayland::compositor::{with_surface_tree_downward, SubsurfaceCachedState, TraversalAction};

use super::frames::SceneElement;

thread_local! {
    /// The conversion shader, for the renderer it was made for.
    static PROGRAM: RefCell<Option<(ContextId<GlesTexture>, GlesTexProgram)>> = const { RefCell::new(None) };
}

fn program(renderer: &mut GlesRenderer) -> Option<GlesTexProgram> {
    let context = renderer.context_id();
    PROGRAM.with(|p| {
        let mut p = p.borrow_mut();
        if let Some((c, program)) = p.as_ref() {
            if *c == context {
                return Some(program.clone());
            }
        }
        let uniforms = [
            UniformName::new("tf_kind", UniformType::_1i),
            UniformName::new("tf_power", UniformType::_1f),
            UniformName::new("scale", UniformType::_1f),
            UniformName::new("to_bt709", UniformType::Matrix3x3),
        ];
        match renderer.compile_custom_texture_shader(colorimetry::SHADER, &uniforms) {
            Ok(program) => {
                *p = Some((context, program.clone()));
                Some(program)
            }
            Err(err) => {
                tracing::warn!(?err, "cannot compile the color conversion shader; described surfaces are drawn as they are");
                None
            }
        }
    })
}

fn uniforms(c: &Conversion) -> Vec<Uniform<'static>> {
    vec![
        Uniform::new("tf_kind", UniformValue::_1i(c.tf_kind)),
        Uniform::new("tf_power", UniformValue::_1f(c.power)),
        Uniform::new("scale", UniformValue::_1f(c.scale)),
        Uniform::new("to_bt709", UniformValue::Matrix3x3 { matrices: vec![c.matrix], transpose: true }),
    ]
}

/// The conversion a surface needs (from its state, already locked).
fn conversion_of(states: &smithay::wayland::compositor::SurfaceData) -> Option<Conversion> {
    helix_comp::color_management::colorimetry_of(states).and_then(|c| colorimetry::conversion(&c))
}

/// Whether any surface of `root`'s tree needs converting.
fn needs(root: &WlSurface) -> bool {
    let found = std::cell::Cell::new(false);
    with_surface_tree_downward(
        root,
        (),
        |_, _, _| if found.get() { TraversalAction::Break } else { TraversalAction::DoChildren(()) },
        |_, states, _| {
            if conversion_of(states).is_some() {
                found.set(true);
            }
        },
        |_, _, _| !found.get(),
    );
    found.get()
}

/// The elements of `window` (front to back) when it has a surface to
/// convert; `None` otherwise (it is drawn as usual). `at` is where its
/// root surface's origin goes on the output (physical).
pub fn window_elements(renderer: &mut GlesRenderer, window: &Window, at: Point<i32, Physical>, scale: f64, alpha: f32) -> Option<Vec<SceneElement>> {
    let root = window.wl_surface()?.into_owned();
    if !needs(&root) {
        return None;
    }
    let program = program(renderer)?;
    // The surfaces' buffers as textures of this renderer (the usual
    // drawing does this on the way).
    if let Err(err) = smithay::backend::renderer::utils::import_surface_tree(renderer, &root) {
        tracing::debug!(?err, "cannot import a color-managed window's buffers");
    }
    let context = renderer.context_id();
    let origin = at.to_f64();
    let mut out: Vec<SceneElement> = Vec::new();
    let place = |states: &smithay::wayland::compositor::SurfaceData, at: &Point<f64, Logical>| {
        let mut at = *at;
        if states.role == Some("subsurface") {
            at += states.cached_state.get::<SubsurfaceCachedState>().current().location.to_f64();
        }
        at
    };
    with_surface_tree_downward(
        &root,
        Point::<f64, Logical>::default(),
        |_, states, at| TraversalAction::DoChildren(place(states, at)),
        |_, states, at| {
            let at = place(states, at);
            let Some(data) = states.data_map.get::<RendererSurfaceStateUserData>() else { return };
            let data = data.lock().unwrap();
            let (Some(view), Some(texture)) = (data.view(), data.texture::<GlesTexture>(context.clone())) else { return };
            let location: Point<f64, Physical> = origin + (at + view.offset.to_f64()).to_physical(scale);
            let element = TextureRenderElement::from_static_texture(
                Id::new(),
                context.clone(),
                location,
                texture.clone(),
                data.buffer_scale(),
                data.buffer_transform(),
                Some(alpha),
                Some(view.src),
                Some(view.dst),
                None,
                Kind::Unspecified,
            );
            out.push(match conversion_of(states) {
                Some(c) => SceneElement::Converted(TextureShaderElement::new(element, program.clone(), uniforms(&c))),
                None => SceneElement::Ghost(element),
            });
        },
        |_, _, _| true,
    );
    // Children were added after their parents; the front comes first.
    out.reverse();
    Some(out)
}
