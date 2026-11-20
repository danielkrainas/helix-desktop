//! Server side of the private `helix_screenshot_v1` protocol: the shell asks
//! for screen, area or window captures; the compositor renders them
//! off-screen and writes PNG images to the file descriptors it was given
//! (helixdesktop/spec `contracts/wayland/README.md`, "Screen capture").
//!
//! Requests queue up and are rendered when a renderer is at hand (the
//! nested backend's next frame, or right away on bare hardware). The
//! headless backend (tests) has no renderer: it draws shared-memory
//! surfaces in software once idle, at scale 1, without title bars or the
//! pointer (`headless.rs`). Encoding and writing happen on a worker
//! thread; the result comes back to the event loop, which sends `done` or
//! `failed`.

use std::os::fd::OwnedFd;

use helix_protocols::server::helix_screenshot_frame_v1::{self, HelixScreenshotFrameV1, Reason};
use helix_protocols::server::helix_screenshot_manager_v1::{self, Cursor, HelixScreenshotManagerV1};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::AsRenderElements;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::{Bind, ExportMem, Offscreen, TextureMapping};
use smithay::desktop::Window;
use smithay::reexports::calloop::channel::{self, Sender};
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum};
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform};

use super::{ClientState, Helix};

/// The background where no window is (same as the screen).
pub const CLEAR: [f32; 4] = [0.12, 0.12, 0.14, 1.0];

smithay::backend::renderer::element::render_elements! {
    pub CaptureElement<=GlesRenderer>;
    Scene = super::frames::SceneElement,
    /// The pointer, or a title bar.
    Memory = MemoryRenderBufferRenderElement<GlesRenderer>,
    Surface = WaylandSurfaceRenderElement<GlesRenderer>,
}

/// What to draw: logical size, scale, and the elements front to back.
pub type Scene = (Size<i32, Logical>, f64, Vec<CaptureElement>);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Target {
    Screen,
    Area(Rectangle<i32, Logical>),
    /// The focused window, with its title bar or not.
    Window { frame: bool },
    /// The window shown at a point, with its title bar.
    WindowAt(Point<i32, Logical>),
    /// A window by its window list id, shown or not.
    WindowId(u32),
}

pub struct Pending {
    frame: HelixScreenshotFrameV1,
    target: Target,
    cursor: bool,
    fd: OwnedFd,
    /// An Agent Access screenshot (`target` is then unused).
    agent: Option<AgentShot>,
}

/// What `agent_capture` asked for.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentShot {
    /// A window list id; 0: the layout.
    pub window: u32,
    /// In the layout, or in the window; `None`: all of it.
    pub area: Option<Rectangle<i32, Logical>>,
    /// Windows that may be shown.
    pub shown: Vec<u32>,
    /// Layer surface namespaces that may be shown.
    pub layers: Vec<String>,
}

/// The widest or tallest agent screenshot, in pixels.
pub const AGENT_MAX_PX: u32 = 16384;

/// `image` (of `rect`) cut to `area` (in the same coordinates).
pub fn crop(image: &Image, rect: Rectangle<i32, Logical>, area: Rectangle<i32, Logical>) -> Option<Image> {
    if rect.size.w <= 0 || rect.size.h <= 0 || area.size.w <= 0 || area.size.h <= 0 {
        return None;
    }
    let sx = f64::from(image.width) / f64::from(rect.size.w);
    let sy = f64::from(image.height) / f64::from(rect.size.h);
    let x0 = (f64::from(area.loc.x - rect.loc.x) * sx).round() as u32;
    let y0 = (f64::from(area.loc.y - rect.loc.y) * sy).round() as u32;
    let w = ((f64::from(area.size.w) * sx).round() as u32).max(1).min(image.width.saturating_sub(x0));
    let h = ((f64::from(area.size.h) * sy).round() as u32).max(1).min(image.height.saturating_sub(y0));
    if w == 0 || h == 0 {
        return None;
    }
    let mut out = Image::transparent(w, h);
    for y in 0..h {
        let src = ((y0 + y) as usize * image.width as usize + x0 as usize) * 4;
        let dst = y as usize * w as usize * 4;
        out.rgba[dst..dst + w as usize * 4].copy_from_slice(&image.rgba[src..src + w as usize * 4]);
    }
    Some(out)
}

type Outcome = (HelixScreenshotFrameV1, Result<Rectangle<i32, Logical>, Reason>);

/// Queued captures and the way results come back.
#[derive(Default)]
pub struct Captures {
    pending: Vec<Pending>,
    results: Option<Sender<Outcome>>,
}

/// Straight-alpha RGBA pixels, rows top to bottom.
#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Image {
    pub fn transparent(width: u32, height: u32) -> Self {
        Self { width, height, rgba: vec![0; width as usize * height as usize * 4] }
    }
}

/// One rendered monitor (or window): where it is and at what scale.
pub struct Piece {
    pub geometry: Rectangle<i32, Logical>,
    pub scale: f64,
    pub image: Image,
}

