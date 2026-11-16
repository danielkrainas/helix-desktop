//! Drawing into ARGB8888 buffers: rectangles, simple anti-aliased shapes
//! and text. Used by the shell (its surfaces) and the compositor (window
//! frames).
//!
//! Text uses the system's fonts (found through fontconfig's `fc-match`,
//! with a fallback font for characters the main one lacks), rasterized
//! with `ab_glyph`, in visual order for right-to-left scripts (see
//! [`bidi`]).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use ab_glyph::{point, Font, FontVec, GlyphId, PxScale, ScaleFont};

pub mod accent;
pub mod backdrop;
pub mod bidi;
pub use bidi::{rtl, set_rtl};
pub mod motion;
pub mod screen;
pub mod theme;

/// An ARGB8888 (premultiplied, little-endian) pixel buffer. Drawing takes
/// logical coordinates (surface-local); `scale` turns them into the
/// buffer's pixels.
pub struct Canvas<'a> {
    pub data: &'a mut [u8],
    /// Buffer size in pixels.
    pub width: usize,
    pub height: usize,
    pub scale: f64,
    /// Drawing outside this (physical x0, y0, x1, y1) is dropped.
    clip: Option<[i32; 4]>,
    /// Draw the layout mirrored left to right (a right-to-left
    /// interface): shapes flip; text and pictures move to their mirrored
    /// places but stay readable.
    mirror: bool,
}

/// Pixels for `logical` units at `scale` (rounded, as the fractional-scale
/// protocol asks of buffer sizes).
pub fn physical(logical: usize, scale: f64) -> usize {
    (logical as f64 * scale).round() as usize
}

