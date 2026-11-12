//! Animations (helix-desktop-spec `contracts/wayland/README.md`,
//! "Animations"): windows fade and grow in when they open, fade and
//! shrink when they close, shrink towards the dock when minimized; shell
//! surfaces (overview, menus, dialogs) fade in and out; workspaces slide.
//!
//! What goes away is drawn from a snapshot of its surfaces' textures,
//! taken while they still exist.

use std::time::Instant;

use helix_comp::animation::{self as anim, Kind, Motion};
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::element::{Id, Kind as ElementKind};
use smithay::backend::renderer::gles::GlesTexture;
use smithay::backend::renderer::utils::RendererSurfaceStateUserData;
use smithay::backend::renderer::ContextId;
use smithay::desktop::Window;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::IsAlive;
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};
use smithay::wayland::compositor::{with_surface_tree_downward, SubsurfaceCachedState, TraversalAction};

/// One surface of a snapshot.
#[derive(Debug)]
struct Piece {
    id: Id,
    texture: GlesTexture,
    /// From the snapshot's top-left corner.
    offset: Point<f64, Logical>,
    size: Size<f64, Logical>,
    src: Rectangle<f64, Logical>,
    scale: i32,
    transform: Transform,
}

/// Something drawn from a snapshot while it animates away.
#[derive(Debug)]
pub struct Ghost {
    pieces: Vec<Piece>,
    rect: Rectangle<f64, Logical>,
    start: Instant,
    kind: Kind,
    /// Where minimizing heads (layout coordinates).
    target: Point<f64, Logical>,
    /// Workspace slides: +1 when the new workspace is to the right.
    direction: i32,
    /// Shell surfaces are drawn above windows.
    layer: bool,
}

#[derive(Debug, Default)]
pub struct Animations {
    /// The renderer whose textures snapshots take.
    pub context: Option<ContextId<GlesTexture>>,
    opening: Vec<(Window, Instant)>,
    layers: Vec<(WlSurface, Instant)>,
    ghosts: Vec<Ghost>,
    /// A workspace switch: when, which way, and the windows arriving.
    slide: Option<(Instant, i32, Vec<Window>)>,
}

/// The textures of `surface` and its subsurfaces, placed from the
/// surface's origin.
fn snapshot(context: &ContextId<GlesTexture>, surface: &WlSurface) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let place = |states: &smithay::wayland::compositor::SurfaceData, at: &Point<f64, Logical>| {
        let mut at = *at;
        if states.role == Some("subsurface") {
            at += states.cached_state.get::<SubsurfaceCachedState>().current().location.to_f64();
        }
        at
    };
    with_surface_tree_downward(
        surface,
        Point::<f64, Logical>::default(),
        |_, states, at| TraversalAction::DoChildren(place(states, at)),
        |_, states, at| {
            let at = place(states, at);
            let Some(data) = states.data_map.get::<RendererSurfaceStateUserData>() else { return };
            let data = data.lock().unwrap();
            if let (Some(view), Some(texture)) = (data.view(), data.texture::<GlesTexture>(context.clone())) {
                pieces.push(Piece {
                    id: Id::new(),
                    texture: texture.clone(),
                    offset: at + view.offset.to_f64(),
                    size: view.dst.to_f64(),
                    src: view.src,
                    scale: data.buffer_scale(),
                    transform: data.buffer_transform(),
                });
            }
        },
        |_, _, _| true,
    );
    pieces
}

impl Animations {
    /// Forget what has finished.
    pub fn prune(&mut self) {
        let now = Instant::now();
        let alive = |kind: Kind, start: Instant| anim::progress(kind, now - start).is_some();
        self.opening.retain(|(w, s)| alive(Kind::Open, *s) && w.alive());
        self.layers.retain(|(l, s)| alive(Kind::LayerIn, *s) && l.is_alive());
        self.ghosts.retain(|g| alive(g.kind, g.start));
        if self.slide.as_ref().is_some_and(|(s, _, _)| !alive(Kind::Slide, *s)) {
            self.slide = None;
        }
    }

    /// Whether anything is animating (frames must keep coming).
    pub fn active(&self) -> bool {
        !self.opening.is_empty() || !self.layers.is_empty() || !self.ghosts.is_empty() || self.slide.is_some()
    }

    pub fn window_opened(&mut self, motion: Motion, window: &Window) {
        if motion != Motion::Off {
            self.opening.retain(|(w, _)| w != window);
            self.opening.push((window.clone(), Instant::now()));
        }
    }