/// Put `pieces` together into an image of `area` at `scale` pixels per
/// logical pixel (nearest-neighbour sampling). Uncovered parts stay
/// transparent.
pub fn compose(area: Rectangle<i32, Logical>, scale: f64, pieces: &[Piece]) -> Image {
    let width = (f64::from(area.size.w) * scale).round().max(1.0) as u32;
    let height = (f64::from(area.size.h) * scale).round().max(1.0) as u32;
    let mut out = Image::transparent(width, height);
    for piece in pieces {
        let Some(overlap) = piece.geometry.intersection(area) else { continue };
        let x0 = (f64::from(overlap.loc.x - area.loc.x) * scale).round() as u32;
        let y0 = (f64::from(overlap.loc.y - area.loc.y) * scale).round() as u32;
        let x1 = ((f64::from(overlap.loc.x + overlap.size.w - area.loc.x) * scale).round() as u32).min(width);
        let y1 = ((f64::from(overlap.loc.y + overlap.size.h - area.loc.y) * scale).round() as u32).min(height);
        for y in y0..y1 {
            // Logical position of this row's centre, then the source row.
            let ly = f64::from(area.loc.y) + (f64::from(y) + 0.5) / scale - f64::from(piece.geometry.loc.y);
            let sy = ((ly * piece.scale) as u32).min(piece.image.height.saturating_sub(1));
            for x in x0..x1 {
                let lx = f64::from(area.loc.x) + (f64::from(x) + 0.5) / scale - f64::from(piece.geometry.loc.x);
                let sx = ((lx * piece.scale) as u32).min(piece.image.width.saturating_sub(1));
                let src = (sy as usize * piece.image.width as usize + sx as usize) * 4;
                let dst = (y as usize * width as usize + x as usize) * 4;
                out.rgba[dst..dst + 4].copy_from_slice(&piece.image.rgba[src..src + 4]);
            }
        }
    }
    out
}

/// Write `png` to `fd` (a client pasting it), off the event loop.
pub fn serve_image(png: std::sync::Arc<Vec<u8>>, mime_type: &str, fd: OwnedFd) {
    if mime_type != "image/png" {
        return;
    }
    std::thread::spawn(move || {
        use std::io::Write;
        if let Err(err) = std::fs::File::from(fd).write_all(&png) {
            tracing::debug!(%err, "clipboard image not delivered");
        }
    });
}

/// The types text copied by the compositor is offered as.
pub const TEXT_MIMES: [&str; 5] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "STRING", "TEXT"];

/// Write `text` into `fd` when asked for one of [`TEXT_MIMES`].
pub fn serve_text(text: std::sync::Arc<Vec<u8>>, mime_type: &str, fd: OwnedFd) {
    if !TEXT_MIMES.contains(&mime_type) {
        return;
    }
    std::thread::spawn(move || {
        use std::io::Write;
        if let Err(err) = std::fs::File::from(fd).write_all(&text) {
            tracing::debug!(%err, "clipboard text not delivered");
        }
    });
}

/// Write `data` into `fd` when asked for `offered`, its one type.
pub fn serve_data(data: std::sync::Arc<Vec<u8>>, offered: &str, mime_type: &str, fd: OwnedFd) {
    if mime_type != offered {
        return;
    }
    std::thread::spawn(move || {
        use std::io::Write;
        if let Err(err) = std::fs::File::from(fd).write_all(&data) {
            tracing::debug!(%err, "clipboard data not delivered");
        }
    });
}

/// Encode as PNG.
pub fn encode_png(image: &Image, out: impl std::io::Write) -> Result<(), png::EncodingError> {
    let mut encoder = png::Encoder::new(out, image.width, image.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&image.rgba)?;
    writer.finish()
}

/// Render `elements` (front to back) into an image of `size` logical
/// pixels at `scale`.
pub(super) fn render(
    renderer: &mut GlesRenderer,
    size: Size<i32, Logical>,
    scale: f64,
    elements: &[CaptureElement],
) -> Result<Image, String> {
    let physical: Size<i32, Physical> = size.to_f64().to_physical(scale).to_i32_round();
    if physical.w <= 0 || physical.h <= 0 {
        return Err("empty".into());
    }
    let buffer_size: Size<i32, Buffer> = (physical.w, physical.h).into();
    let mut texture: GlesTexture = renderer
        .create_buffer(Fourcc::Abgr8888, buffer_size)
        .map_err(|e| format!("cannot create buffer: {e:?}"))?;
    let mut framebuffer = renderer.bind(&mut texture).map_err(|e| format!("cannot bind: {e:?}"))?;
    let mut tracker = OutputDamageTracker::new(physical, scale, Transform::Normal);
    tracker
        .render_output(renderer, &mut framebuffer, 0, elements, CLEAR)
        .map_err(|e| format!("cannot render: {e:?}"))?;
    let mapping = renderer
        .copy_framebuffer(&framebuffer, Rectangle::from_size(buffer_size), Fourcc::Abgr8888)
        .map_err(|e| format!("cannot read back: {e:?}"))?;
    // GL reads rows bottom-up; a mapping marked flipped is already top-down.
    let bottom_up = !mapping.flipped();
    let data = renderer.map_texture(&mapping).map_err(|e| format!("cannot map: {e:?}"))?;
    let (w, h) = (physical.w as usize, physical.h as usize);
    let mut rgba = vec![0u8; w * h * 4];
    for row in 0..h {
        let src = if bottom_up { h - 1 - row } else { row };
        rgba[row * w * 4..(row + 1) * w * 4].copy_from_slice(&data[src * w * 4..(src + 1) * w * 4]);
    }
    unpremultiply(&mut rgba);
    Ok(Image { width: w as u32, height: h as u32, rgba })
}