static HIGH_CONTRAST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// High contrast (helixdesktop/spec `a11y-interface.md`): text white,
/// dark shapes black and light ones white, saturated colors kept.
pub fn set_high_contrast(on: bool) {
    HIGH_CONTRAST.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub fn high_contrast() -> bool {
    HIGH_CONTRAST.load(std::sync::atomic::Ordering::Relaxed)
}

static STATUS_SHAPES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `show-status-shapes` (helixdesktop/spec `a11y-interface.md`): on/off
/// states are shown with shapes as well as color.
pub fn set_status_shapes(on: bool) {
    STATUS_SHAPES.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub fn status_shapes() -> bool {
    STATUS_SHAPES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Saturated colors (the accent, warnings) are kept in high contrast.
fn saturated(color: u32) -> bool {
    let [r, g, b, _] = rgba(color);
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    max > 0.2 && (max - min) / max > 0.5
}

fn luminance(color: u32) -> f32 {
    let [r, g, b, _] = rgba(color);
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// A shape's high-contrast color.
fn contrast_shape(color: u32) -> u32 {
    if saturated(color) {
        return color;
    }
    // Edges and separators are lines: white.
    if color == theme::BORDER {
        return 0xFFFF_FFFF;
    }
    let alpha = color & 0xFF00_0000;
    if luminance(color) >= 0.4 { alpha | 0x00FF_FFFF } else { alpha }
}

/// Text's high-contrast color.
fn contrast_text(color: u32) -> u32 {
    if saturated(color) { color } else { (color & 0xFF00_0000) | 0x00FF_FFFF }
}

/// A shape's color, in high contrast when it is on.
pub fn shape_color(color: u32) -> u32 {
    if high_contrast() { contrast_shape(color) } else { color }
}

/// Text's color, in high contrast when it is on.
pub fn text_color(color: u32) -> u32 {
    if high_contrast() { contrast_text(color) } else { color }
}

/// Colors are `0xAARRGGBB`, not premultiplied.
pub fn rgba(color: u32) -> [f32; 4] {
    let c = |shift: u32| ((color >> shift) & 0xff) as f32 / 255.0;
    [c(16), c(8), c(0), c(24)]
}

impl<'a> Canvas<'a> {
    /// A canvas for a `width`×`height` (logical) surface at `scale`; `data`
    /// holds the physical pixels.
    pub fn scaled(data: &'a mut [u8], width: usize, height: usize, scale: f64) -> Self {
        Self { data, width: physical(width, scale), height: physical(height, scale), scale, clip: None, mirror: false }
    }

    /// This canvas, mirrored when `on` (see [`bidi`]): logical coordinates
    /// stay those of a left-to-right layout and land mirrored.
    pub fn mirrored(mut self, on: bool) -> Self {
        self.mirror = on;
        self
    }

    /// Whether drawing is mirrored.
    pub fn is_mirrored(&self) -> bool {
        self.mirror
    }

    /// The width in logical units.
    pub fn logical_width(&self) -> f64 {
        self.width as f64 / self.scale
    }

    /// Where a box at logical `x`, `w` wide, lands: mirrored or not.
    pub fn place_x(&self, x: f64, w: f64) -> f64 {
        if self.mirror { self.logical_width() - x - w } else { x }
    }

    /// Draw only inside the logical rectangle (x, y, w, h) until
    /// [`Self::unclip`] (a scrolled part of a surface).
    pub fn clip(&mut self, x: i32, y: i32, w: i32, h: i32) {
        let edge = |v: i32| (f64::from(v) * self.scale).round() as i32;
        let (x0, x1) = if self.mirror { (self.width as i32 - edge(x + w), self.width as i32 - edge(x)) } else { (edge(x), edge(x + w)) };
        self.clip = Some([x0, edge(y), x1, edge(y + h)]);
    }

    pub fn unclip(&mut self) {
        self.clip = None;
    }

    /// Draw only where both the current clip and the logical rectangle
    /// (x, y, w, h) allow; returns the clip before, for [`Self::restore_clip`].
    pub fn push_clip(&mut self, x: i32, y: i32, w: i32, h: i32) -> Option<[i32; 4]> {
        let before = self.clip;
        self.clip(x, y, w, h);
        if let (Some([a0, b0, a1, b1]), Some([c0, d0, c1, d1])) = (before, self.clip) {
            self.clip = Some([a0.max(c0), b0.max(d0), a1.min(c1), b1.min(d1)]);
        }
        before
    }

    /// Put back a clip [`Self::push_clip`] returned.
    pub fn restore_clip(&mut self, clip: Option<[i32; 4]>) {
        self.clip = clip;
    }

    /// Whether physical pixel (x, y) is outside the clip.
    fn clipped(&self, x: i32, y: i32) -> bool {
        self.clip.is_some_and(|[x0, y0, x1, y1]| x < x0 || y < y0 || x >= x1 || y >= y1)
    }

    /// Blend `color` over the buffer pixel at (x, y) with extra coverage
    /// `cov` (x mirrored on a mirrored canvas).
    pub fn blend_px(&mut self, x: i32, y: i32, color: u32, cov: f32) {
        let x = if self.mirror { self.width as i32 - 1 - x } else { x };
        self.blend_raw(x, y, color, cov);
    }

    /// [`Self::blend_px`] at the buffer's own pixel (x, y), never mirrored.
    fn blend_raw(&mut self, x: i32, y: i32, color: u32, cov: f32) {
        if x < 0 || y < 0 || x as usize >= self.width || y as usize >= self.height || self.clipped(x, y) {
            return;
        }
        let [r, g, b, a] = rgba(color);
        let a = a * cov.clamp(0.0, 1.0);
        if a <= 0.0 {
            return;
        }
        let i = (y as usize * self.width + x as usize) * 4;
        let px = &mut self.data[i..i + 4];
        // Byte order in memory: B, G, R, A.
        let src = [b * a, g * a, r * a, a];
        for (dst, s) in px.iter_mut().zip(src) {
            let d = f32::from(*dst) / 255.0;
            *dst = ((s + d * (1.0 - a)) * 255.0).round() as u8;
        }
    }

    pub fn fill(&mut self, color: u32) {
        let [r, g, b, a] = rgba(shape_color(color));
        let px = [(b * a * 255.0) as u8, (g * a * 255.0) as u8, (r * a * 255.0) as u8, (a * 255.0) as u8];
        for chunk in self.data.as_chunks_mut::<4>().0 {
            *chunk = px;
        }
    }

    /// Fill an anti-aliased circle (logical center and radius).
    pub fn circle(&mut self, cx: f64, cy: f64, r: f64, color: u32) {
        self.shape(cx - r, cy - r, cx + r, cy + r, |x, y| r - ((x - cx).powi(2) + (y - cy).powi(2)).sqrt(), color);
    }

    /// A ring of radius `r` and `width` around (cx, cy).
    pub fn ring(&mut self, cx: f64, cy: f64, r: f64, width: f64, color: u32) {
        let half = width / 2.0;
        self.shape(cx - r - half, cy - r - half, cx + r + half, cy + r + half, |x, y| half - (((x - cx).powi(2) + (y - cy).powi(2)).sqrt() - r).abs(), color);
    }

    /// A status shape centered at (cx, cy), `size` tall: a bar for on, a
    /// ring for off.
    pub fn status_shape(&mut self, cx: f64, cy: f64, size: f64, on: bool, color: u32) {
        if on {
            self.line((cx, cy - size / 2.0 + 1.0), (cx, cy + size / 2.0 - 1.0), 2.0, color);
        } else {
            self.ring(cx, cy, size / 2.0 - 1.0, 1.6, color);
        }
    }

    /// Draw an anti-aliased line `width` logical units thick from (x0, y0)
    /// to (x1, y1).
    pub fn line(&mut self, (x0, y0): (f64, f64), (x1, y1): (f64, f64), width: f64, color: u32) {
        let (dx, dy) = (x1 - x0, y1 - y0);
        let len2 = (dx * dx + dy * dy).max(f64::EPSILON);
        let half = width / 2.0;
        self.shape(x0.min(x1) - half, y0.min(y1) - half, x0.max(x1) + half, y0.max(y1) + half, |x, y| {
            // Distance to the segment.
            let t = (((x - x0) * dx + (y - y0) * dy) / len2).clamp(0.0, 1.0);
            let (px, py) = (x0 + t * dx, y0 + t * dy);
            half - ((x - px).powi(2) + (y - py).powi(2)).sqrt()
        }, color);
    }

    /// Fill the pixels of the logical box (x0, y0)–(x1, y1) by `inside`:
    /// the signed distance (logical units) of a point to the shape's edge,
    /// positive inside.
    fn shape(&mut self, x0: f64, y0: f64, x1: f64, y1: f64, inside: impl Fn(f64, f64) -> f64, color: u32) {
        let color = shape_color(color);
        let s = self.scale;
        let (px0, py0) = ((x0 * s).floor() as i32 - 1, (y0 * s).floor() as i32 - 1);
        let (px1, py1) = ((x1 * s).ceil() as i32 + 1, (y1 * s).ceil() as i32 + 1);
        for py in py0.max(0)..py1.min(self.height as i32) {
            for px in px0.max(0)..px1.min(self.width as i32) {
                // Pixel center, in logical units; coverage over one pixel.
                let (x, y) = ((f64::from(px) + 0.5) / s, (f64::from(py) + 0.5) / s);
                let cov = (inside(x, y) * s + 0.5).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.blend_px(px, py, color, cov as f32);
                }
            }
        }
    }

    /// Fill a rectangle given in logical units.
    /// Fill an anti-aliased rectangle with corners of radius `r` (logical).
    pub fn round_rect(&mut self, x: f64, y: f64, w: f64, h: f64, r: f64, color: u32) {
        let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
        let (cx, cy, hw, hh) = (x + w / 2.0, y + h / 2.0, w / 2.0 - r, h / 2.0 - r);
        self.shape(x, y, x + w, y + h, |px, py| {
            // Signed distance to the rounded box, as "how far inside".
            let (dx, dy) = ((px - cx).abs() - hw, (py - cy).abs() - hh);
            let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt() + dx.max(dy).min(0.0);
            r - outside
        }, color);
    }

    /// A soft shadow around the rounded box (x, y, w, h) (corner `r`):
    /// `color` just outside its edge, fading (quadratically) to nothing
    /// `spread` logical px out. The inside is left alone (the box covers
    /// it), so rows deep inside are skipped.
    #[allow(clippy::too_many_arguments)]
    pub fn shadow(&mut self, x: f64, y: f64, w: f64, h: f64, r: f64, spread: f64, color: u32) {
        let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
        let (cx, cy, hw, hh) = (x + w / 2.0, y + h / 2.0, w / 2.0 - r, h / 2.0 - r);
        let s = self.scale;
        let (px0, py0) = (((x - spread) * s).floor() as i32, ((y - spread) * s).floor() as i32);
        let (px1, py1) = (((x + w + spread) * s).ceil() as i32, ((y + h + spread) * s).ceil() as i32);
        // Inside the box by more than this: covered, skipped.
        let (ix0, ix1) = (((x + r + 1.0) * s).ceil() as i32, ((x + w - r - 1.0) * s).floor() as i32);
        let (iy0, iy1) = (((y + r + 1.0) * s).ceil() as i32, ((y + h - r - 1.0) * s).floor() as i32);
        for py in py0.max(0)..py1.min(self.height as i32) {
            let mut px = px0.max(0);
            while px < px1.min(self.width as i32) {
                if py >= iy0 && py < iy1 && px >= ix0 && px < ix1 {
                    px = ix1;
                    continue;
                }
                let (lx, ly) = ((f64::from(px) + 0.5) / s, (f64::from(py) + 0.5) / s);
                let (dx, dy) = ((lx - cx).abs() - hw, (ly - cy).abs() - hh);
                let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt() + dx.max(dy).min(0.0) - r;
                let t = (1.0 - outside.max(0.0) / spread).clamp(0.0, 1.0);
                if t > 0.0 {
                    self.blend_px(px, py, color, (t * t) as f32);
                }
                px += 1;
            }
        }
    }

    pub fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        let color = shape_color(color);
        let edge = |v: i32| (f64::from(v) * self.scale).round() as i32;
        let (x0, y0, x1, y1) = (edge(x), edge(y), edge(x + w), edge(y + h));
        for yy in y0.max(0)..y1.min(self.height as i32) {
            for xx in x0.max(0)..x1.min(self.width as i32) {
                self.blend_px(xx, yy, color, 1.0);
            }
        }
    }
}

pub struct Text {
    font: FontVec,
}

/// Fonts for characters the main font lacks, by character (fontconfig's
/// best font with that character), loaded once per process. A font
/// already loaded for another character that has this one is used without
/// asking fontconfig again: a CJK text needs hundreds of characters from
/// one font, and asking for each would stall the first frame.
fn fallback_font(c: char) -> Option<Arc<FontVec>> {
    type Cache = Mutex<(HashMap<char, Option<Arc<FontVec>>>, Vec<((String, u32), Arc<FontVec>)>)>;
    static CACHE: OnceLock<Cache> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    {
        let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(found) = cache.0.get(&c) {
            return found.clone();
        }
        if let Some(font) = cache.1.iter().map(|(_, f)| f).find(|f| f.glyph_id(c).0 != 0).cloned() {
            cache.0.insert(c, Some(font.clone()));
            return Some(font);
        }
    }
    // The face's index too: CJK fonts come as collections (.ttc) whose
    // faces differ by language (Noto Sans CJK JP, KR, SC…), and fontconfig
    // picks the one for the locale's language.
    let found = std::process::Command::new("fc-match")
        .args(["-f", "%{file}\n%{index}", &format!("sans-serif:charset={:x}", c as u32)])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| {
            let out = String::from_utf8_lossy(&out.stdout).into_owned();
            let (path, index) = out.split_once('\n')?;
            Some((path.to_owned(), index.trim().parse::<u32>().unwrap_or(0)))
        });
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    let font = found.and_then(|key| {
        if let Some((_, font)) = cache.1.iter().find(|(k, _)| *k == key) {
            return Some(font.clone());
        }
        let font = Arc::new(FontVec::try_from_vec_and_index(std::fs::read(&key.0).ok()?, key.1).ok()?);
        tracing::debug!(path = key.0, index = key.1, "loaded fallback font");
        cache.1.push((key, font.clone()));
        Some(font)
    });
    let font = font.filter(|f| f.glyph_id(c).0 != 0);
    cache.0.insert(c, font.clone());
    font
}

/// A glyph laid out: the font that draws it (`None`: the main font), its
/// id, the kerning before it and its advance.
type Laid = (Option<Arc<FontVec>>, GlyphId, f32, f32);

/// The family of a font description such as `org.gnome.desktop.interface
/// font-name`'s ("Cantarell 11", "Noto Sans Bold Italic 10", "Inter,
/// Sans 11"): its first family, without the style words and the size.
/// `None` when nothing is left.
pub fn font_family(description: &str) -> Option<String> {
    const STYLES: [&str; 26] = [
        "thin", "ultra-light", "extra-light", "light", "semi-light", "demi-light", "book", "regular", "normal", "medium", "semi-bold", "semibold", "demi-bold", "bold", "ultra-bold", "extra-bold", "heavy", "black", "ultra-heavy",
        "italic", "oblique", "condensed", "semi-condensed", "expanded", "semi-expanded", "small-caps",
    ];
    let first = description.split(',').next().unwrap_or("");
    let mut words: Vec<&str> = first.split_whitespace().collect();
    // The size ("11", "10.5", "12px"), when the description has one.
    if words.last().is_some_and(|w| w.trim_end_matches("px").parse::<f64>().is_ok()) {
        words.pop();
    }
    while words.len() > 1 && words.last().is_some_and(|w| STYLES.contains(&w.to_lowercase().as_str())) {
        words.pop();
    }
    let family = words.join(" ");
    (!family.is_empty()).then_some(family)
}

/// Size, color and clipping width for a run of text.
#[derive(Debug, Clone, Copy)]
pub struct Style {
    pub px: f32,
    pub color: u32,
    /// Text wider than this is shortened with an ellipsis.
    pub max_width: f32,
}

fn font_candidates() -> Vec<String> {
    let mut paths = Vec::new();
    if let Ok(out) = std::process::Command::new("fc-match")
        .args(["-f", "%{file}", "sans-serif"])
        .output()
    {
        if out.status.success() {
            paths.push(String::from_utf8_lossy(&out.stdout).into_owned());
        }
    }
    paths.extend(
        [
            "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
        ]
        .map(String::from),
    );
    paths
}

fn load_path(path: &str) -> Option<Text> {
    let bytes = std::fs::read(path).ok()?;
    let font = FontVec::try_from_vec(bytes).ok()?;
    tracing::debug!(path, "loaded font");
    Some(Text { font })
}

impl Text {
    /// Load the system sans-serif font, if one can be found.
    pub fn load() -> Option<Self> {
        font_candidates().into_iter().find_map(|path| load_path(&path))
    }

    /// Load the font file at `path`.
    pub fn load_file(path: &str) -> Option<Self> {
        load_path(path)
    }

    /// Load the font fontconfig picks for `pattern` (e.g.
    /// `Adwaita Sans:bold`), else the system sans-serif font.
    pub fn load_matching(pattern: &str) -> Option<Self> {
        let matched = std::process::Command::new("fc-match")
            .args(["-f", "%{file}", pattern])
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned());
        matched.and_then(|path| load_path(&path)).or_else(Self::load)
    }

    /// The interface font: `family` (from [`font_family`]; the system
    /// sans-serif without one) at `weight` (fontconfig's "bold", "light";
    /// regular without one). Falls back to the system font.
    pub fn load_family(family: Option<&str>, weight: Option<&str>) -> Option<Self> {
        let family = family.filter(|f| !f.trim().is_empty()).unwrap_or("sans-serif");
        // Fontconfig pattern syntax: escape what it treats as separators.
        let family: String = family.chars().flat_map(|c| if matches!(c, '-' | ':' | ',' | '\\') { vec!['\\', c] } else { vec![c] }).collect();
        match weight {
            Some(weight) => Self::load_matching(&format!("{family}:{weight}")),
            None => Self::load_matching(&family),
        }
    }

    /// Whether the font has a glyph for every character of `text`.
    pub fn covers(&self, text: &str) -> bool {
        text.chars().all(|c| self.font.glyph_id(c).0 != 0)
    }

    /// The font for `c`: `None` for the main font, else a fallback that
    /// has it.
    fn face_for(&self, c: char) -> Option<Arc<FontVec>> {
        if self.font.glyph_id(c).0 != 0 || c.is_whitespace() || c.is_control() {
            return None;
        }
        fallback_font(c)
    }

    /// Each character's glyph at `px` (pixels), kerned.
    fn layout(&self, text: &str, px: f32) -> Vec<Laid> {
        let scale = PxScale::from(px);
        let main = self.font.as_scaled(scale);
        let mut out = Vec::with_capacity(text.len());
        let mut prev: Option<GlyphId> = None;
        for c in text.chars() {
            match self.face_for(c) {
                None => {
                    let id = main.glyph_id(c);
                    let kern = prev.map_or(0.0, |p| main.kern(p, id));
                    out.push((None, id, kern, main.h_advance(id)));
                    prev = Some(id);
                }
                Some(font) => {
                    let scaled = font.as_scaled(scale);
                    let id = scaled.glyph_id(c);
                    let advance = scaled.h_advance(id);
                    out.push((Some(font.clone()), id, 0.0, advance));
                    prev = None;
                }
            }
        }
        out
    }

    /// The width of `text` at `px` (times the text scale, see
    /// [`theme::set_text_scale`]).
    pub fn width(&self, text: &str, px: f32) -> f32 {
        self.layout(text, px * theme::text_scale()).iter().map(|(_, _, kern, advance)| kern + advance).sum()
    }

    /// How far each character of `text` moves the pen at `px` (with its
    /// kerning; times the text scale): their sum is [`Text::width`].
    pub fn advances(&self, text: &str, px: f32) -> Vec<f32> {
        self.layout(text, px * theme::text_scale()).iter().map(|(_, _, kern, advance)| kern + advance).collect()
    }

    /// Height of the font's ascent at `px`, for vertical centering.
    pub fn ascent(&self, px: f32) -> f32 {
        self.font.as_scaled(PxScale::from(px * theme::text_scale())).ascent()
    }

    /// Draw `text` with its baseline at `y` (logical units; the glyphs are
    /// rasterized at the canvas's scale).
    ///
    /// Right-to-left text is drawn in visual order ([`bidi::visual`]). On a
    /// mirrored canvas the text lands at the mirrored place of its box
    /// (its end where its start would be) and reads normally.
    pub fn draw(&self, canvas: &mut Canvas, text: &str, x: f32, y: f32, style: Style) {
        let Style { px, color, max_width } = style;
        let color = text_color(color);
        let text = self.fit(text, px, max_width);
        let text = bidi::visual(&text);
        let x = if canvas.mirror { canvas.logical_width() as f32 - x - self.width(&text, px) } else { x };
        let scale = canvas.scale as f32;
        let (px, x, y) = (px * theme::text_scale() * scale, x * scale, y * scale);
        let mut caret = x;
        for (font, id, kern, advance) in self.layout(&text, px) {
            caret += kern;
            let glyph = id.with_scale_and_position(PxScale::from(px), point(caret, y));
            caret += advance;
            let outlined = match &font {
                None => self.font.outline_glyph(glyph),
                Some(font) => font.outline_glyph(glyph),
            };
            if let Some(outlined) = outlined {
                let bounds = outlined.px_bounds();
                outlined.draw(|gx, gy, cov| {
                    canvas.blend_raw(bounds.min.x as i32 + gx as i32, bounds.min.y as i32 + gy as i32, color, cov);
                });
            }
        }
    }

    /// Word-wrap `text` into at most `max_lines` lines of `max_width`; the
    /// last line is ellipsized if text remains. Lines break at spaces and
    /// between the characters of scripts written without spaces (see
    /// [`break_pieces`]); a word wider than a line is broken between its
    /// characters.
    pub fn wrap(&self, text: &str, px: f32, max_width: f32, max_lines: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        let mut current = String::new();
        let pieces = self.split_wide(break_pieces(text.trim()), px, max_width);
        let mut pieces = pieces.into_iter().peekable();
        for piece in pieces.by_ref() {
            let candidate = format!("{current}{piece}");
            if self.width(candidate.trim_end(), px) <= max_width || current.is_empty() {
                current = candidate;
                continue;
            }
            lines.push(std::mem::take(&mut current).trim_end().to_owned());
            current = piece;
            if lines.len() == max_lines {
                break;
            }
        }
        let current = current.trim_end().to_owned();
        if lines.len() < max_lines && !current.is_empty() {
            lines.push(current);
        } else if lines.len() == max_lines && (!current.is_empty() || pieces.peek().is_some()) {
            let last = lines.pop().unwrap_or_default();
            lines.push(self.fit(&format!("{last}…"), px, max_width));
        }
        lines.iter().map(|l| self.fit(l, px, max_width)).collect()
    }

    /// `pieces` with each one wider than `max_width` cut between its
    /// characters into pieces that fit (the trailing space stays with the
    /// last).
    fn split_wide(&self, pieces: Vec<String>, px: f32, max_width: f32) -> Vec<String> {
        let mut out = Vec::with_capacity(pieces.len());
        for piece in pieces {
            if self.width(piece.trim_end(), px) <= max_width {
                out.push(piece);
                continue;
            }
            let mut part = String::new();
            for c in piece.chars() {
                let candidate = format!("{part}{c}");
                if !part.is_empty() && !c.is_whitespace() && self.width(&candidate, px) > max_width {
                    out.push(std::mem::replace(&mut part, c.to_string()));
                } else {
                    part = candidate;
                }
            }
            if !part.is_empty() {
                out.push(part);
            }
        }
        out
    }

    fn fit(&self, text: &str, px: f32, max_width: f32) -> String {
        if self.width(text, px) <= max_width {
            return text.to_owned();
        }
        let mut s: String = text.to_owned();
        while !s.is_empty() && self.width(&format!("{s}…"), px) > max_width {
            s.pop();
        }
        format!("{s}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_families() {
        assert_eq!(font_family("Cantarell 11").as_deref(), Some("Cantarell"));
        assert_eq!(font_family("Noto Sans Bold Italic 10.5").as_deref(), Some("Noto Sans"));
        assert_eq!(font_family("Inter, Sans 11").as_deref(), Some("Inter"));
        assert_eq!(font_family("Ubuntu Sans").as_deref(), Some("Ubuntu Sans"));
        assert_eq!(font_family("Bold 11").as_deref(), Some("Bold"), "a family is never all style");
        assert_eq!(font_family(" 11"), None);
        assert_eq!(font_family(""), None);
    }

    #[test]
    fn high_contrast_colors() {
        // Dark backgrounds black, light text and borders white, alpha kept.
        assert_eq!(contrast_shape(0xF01E1E24), 0xF0000000);
        assert_eq!(contrast_shape(0xFF9A9AA2), 0xFFFFFFFF);
        assert_eq!(contrast_shape(0xFF45454D), 0xFF000000, "mid gray fills go dark");
        // The accent stays.
        let blue = crate::accent::Accent::Blue.palette().color;
        assert_eq!(contrast_shape(blue), blue);
        // Text is white, whatever gray it was.
        assert_eq!(contrast_text(0xFF9A9AA2), 0xFFFFFFFF);
        assert_eq!(contrast_text(0x80303030), 0x80FFFFFF);
        assert_eq!(contrast_text(0xFFE01B24), 0xFFE01B24, "warnings stay");
    }

    #[test]
    fn blend_and_rect() {
        let mut data = vec![0u8; 4 * 4 * 4];
        let mut canvas = Canvas::scaled(&mut data, 4, 4, 1.0);
        canvas.fill(0xFF000000);
        canvas.rect(1, 1, 2, 2, 0xFFFF0000);
        // Pixel (1,1) is opaque red: B=0, G=0, R=255, A=255.
        let i = (4 + 1) * 4;
        assert_eq!(&data[i..i + 4], &[0, 0, 255, 255]);
        // Pixel (0,0) stayed black.
        assert_eq!(&data[0..4], &[0, 0, 0, 255]);
    }

    #[test]
    fn scaled_rects() {
        // A 4×2 surface at 1.5: a 6×3 buffer.
        let mut data = vec![0u8; 6 * 3 * 4];
        let mut canvas = Canvas::scaled(&mut data, 4, 2, 1.5);
        assert_eq!((canvas.width, canvas.height), (6, 3));
        canvas.rect(2, 0, 2, 2, 0xFFFFFFFF);
        // Logical x 2..4 covers pixels 3..6 on every row.
        let lit: Vec<bool> = (0..6).map(|x| data[x * 4 + 3] == 255).collect();
        assert_eq!(lit, [false, false, false, true, true, true]);
        assert!(data[(2 * 6 + 5) * 4 + 3] == 255, "last row");
    }

    #[test]
    fn text_measures_and_ellipsizes() {
        let Some(text) = Text::load() else {
            eprintln!("no system font; skipping");
            return;
        };
        let short = text.width("Files", 15.0);
        let long = text.width("Files and Folders", 15.0);
        assert!(short > 0.0 && long > short);
        let lines = text.wrap("one two three four five six seven eight nine ten", 15.0, 90.0, 2);
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| text.width(l, 15.0) <= 90.0));
        assert!(lines[1].ends_with('…'), "{lines:?}");
        assert_eq!(text.wrap("short", 15.0, 200.0, 2), vec!["short".to_owned()]);
        let fitted = text.fit("A very long application name", 15.0, 80.0);
        assert!(fitted.ends_with('…'));
        assert!(text.width(&fitted, 15.0) <= 80.0);
    }
}

