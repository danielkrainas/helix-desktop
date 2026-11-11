//! Server side of the private `helix_screenshot_v1` protocol: the shell asks
//! for screen, area or window captures; the compositor renders them
//! off-screen and writes PNG images to the file descriptors it was given
//! (helix-desktop-spec `contracts/wayland/README.md`, "Screen capture").
//!
//! Requests queue up and are rendered when a renderer is at hand (the
//! nested backend's next frame, or right away on bare hardware). Encoding
//! and writing happen on a worker thread; the result comes back to the
//! event loop, which sends `done` or `failed`.

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
        self.clipboard_changed(mime, None);
        tracing::info!("image copied to the clipboard");
    }

    fn queue_capture(&mut self, pending: Pending) {
        self.captures.pending.push(pending);
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
            let Pending { frame, target, cursor, fd } = pending;
            if !frame.is_alive() {
                continue;
            }
            let result = if self.lock.locked {
                Err(Reason::Locked)
            } else {
                self.capture(renderer, target, cursor.then_some(named_cursor), &frame)
            };
            match result {
                Ok((rect, image)) => {
                    let Some(results) = self.captures.results.clone() else {
                        frame.failed(Reason::Failed);
                        continue;
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
    }

    fn capture(
        &mut self,
        renderer: &mut GlesRenderer,
        target: Target,
        cursor: Option<bool>,
        _frame: &HelixScreenshotFrameV1,
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
                .or_else(|| self.space.element_under(at).map(|(w, _)| w.clone()))
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
        let scene = super::frames::scene(renderer, &self.space, &self.frames, output, focused.as_ref());
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
            Request::Destroy => return,
            _ => return,
        };
        let frame = data_init.init(id, ());
        tracing::info!(?target, cursor, "screen capture requested");
        state.queue_capture(Pending { frame, target, cursor, fd });
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
    fn straight_alpha() {
        let mut px = [64, 32, 0, 128];
        unpremultiply(&mut px);
        assert_eq!(px, [128, 64, 0, 128]);
    }
}
