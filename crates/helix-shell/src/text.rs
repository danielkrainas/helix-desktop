//! Drawing into ARGB8888 shm buffers: rectangles and anti-aliased text.
//!
//! Text uses the system's sans-serif font (found through fontconfig's
//! `fc-match`), rasterized with `ab_glyph`.

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
        let [r, g, b, a] = rgba(color);
        let px = [(b * a * 255.0) as u8, (g * a * 255.0) as u8, (r * a * 255.0) as u8, (a * 255.0) as u8];
        for chunk in self.data.as_chunks_mut::<4>().0 {
            *chunk = px;
        }
    }

    /// Fill a rectangle given in logical units.
    pub fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
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

impl Text {
    /// Load the system sans-serif font, if one can be found.
    pub fn load() -> Option<Self> {
        font_candidates().into_iter().find_map(|path| {
            let bytes = std::fs::read(&path).ok()?;
            let font = FontVec::try_from_vec(bytes).ok()?;
            tracing::debug!(path, "loaded font");
            Some(Self { font })
        })
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
