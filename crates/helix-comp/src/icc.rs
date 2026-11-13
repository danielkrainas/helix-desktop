//! ICC profiles for image descriptions (helix-desktop-spec
//! `contracts/wayland/README.md`, "Color management", "ICC profiles"): RGB
//! profiles to primaries and per-channel curves. Matrix/curve profiles are
//! read as they are; LUT-based ones (`A2B0`: `mft1`, `mft2`, `mAB `) are
//! evaluated to find their primaries, white and curves.

/// A curve in ICC's parametric form (type 4): `Y = (aX + b)^g + e` for
/// `X >= d`, else `cX + f`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Para {
    pub g: f64,
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
}

impl Para {
    pub fn power(g: f64) -> Self {
        Self { g, a: 1.0, b: 0.0, c: 0.0, d: 0.0, e: 0.0, f: 0.0 }
    }

    pub fn eval(&self, x: f64) -> f64 {
        if x >= self.d {
            (self.a * x + self.b).max(0.0).powf(self.g) + self.e
        } else {
            self.c * x + self.f
        }
    }
}

/// What a profile gives: xy chromaticities of red, green, blue and white,
/// and each channel's curve.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub primaries: [f64; 8],
    pub curves: [Para; 3],
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn u8(&self, at: usize) -> Option<u8> {
        self.0.get(at).copied()
    }
    fn u16(&self, at: usize) -> Option<u16> {
        self.0.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&self, at: usize) -> Option<u32> {
        self.0.get(at..at + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn s15(&self, at: usize) -> Option<f64> {
        self.u32(at).map(|v| f64::from(v as i32) / 65536.0)
    }
    fn sig(&self, at: usize) -> Option<&[u8]> {
        self.0.get(at..at + 4)
    }
}

/// A sampled curve (0–1 in, 0–1 out).
fn table(values: &[f64], x: f64) -> f64 {
    let n = values.len();
    if n == 0 {
        return x;
    }
    if n == 1 {
        return values[0];
    }
    let pos = x.clamp(0.0, 1.0) * (n - 1) as f64;
    let (i, f) = (pos.floor() as usize, pos.fract());
    values[i] * (1.0 - f) + values[(i + 1).min(n - 1)] * f
}

/// A `curv` or `para` element at `at`: as a function, and its size in
/// bytes (padded to 4).
enum Curve {
    Para(Para),
    Table(Vec<f64>),
}

impl Curve {
    fn eval(&self, x: f64) -> f64 {
        match self {
            Curve::Para(p) => p.eval(x),
            Curve::Table(t) => table(t, x),
        }
    }
}

fn curve(r: &Reader, at: usize) -> Option<(Curve, usize)> {
    match r.sig(at)? {
        b"curv" => {
            let n = r.u32(at + 8)? as usize;
            let size = (12 + 2 * n + 3) & !3;
            Some(match n {
                0 => (Curve::Para(Para::power(1.0)), size),
                1 => (Curve::Para(Para::power(f64::from(r.u16(at + 12)?) / 256.0)), size),
                _ => (Curve::Table((0..n).map(|i| r.u16(at + 12 + 2 * i).map(|v| f64::from(v) / 65535.0)).collect::<Option<Vec<_>>>()?), size),
            })
        }
        b"para" => {
            let kind = r.u16(at + 8)?;
            let count = [1, 3, 4, 5, 7].get(usize::from(kind)).copied()?;
            let p: Vec<f64> = (0..count).map(|i| r.s15(at + 12 + 4 * i)).collect::<Option<_>>()?;
            let para = match kind {
                0 => Para::power(p[0]),
                1 => Para { g: p[0], a: p[1], b: p[2], c: 0.0, d: -p[2] / p[1], e: 0.0, f: 0.0 },
                2 => Para { g: p[0], a: p[1], b: p[2], c: 0.0, d: -p[2] / p[1], e: p[3], f: p[3] },
                3 => Para { g: p[0], a: p[1], b: p[2], c: p[3], d: p[4], e: 0.0, f: 0.0 },
                _ => Para { g: p[0], a: p[1], b: p[2], c: p[3], d: p[4], e: p[5], f: p[6] },
            };
            Some((Curve::Para(para), (12 + 4 * count + 3) & !3))
        }
        _ => None,
    }
}

/// A sampled curve as a power: the one through its value at mid-gray.
pub fn fit_power(f: impl Fn(f64) -> f64) -> Para {
    let mid = f(0.5);
    if mid <= 0.0 || mid >= 1.0 {
        return Para::power(1.0);
    }
    Para::power((mid.ln() / 0.5f64.ln()).clamp(0.1, 10.0))
}

/// CIE Lab (D50) to XYZ.
fn lab_to_xyz(l: f64, a: f64, b: f64) -> [f64; 3] {
    let (xn, yn, zn) = (0.9642, 1.0, 0.8249);
    let fy = (l + 16.0) / 116.0;
    let (fx, fz) = (fy + a / 500.0, fy - b / 200.0);
    let finv = |t: f64| if t > 6.0 / 29.0 { t * t * t } else { 3.0 * (6.0f64 / 29.0).powi(2) * (t - 4.0 / 29.0) };
    [xn * finv(fx), yn * finv(fy), zn * finv(fz)]
}

/// A colour lookup table: `grid` points per input, `out` outputs, values
/// 0–1, read by `value(index)`.
fn clut(grid: &[usize; 3], out: usize, value: &dyn Fn(usize) -> Option<f64>, rgb: [f64; 3]) -> Option<Vec<f64>> {
    // Trilinear interpolation.
    let pos: Vec<(usize, usize, f64)> = (0..3)
        .map(|k| {
            let n = grid[k].max(1);
            let p = rgb[k].clamp(0.0, 1.0) * (n - 1) as f64;
            let i = p.floor() as usize;
            (i.min(n - 1), (i + 1).min(n - 1), p - i as f64)
        })
        .collect();
    let index = |r: usize, g: usize, b: usize| ((r * grid[1] + g) * grid[2] + b) * out;
    let mut result = vec![0.0; out];
    for corner in 0..8 {
        let pick = |k: usize| if corner >> (2 - k) & 1 == 1 { (pos[k].1, pos[k].2) } else { (pos[k].0, 1.0 - pos[k].2) };
        let ((ri, rw), (gi, gw), (bi, bw)) = (pick(0), pick(1), pick(2));
        let w = rw * gw * bw;
        if w == 0.0 {
            continue;
        }
        let base = index(ri, gi, bi);
        for (o, r) in result.iter_mut().enumerate() {
            *r += w * value(base + o)?;
        }
    }
    Some(result)
}

/// The `A2B0` transform at `at` (device RGB 0–1 to PCS XYZ, D50), when it
/// is a 3-in, 3-out `mft1`, `mft2` or `mAB `.
/// A device-to-connection-space transform.
type Transform<'a> = Box<dyn Fn([f64; 3]) -> Option<[f64; 3]> + 'a>;

fn a2b<'a>(r: &'a Reader<'a>, at: usize, lab_pcs: bool) -> Option<Transform<'a>> {
    match r.sig(at)? {
        sig @ (b"mft1" | b"mft2") => {
            let wide = sig == b"mft2";
            let (inc, outc, grid) = (usize::from(r.u8(at + 8)?), usize::from(r.u8(at + 9)?), usize::from(r.u8(at + 10)?));
            if inc != 3 || outc != 3 || grid < 2 {
                return None;
            }
            let (n_in, n_out, tables) = if wide { (usize::from(r.u16(at + 48)?), usize::from(r.u16(at + 50)?), at + 52) } else { (256, 256, at + 48) };
            let width = if wide { 2 } else { 1 };
            let max = if wide { 65535.0 } else { 255.0 };
            let read = move |p: usize| -> Option<f64> { Some(if wide { f64::from(r.u16(p)?) } else { f64::from(r.u8(p)?) } / max) };
            let clut_at = tables + 3 * n_in * width;
            let out_at = clut_at + grid * grid * grid * 3 * width;
            r.0.get(out_at + 3 * n_out * width - 1)?;
            Some(Box::new(move |rgb: [f64; 3]| {
                let curve = |c: usize, n: usize, base: usize, x: f64| -> Option<f64> {
                    let values: Vec<f64> = (0..n).map(|i| read(base + (c * n + i) * width)).collect::<Option<_>>()?;
                    Some(table(&values, x))
                };
                let inp = [curve(0, n_in, tables, rgb[0])?, curve(1, n_in, tables, rgb[1])?, curve(2, n_in, tables, rgb[2])?];
                let mid = clut(&[grid; 3], 3, &|i| read(clut_at + i * width), inp)?;
                let o = [curve(0, n_out, out_at, mid[0])?, curve(1, n_out, out_at, mid[1])?, curve(2, n_out, out_at, mid[2])?];
                Some(if lab_pcs {
                    // v2 Lab: 0xFF00 (or 0xFF) is 100.
                    let k = if wide { 65535.0 / 65280.0 } else { 1.0 };
                    lab_to_xyz(o[0] * k * 100.0, o[1] * k * 255.0 - 128.0, o[2] * k * 255.0 - 128.0)
                } else {
                    // XYZ: 0x8000 is 1.0.
                    o.map(|v| v * 65535.0 / 32768.0)
                })
            }))
        }
        b"mAB " => {
            let (inc, outc) = (usize::from(r.u8(at + 8)?), usize::from(r.u8(at + 9)?));
            if inc != 3 || outc != 3 {
                return None;
            }
            let offset = |k: usize| r.u32(at + 12 + 4 * k).map(|o| o as usize);
            let (b_off, m_off, matrix_off, clut_off, a_off) = (offset(0)?, offset(2)?, offset(1)?, offset(3)?, offset(4)?);
            let curves = |off: usize| -> Option<Vec<Curve>> {
                if off == 0 {
                    return Some(Vec::new());
                }
                let mut out = Vec::new();
                let mut p = at + off;
                for _ in 0..3 {
                    let (c, size) = curve(r, p)?;
                    out.push(c);
                    p += size;
                }
                Some(out)
            };
            let (bc, mc, ac) = (curves(b_off)?, curves(m_off)?, curves(a_off)?);
            let matrix: Option<Vec<f64>> = if matrix_off == 0 { None } else { (0..12).map(|i| r.s15(at + matrix_off + 4 * i)).collect::<Option<_>>() };
            let grid_info = if clut_off == 0 {
                None
            } else {
                let c = at + clut_off;
                let grid = [usize::from(r.u8(c)?), usize::from(r.u8(c + 1)?), usize::from(r.u8(c + 2)?)];
                let precision = usize::from(r.u8(c + 16)?);
                Some((grid, precision, c + 20))
            };
            let apply = |cs: &[Curve], v: [f64; 3]| if cs.len() == 3 { [cs[0].eval(v[0]), cs[1].eval(v[1]), cs[2].eval(v[2])] } else { v };
            Some(Box::new(move |rgb: [f64; 3]| {
                let mut v = apply(&ac, rgb);
                if let Some((grid, precision, data)) = grid_info {
                    let read = |i: usize| -> Option<f64> { if precision == 1 { r.u8(data + i).map(|x| f64::from(x) / 255.0) } else { r.u16(data + 2 * i).map(|x| f64::from(x) / 65535.0) } };
                    let o = clut(&grid, 3, &read, v)?;
                    v = [o[0], o[1], o[2]];
                }
                v = apply(&mc, v);
                if let Some(m) = &matrix {
                    v = [0, 1, 2].map(|row| m[row * 3] * v[0] + m[row * 3 + 1] * v[1] + m[row * 3 + 2] * v[2] + m[9 + row]);
                }
                v = apply(&bc, v);
                Some(if lab_pcs { lab_to_xyz(v[0] * 100.0, v[1] * 255.0 - 128.0, v[2] * 255.0 - 128.0) } else { v.map(|x| x * 65535.0 / 32768.0) })
            }))
        }
        _ => None,
    }
}

fn xy(v: [f64; 3]) -> Option<(f64, f64)> {
    let sum = v[0] + v[1] + v[2];
    (sum > 1e-9).then(|| (v[0] / sum, v[1] / sum))
}

/// Read an RGB ICC profile (v2 or v4).
pub fn parse(icc: &[u8]) -> Result<Profile, String> {
    let r = Reader(icc);
    if icc.len() < 132 || r.sig(36) != Some(b"acsp") {
        return Err("not an ICC profile".into());
    }
    if r.sig(16) != Some(b"RGB ") {
        return Err("not an RGB profile".into());
    }
    let lab_pcs = r.sig(20) == Some(b"Lab ");
    let count = r.u32(128).ok_or("no tag table")? as usize;
    let tag = |sig: &[u8; 4]| -> Option<usize> {
        (0..count.min(1000)).find_map(|i| {
            let at = 132 + i * 12;
            (r.sig(at)? == sig).then(|| r.u32(at + 4).map(|o| o as usize))?
        })
    };
    let colorant = |sig: &[u8; 4]| -> Option<(f64, f64)> {
        let at = tag(sig)?;
        (r.sig(at)? == b"XYZ ").then_some(())?;
        xy([r.s15(at + 8)?, r.s15(at + 12)?, r.s15(at + 16)?])
    };
    const D50: (f64, f64) = (0.3457, 0.3585);
    // A matrix/curve profile.
    if let (Some(red), Some(green), Some(blue)) = (colorant(b"rXYZ"), colorant(b"gXYZ"), colorant(b"bXYZ")) {
        let channel = |sig: &[u8; 4]| -> Result<Para, String> {
            let at = tag(sig).ok_or("no tone curve")?;
            match curve(&r, at).ok_or("an unknown tone curve type")?.0 {
                Curve::Para(p) => Ok(p),
                Curve::Table(t) => Ok(fit_power(|x| table(&t, x))),
            }
        };
        let red_curve = channel(b"rTRC")?;
        let curves = [red_curve, channel(b"gTRC").unwrap_or(red_curve), channel(b"bTRC").unwrap_or(red_curve)];
        return Ok(Profile { primaries: [red.0, red.1, green.0, green.1, blue.0, blue.1, D50.0, D50.1], curves });
    }
    // A LUT-based profile: evaluate it.
    let at = tag(b"A2B0").or_else(|| tag(b"A2B1")).ok_or("neither colorants nor an A2B0 table")?;
    let f = a2b(&r, at, lab_pcs).ok_or("an unsupported A2B0 table")?;
    let eval = |rgb: [f64; 3]| f(rgb).ok_or_else(|| "a broken A2B0 table".to_owned());
    let (rr, gg, bb, white) = (eval([1.0, 0.0, 0.0])?, eval([0.0, 1.0, 0.0])?, eval([0.0, 0.0, 1.0])?, eval([1.0, 1.0, 1.0])?);
    let point = |v: [f64; 3]| xy(v).ok_or_else(|| "a primary is black".to_owned());
    let (red, green, blue, w) = (point(rr)?, point(gg)?, point(bb)?, point(white)?);
    // Each channel's curve: its luminance relative to the full channel.
    let curve_of = |k: usize, full: [f64; 3]| -> Result<Para, String> {
        let mut at = [0.0; 3];
        at[k] = 0.5;
        let mid = eval(at)?;
        let ratio = if full[1] > 1e-9 { mid[1] / full[1] } else { 0.0 };
        Ok(fit_power(|_| ratio))
    };
    let curves = [curve_of(0, rr)?, curve_of(1, gg)?, curve_of(2, bb)?];
    Ok(Profile { primaries: [red.0, red.1, green.0, green.1, blue.0, blue.1, w.0, w.1], curves })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn parametric_types() {
        let srgb = Para { g: 2.4, a: 1.0 / 1.055, b: 0.055 / 1.055, c: 1.0 / 12.92, d: 0.04045, e: 0.0, f: 0.0 };
        assert!((srgb.eval(1.0) - 1.0).abs() < 1e-6);
        assert!((srgb.eval(0.5) - 0.21404).abs() < 1e-4);
        assert!((srgb.eval(0.02) - 0.02 / 12.92).abs() < 1e-9);
        assert!((fit_power(|x| x.powf(2.2)).g - 2.2).abs() < 1e-9);
    }

    /// A tiny lut16 (mft2) profile: identity curves, a 2-point grid whose
    /// corners are XYZ of an sRGB-like display (D50), XYZ PCS.
    pub(crate) fn lut_profile() -> Vec<u8> {
        let colorants = [[0.4361, 0.2225, 0.0139], [0.3851, 0.7169, 0.0971], [0.1431, 0.0606, 0.7141]];
        let mut tag = b"mft2\0\0\0\0".to_vec();
        tag.extend_from_slice(&[3, 3, 2, 0]);
        for (i, v) in [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0].iter().enumerate() {
            let _ = i;
            tag.extend_from_slice(&((v * 65536.0f64) as i32).to_be_bytes());
        }
        tag.extend_from_slice(&2u16.to_be_bytes());
        tag.extend_from_slice(&2u16.to_be_bytes());
        for _ in 0..3 {
            tag.extend_from_slice(&0u16.to_be_bytes());
            tag.extend_from_slice(&65535u16.to_be_bytes());
        }
        // Grid corners r,g,b in {0,1}: the sum of the colorants that are on.
        for ri in 0..2 {
            for gi in 0..2 {
                for bi in 0..2 {
                    let mut xyz = [0.0f64; 3];
                    for (k, on) in [ri, gi, bi].iter().enumerate() {
                        if *on == 1 {
                            for c in 0..3 {
                                xyz[c] += colorants[k][c];
                            }
                        }
                    }
                    for v in xyz {
                        tag.extend_from_slice(&((v * 32768.0).round().min(65535.0) as u16).to_be_bytes());
                    }
                }
            }
        }
        for _ in 0..3 {
            tag.extend_from_slice(&0u16.to_be_bytes());
            tag.extend_from_slice(&65535u16.to_be_bytes());
        }
        let mut out = vec![0u8; 132 + 12];
        out[16..20].copy_from_slice(b"RGB ");
        out[20..24].copy_from_slice(b"XYZ ");
        out[36..40].copy_from_slice(b"acsp");
        out[128..132].copy_from_slice(&1u32.to_be_bytes());
        out[132..136].copy_from_slice(b"A2B0");
        out[136..140].copy_from_slice(&144u32.to_be_bytes());
        out[140..144].copy_from_slice(&(tag.len() as u32).to_be_bytes());
        out.extend_from_slice(&tag);
        out
    }

    #[test]
    fn lut_profiles() {
        let p = parse(&lut_profile()).unwrap();
        // sRGB's red (D50-adapted colorant) and a linear curve.
        assert!((p.primaries[0] - 0.6484).abs() < 2e-3 && (p.primaries[1] - 0.3309).abs() < 2e-3, "{:?}", p.primaries);
        assert!((p.primaries[6] - 0.3457).abs() < 2e-3, "white D50: {:?}", p.primaries);
        assert!((p.curves[0].g - 1.0).abs() < 0.02, "{:?}", p.curves[0]);
    }
}