#[cfg(test)]
mod shape_tests {
    use super::*;

    #[test]
    fn status_shapes() {
        let mut data = vec![0u8; 20 * 20 * 4];
        let mut canvas = Canvas::scaled(&mut data, 20, 20, 1.0);
        canvas.status_shape(10.0, 10.0, 12.0, false, 0xFFFFFFFF);
        let px = |d: &[u8], x: usize, y: usize| d[(y * 20 + x) * 4 + 3];
        assert!(px(&data, 15, 10) > 40, "the ring's edge");
        assert_eq!(px(&data, 10, 10), 0, "a hollow ring");
        let mut data = vec![0u8; 20 * 20 * 4];
        let mut canvas = Canvas::scaled(&mut data, 20, 20, 1.0);
        canvas.status_shape(10.0, 10.0, 12.0, true, 0xFFFFFFFF);
        assert!(px(&data, 10, 10) > 200 && px(&data, 15, 10) == 0, "a bar");
    }

    #[test]
    fn mirrored_canvas() {
        // A 10×2 canvas: a rect at the left lands at the right.
        let mut data = vec![0u8; 10 * 2 * 4];
        let mut canvas = Canvas::scaled(&mut data, 10, 2, 1.0).mirrored(true);
        canvas.rect(0, 0, 3, 2, 0xFFFFFFFF);
        let lit: Vec<bool> = (0..10).map(|x| data[x * 4 + 3] == 255).collect();
        assert_eq!(lit, [false, false, false, false, false, false, false, true, true, true]);
        // Clipping follows the mirror.
        let mut data = vec![0u8; 10 * 2 * 4];
        let mut canvas = Canvas::scaled(&mut data, 10, 2, 1.0).mirrored(true);
        canvas.clip(0, 0, 2, 2);
        canvas.rect(0, 0, 10, 2, 0xFFFFFFFF);
        let lit: Vec<bool> = (0..10).map(|x| data[x * 4 + 3] == 255).collect();
        assert_eq!(lit, [false, false, false, false, false, false, false, false, true, true]);
        assert_eq!(Canvas::scaled(&mut [0u8; 40], 10, 1, 1.0).mirrored(true).place_x(2.0, 3.0), 5.0);
    }