/// GL output is premultiplied; PNG wants straight alpha.
fn unpremultiply(rgba: &mut [u8]) {
    for px in rgba.as_chunks_mut::<4>().0 {
        let a = px[3];
        if a != 0 && a != 255 {
            for c in &mut px[..3] {
                *c = ((u16::from(*c) * 255 + u16::from(a) / 2) / u16::from(a)).min(255) as u8;
            }
        }
    }
}

impl Helix {
    /// Set up the channel that brings worker results back to the loop.
    pub fn init_captures(&mut self) {
        let (tx, rx) = channel::channel::<Outcome>();
        let inserted = self.loop_handle.insert_source(rx, |event, _, state| {
            if let channel::Event::Msg((frame, result)) = event {
                match result {
                    Ok(rect) => frame.done(rect.loc.x, rect.loc.y, rect.size.w, rect.size.h),
                    Err(reason) => frame.failed(reason),
                }
                let _ = state.display_handle.flush_clients();
            }
        });
        match inserted {
            Ok(_) => self.captures.results = Some(tx),
            Err(err) => tracing::warn!(%err, "screen capture unavailable"),
        }
    }

    /// Take the clipboard with the PNG image read from `fd`.
    fn copy_image(&mut self, fd: OwnedFd) {
        use std::io::Read;
        let mut png = Vec::new();
        if let Err(err) = std::fs::File::from(fd).read_to_end(&mut png) {
            tracing::warn!(%err, "cannot read the image to copy");
            return;
        }
        if !png.starts_with(b"\x89PNG") {
            tracing::warn!("not copying: not a PNG image");
            return;
        }
        let mime = vec!["image/png".to_owned()];
        smithay::wayland::selection::data_device::set_data_device_selection(
            &self.display_handle,
            &self.seat,
            mime.clone(),
            super::ServerSelection::Image(std::sync::Arc::new(png)),
        );
        if let Some(xwm) = self.xwm.as_mut() {
            if let Err(err) = xwm.new_selection(smithay::wayland::selection::SelectionTarget::Clipboard, Some(mime.clone())) {
                tracing::warn!(?err, "cannot offer the image to X11");
            }
        }
        self.clipboard_changed(mime, super::clipboard::Source::of(super::clipboard::Kind::Shell));
        tracing::info!("image copied to the clipboard");
    }

    /// Take the clipboard with the UTF-8 text read from `fd`.
    fn copy_text(&mut self, fd: OwnedFd) {
        use std::io::Read;
        let mut text = Vec::new();
        if let Err(err) = std::fs::File::from(fd).read_to_end(&mut text) {
            tracing::warn!(%err, "cannot read the text to copy");
            return;
        }
        if std::str::from_utf8(&text).is_err() {
            tracing::warn!("not copying: not UTF-8 text");
            return;
        }
        let mime: Vec<String> = TEXT_MIMES.iter().map(|m| (*m).to_owned()).collect();
        smithay::wayland::selection::data_device::set_data_device_selection(&self.display_handle, &self.seat, mime.clone(), super::ServerSelection::Text(std::sync::Arc::new(text)));
        if let Some(xwm) = self.xwm.as_mut() {
            if let Err(err) = xwm.new_selection(smithay::wayland::selection::SelectionTarget::Clipboard, Some(mime.clone())) {
                tracing::warn!(?err, "cannot offer the text to X11");
            }
        }
        self.clipboard_changed(mime, super::clipboard::Source::of(super::clipboard::Kind::Shell));
        tracing::info!("text copied to the clipboard");
    }

    fn queue_capture(&mut self, pending: Pending) {
        self.captures.pending.push(pending);
        if self.headless {
            // Tests that need captures left unanswered say so.
            if std::env::var_os("HELIX_HEADLESS_NO_CAPTURE").is_none() {
                self.loop_handle.insert_idle(|state| state.run_captures_headless());
            }
            return;
        }
        if self.udev.is_some() {
            // Bare hardware: the renderer is ours; capture once idle.
            self.loop_handle.insert_idle(|state| state.run_captures_udev());
        }
        // Nested: the next frame runs them (it has the renderer).
    }

    fn run_captures_udev(&mut self) {
        let Some(mut udev) = self.udev.take() else { return };
        self.run_captures(udev.renderer(), true);
        self.udev = Some(udev);
    }

    /// Render queued captures with `renderer`. `named_cursor`: Helix draws
    /// named cursors itself (bare hardware).
    pub fn run_captures(&mut self, renderer: &mut GlesRenderer, named_cursor: bool) {
        for pending in std::mem::take(&mut self.captures.pending) {
            let Pending { frame, target, cursor, fd, agent } = pending;
            if !frame.is_alive() {
                continue;
            }
            if let Some(agent) = agent {
                let result = self.capture_agent(Some(renderer), &agent);
                self.finish_agent_capture(frame, fd, result);
                continue;
            }
            let result = if self.lock.locked {
                Err(Reason::Locked)
            } else {
                self.capture(renderer, target, cursor.then_some(named_cursor), Some(&frame))
            };
            self.finish_capture(frame, fd, result);
        }
    }

