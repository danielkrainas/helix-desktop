//! The background of the lock and login screens (helix-desktop-spec
//! `contracts/gsettings/background.md`, "Lock screen"): the picture
//! blurred and toned so that text drawn on it stays readable.
//!
//! The picture is shrunk, blurred (three box passes, close to a Gaussian)
//! and toned while small, then stretched back (bilinear, dithered). Toning
//! caps each pixel's luminance: lower in the middle column, where the text
//! is, than at the edges, so light text on it keeps its contrast whatever
//! the picture.

/// How the picture is blurred and toned.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tone {
    /// Blur radius, logical units.
    pub blur: f64,
    /// How much of the colors' saturation is kept (1: all).
    pub saturation: f32,
    /// Overall brightness factor (on linear light).
    pub dim: f32,
    /// The highest luminance (linear, 0 to 1) a pixel keeps in the middle
    /// column and at the left and right edges.
    pub max_center: f32,
    pub max_edge: f32,
}

/// The lock screen's tone. Text in [`TEXT`] on it reaches 4.5:1 in the
/// middle column; text in the regular foreground (#EDEDEF) reaches it
/// everywhere.
pub const LOCK: Tone = Tone { blur: 48.0, saturation: 0.85, dim: 0.8, max_center: 0.075, max_edge: 0.14 };

/// The dimmest text color meant for the lock background's middle column.
pub const TEXT: u32 = 0xFFC4C4CC;

fn to_linear(c: f32) -> f32 {
    let c = c / 255.0;
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

fn to_srgb(l: f32) -> f32 {
    let l = l.clamp(0.0, 1.0);
    255.0 * if l <= 0.003_130_8 { l * 12.92 } else { 1.055 * l.powf(1.0 / 2.4) - 0.055 }
}

/// The luminance a pixel ends with: `y` unchanged below the knee, then
/// approaching `max` without reaching it.
fn compress(y: f32, max: f32) -> f32 {
    let knee = 0.6 * max;
    if y <= knee {
        y
    } else {
        knee + (max - knee) * (1.0 - (-(y - knee) / (max - knee)).exp())
    }
}

fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// One box-blur pass of radius `r` along rows (`stride` 1) or columns.
fn box_pass(src: &[[f32; 3]], dst: &mut [[f32; 3]], w: usize, h: usize, r: usize, horizontal: bool) {
    let (lines, len) = if horizontal { (h, w) } else { (w, h) };
    let at = |line: usize, i: usize| if horizontal { line * w + i } else { i * w + line };
    let norm = 1.0 / (2 * r + 1) as f32;
    for line in 0..lines {
        let px = |i: isize| src[at(line, i.clamp(0, len as isize - 1) as usize)];
        let mut sum = [0.0f32; 3];
        for i in -(r as isize)..=(r as isize) {
            let p = px(i);
            for k in 0..3 {
                sum[k] += p[k];
            }
        }
        for i in 0..len {
            dst[at(line, i)] = sum.map(|s| s * norm);
            let (add, sub) = (px(i as isize + r as isize + 1), px(i as isize - r as isize));
            for k in 0..3 {
                sum[k] += add[k] - sub[k];
            }
        }
    }
}

/// Blur and tone the opaque ARGB8888 buffer `buf` (`pw`×`ph` pixels of a
/// surface at `scale`) in place.
pub fn apply(buf: &mut [u8], pw: usize, ph: usize, scale: f64, tone: &Tone) {
    if pw == 0 || ph == 0 {
        return;
    }
    let radius = (tone.blur * scale).max(1.0);
    // Shrunk by f, where the blur needs few taps.
    let f = ((radius / 6.0).round() as usize).clamp(1, 16);
    let (sw, sh) = (pw.div_ceil(f), ph.div_ceil(f));
    let mut small = vec![[0.0f32; 3]; sw * sh];
    for sy in 0..sh {
        for sx in 0..sw {
            let (x0, y0) = (sx * f, sy * f);
            let (x1, y1) = ((x0 + f).min(pw), (y0 + f).min(ph));
            let mut sum = [0.0f32; 3];
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = (y * pw + x) * 4;
                    // B, G, R in memory.
                    sum[0] += f32::from(buf[i + 2]);
                    sum[1] += f32::from(buf[i + 1]);
                    sum[2] += f32::from(buf[i]);
                }
            }
            let n = ((x1 - x0) * (y1 - y0)) as f32;
            small[sy * sw + sx] = sum.map(|s| s / n);
        }
    }
    // Three box passes each way: about a Gaussian of σ = radius / 2.
    let r = ((radius / 2.0 / f as f64).round() as usize).max(1);
    let mut tmp = vec![[0.0f32; 3]; sw * sh];
    for _ in 0..3 {
        box_pass(&small, &mut tmp, sw, sh, r, true);
        box_pass(&tmp, &mut small, sw, sh, r, false);
    }
    // Tone, while small: the picture changes slowly at this size.
    for sy in 0..sh {
        for sx in 0..sw {
            let p = &mut small[sy * sw + sx];
            let gray = 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2];
            let lin = [0, 1, 2].map(|k| to_linear(gray + (p[k] - gray) * tone.saturation) * tone.dim);
            let y = 0.2126 * lin[0] + 0.7152 * lin[1] + 0.0722 * lin[2];
            // 0 in the middle column, 1 at the left and right edges.
            let u = (((sx as f32 + 0.5) / sw as f32) - 0.5).abs() * 2.0;
            let max = tone.max_center + (tone.max_edge - tone.max_center) * smoothstep(0.35, 0.9, u);
            let k = if y > 0.0 { compress(y, max) / y } else { 1.0 };
            *p = lin.map(|c| to_srgb(c * k));
        }
    }
    // Back to full size: bilinear, with a little ordered dither against
    // banding in the smooth gradients.
    const BAYER: [[f32; 4]; 4] = [[0.0, 8.0, 2.0, 10.0], [12.0, 4.0, 14.0, 6.0], [3.0, 11.0, 1.0, 9.0], [15.0, 7.0, 13.0, 5.0]];
    let coord = |v: usize, n: usize| {
        let s = ((v as f32 + 0.5) / f as f32 - 0.5).clamp(0.0, (n - 1) as f32);
        let i = (s as usize).min(n - 1);
        (i, (i + 1).min(n - 1), s - i as f32)
    };
    let xs: Vec<(usize, usize, f32)> = (0..pw).map(|x| coord(x, sw)).collect();
    for y in 0..ph {
        let (y0, y1, fy) = coord(y, sh);
        let (row0, row1) = (&small[y0 * sw..(y0 + 1) * sw], &small[y1 * sw..(y1 + 1) * sw]);
        for (x, &(x0, x1, fx)) in xs.iter().enumerate() {
            let d = BAYER[y % 4][x % 4] / 16.0 - 0.47;
            let i = (y * pw + x) * 4;
            for k in 0..3 {
                let top = row0[x0][k] + (row0[x1][k] - row0[x0][k]) * fx;
                let bottom = row1[x0][k] + (row1[x1][k] - row1[x0][k]) * fx;
                let v = (top + (bottom - top) * fy + d).floor().clamp(0.0, 255.0) as u8;
                buf[i + 2 - k] = v;
            }
            buf[i + 3] = 255;
        }
    }
}

