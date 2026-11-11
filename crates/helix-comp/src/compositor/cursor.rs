//! Pointer images Helix draws (helix-desktop-spec `contracts/wayland/README.md`,
//! "Pointer image"): named cursors from the XCursor theme, with a plain
//! arrow when there is no usable theme.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::input::pointer::CursorIcon;
use smithay::utils::Transform;

/// A straight-alpha BGRA image: pixels, width, height.
pub type Bitmap = Arc<(Vec<u8>, u32, u32)>;

type FrameCache = HashMap<(CursorIcon, i32), Rc<Vec<CursorFrame>>>;

/// One image of a named cursor, ready to draw.
#[derive(Debug, Clone)]
pub struct CursorFrame {
    /// Premultiplied, at `scale` buffer pixels per logical pixel.
    pub buffer: MemoryRenderBuffer,
    /// The same image as straight-alpha BGRA (cursor metadata).
    pub bgra: Bitmap,
    /// Hotspot in image pixels.
    pub hotspot: (i32, i32),
    /// Image pixels per logical pixel.
    pub scale: i32,
    /// How long it shows (animated cursors; 0: still).
    delay_ms: u32,
}

/// Named cursors of the configured theme, loaded on first use.
#[derive(Debug)]
pub struct Cursors {
    theme: String,
    size: u32,
    loaded: RefCell<Option<xcursor::CursorTheme>>,
    cache: RefCell<FrameCache>,
    start: Instant,
}

impl Cursors {
    pub fn new(theme: &str, size: u32) -> Self {
        Self {
            theme: theme.to_owned(),
            size: size.max(1),
            loaded: RefCell::new(None),
            cache: RefCell::new(HashMap::new()),
            start: Instant::now(),
        }
    }

    /// `cursor-theme` / `cursor-size` changed.
    pub fn set_theme(&mut self, theme: &str, size: u32) {
        if theme != self.theme || size.max(1) != self.size {
            *self = Self::new(theme, size);
        }
    }

    /// The image of `icon` for a monitor of `scale`, now.
    pub fn frame(&self, icon: CursorIcon, scale: f64) -> CursorFrame {
        let buffer_scale = (scale.ceil() as i32).max(1);
        let frames = self
            .cache
            .borrow_mut()
            .entry((icon, buffer_scale))
            .or_insert_with(|| Rc::new(self.load(icon, buffer_scale)))
            .clone();
        let total: u64 = frames.iter().map(|f| u64::from(f.delay_ms)).sum();
        if frames.len() > 1 && total > 0 {
            let mut t = self.start.elapsed().as_millis() as u64 % total;
            for f in frames.iter() {
                if t < u64::from(f.delay_ms) {
                    return f.clone();
                }
                t -= u64::from(f.delay_ms);
            }
        }
        frames[0].clone()
    }

    fn load(&self, icon: CursorIcon, buffer_scale: i32) -> Vec<CursorFrame> {
        let pixels = self.size * buffer_scale as u32;
        let mut loaded = self.loaded.borrow_mut();
        let theme = loaded.get_or_insert_with(|| xcursor::CursorTheme::load(&self.theme));
        let names = std::iter::once(icon.name()).chain(icon.alt_names().iter().copied()).chain(["default", "left_ptr"]);
        for name in names {
            let Some(path) = theme.load_icon(name) else { continue };
            let Ok(data) = std::fs::read(&path) else { continue };
            let Some(images) = xcursor::parser::parse_xcursor(&data) else { continue };
            let frames = pick(&images, pixels, buffer_scale);
            if !frames.is_empty() {
                return frames;
            }
        }
        vec![builtin()]
    }
}

/// The frames of the nominal size nearest to `pixels`.
fn pick(images: &[xcursor::parser::Image], pixels: u32, scale: i32) -> Vec<CursorFrame> {
    let Some(best) = images.iter().map(|i| i.size).min_by_key(|s| s.abs_diff(pixels)) else { return Vec::new() };
    images
        .iter()
        .filter(|i| i.size == best && i.width > 0 && i.height > 0)
        // The file's byte order is BGRA, premultiplied.
        .map(|i| frame(i.pixels_rgba.clone(), i.width, i.height, (i.xhot as i32, i.yhot as i32), scale, i.delay))
        .collect()
}