    /// Write a capture's image to its file on a worker thread (then
    /// `done`), or send `failed`.
    fn finish_capture(&mut self, frame: HelixScreenshotFrameV1, fd: OwnedFd, result: Result<(Rectangle<i32, Logical>, Image), Reason>) {
        match result {
            Ok((rect, image)) => {
                let Some(results) = self.captures.results.clone() else {
                    frame.failed(Reason::Failed);
                    return;
                };
                std::thread::spawn(move || {
                    let file = std::fs::File::from(fd);
                    let written = encode_png(&image, std::io::BufWriter::new(file));
                    let outcome = match written {
                        Ok(()) => Ok(rect),
                        Err(err) => {
                            tracing::warn!(%err, "cannot write screenshot");
                            Err(Reason::Failed)
                        }
                    };
                    let _ = results.send((frame, outcome));
                });
            }
            Err(reason) => frame.failed(reason),
        }
    }

    /// An agent capture's result: `filled`, then the image as any other.
    fn finish_agent_capture(&mut self, frame: HelixScreenshotFrameV1, fd: OwnedFd, result: Result<(Rectangle<i32, Logical>, Image, u32), Reason>) {
        match result {
            Ok((rect, image, filled)) => {
                tracing::info!(width = image.width, height = image.height, filled, "agent screenshot taken");
                frame.filled(filled);
                self.finish_capture(frame, fd, Ok((rect, image)));
            }
            Err(reason) => {
                tracing::info!(?reason, "agent screenshot refused");
                self.finish_capture(frame, fd, Err(reason));
            }
        }
    }

    /// Where what an agent may not see is, in a capture of the layout
    /// (for counting): every window not in `shown` (with its popups and
    /// title bar) and every layer surface whose namespace is not in
    /// `layers` (with its popups), in layout coordinates.
    pub fn agent_fills(&self, shown: &[u32], layers: &[String]) -> Vec<Rectangle<i32, Logical>> {
        let mut out = Vec::new();
        for window in self.space.elements() {
            let id = self.window_list.ids.iter().find(|(w, _)| w == window).map(|(_, id)| *id);
            if id.is_some_and(|id| shown.contains(&id)) {
                continue;
            }
            if let Some(rect) = super::frames::mask_rect(&self.space, window) {
                out.push(rect);
            }
        }
        for output in self.space.outputs() {
            let Some(output_geo) = self.space.output_geometry(output) else { continue };
            let fullscreen = self.in_fullscreen(output);
            let map = smithay::desktop::layer_map_for_output(output);
            for layer in map.layers() {
                if layers.iter().any(|n| n == layer.namespace()) {
                    continue;
                }
                let level = layer.layer();
                if level == smithay::wayland::shell::wlr_layer::Layer::Top && fullscreen && super::wm::hidden_in_fullscreen(level, layer.namespace()) {
                    continue;
                }
                let Some(geometry) = map.layer_geometry(layer) else { continue };
                let mut rect = layer.bbox_with_popups();
                rect.loc += geometry.loc + output_geo.loc;
                out.push(rect.merge(Rectangle::new(geometry.loc + output_geo.loc, geometry.size)));
            }
        }
        out
    }