    pub fn layer_opened(&mut self, motion: Motion, surface: &WlSurface) {
        if motion != Motion::Off {
            self.layers.push((surface.clone(), Instant::now()));
        }
    }

    /// Something goes away: draw its last look (at `rect`, layout
    /// coordinates; `origin` is where its surface's origin was) as `kind`.
    #[allow(clippy::too_many_arguments)]
    pub fn leave(&mut self, motion: Motion, surface: &WlSurface, rect: Rectangle<f64, Logical>, origin: Point<f64, Logical>, kind: Kind, target: Point<f64, Logical>, direction: i32, layer: bool) {
        if motion == Motion::Off {
            return;
        }
        let Some(context) = self.context.clone() else { return };
        let mut pieces = snapshot(&context, surface);
        if pieces.is_empty() {
            return;
        }
        for p in &mut pieces {
            p.offset += origin - rect.loc;
        }
        self.ghosts.push(Ghost { pieces, rect, start: Instant::now(), kind, target, direction, layer });
    }

    /// A workspace switch towards `direction` brings `arriving`.
    pub fn slide(&mut self, motion: Motion, direction: i32, arriving: Vec<Window>) {
        if motion != Motion::Off {
            self.slide = Some((Instant::now(), direction, arriving));
        }
    }

    /// How a window looks now (opening, or arriving with a workspace).
    pub fn window_look(&self, motion: Motion, window: &Window) -> anim::Look {
        let now = Instant::now();
        let mut look = anim::Look::STILL;
        if let Some((_, start)) = self.opening.iter().find(|(w, _)| w == window) {
            if let Some(t) = anim::progress(Kind::Open, now - *start) {
                look = anim::look(Kind::Open, t, motion, 0, false);
            }
        }
        if let Some((start, direction, arriving)) = &self.slide {
            if arriving.contains(window) {
                if let Some(t) = anim::progress(Kind::Slide, now - *start) {
                    let slide = anim::look(Kind::Slide, t, motion, *direction, false);
                    look.shift = slide.shift;
                    look.alpha *= slide.alpha;
                }
            }
        }
        look
    }

    /// A shell surface's opacity now.
    pub fn layer_alpha(&self, motion: Motion, surface: &WlSurface) -> f32 {
        let now = Instant::now();
        self.layers
            .iter()
            .find(|(l, _)| l == surface)
            .and_then(|(_, start)| anim::progress(Kind::LayerIn, now - *start))
            .map_or(1.0, |t| anim::look(Kind::LayerIn, t, motion, 0, false).alpha as f32)
    }

    /// The ghosts' elements for a monitor at `output` (layout area) and
    /// `scale`: shell surfaces (`layer`) or windows.
    pub fn ghost_elements(&self, motion: Motion, output: Rectangle<i32, Logical>, scale: f64, layer: bool) -> Vec<TextureRenderElement<GlesTexture>> {
        let Some(context) = self.context.clone() else { return Vec::new() };
        let now = Instant::now();
        let mut out = Vec::new();
        for ghost in self.ghosts.iter().filter(|g| g.layer == layer) {
            let Some(t) = anim::progress(ghost.kind, now - ghost.start) else { continue };
            let look = anim::look(ghost.kind, t, motion, ghost.direction, true);
            let r = ghost.rect;
            let (x, y, w, _) = anim::place((r.loc.x, r.loc.y, r.size.w, r.size.h), &look, (ghost.target.x, ghost.target.y), f64::from(output.size.w));
            let k = if r.size.w > 0.0 { w / r.size.w } else { 1.0 };
            if !Rectangle::new(Point::from((x, y)), r.size.upscale(k)).overlaps(output.to_f64()) {
                continue;
            }
            for p in &ghost.pieces {
                let at = Point::<f64, Logical>::from((x + p.offset.x * k, y + p.offset.y * k)) - output.loc.to_f64();
                let size = Size::<f64, Logical>::from((p.size.w * k, p.size.h * k));
                out.push(TextureRenderElement::from_static_texture(
                    p.id.clone(),
                    context.clone(),
                    at.to_physical(scale),
                    p.texture.clone(),
                    p.scale,
                    p.transform,
                    Some(look.alpha as f32),
                    Some(p.src),
                    Some(size.to_i32_round()),
                    None,
                    ElementKind::Unspecified,
                ));
            }
        }
        out
    }
}