    #[test]
    fn text_lands_mirrored_and_falls_back() {
        let Some(text) = Text::load() else { return };
        let ink = |data: &[u8], w: usize, x0: usize, x1: usize| (0..data.len() / 4).filter(|i| (x0..x1).contains(&(i % w)) && data[i * 4 + 3] > 0).count();
        let (w, h) = (200, 30);
        let style = Style { px: 16.0, color: 0xFFFFFFFF, max_width: f32::MAX };
        let mut data = vec![0u8; w * h * 4];
        let mut canvas = Canvas::scaled(&mut data, w, h, 1.0).mirrored(true);
        text.draw(&mut canvas, "Files", 4.0, 20.0, style);
        assert!(ink(&data, w, 0, 100) == 0 && ink(&data, w, 100, 200) > 0, "at the right end");
        // Hebrew letters, which the main font may lack, still draw.
        let mut data = vec![0u8; w * h * 4];
        let mut canvas = Canvas::scaled(&mut data, w, h, 1.0);
        text.draw(&mut canvas, "שלום", 4.0, 20.0, style);
        if text.width("שלום", 16.0) > 0.0 && fallback_font('ש').is_some() || text.covers("שלום") {
            assert!(ink(&data, w, 0, w) > 0);
        }
    }

    #[test]
    fn circles_and_lines() {
        let mut data = vec![0u8; 20 * 20 * 4];
        let mut canvas = Canvas::scaled(&mut data, 10, 10, 2.0);
        canvas.circle(5.0, 5.0, 3.0, 0xFFFFFFFF);
        let alpha = |data: &[u8], x: usize, y: usize| data[(y * 20 + x) * 4 + 3];
        assert_eq!(alpha(&data, 10, 10), 255, "center");
        assert_eq!(alpha(&data, 1, 1), 0, "corner");
        let mut data = vec![0u8; 20 * 20 * 4];
        let mut canvas = Canvas::scaled(&mut data, 10, 10, 2.0);
        canvas.line((2.0, 2.0), (8.0, 8.0), 1.0, 0xFFFFFFFF);
        assert_eq!(alpha(&data, 10, 10), 255, "on the diagonal");
        assert_eq!(alpha(&data, 16, 4), 0, "off it");
    }
}