    /// An Agent Access screenshot (`agent_capture`): the image, its
    /// rectangle and how many windows and surfaces were filled in it.
    /// `renderer`: `None` on the headless backend (drawn in software).
    fn capture_agent(&mut self, renderer: Option<&mut GlesRenderer>, agent: &AgentShot) -> Result<(Rectangle<i32, Logical>, Image, u32), Reason> {
        if self.lock.locked {
            return Err(Reason::Locked);
        }
        let failed = |err: String| {
            tracing::warn!(err, "agent screenshot failed");
            Reason::Failed
        };
        let too_big = |image: &Image| image.width > AGENT_MAX_PX || image.height > AGENT_MAX_PX;
        if agent.window != 0 {
            if !agent.shown.contains(&agent.window) {
                return Err(Reason::NoWindow);
            }
            let window = self.window_by_list_id(agent.window).ok_or(Reason::NoWindow)?;
            let (rect, image) = match renderer {
                Some(renderer) => {
                    let geometry = window.geometry();
                    let scale = self.primary().map_or(1.0, |o| o.current_scale().fractional_scale());
                    let at: Point<i32, Physical> = Point::<i32, Logical>::from((-geometry.loc.x, -geometry.loc.y)).to_physical_precise_round(scale);
                    let elements: Vec<CaptureElement> = window
                        .render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(renderer, at, Scale::from(scale), 1.0)
                        .into_iter()
                        .map(CaptureElement::Surface)
                        .collect();
                    let image = render(renderer, geometry.size, scale, &elements).map_err(failed)?;
                    (Rectangle::new(self.window_geometry_location(&window), geometry.size), image)
                }
                None => self.capture_soft(Target::WindowId(agent.window))?,
            };
            let Some(area) = agent.area else {
                return if too_big(&image) { Err(Reason::Invalid) } else { Ok((rect, image, 0)) };
            };
            // The area is in the window.
            let inside = Rectangle::new(area.loc + rect.loc, area.size);
            if area.size.w <= 0 || area.size.h <= 0 || inside.intersection(rect) != Some(inside) {
                return Err(Reason::Invalid);
            }
            let image = crop(&image, rect, inside).ok_or(Reason::Invalid)?;
            return if too_big(&image) { Err(Reason::Invalid) } else { Ok((inside, image, 0)) };
        }
        let layout = self.space.outputs().filter_map(|o| self.space.output_geometry(o)).reduce(|a, b| a.merge(b)).ok_or(Reason::Invalid)?;
        let area = agent.area.unwrap_or(layout);
        if area.size.w <= 0 || area.size.h <= 0 || area.intersection(layout) != Some(area) {
            return Err(Reason::Invalid);
        }
        // Not drawn at all when it would be too big.
        let scale = self
            .space
            .outputs()
            .filter(|o| self.space.output_geometry(o).is_some_and(|g| g.overlaps(area)))
            .map(|o| if renderer.is_some() { o.current_scale().fractional_scale() } else { 1.0 })
            .fold(1.0, f64::max);
        if f64::from(area.size.w) * scale > f64::from(AGENT_MAX_PX) || f64::from(area.size.h) * scale > f64::from(AGENT_MAX_PX) {
            return Err(Reason::Invalid);
        }
        // Hidden: windows the shell did not clear (and those it does not
        // know, such as X11 menus), and layer surfaces of other namespaces.
        let ids = &self.window_list.ids;
        let hidden_window = |w: &Window| !ids.iter().find(|(x, _)| x == w).is_some_and(|(_, id)| agent.shown.contains(id));
        let hidden_layer = |l: &smithay::desktop::LayerSurface| !agent.layers.iter().any(|n| n == l.namespace());
        let mask = super::frames::Mask { window: &hidden_window, layer: &hidden_layer };
        let outputs: Vec<_> = self.space.outputs().filter(|o| self.space.output_geometry(o).is_some_and(|g| g.overlaps(area))).cloned().collect();
        let mut pieces = Vec::new();
        match renderer {
            Some(renderer) => {
                let focused = self.focused_window();
                for output in &outputs {
                    let Some(geometry) = self.space.output_geometry(output) else { continue };
                    let output_scale = output.current_scale().fractional_scale();
                    let elements: Vec<CaptureElement> = super::frames::scene(renderer, &self.space, &self.frames, output, focused.as_ref(), None, self.in_fullscreen(output), Some(&mask))
                        .into_iter()
                        .map(CaptureElement::Scene)
                        .collect();
                    let image = render(renderer, geometry.size, output_scale, &elements).map_err(failed)?;
                    pieces.push(Piece { geometry, scale: output_scale, image });
                }
            }
            None => {
                for output in &outputs {
                    let Some(geometry) = self.space.output_geometry(output) else { continue };
                    let image = self.headless_image_masked(output, Some(&mask)).ok_or(Reason::Failed)?.to_rgba();
                    pieces.push(Piece { geometry, scale: 1.0, image });
                }
            }
        }
        let image = compose(area, scale, &pieces);
        let rect = area;
        let filled = self.agent_fills(&agent.shown, &agent.layers).iter().filter(|r| r.overlaps(rect)).count() as u32;
        Ok((rect, image, filled))
    }

    /// The headless backend's captures, drawn in software.
    fn run_captures_headless(&mut self) {
        for Pending { frame, target, fd, agent, .. } in std::mem::take(&mut self.captures.pending) {
            if !frame.is_alive() {
                continue;
            }
            if let Some(agent) = agent {
                let result = self.capture_agent(None, &agent);
                self.finish_agent_capture(frame, fd, result);
                continue;
            }
            let result = if self.lock.locked { Err(Reason::Locked) } else { self.capture_soft(target) };
            self.finish_capture(frame, fd, result);
        }
    }

    /// [`Self::capture`] from shared-memory buffers, at scale 1 (the
    /// headless backend): monitors as `headless_image` draws them, windows
    /// from their surfaces (a title bar asked for is left plain).
    fn capture_soft(&mut self, target: Target) -> Result<(Rectangle<i32, Logical>, Image), Reason> {
        let window = match target {
            Target::Window { frame } => {
                let focus = self.seat.get_keyboard().and_then(|k| k.current_focus());
                Some((focus.and_then(|f| self.window_for_focus(&f)).ok_or(Reason::NoWindow)?, frame))
            }
            Target::WindowAt(point) => {
                let at = point.to_f64();
                let window = self
                    .frame_under(at)
                    .map(|(w, _)| w)
                    .or_else(|| self.window_under(at).map(|(w, _)| w.clone()))
                    .filter(|w| !super::wm::is_override_redirect(w))
                    .ok_or(Reason::NoWindow)?;
                Some((window, true))
            }
            Target::WindowId(id) => Some((self.window_by_list_id(id).ok_or(Reason::NoWindow)?, false)),
            Target::Screen | Target::Area(_) => None,
        };
        if let Some((window, frame)) = window {
            let geometry = window.geometry();
            let top = if frame { super::frames::frame_top(&window) } else { 0 };
            let location = match target {
                Target::WindowId(_) => self.window_geometry_location(&window),
                _ => self.space.element_location(&window).ok_or(Reason::NoWindow)?,
            };
            let rect = Rectangle::new((location.x, location.y - top).into(), (geometry.size.w, geometry.size.h + top).into());
            if rect.size.w <= 0 || rect.size.h <= 0 {
                return Err(Reason::Failed);
            }
            let mut image = super::headless::Image::filled(rect.size.w, rect.size.h);
            if let Some(surface) = super::wm::surface_of(&window) {
                image.draw_tree(&surface, Point::from((0, top)) - geometry.loc);
            }
            return Ok((rect, image.to_rgba()));
        }
        let outputs: Vec<_> = self.space.outputs().cloned().collect();
        let layout = outputs.iter().filter_map(|o| self.space.output_geometry(o)).reduce(|a, b| a.merge(b)).ok_or(Reason::Invalid)?;
        let area = match target {
            Target::Area(area) => area,
            _ => layout,
        };
        if area.size.w <= 0 || area.size.h <= 0 {
            return Err(Reason::Invalid);
        }
        let pieces: Vec<Piece> = outputs
            .iter()
            .filter_map(|o| {
                let geometry = self.space.output_geometry(o)?;
                geometry.overlaps(area).then_some(())?;
                Some(Piece { geometry, scale: 1.0, image: self.headless_image(o)?.to_rgba() })
            })
            .collect();
        Ok((area, compose(area, 1.0, &pieces)))
    }