fn frame(premultiplied: Vec<u8>, width: u32, height: u32, hotspot: (i32, i32), scale: i32, delay_ms: u32) -> CursorFrame {
    let buffer = MemoryRenderBuffer::from_slice(
        &premultiplied,
        Fourcc::Argb8888,
        (width as i32, height as i32),
        scale,
        Transform::Normal,
        None,
    );
    let mut straight = premultiplied;
    unpremultiply_bgra(&mut straight);
    CursorFrame { buffer, bgra: Arc::new((straight, width, height)), hotspot, scale, delay_ms }
}

fn unpremultiply_bgra(bgra: &mut [u8]) {
    for px in bgra.as_chunks_mut::<4>().0 {
        let a = u32::from(px[3]);
        if a > 0 && a < 255 {
            for c in &mut px[..3] {
                *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}

/// Helix's plain arrow.
pub fn builtin() -> CursorFrame {
    let (pixels, w, h) = arrow();
    frame(pixels, w as u32, h as u32, (0, 0), 1, 0)
}

type Gles = smithay::backend::renderer::gles::GlesRenderer;
type LogicalPoint = smithay::utils::Point<f64, smithay::utils::Logical>;

/// The app's own pointer image (`wl_pointer.set_cursor`) with the pointer
/// at `pointer`, as elements for a target whose top-left is `origin`
/// (logical) drawn at `scale`; empty unless an app set one.
pub fn surface_elements(
    renderer: &mut Gles,
    status: &smithay::input::pointer::CursorImageStatus,
    pointer: LogicalPoint,
    origin: LogicalPoint,
    scale: f64,
) -> Vec<smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement<Gles>> {
    use smithay::input::pointer::{CursorImageStatus, CursorImageSurfaceData};
    let CursorImageStatus::Surface(surface) = status else { return Vec::new() };
    let hotspot = smithay::wayland::compositor::with_states(surface, |states| {
        states
            .data_map
            .get::<CursorImageSurfaceData>()
            .map(|d| d.lock().unwrap().hotspot)
            .unwrap_or_default()
    });
    let at = (pointer - origin - hotspot.to_f64()).to_physical(scale).to_i32_round();
    smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
        renderer,
        surface,
        at,
        smithay::utils::Scale::from(scale),
        1.0,
        smithay::backend::renderer::element::Kind::Cursor,
    )
}

/// The named cursor as an element, with the pointer at `pointer`, for a
/// target at `origin` drawn at `scale`.
pub fn named_element(
    renderer: &mut Gles,
    status: &smithay::input::pointer::CursorImageStatus,
    cursors: &Cursors,
    pointer: LogicalPoint,
    origin: LogicalPoint,
    scale: f64,
) -> Option<smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement<Gles>> {
    use smithay::input::pointer::CursorImageStatus;
    let CursorImageStatus::Named(icon) = status else { return None };
    let frame = cursors.frame(*icon, scale);
    let hotspot: LogicalPoint =
        (f64::from(frame.hotspot.0) / f64::from(frame.scale), f64::from(frame.hotspot.1) / f64::from(frame.scale)).into();
    let at = (pointer - origin - hotspot).to_physical(scale);
    smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        at,
        &frame.buffer,
        None,
        None,
        None,
        smithay::backend::renderer::element::Kind::Cursor,
    )
    .map_err(|err| tracing::warn!(?err, "cannot draw cursor"))
    .ok()
}

impl super::Helix {
    fn pointer_location(&self) -> LogicalPoint {
        self.seat.get_pointer().map(|p| p.current_location()).unwrap_or_default()
    }

    /// [`surface_elements`] for the current pointer.
    pub fn cursor_surface_elements(&self, renderer: &mut Gles, origin: LogicalPoint, scale: f64) -> Vec<smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement<Gles>> {
        surface_elements(renderer, &self.cursor_status, self.pointer_location(), origin, scale)
    }

    /// [`named_element`] for the current pointer.
    pub fn named_cursor_element(&self, renderer: &mut Gles, origin: LogicalPoint, scale: f64) -> Option<smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement<Gles>> {
        named_element(renderer, &self.cursor_status, &self.cursors, self.pointer_location(), origin, scale)
    }

    /// Frame callbacks for the app's pointer image shown on `output`.
    pub fn send_cursor_frame(&self, output: &smithay::output::Output, time: std::time::Duration) {
        use smithay::input::pointer::CursorImageStatus;
        let CursorImageStatus::Surface(surface) = &self.cursor_status else { return };
        let on = self.space.output_geometry(output).is_some_and(|g| g.to_f64().contains(self.pointer_location()));
        if on {
            smithay::desktop::utils::send_frames_surface_tree(surface, output, time, Some(std::time::Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        }
    }
}

/// A left-pointing arrow; `#` is the outline, `.` the fill, space is clear.
/// The hotspot is the top-left pixel.
const ARROW: &[&str] = &[
    "#",
    "##",
    "#.#",
    "#..#",
    "#...#",
    "#....#",
    "#.....#",
    "#......#",
    "#.......#",
    "#........#",
    "#.........#",
    "#..........#",
    "#......#####",
    "#...#..#",
    "#..##..#",
    "#.#  #..#",
    "##   #..#",
    "#     #..#",
    "      #..#",
    "       ##",
];

/// Scale of the arrow in pixels per art cell.
pub const SCALE: usize = 1;

/// The arrow as little-endian ARGB8888 (B, G, R, A per pixel), with its size.
pub fn arrow() -> (Vec<u8>, usize, usize) {
    let width = ARROW.iter().map(|row| row.len()).max().unwrap_or(0) * SCALE;
    let height = ARROW.len() * SCALE;
    let mut pixels = vec![0u8; width * height * 4];
    for (y, row) in ARROW.iter().enumerate() {
        for (x, cell) in row.bytes().enumerate() {
            let bgra = match cell {
                b'#' => [0x00, 0x00, 0x00, 0xFF],
                b'.' => [0xFF, 0xFF, 0xFF, 0xFF],
                _ => continue,
            };
            for dy in 0..SCALE {
                for dx in 0..SCALE {
                    let i = ((y * SCALE + dy) * width + x * SCALE + dx) * 4;
                    pixels[i..i + 4].copy_from_slice(&bgra);
                }
            }
        }
    }
    (pixels, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_size_and_animation() {
        let img = |size: u32, delay: u32| xcursor::parser::Image {
            size,
            width: size,
            height: size,
            xhot: size / 2,
            yhot: 1,
            delay,
            pixels_rgba: [0x40, 0x40, 0x40, 0x80].repeat((size * size) as usize),
            pixels_argb: vec![0; (size * size * 4) as usize],
        };
        let images = [img(24, 0), img(48, 30), img(48, 70), img(32, 0)];
        let frames = pick(&images, 48, 2);
        assert_eq!(frames.len(), 2, "both frames of the 48 px size");
        assert_eq!(frames[0].hotspot, (24, 1));
        assert_eq!(frames[0].scale, 2);
        // Straight alpha for metadata: 0x40 at alpha 0x80 → 0x80.
        assert_eq!(&frames[0].bgra.0[0..4], &[0x80, 0x80, 0x80, 0x80]);
        assert_eq!(pick(&images, 30, 1)[0].bgra.1, 32);
        assert!(pick(&[], 24, 1).is_empty());
    }

    #[test]
    fn missing_theme_falls_back_to_the_arrow() {
        let cursors = Cursors::new("no-such-theme-helix-test", 24);
        let f = cursors.frame(CursorIcon::Wait, 1.0);
        assert_eq!((f.bgra.1, f.bgra.2), (12, 20));
        assert_eq!(f.hotspot, (0, 0));
    }

    #[test]
    fn arrow_shape() {
        let (pixels, w, h) = arrow();
        assert_eq!((w, h), (12, 20));
        assert_eq!(pixels.len(), w * h * 4);
        // Hotspot (0,0) is opaque outline; the far top-right is transparent.
        assert_eq!(&pixels[0..4], &[0, 0, 0, 0xFF]);
        assert_eq!(pixels[(w - 1) * 4 + 3], 0);
        // There is white fill inside.
        let white = pixels.as_chunks::<4>().0.iter().filter(|p| **p == [0xFF; 4]).count();
        assert!(white > 30);
    }
}