/// A decoded image: premultiplied RGBA, row by row.
#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

impl Canvas<'_> {
    /// Draw `image` scaled into the logical rectangle (x, y, w, h)
    /// (bilinear sampling).
    pub fn image(&mut self, x: f64, y: f64, w: f64, h: f64, image: &Image) {
        self.image_alpha(x, y, w, h, image, 1.0);
    }

    /// Draw `image` to cover the logical rectangle (x, y, w, h) (cropped
    /// to its aspect, centered) inside corners of radius `r`, with an
    /// anti-aliased edge; `r` of half the size makes a circle.
    #[allow(clippy::too_many_arguments)]
    pub fn image_rounded(&mut self, x: f64, y: f64, w: f64, h: f64, r: f64, image: &Image, opacity: f32) {
        let x = self.place_x(x, w);
        if image.width == 0 || image.height == 0 || w <= 0.0 || h <= 0.0 {
            return;
        }
        // The part of the image shown: centered, the box's aspect.
        let (iw, ih) = (image.width as f64, image.height as f64);
        let k = (w / iw).max(h / ih);
        let (cw, ch) = (w / k, h / k);
        let (cx0, cy0) = ((iw - cw) / 2.0, (ih - ch) / 2.0);
        let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
        let (mx, my, hw, hh) = (x + w / 2.0, y + h / 2.0, w / 2.0 - r, h / 2.0 - r);
        let s = self.scale;
        let (px0, py0) = ((x * s).floor() as i32, (y * s).floor() as i32);
        let (px1, py1) = (((x + w) * s).ceil() as i32, ((y + h) * s).ceil() as i32);
        let texel = |ix: usize, iy: usize| -> [f32; 4] {
            let i = (iy * image.width + ix) * 4;
            let p = &image.rgba[i..i + 4];
            [f32::from(p[0]), f32::from(p[1]), f32::from(p[2]), f32::from(p[3])]
        };
        for py in py0.max(0)..py1.min(self.height as i32) {
            for px in px0.max(0)..px1.min(self.width as i32) {
                let (lx, ly) = ((f64::from(px) + 0.5) / s, (f64::from(py) + 0.5) / s);
                let (dx, dy) = ((lx - mx).abs() - hw, (ly - my).abs() - hh);
                let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt() + dx.max(dy).min(0.0);
                let cov = ((r - outside) * s + 0.5).clamp(0.0, 1.0) as f32 * opacity.clamp(0.0, 1.0);
                if cov <= 0.0 {
                    continue;
                }
                let sx = (cx0 + (lx - x) / k - 0.5).clamp(0.0, iw - 1.0);
                let sy = (cy0 + (ly - y) / k - 0.5).clamp(0.0, ih - 1.0);
                let (x0, y0) = (sx.floor() as usize, sy.floor() as usize);
                let (x1, y1) = ((x0 + 1).min(image.width - 1), (y0 + 1).min(image.height - 1));
                let (fx, fy) = ((sx - x0 as f64) as f32, (sy - y0 as f64) as f32);
                let mix = |a: [f32; 4], b: [f32; 4], t: f32| [0, 1, 2, 3].map(|k| a[k] + (b[k] - a[k]) * t);
                let [cr, cg, cb, ca] = mix(mix(texel(x0, y0), texel(x1, y0), fx), mix(texel(x0, y1), texel(x1, y1), fx), fy).map(|c| c * cov);
                if self.clipped(px, py) {
                    continue;
                }
                let i = (py as usize * self.width + px as usize) * 4;
                let dst = &mut self.data[i..i + 4];
                let inv = 1.0 - ca / 255.0;
                for (k, src) in [cb, cg, cr, ca].into_iter().enumerate() {
                    dst[k] = (src + f32::from(dst[k]) * inv).round().clamp(0.0, 255.0) as u8;
                }
            }
        }
    }

    /// An anti-aliased outline `width` thick just outside the rounded
    /// rectangle (x, y, w, h) of corner radius `r`.
    #[allow(clippy::too_many_arguments)]
    pub fn round_outline(&mut self, x: f64, y: f64, w: f64, h: f64, r: f64, width: f64, color: u32) {
        let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
        let (cx, cy, hw, hh) = (x + w / 2.0, y + h / 2.0, w / 2.0 - r, h / 2.0 - r);
        self.shape(x - width, y - width, x + w + width, y + h + width, |px, py| {
            let (dx, dy) = ((px - cx).abs() - hw, (py - cy).abs() - hh);
            let d = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt() + dx.max(dy).min(0.0) - r;
            // Inside the band 0..width beyond the edge.
            (d).min(width - d)
        }, color);
    }

    /// An arc of a circle of radius `r` around (cx, cy), `width` thick,
    /// from angle `start` sweeping `sweep` radians clockwise (0: to the
    /// right), with round ends.
    #[allow(clippy::too_many_arguments)]
    pub fn arc(&mut self, cx: f64, cy: f64, r: f64, width: f64, start: f64, sweep: f64, color: u32) {
        let half = width / 2.0;
        let tau = std::f64::consts::TAU;
        let sweep = sweep.clamp(0.0, tau);
        let end = |a: f64| (cx + r * a.cos(), cy + r * a.sin());
        let (a, b) = (end(start), end(start + sweep));
        self.shape(cx - r - half, cy - r - half, cx + r + half, cy + r + half, |x, y| {
            let angle = ((y - cy).atan2(x - cx) - start).rem_euclid(tau);
            let ring = half - (((x - cx).powi(2) + (y - cy).powi(2)).sqrt() - r).abs();
            if angle <= sweep {
                ring
            } else {
                // Round caps at both ends.
                let cap = |p: (f64, f64)| half - ((x - p.0).powi(2) + (y - p.1).powi(2)).sqrt();
                cap(a).max(cap(b))
            }
        }, color);
    }

    /// [`Canvas::image`] at `opacity` (0 to 1).
    pub fn image_alpha(&mut self, x: f64, y: f64, w: f64, h: f64, image: &Image, opacity: f32) {
        let x = self.place_x(x, w);
        let opacity = opacity.clamp(0.0, 1.0);
        if image.width == 0 || image.height == 0 || w <= 0.0 || h <= 0.0 || opacity <= 0.0 {
            return;
        }
        let s = self.scale;
        let (px0, py0) = ((x * s).round() as i32, (y * s).round() as i32);
        let (px1, py1) = (((x + w) * s).round() as i32, ((y + h) * s).round() as i32);
        let (dw, dh) = (f64::from((px1 - px0).max(1)), f64::from((py1 - py0).max(1)));
        let texel = |ix: usize, iy: usize| -> [f32; 4] {
            let i = (iy * image.width + ix) * 4;
            let p = &image.rgba[i..i + 4];
            [f32::from(p[0]), f32::from(p[1]), f32::from(p[2]), f32::from(p[3])]
        };
        for py in py0.max(0)..py1.min(self.height as i32) {
            for px in px0.max(0)..px1.min(self.width as i32) {
                // Source position of this pixel's center.
                let sx = ((f64::from(px - px0) + 0.5) / dw * image.width as f64 - 0.5).clamp(0.0, (image.width - 1) as f64);
                let sy = ((f64::from(py - py0) + 0.5) / dh * image.height as f64 - 0.5).clamp(0.0, (image.height - 1) as f64);
                let (x0, y0) = (sx.floor() as usize, sy.floor() as usize);
                let (x1, y1) = ((x0 + 1).min(image.width - 1), (y0 + 1).min(image.height - 1));
                let (fx, fy) = ((sx - x0 as f64) as f32, (sy - y0 as f64) as f32);
                let mix = |a: [f32; 4], b: [f32; 4], t: f32| [0, 1, 2, 3].map(|k| a[k] + (b[k] - a[k]) * t);
                let top = mix(texel(x0, y0), texel(x1, y0), fx);
                let bottom = mix(texel(x0, y1), texel(x1, y1), fx);
                let [r, g, b, a] = mix(top, bottom, fy).map(|c| c * opacity);
                if a <= 0.0 {
                    continue;
                }
                // Premultiplied source over the (premultiplied BGRA) pixel.
                if self.clipped(px, py) {
                    continue;
                }
                let i = (py as usize * self.width + px as usize) * 4;
                let dst = &mut self.data[i..i + 4];
                let inv = 1.0 - a / 255.0;
                for (k, src) in [b, g, r, a].into_iter().enumerate() {
                    dst[k] = (src + f32::from(dst[k]) * inv).round().clamp(0.0, 255.0) as u8;
                }
            }
        }
    }
}