    fn capture(
        &mut self,
        renderer: &mut GlesRenderer,
        target: Target,
        cursor: Option<bool>,
        _frame: Option<&HelixScreenshotFrameV1>,
    ) -> Result<(Rectangle<i32, Logical>, Image), Reason> {
        let failed = |err: String| {
            tracing::warn!(err, "screen capture failed");
            Reason::Failed
        };
        if let Target::Window { frame } = target {
            return self.capture_window(renderer, cursor, frame).map_err(|e| match e {
                None => Reason::NoWindow,
                Some(err) => failed(err),
            });
        }
        if let Target::WindowId(id) = target {
            let window = self.window_by_list_id(id).ok_or(Reason::NoWindow)?;
            let geometry = window.geometry();
            let scale = self.primary().map_or(1.0, |o| o.current_scale().fractional_scale());
            let at: Point<i32, Physical> = Point::<i32, Logical>::from((-geometry.loc.x, -geometry.loc.y)).to_physical_precise_round(scale);
            let elements: Vec<CaptureElement> = window
                .render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(renderer, at, Scale::from(scale), 1.0)
                .into_iter()
                .map(CaptureElement::Surface)
                .collect();
            let image = render(renderer, geometry.size, scale, &elements).map_err(failed)?;
            let location = self.window_geometry_location(&window);
            return Ok((Rectangle::new(location, geometry.size), image));
        }
        if let Target::WindowAt(point) = target {
            let at = point.to_f64();
            // The window under the point, or the one whose title bar it is.
            let window = self
                .frame_under(at)
                .map(|(w, _)| w)
                .or_else(|| self.window_under(at).map(|(w, _)| w.clone()))
                .filter(|w| !super::wm::is_override_redirect(w))
                .ok_or(Reason::NoWindow)?;
            return self.window_image(renderer, &window, cursor, true).map_err(|e| match e {
                None => Reason::NoWindow,
                Some(err) => failed(err),
            });
        }
        let outputs: Vec<_> = self.space.outputs().cloned().collect();
        let layout = outputs
            .iter()
            .filter_map(|o| self.space.output_geometry(o))
            .reduce(|a, b| a.merge(b))
            .ok_or(Reason::Invalid)?;
        let area = match target {
            Target::Area(area) => area,
            _ => layout,
        };
        if area.size.w <= 0 || area.size.h <= 0 {
            return Err(Reason::Invalid);
        }
        let mut pieces = Vec::new();
        let mut scale: f64 = 0.0;
        for output in &outputs {
            let Some(geometry) = self.space.output_geometry(output) else { continue };
            if !geometry.overlaps(area) {
                continue;
            }
            let output_scale = output.current_scale().fractional_scale();
            scale = scale.max(output_scale);
            let image = self.output_image(renderer, output, cursor).map_err(failed)?;
            pieces.push(Piece { geometry, scale: output_scale, image });
        }
        let scale = if scale > 0.0 { scale } else { 1.0 };
        Ok((area, compose(area, scale, &pieces)))
    }

    /// One monitor as it is shown (upright, at its scale).
    pub fn output_image(
        &self,
        renderer: &mut GlesRenderer,
        output: &smithay::output::Output,
        cursor: Option<bool>,
    ) -> Result<Image, String> {
        let (size, scale, elements) = self.output_elements(renderer, output, cursor)?;
        render(renderer, size, scale, &elements)
    }

    /// What one monitor shows: its logical size, scale and elements.
    pub fn output_elements(
        &self,
        renderer: &mut GlesRenderer,
        output: &smithay::output::Output,
        cursor: Option<bool>,
    ) -> Result<Scene, String> {
        let geometry = self.space.output_geometry(output).ok_or("output not mapped")?;
        let scale = output.current_scale().fractional_scale();
        let mut elements = match cursor {
            Some(named) => self.cursor_elements(renderer, geometry.loc.to_f64(), scale, named),
            None => Vec::new(),
        };
        let focused = self.focused_window();
        let fullscreen = self.in_fullscreen(output);
        let scene = super::frames::scene(renderer, &self.space, &self.frames, output, focused.as_ref(), None, fullscreen, None);
        elements.extend(scene.into_iter().map(CaptureElement::Scene));
        Ok((geometry.size, scale, elements))
    }

