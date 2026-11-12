//! Drawing into ARGB8888 buffers: rectangles, simple anti-aliased shapes
//! and text. Used by the shell (its surfaces) and the compositor (window
//! frames).
//!
//! Text uses the system's fonts (found through fontconfig's `fc-match`),
//! rasterized with `ab_glyph`.

use ab_glyph::{point, Font, FontVec, PxScale, ScaleFont};

/// An ARGB8888 (premultiplied, little-endian) pixel buffer. Drawing takes
/// logical coordinates (surface-local); `scale` turns them into the
/// buffer's pixels.
pub struct Canvas<'a> {
    pub data: &'a mut [u8],
    /// Buffer size in pixels.
    pub width: usize,
    pub height: usize,
    pub scale: f64,
}

/// Pixels for `logical` units at `scale` (rounded, as the fractional-scale
/// protocol asks of buffer sizes).
pub fn physical(logical: usize, scale: f64) -> usize {
    (logical as f64 * scale).round() as usize
}

static HIGH_CONTRAST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// High contrast (helix-desktop-spec `a11y-interface.md`): text white,
/// dark shapes black and light ones white, saturated colors kept.
pub fn set_high_contrast(on: bool) {
    HIGH_CONTRAST.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub fn high_contrast() -> bool {
    HIGH_CONTRAST.load(std::sync::atomic::Ordering::Relaxed)
}

static STATUS_SHAPES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `show-status-shapes` (helix-desktop-spec `a11y-interface.md`): on/off
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
        Self { data, width: physical(width, scale), height: physical(height, scale), scale }
    }

    /// Blend `color` over the buffer pixel at (x, y) with extra coverage `cov`.
    pub fn blend_px(&mut self, x: i32, y: i32, color: u32, cov: f32) {
        if x < 0 || y < 0 || x as usize >= self.width || y as usize >= self.height {
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

    /// Whether the font has a glyph for every character of `text`.
    pub fn covers(&self, text: &str) -> bool {
        text.chars().all(|c| self.font.glyph_id(c).0 != 0)
    }

    pub fn width(&self, text: &str, px: f32) -> f32 {
        let font = self.font.as_scaled(PxScale::from(px));
        let mut width = 0.0;
        let mut prev = None;
        for c in text.chars() {
            let id = font.glyph_id(c);
            if let Some(p) = prev {
                width += font.kern(p, id);
            }
            width += font.h_advance(id);
            prev = Some(id);
        }
        width
    }

    /// Height of the font's ascent at `px`, for vertical centering.
    pub fn ascent(&self, px: f32) -> f32 {
        self.font.as_scaled(PxScale::from(px)).ascent()
    }

    /// Draw `text` with its baseline at `y` (logical units; the glyphs are
    /// rasterized at the canvas's scale).
    pub fn draw(&self, canvas: &mut Canvas, text: &str, x: f32, y: f32, style: Style) {
        let Style { px, color, max_width } = style;
        let color = text_color(color);
        let text = self.fit(text, px, max_width);
        let scale = canvas.scale as f32;
        let (px, x, y) = (px * scale, x * scale, y * scale);
        let font = self.font.as_scaled(PxScale::from(px));
        let mut caret = x;
        let mut prev = None;
        for c in text.chars() {
            let id = font.glyph_id(c);
            if let Some(p) = prev {
                caret += font.kern(p, id);
            }
            let glyph = id.with_scale_and_position(PxScale::from(px), point(caret, y));
            caret += font.h_advance(id);
            prev = Some(id);
            if let Some(outlined) = self.font.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                outlined.draw(|gx, gy, cov| {
                    canvas.blend_px(bounds.min.x as i32 + gx as i32, bounds.min.y as i32 + gy as i32, color, cov);
                });
            }
        }
    }

    /// Word-wrap `text` into at most `max_lines` lines of `max_width`; the
    /// last line is ellipsized if text remains.
    pub fn wrap(&self, text: &str, px: f32, max_width: f32, max_lines: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut words = text.split_whitespace().peekable();
        for word in words.by_ref() {
            let candidate = if current.is_empty() { word.to_owned() } else { format!("{current} {word}") };
            if self.width(&candidate, px) <= max_width || current.is_empty() {
                current = candidate;
                continue;
            }
            lines.push(std::mem::take(&mut current));
            current = word.to_owned();
            if lines.len() == max_lines {
                break;
            }
        }
        if lines.len() < max_lines && !current.is_empty() {
            lines.push(current);
        } else if lines.len() == max_lines && (!current.is_empty() || words.peek().is_some()) {
            let last = lines.pop().unwrap_or_default();
            lines.push(self.fit(&format!("{last}…"), px, max_width));
        }
        lines.iter().map(|l| self.fit(l, px, max_width)).collect()
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
    fn high_contrast_colors() {
        // Dark backgrounds black, light text and borders white, alpha kept.
        assert_eq!(contrast_shape(0xF01E1E24), 0xF0000000);
        assert_eq!(contrast_shape(0xFF9A9AA2), 0xFFFFFFFF);
        assert_eq!(contrast_shape(0xFF45454D), 0xFF000000, "mid gray fills go dark");
        // The accent stays.
        assert_eq!(contrast_shape(0xFF3584E4), 0xFF3584E4);
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
        if image.width == 0 || image.height == 0 || w <= 0.0 || h <= 0.0 {
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
                let [r, g, b, a] = mix(top, bottom, fy);
                if a <= 0.0 {
                    continue;
                }
                // Premultiplied source over the (premultiplied BGRA) pixel.
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

/// Word-wrap `spans` into at most `max_lines` lines of `max_width`; the
/// last line ends in an ellipsis if text remains. Newlines break lines.
pub fn wrap_spans(fonts: &Fonts, spans: &[Span], px: f32, max_width: f32, max_lines: usize) -> Vec<Vec<Piece>> {
    // Words and the spaces before them, with their span.
    let mut tokens: Vec<(String, usize, bool)> = Vec::new(); // (text, span, newline before)
    let mut newline = false;
    for (i, span) in spans.iter().enumerate() {
        let mut word = String::new();
        for c in span.text.chars() {
            if c == '\n' {
                if !word.is_empty() {
                    tokens.push((std::mem::take(&mut word), i, newline));
                }
                newline = true;
            } else if c.is_whitespace() {
                word.push(' ');
                tokens.push((std::mem::take(&mut word), i, newline));
                newline = false;
            } else {
                word.push(c);
            }
        }
        if !word.is_empty() {
            tokens.push((word, i, newline));
            newline = false;
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
        for (i, piece) in line.iter().enumerate() {
            let span = &spans[piece.span];
            let face = fonts.face(span.bold);
            let c = if span.link.is_some() { link_color } else { color };
            face.draw(canvas, &piece.text, x + piece.x, base, Style { px, color: c, max_width: f32::MAX });
            // Underlines run across spaces between words of the same span.
            let continues = line.get(i + 1).is_some_and(|next| next.span == piece.span);
            let drawn = if continues { piece.width } else { face.width(piece.text.trim_end(), px) };
            if span.underline || span.link.is_some() {
                canvas.rect((x + piece.x) as i32, (base + 2.0) as i32, drawn.ceil() as i32, 1, c);
            }
            if let Some(uri) = &span.link {
                links.push(((x + piece.x, base - px, drawn, line_height), uri.clone()));
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
}