/// A run of text with one style.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Span {
    pub text: String,
    pub bold: bool,
    pub underline: bool,
    /// A link's URI.
    pub link: Option<String>,
}

/// Fonts for rich text.
pub struct Fonts<'a> {
    pub regular: &'a Text,
    /// Falls back to the regular face.
    pub bold: Option<&'a Text>,
}

impl Fonts<'_> {
    fn face(&self, bold: bool) -> &Text {
        if bold {
            self.bold.unwrap_or(self.regular)
        } else {
            self.regular
        }
    }
}

/// A piece of a laid-out line: text, the span it came from, x and width.
#[derive(Debug, Clone, PartialEq)]
pub struct Piece {
    pub text: String,
    pub span: usize,
    pub x: f32,
    pub width: f32,
}

/// Whether `c` belongs to a script written without spaces between words
/// (Han ideographs, kana, CJK punctuation and full-width forms), so a
/// line may break before and after it. Hangul is written with spaces and
/// breaks at them, as Korean typesetting prefers.
fn breaks_around(c: char) -> bool {
    matches!(c as u32,
        0x2E80..=0x2FFF        // CJK radicals, Kangxi radicals
        | 0x3000..=0x303F      // CJK symbols and punctuation
        | 0x3040..=0x30FF      // Hiragana, Katakana
        | 0x3100..=0x31FF      // Bopomofo, Katakana extensions…
        | 0x3200..=0x33FF      // enclosed CJK letters, CJK compatibility
        | 0x3400..=0x4DBF      // CJK unified ideographs extension A
        | 0x4E00..=0x9FFF      // CJK unified ideographs
        | 0xF900..=0xFAFF      // CJK compatibility ideographs
        | 0xFE30..=0xFE4F      // CJK compatibility forms
        | 0xFF00..=0xFFEF      // half- and full-width forms
        | 0x20000..=0x3FFFF)   // ideographs of the supplementary planes
}