    /// `Err(None)`: no focused window.
    fn capture_window(
        &mut self,
        renderer: &mut GlesRenderer,
        cursor: Option<bool>,
        frame: bool,
    ) -> Result<(Rectangle<i32, Logical>, Image), Option<String>> {
        let focus = self.seat.get_keyboard().and_then(|k| k.current_focus());
        let window: Window = focus.and_then(|f| self.window_for_focus(&f)).ok_or(None)?;
        self.window_image(renderer, &window, cursor, frame)
    }

    /// One window, cropped to its window geometry (plus its title bar with
    /// `frame`), at the highest scale of the monitors it is on.
    /// `Err(None)`: not shown (hidden or minimized).
    pub fn window_image(
        &self,
        renderer: &mut GlesRenderer,
        window: &Window,
        cursor: Option<bool>,
        frame: bool,
    ) -> Result<(Rectangle<i32, Logical>, Image), Option<String>> {
        let (rect, scale, elements) = self.window_elements(renderer, window, cursor, frame).ok_or(None)?;
        let image = render(renderer, rect.size, scale, &elements).map_err(Some)?;
        Ok((rect, image))
    }

    /// What one window shows: its rectangle, scale and elements; `None`
    /// when it is not shown.
    pub fn window_elements(
        &self,
        renderer: &mut GlesRenderer,
        window: &Window,
        cursor: Option<bool>,
        frame: bool,
    ) -> Option<(Rectangle<i32, Logical>, f64, Vec<CaptureElement>)> {
        let window = window.clone();
        let location = self.space.element_location(&window)?;
        let geometry = window.geometry();
        let top = if frame { super::frames::frame_top(&window) } else { 0 };
        let rect = Rectangle::new((location.x, location.y - top).into(), (geometry.size.w, geometry.size.h + top).into());
        let scale = self
            .space
            .outputs_for_element(&window)
            .iter()
            .map(|o| o.current_scale().fractional_scale())
            .fold(0.0, f64::max);
        let scale = if scale > 0.0 { scale } else { 1.0 };
        let mut elements = match cursor {
            Some(named) => self.cursor_elements(renderer, rect.loc.to_f64(), scale, named),
            None => Vec::new(),
        };
        let at: Point<i32, Physical> = (Point::<i32, Logical>::from((0, top)) - geometry.loc).to_physical_precise_round(scale);
        let surfaces: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
            window.render_elements(renderer, at, Scale::from(scale), 1.0);
        elements.extend(surfaces.into_iter().map(CaptureElement::Surface));
        if top > 0 {
            let focused = self.focused_window().as_ref() == Some(&window);
            if let Some(title) = super::frames::frame_element(renderer, &self.space, &self.frames, &window, rect.loc, scale, focused) {
                elements.push(CaptureElement::Memory(title));
            }
        }
        Some((rect, scale, elements))
    }

    /// The pointer, relative to `origin`, if it is drawn by us (`named`:
    /// named cursors too).
    fn cursor_elements(
        &self,
        renderer: &mut GlesRenderer,
        origin: Point<f64, Logical>,
        scale: f64,
        named: bool,
    ) -> Vec<CaptureElement> {
        let mut out: Vec<CaptureElement> =
            self.cursor_surface_elements(renderer, origin, scale).into_iter().map(CaptureElement::Surface).collect();
        if named {
            out.extend(self.named_cursor_element(renderer, origin, scale).map(CaptureElement::Memory));
        }
        out
    }
}

impl GlobalDispatch<HelixScreenshotManagerV1, ()> for Helix {
    fn bind(
        _state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<HelixScreenshotManagerV1>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }

    fn can_view(client: Client, _data: &()) -> bool {
        client.get_data::<ClientState>().is_some_and(|c| c.privileged)
    }
}

fn shown(cursor: WEnum<Cursor>) -> bool {
    matches!(cursor, WEnum::Value(Cursor::Shown))
}