/// The high-contrast background: black.
pub fn high_contrast(buf: &mut [u8]) {
    for px in buf.as_chunks_mut::<4>().0 {
        *px = [0, 0, 0, 255];
    }
}

/// WCAG contrast ratio between two opaque colors (`0xAARRGGBB`).
pub fn contrast(a: u32, b: u32) -> f32 {
    let lum = |c: u32| {
        let ch = |shift: u32| to_linear(((c >> shift) & 0xFF) as f32);
        0.2126 * ch(16) + 0.7152 * ch(8) + 0.0722 * ch(0)
    };
    let (la, lb) = (lum(a), lum(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled(pw: usize, ph: usize, f: impl Fn(usize, usize) -> [u8; 3]) -> Vec<u8> {
        let mut buf = vec![0u8; pw * ph * 4];
        for y in 0..ph {
            for x in 0..pw {
                let [r, g, b] = f(x, y);
                buf[(y * pw + x) * 4..][..4].copy_from_slice(&[b, g, r, 255]);
            }
        }
        buf
    }

    fn pixel(buf: &[u8], pw: usize, x: usize, y: usize) -> u32 {
        let i = (y * pw + x) * 4;
        0xFF00_0000 | u32::from(buf[i + 2]) << 16 | u32::from(buf[i + 1]) << 8 | u32::from(buf[i])
    }

    #[test]
    fn white_is_toned_for_text() {
        let (pw, ph) = (400, 200);
        let mut buf = filled(pw, ph, |_, _| [255, 255, 255]);
        apply(&mut buf, pw, ph, 1.0, &LOCK);
        let middle = pixel(&buf, pw, pw / 2, ph / 2);
        assert!(contrast(TEXT, middle) >= 4.5, "{middle:08x}: {}", contrast(TEXT, middle));
        assert!(contrast(0xFFEDEDEF, middle) >= 4.5);
        let edge = pixel(&buf, pw, 2, ph / 2);
        assert!(contrast(0xFFEDEDEF, edge) >= 4.5, "{edge:08x}");
        assert!(edge > middle, "the edges are lighter than the middle");
        assert!(buf.as_chunks::<4>().0.iter().all(|p| p[3] == 255), "opaque");
    }

    #[test]
    fn blurs_edges_away() {
        // Black and white halves: the seam becomes a gradient.
        let (pw, ph) = (200, 40);
        let mut buf = filled(pw, ph, |x, _| if x < pw / 2 { [0, 0, 0] } else { [255, 255, 255] });
        // (Luminance caps far above white: untoned.)
        apply(&mut buf, pw, ph, 1.0, &Tone { blur: 20.0, saturation: 1.0, dim: 1.0, max_center: 100.0, max_edge: 100.0 });
        let row: Vec<u8> = (0..pw).map(|x| buf[(20 * pw + x) * 4]).collect();
        assert!(row[pw / 2 - 1] > 40 && row[pw / 2] < 215, "{:?}", &row[90..110]);
        assert!(row[0] < 4 && row[pw - 1] > 250);
        assert!(row.windows(2).all(|w| i16::from(w[1]) + 2 >= i16::from(w[0])), "rises steadily");
    }

    /// How long a 4K monitor's background takes (run with `--release
    /// --ignored --nocapture`).
    #[test]
    #[ignore]
    fn time_4k() {
        let (pw, ph) = (3840, 2160);
        let mut buf = filled(pw, ph, |x, y| [(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8]);
        let start = std::time::Instant::now();
        apply(&mut buf, pw, ph, 2.0, &LOCK);
        eprintln!("3840x2160 at scale 2: {:?}", start.elapsed());
    }

    #[test]
    fn dark_pictures_keep_their_colors() {
        let (pw, ph) = (64, 64);
        let mut buf = filled(pw, ph, |_, _| [40, 60, 90]);
        apply(&mut buf, pw, ph, 1.0, &LOCK);
        let p = pixel(&buf, pw, 32, 32);
        let (r, b) = ((p >> 16) & 0xFF, p & 0xFF);
        assert!(b > r + 20, "still blue: {p:08x}");
    }
}