/// Characters a line must not start with (closing brackets and
/// punctuation, small kana, the prolonged sound mark): Japanese and
/// Chinese line-breaking rules (kinsoku), as in UAX #14.
fn no_break_before(c: char) -> bool {
    "、。，．,.：；:;！？!?)）]］}｝」』】〕〉》〙〗〛”’'\"%％‰ーぁぃぅぇぉっゃゅょゎゕゖァィゥェォッャュョヮヵヶ・…‥々〻ゝゞヽヾ゛゜°℃".contains(c)
}

/// Characters a line must not end with (opening brackets and quotes).
fn no_break_after(c: char) -> bool {
    "(（[［{｛「『【〔〈《〘〖〚“‘".contains(c)
}

/// `text` cut into the pieces between which a line may break: at spaces
/// (a piece keeps one space after it; runs of white space become one) and
/// before and after each character of a script written without spaces
/// (Chinese, Japanese), except where the line-breaking rules keep
/// punctuation with its neighbour. The pieces put together are `text`
/// with its white space collapsed.
pub fn break_pieces(text: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut current = String::new();
    let mut prev: Option<char> = None;
    for c in text.chars() {
        if c.is_whitespace() {
            if !current.ends_with(' ') {
                current.push(' ');
            }
            prev = None;
            continue;
        }
        let after_space = current.ends_with(' ');
        let between = prev.is_some_and(|p| (breaks_around(p) || breaks_around(c)) && !no_break_before(c) && !no_break_after(p));
        if !current.is_empty() && (after_space || between) {
            pieces.push(std::mem::take(&mut current));
        }
        current.push(c);
        prev = Some(c);
    }
    if !current.is_empty() {
        pieces.push(current);
    }
    pieces
}

/// Word-wrap `spans` into at most `max_lines` lines of `max_width`; the
/// last line ends in an ellipsis if text remains. Newlines break lines.
pub fn wrap_spans(fonts: &Fonts, spans: &[Span], px: f32, max_width: f32, max_lines: usize) -> Vec<Vec<Piece>> {
    // Pieces between which lines may break (words with the space after
    // them, single CJK characters: see `break_pieces`), with their span.
    let mut tokens: Vec<(String, usize, bool)> = Vec::new(); // (text, span, newline before)
    let mut newline = false;
    for (i, span) in spans.iter().enumerate() {
        let face = fonts.face(span.bold);
        for (n, part) in span.text.split('\n').enumerate() {
            if n > 0 {
                newline = true;
            }
            for piece in face.split_wide(break_pieces(part), px, max_width) {
                tokens.push((piece, i, newline));
                newline = false;
            }
        }
    }
    let mut lines: Vec<Vec<Piece>> = vec![Vec::new()];
    let mut x = 0.0f32;
    let mut truncated = false;
    for (text, span, break_before) in tokens {
        let face = fonts.face(spans[span].bold);
        let trimmed = text.trim_end();
        let width = face.width(&text, px);
        let fits = x + face.width(trimmed, px) <= max_width;
        if break_before || (!fits && x > 0.0) {
            if lines.len() == max_lines {
                truncated = true;
                break;
            }
            lines.push(Vec::new());
            x = 0.0;
        }
        let text = if x == 0.0 { text.trim_start().to_owned() } else { text };
        if text.is_empty() {
            continue;
        }
        let width = if x == 0.0 { face.width(&text, px) } else { width };
        lines.last_mut().unwrap().push(Piece { text, span, x, width });
        x += width;
    }
    // Shorten the last line to fit, with an ellipsis if text was cut.
    if let Some(last) = lines.last_mut() {
        let overflow = last.last().is_some_and(|p| p.x + p.width > max_width);
        if truncated || overflow {
            let ellipsis = "…";
            while let Some(piece) = last.last_mut() {
                let face = fonts.face(spans[piece.span].bold);
                let room = max_width - piece.x - face.width(ellipsis, px);
                if face.width(piece.text.trim_end(), px) <= room {
                    piece.text = format!("{}{ellipsis}", piece.text.trim_end());
                    piece.width = face.width(&piece.text, px);
                    break;
                }
                piece.text.pop();
                if piece.text.trim_end().is_empty() {
                    last.pop();
                    if last.is_empty() {
                        break;
                    }
                }
            }
        }
    }
    lines.retain(|l| !l.is_empty());
    lines
}

/// Where a link was drawn: logical rectangle (x, y, w, h) and URI.
pub type LinkArea = ((f32, f32, f32, f32), String);