impl Dispatch<HelixScreenshotManagerV1, ()> for Helix {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &HelixScreenshotManagerV1,
        request: helix_screenshot_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use helix_screenshot_manager_v1::Request;
        let (id, target, cursor, fd) = match request {
            Request::CaptureScreen { id, cursor, fd } => (id, Target::Screen, shown(cursor), fd),
            Request::CaptureArea { id, x, y, width, height, cursor, fd } => {
                (id, Target::Area(Rectangle::new((x, y).into(), (width, height).into())), shown(cursor), fd)
            }
            Request::CaptureWindow { id, cursor, fd } => (id, Target::Window { frame: false }, shown(cursor), fd),
            Request::CaptureWindowWithFrame { id, cursor, fd } => (id, Target::Window { frame: true }, shown(cursor), fd),
            Request::CaptureWindowAt { id, x, y, cursor, fd } => (id, Target::WindowAt((x, y).into()), shown(cursor), fd),
            Request::CaptureWindowId { id, window, fd } => (id, Target::WindowId(window), false, fd),
            Request::CopyImage { fd } => {
                state.copy_image(fd);
                return;
            }
            Request::CopyText { fd } => {
                state.copy_text(fd);
                return;
            }
            Request::CopyData { mime_type, fd } => {
                state.copy_data(mime_type, fd);
                return;
            }
            Request::GetClipboard { id } => {
                let info = data_init.init(id, ());
                state.describe_clipboard(&info);
                return;
            }
            Request::ReceiveClipboard { generation, mime_type, fd } => {
                state.receive_clipboard(generation, mime_type, fd);
                return;
            }
            Request::AgentCapture { id, window, x, y, width, height, shown, layers, fd } => {
                let frame = data_init.init(id, ());
                let shown: Vec<u32> = shown.as_chunks::<4>().0.iter().map(|b| u32::from_ne_bytes(*b)).collect();
                let layers: Vec<String> = layers.split(',').filter(|l| !l.is_empty()).map(str::to_owned).collect();
                let area = (width > 0 || height > 0).then(|| Rectangle::new((x, y).into(), (width, height).into()));
                tracing::info!(window, ?area, shown = shown.len(), "agent screenshot requested");
                let agent = AgentShot { window, area, shown, layers };
                state.queue_capture(Pending { frame, target: Target::Screen, cursor: false, fd, agent: Some(agent) });
                return;
            }
            Request::AgentInput { id, window } => {
                state.agent_input_new(data_init, id, window);
                return;
            }
            Request::Destroy => return,
            _ => return,
        };
        let frame = data_init.init(id, ());
        if matches!(target, Target::WindowId(_)) {
            tracing::debug!(?target, "window picture requested");
        } else {
            tracing::info!(?target, cursor, "screen capture requested");
        }
        state.queue_capture(Pending { frame, target, cursor, fd, agent: None });
    }
}

impl Dispatch<HelixScreenshotFrameV1, ()> for Helix {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &HelixScreenshotFrameV1,
        _request: helix_screenshot_frame_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, px: [u8; 4]) -> Image {
        Image { width: w, height: h, rgba: px.repeat(w as usize * h as usize) }
    }

    fn at(image: &Image, x: u32, y: u32) -> [u8; 4] {
        let i = (y as usize * image.width as usize + x as usize) * 4;
        image.rgba[i..i + 4].try_into().unwrap()
    }

    #[test]
    fn composes_monitors_at_the_highest_scale() {
        // A 4x2 logical monitor at scale 1 and, to its right, a 2x2 at scale 2.
        let red = [255, 0, 0, 255];
        let blue = [0, 0, 255, 255];
        let pieces = [
            Piece { geometry: Rectangle::new((0, 0).into(), (4, 2).into()), scale: 1.0, image: solid(4, 2, red) },
            Piece { geometry: Rectangle::new((4, 0).into(), (2, 2).into()), scale: 2.0, image: solid(4, 4, blue) },
        ];
        let out = compose(Rectangle::new((0, 0).into(), (6, 3).into()), 2.0, &pieces);
        assert_eq!((out.width, out.height), (12, 6));
        assert_eq!(at(&out, 0, 0), red);
        assert_eq!(at(&out, 7, 3), red, "scaled up");
        assert_eq!(at(&out, 8, 0), blue);
        assert_eq!(at(&out, 11, 3), blue);
        assert_eq!(at(&out, 0, 5), [0, 0, 0, 0], "no monitor there");
    }

    #[test]
    fn composes_an_area() {
        let mut image = Image::transparent(4, 4);
        // Mark logical pixel (2, 1).
        let i = (4 + 2) * 4;
        image.rgba[i..i + 4].copy_from_slice(&[1, 2, 3, 255]);
        let pieces = [Piece { geometry: Rectangle::new((10, 10).into(), (4, 4).into()), scale: 1.0, image }];
        let out = compose(Rectangle::new((12, 11).into(), (1, 1).into()), 1.0, &pieces);
        assert_eq!((out.width, out.height), (1, 1));
        assert_eq!(at(&out, 0, 0), [1, 2, 3, 255]);
    }

    #[test]
    fn png_round_trip() {
        let image = solid(3, 2, [10, 20, 30, 255]);
        let mut bytes = Vec::new();
        encode_png(&image, &mut bytes).unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (3, 2));
        assert_eq!(&buf[..4], &[10, 20, 30, 255]);
    }

    #[test]
    fn crops_to_an_area() {
        let mut image = solid(8, 8, [1, 1, 1, 255]);
        let i = (3 * 8 + 4) * 4;
        image.rgba[i..i + 4].copy_from_slice(&[7, 7, 7, 255]);
        // The window at (10, 10), 4x4 at scale 2; its pixel (2, 1) logical.
        let out = crop(&image, Rectangle::new((10, 10).into(), (4, 4).into()), Rectangle::new((12, 11).into(), (2, 2).into())).unwrap();
        assert_eq!((out.width, out.height), (4, 4));
        assert_eq!(at(&out, 0, 1), [7, 7, 7, 255]);
        assert_eq!(at(&out, 1, 1), [1, 1, 1, 255]);
    }

    #[test]
    fn straight_alpha() {
        let mut px = [64, 32, 0, 128];
        unpremultiply(&mut px);
        assert_eq!(px, [128, 64, 0, 128]);
    }
}
