//! Drawing into ARGB8888 shm buffers: rectangles and anti-aliased text.
//!
//! Text uses the system's sans-serif font (found through fontconfig's
//! `fc-match`), rasterized with `ab_glyph`.

use ab_glyph::{point, Font, FontVec, PxScale, ScaleFont};

/// An ARGB8888 (premultiplied, little-endian) pixel buffer.
pub struct Canvas<'a> {
    pub data: &'a mut [u8],
    pub width: usize,
    pub height: usize,
}

/// Colors are `0xAARRGGBB`, not premultiplied.
pub fn rgba(color: u32) -> [f32; 4] {
    let c = |shift: u32| ((color >> shift) & 0xff) as f32 / 255.0;
    [c(16), c(8), c(0), c(24)]
}

impl Canvas<'_> {
    /// Blend `color` over the pixel at (x, y) with extra coverage `cov`.
    pub fn blend(&mut self, x: i32, y: i32, color: u32, cov: f32) {
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

    pub fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        for yy in y.max(0)..(y + h).min(self.height as i32) {
            for xx in x.max(0)..(x + w).min(self.width as i32) {
                self.blend(xx, yy, color, 1.0);
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

    /// Draw `text` with its baseline at `y`.
    pub fn draw(&self, canvas: &mut Canvas, text: &str, x: f32, y: f32, style: Style) {
        let Style { px, color, max_width } = style;
        let text = self.fit(text, px, max_width);
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
                    canvas.blend(bounds.min.x as i32 + gx as i32, bounds.min.y as i32 + gy as i32, color, cov);
                });
            }
        }
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
        let mut canvas = Canvas { data: &mut data, width: 4, height: 4 };
        canvas.fill(0xFF000000);
        canvas.rect(1, 1, 2, 2, 0xFFFF0000);
        // Pixel (1,1) is opaque red: B=0, G=0, R=255, A=255.
        let i = (4 + 1) * 4;
        assert_eq!(&data[i..i + 4], &[0, 0, 255, 255]);
        // Pixel (0,0) stayed black.
        assert_eq!(&data[0..4], &[0, 0, 0, 255]);
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
        let fitted = text.fit("A very long application name", 15.0, 80.0);
        assert!(fitted.ends_with('…'));
        assert!(text.width(&fitted, 15.0) <= 80.0);
    }
}