/// Draw lines from [`wrap_spans`] at (x, first baseline), `line_height`
/// apart; links get `link_color` and an underline. Returns the links'
/// areas.
#[allow(clippy::too_many_arguments)]
pub fn draw_lines(
    canvas: &mut Canvas,
    fonts: &Fonts,
    spans: &[Span],
    lines: &[Vec<Piece>],
    (x, baseline): (f32, f32),
    px: f32,
    line_height: f32,
    (color, link_color): (u32, u32),
) -> Vec<LinkArea> {
    let mut links = Vec::new();
    for (row, line) in lines.iter().enumerate() {
        let base = baseline + row as f32 * line_height;
        // Words go right to left in a right-to-left line; a mirrored
        // canvas already reverses them, so a left-to-right line on it is
        // laid out reversed to read in order.
        let text: String = line.iter().map(|p| p.text.as_str()).collect();
        let flip = bidi::line_is_rtl(&text) != canvas.is_mirrored();
        let line_w = line.last().map_or(0.0, |p| p.x + p.width);
        for (i, piece) in line.iter().enumerate() {
            let span = &spans[piece.span];
            let face = fonts.face(span.bold);
            let c = if span.link.is_some() { link_color } else { color };
            // Underlines run across spaces between words of the same span.
            let continues = line.get(i + 1).is_some_and(|next| next.span == piece.span);
            let drawn = if continues { piece.width } else { face.width(piece.text.trim_end(), px) };
            let word = piece.text.trim_end();
            let tx = if flip { line_w - piece.x - face.width(word, px) } else { piece.x };
            face.draw(canvas, word, x + tx, base, Style { px, color: c, max_width: f32::MAX });
            let px_x = if flip { line_w - piece.x - drawn } else { piece.x };
            if span.underline || span.link.is_some() {
                canvas.rect((x + px_x) as i32, (base + 2.0) as i32, drawn.ceil() as i32, 1, c);
            }
            if let Some(uri) = &span.link {
                links.push(((x + px_x, base - px, drawn, line_height), uri.clone()));
            }
        }
    }
    links
}

#[cfg(test)]
mod rich_tests {
    use super::*;

    #[test]
    fn images_scale() {
        // A 2×1 image (red, blue) drawn 4×2: left half red, right half blue.
        let image = Image { width: 2, height: 1, rgba: vec![255, 0, 0, 255, 0, 0, 255, 255] };
        let mut data = vec![0u8; 4 * 2 * 4];
        let mut canvas = Canvas::scaled(&mut data, 4, 2, 1.0);
        canvas.image(0.0, 0.0, 4.0, 2.0, &image);
        assert_eq!(&data[0..4], &[0, 0, 255, 255], "red at the left (BGRA)");
        assert_eq!(&data[12..16], &[255, 0, 0, 255], "blue at the right");
    }

    #[test]
    fn wrapping_spans() {
        let Some(text) = Text::load() else { return };
        let fonts = Fonts { regular: &text, bold: None };
        let spans = vec![
            Span { text: "Hello ".into(), bold: true, ..Default::default() },
            Span { text: "world, this is a long message that wraps".into(), ..Default::default() },
        ];
        let lines = wrap_spans(&fonts, &spans, 14.0, 120.0, 2);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0][0].text, "Hello ");
        assert_eq!(lines[0][0].span, 0);
        assert!(lines[1].last().unwrap().text.ends_with('…'), "{lines:?}");
        for line in &lines {
            let end = line.last().unwrap();
            assert!(end.x + text.width(end.text.trim_end(), 14.0) <= 120.5, "{line:?}");
        }
        let breaks = wrap_spans(&fonts, &[Span { text: "one\ntwo".into(), ..Default::default() }], 14.0, 500.0, 3);
        assert_eq!(breaks.len(), 2, "{breaks:?}");
    }

    #[test]
    fn break_opportunities() {
        let p = |t: &str| break_pieces(t);
        assert_eq!(p("Hello  world\tagain"), ["Hello ", "world ", "again"]);
        assert_eq!(p(""), Vec::<String>::new());
        // Chinese and Japanese break between characters, but closing
        // punctuation stays with the character before it and opening
        // brackets with the one after (kinsoku).
        assert_eq!(p("网络已连接。"), ["网", "络", "已", "连", "接。"]);
        assert_eq!(p("（测试）"), ["（测", "试）"]);
        assert_eq!(p("「設定」を開く"), ["「設", "定」", "を", "開", "く"]);
        assert_eq!(p("ちょっと"), ["ちょっ", "と"], "small kana never start a line");
        assert_eq!(p("メニュー…"), ["メ", "ニュー…"]);
        // Latin words next to CJK stay whole; spaces still break.
        assert_eq!(p("Wi-Fi 网络"), ["Wi-Fi ", "网", "络"]);
        assert_eq!(p("打开Helix设置"), ["打", "开", "Helix", "设", "置"]);
        // Korean breaks at its spaces.
        assert_eq!(p("한국어 텍스트입니다"), ["한국어 ", "텍스트입니다"]);
        // Put back together: the text with its white space collapsed.
        let text = "通知  已经\n清除，请稍后再试。";
        assert_eq!(p(text).concat(), "通知 已经 清除，请稍后再试。");
    }

    /// Text without spaces (Chinese, Japanese) and words wider than the
    /// line wrap instead of being cut off with an ellipsis.
    #[test]
    fn wrapping_cjk() {
        let Some(text) = Text::load() else { return };
        if fallback_font('网').is_none() && !text.covers("网") {
            eprintln!("no CJK font installed (fonts-noto-cjk); skipped");
            return;
        }
        let sentence = "无法连接到网络，请检查您的网络设置后再试一次。";
        let width = text.width("无法连接到网", 14.0) + 1.0;
        let lines = text.wrap(sentence, 14.0, width, 10);
        assert!(lines.len() >= 3, "{lines:?}");
        assert_eq!(lines.concat(), sentence, "nothing lost or ellipsized");
        for line in &lines {
            assert!(text.width(line, 14.0) <= width, "{line:?} wider than {width}");
            assert!(!line.starts_with(['，', '。']), "{line:?} starts with closing punctuation");
        }
        // Cut to two lines: the second ends in an ellipsis.
        let two = text.wrap(sentence, 14.0, width, 2);
        assert_eq!(two.len(), 2);
        assert!(two[1].ends_with('…'), "{two:?}");

        let japanese = "このアプリケーションは応答していません";
        let lines = text.wrap(japanese, 14.0, text.width("このアプリケ", 14.0) + 1.0, 10);
        assert!(lines.len() >= 3, "{lines:?}");
        assert_eq!(lines.concat(), japanese);

        let fonts = Fonts { regular: &text, bold: Some(&text) };
        let spans = vec![
            Span { text: "微信 ".into(), bold: true, ..Default::default() },
            Span { text: sentence.into(), ..Default::default() },
        ];
        let lines = wrap_spans(&fonts, &spans, 14.0, width, 10);
        assert!(lines.len() >= 3, "{lines:?}");
        let joined: String = lines.iter().flatten().map(|p| p.text.as_str()).collect();
        assert_eq!(joined, format!("微信 {sentence}"));
        for line in &lines {
            let end = line.last().unwrap();
            assert!(end.x + text.width(end.text.trim_end(), 14.0) <= width + 0.5, "{line:?}");
        }
    }

    #[test]
    fn wrapping_long_words() {
        let Some(text) = Text::load() else { return };
        let word = "Bildschirmfreigabeberechtigungseinstellungen";
        let width = text.width("Bildschirm", 14.0);
        let lines = text.wrap(&format!("Die {word} öffnen"), 14.0, width, 10);
        assert!(lines.iter().all(|l| text.width(l, 14.0) <= width), "{lines:?}");
        assert_eq!(lines.concat().replace(' ', ""), format!("Die{word}öffnen"));
        assert_eq!(lines[0], "Die", "a word that does not fit starts a new line");
    }
}
