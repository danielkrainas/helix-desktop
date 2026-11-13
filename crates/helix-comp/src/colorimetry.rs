//! Colorimetry for color-managed surfaces (helix-desktop-spec
//! `contracts/wayland/README.md`, "Color management"): named primaries
//! and transfer functions, and the conversion from a surface's image
//! description into the compositor's composition space (BT.709 primaries,
//! gamma 2.2 encoding, 1.0 = reference white).

/// Chromaticities: red, green, blue, white (x, y each).
pub type Chromaticities = [f64; 8];

/// Named primaries (`wp_color_manager_v1.primaries` values) Helix takes,
/// with their chromaticities.
pub fn named_primaries(name: u32) -> Option<Chromaticities> {
    const D65: [f64; 2] = [0.3127, 0.3290];
    const C: [f64; 2] = [0.310, 0.316];
    let p = |r: [f64; 2], g: [f64; 2], b: [f64; 2], w: [f64; 2]| [r[0], r[1], g[0], g[1], b[0], b[1], w[0], w[1]];
    Some(match name {
        1 => p([0.64, 0.33], [0.30, 0.60], [0.15, 0.06], D65),
        2 => p([0.67, 0.33], [0.21, 0.71], [0.14, 0.08], C),
        3 => p([0.64, 0.33], [0.29, 0.60], [0.15, 0.06], D65),
        4 => p([0.630, 0.340], [0.310, 0.595], [0.155, 0.070], D65),
        5 => p([0.243, 0.692], [0.145, 0.049], [0.681, 0.319], C),
        6 => p([0.708, 0.292], [0.170, 0.797], [0.131, 0.046], D65),
        8 => p([0.680, 0.320], [0.265, 0.690], [0.150, 0.060], [0.314, 0.351]),
        9 => p([0.680, 0.320], [0.265, 0.690], [0.150, 0.060], D65),
        10 => p([0.64, 0.33], [0.21, 0.71], [0.15, 0.06], D65),
        _ => return None,
    })
}

/// Named primaries Helix offers.
pub const NAMED_PRIMARIES: [u32; 9] = [1, 2, 3, 4, 5, 6, 8, 9, 10];
pub const SRGB_PRIMARIES: u32 = 1;
pub const BT2020_PRIMARIES: u32 = 6;

/// Transfer functions (`wp_color_manager_v1.transfer_function` values).
pub mod tf {
    pub const BT1886: u32 = 1;
    pub const GAMMA22: u32 = 2;
    pub const GAMMA28: u32 = 3;
    pub const EXT_LINEAR: u32 = 5;
    pub const SRGB: u32 = 9;
    pub const EXT_SRGB: u32 = 10;
    pub const LOG_100: u32 = 6;
    pub const LOG_316: u32 = 7;
    pub const ST2084_PQ: u32 = 11;
    pub const HLG: u32 = 13;
}

/// Named transfer functions Helix offers.
pub const NAMED_TFS: [u32; 10] = [tf::BT1886, tf::GAMMA22, tf::GAMMA28, tf::EXT_LINEAR, tf::LOG_100, tf::LOG_316, tf::SRGB, tf::EXT_SRGB, tf::ST2084_PQ, tf::HLG];

/// A transfer function: named, or a pure power (`set_tf_power`, the
/// exponent).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tf {
    Named(u32),
    Power(f64),
    /// Per-channel curves (ICC profiles).
    Parametric([crate::icc::Para; 3]),
}

/// An image description's colorimetry.
#[derive(Debug, Clone, PartialEq)]
pub struct Colorimetry {
    pub primaries: Chromaticities,
    pub primaries_named: Option<u32>,
    pub tf: Tf,
    /// Primary color volume: minimum, maximum and reference white
    /// luminance (cd/m²).
    pub luminance: (f64, f64, f64),
    /// Mastering display (target) primaries and luminance range, when set.
    pub target_primaries: Option<Chromaticities>,
    pub target_luminance: Option<(f64, f64)>,
    pub max_cll: Option<u32>,
    pub max_fall: Option<u32>,
}

impl Colorimetry {
    /// The default luminances for transfer function `tf`.
    pub fn default_luminance(tf: Tf) -> (f64, f64, f64) {
        match tf {
            Tf::Named(tf::ST2084_PQ) => (0.005, 10000.0, 203.0),
            Tf::Named(tf::HLG) => (0.005, 1000.0, 203.0),
            _ => (0.2, 80.0, 80.0),
        }
    }

    /// sRGB content (what untagged surfaces are taken to be).
    pub fn srgb() -> Self {
        Self::named(SRGB_PRIMARIES, Tf::Named(tf::SRGB))
    }

    pub fn named(primaries: u32, tf: Tf) -> Self {
        Self {
            primaries: named_primaries(primaries).unwrap_or_default(),
            primaries_named: Some(primaries),
            tf,
            luminance: Self::default_luminance(tf),
            target_primaries: None,
            target_luminance: None,
            max_cll: None,
            max_fall: None,
        }
    }

    /// Windows-scRGB: sRGB primaries, extended linear; Helix takes 1.0
    /// (80 cd/m²) as reference white.
    pub fn windows_scrgb() -> Self {
        let mut c = Self::named(SRGB_PRIMARIES, Tf::Named(tf::EXT_LINEAR));
        c.luminance = (0.0, 10000.0, 80.0);
        c
    }
}

/// The colorimetry of an RGB ICC profile (`crate::icc`): its primaries
/// and white, and its per-channel curves; SDR luminances.
pub fn from_icc(icc: &[u8]) -> Result<Colorimetry, String> {
    let p = crate::icc::parse(icc)?;
    let tf = Tf::Parametric(p.curves);
    Ok(Colorimetry {
        primaries: p.primaries,
        primaries_named: None,
        tf,
        luminance: Colorimetry::default_luminance(tf),
        target_primaries: None,
        target_luminance: None,
        max_cll: None,
        max_fall: None,
    })
}

/// How the shader decodes a transfer function (its `tf_kind` uniform).
pub mod kind {
    pub const POWER: i32 = 1;
    pub const SRGB: i32 = 2;
    pub const LINEAR: i32 = 3;
    pub const PQ: i32 = 4;
    pub const HLG: i32 = 5;
    /// Logarithmic, 100:1 and 316:1 (`tf_power` holds 2.0 or 2.5).
    pub const LOG: i32 = 6;
    /// Per-channel ICC parametric curves.
    pub const PARAMETRIC: i32 = 7;
}

/// What the conversion shader needs: how to decode, by what to scale the
/// decoded values so that reference white is 1.0, and the 3×3 matrix
/// (row-major) from the content's linear RGB to linear BT.709.
#[derive(Debug, Clone, PartialEq)]
pub struct Conversion {
    pub tf_kind: i32,
    /// The power of `POWER`; for `HLG`, the OOTF's system gamma.
    pub power: f32,
    pub scale: f32,
    pub matrix: [f32; 9],
    /// The content's luminance weights (the Y row of its RGB to XYZ), for
    /// HLG's OOTF.
    pub luma: [f32; 3],
    /// With `PARAMETRIC`, each channel's curve: (g, a, b, c) and (d, e, f).
    pub curves: [([f32; 4], [f32; 3]); 3],
}

/// HLG's system gamma for a nominal peak of `peak` cd/m² (BT.2100).
pub fn hlg_gamma(peak: f64) -> f64 {
    1.2 + 0.42 * (peak.max(1.0) / 1000.0).log10()
}

/// The conversion of `c` into the composition space; `None` when its
/// content can be drawn as it is (BT.709 primaries with an sRGB, extended
/// sRGB or gamma 2.2 encoding and reference white at its maximum).
pub fn conversion(c: &Colorimetry) -> Option<Conversion> {
    let srgb = named_primaries(SRGB_PRIMARIES).expect("sRGB");
    let same_primaries = c.primaries.iter().zip(srgb.iter()).all(|(a, b)| (a - b).abs() < 1e-4);
    let (min, max, reference) = c.luminance;
    let _ = min;
    let (tf_kind, power, signal_max) = match c.tf {
        Tf::Named(tf::SRGB) | Tf::Named(tf::EXT_SRGB) => (kind::SRGB, 1.0, max),
        Tf::Named(tf::GAMMA22) => (kind::POWER, 2.2, max),
        Tf::Named(tf::GAMMA28) => (kind::POWER, 2.8, max),
        Tf::Named(tf::BT1886) => (kind::POWER, 2.4, max),
        Tf::Named(tf::EXT_LINEAR) => (kind::LINEAR, 1.0, 80.0),
        // Decoded to cd/m².
        Tf::Named(tf::ST2084_PQ) => (kind::PQ, 1.0, 1.0),
        // Scene light through the OOTF at the nominal peak (`max`).
        Tf::Named(tf::HLG) => (kind::HLG, hlg_gamma(max), max),
        Tf::Named(tf::LOG_100) => (kind::LOG, 2.0, max),
        Tf::Named(tf::LOG_316) => (kind::LOG, 2.5, max),
        Tf::Named(_) => return None,
        Tf::Power(p) => (kind::POWER, p, max),
        Tf::Parametric(_) => (kind::PARAMETRIC, 1.0, max),
    };
    let scale = if reference > 0.0 { signal_max / reference } else { 1.0 };
    let plain = same_primaries && (scale - 1.0).abs() < 1e-6 && matches!(c.tf, Tf::Named(tf::SRGB) | Tf::Named(tf::EXT_SRGB) | Tf::Named(tf::GAMMA22));
    if plain {
        return None;
    }
    let to_xyz = rgb_to_xyz(&c.primaries)?;
    let m = mul(&xyz_to_rgb(&srgb)?, &mul(&adapt(white(&c.primaries), white(&srgb)), &to_xyz));
    let luma = [to_xyz[3] as f32, to_xyz[4] as f32, to_xyz[5] as f32];
    let curves = match c.tf {
        Tf::Parametric(ps) => ps.map(|p| ([p.g as f32, p.a as f32, p.b as f32, p.c as f32], [p.d as f32, p.e as f32, p.f as f32])),
        _ => [([1.0, 1.0, 0.0, 0.0], [0.0, 0.0, 0.0]); 3],
    };
    Some(Conversion { tf_kind, power: power as f32, scale: scale as f32, matrix: m.map(|v| v as f32), luma, curves })
}

type M3 = [f64; 9];

fn mul(a: &M3, b: &M3) -> M3 {
    let mut out = [0.0; 9];
    for r in 0..3 {
        for c in 0..3 {
            out[r * 3 + c] = (0..3).map(|k| a[r * 3 + k] * b[k * 3 + c]).sum();
        }
    }
    out
}

fn inverse(m: &M3) -> Option<M3> {
    let [a, b, c, d, e, f, g, h, i] = *m;
    let det = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g);
    if det.abs() < 1e-12 {
        return None;
    }
    let k = 1.0 / det;
    Some([
        (e * i - f * h) * k,
        (c * h - b * i) * k,
        (b * f - c * e) * k,
        (f * g - d * i) * k,
        (a * i - c * g) * k,
        (c * d - a * f) * k,
        (d * h - e * g) * k,
        (b * g - a * h) * k,
        (a * e - b * d) * k,
    ])
}

fn xyz(x: f64, y: f64) -> [f64; 3] {
    [x / y, 1.0, (1.0 - x - y) / y]
}

fn white(p: &Chromaticities) -> [f64; 3] {
    xyz(p[6], p[7])
}

/// Linear RGB (these primaries) to CIE XYZ, white at Y = 1.
pub fn rgb_to_xyz(p: &Chromaticities) -> Option<M3> {
    let (r, g, b) = (xyz(p[0], p[1]), xyz(p[2], p[3]), xyz(p[4], p[5]));
    let m = [r[0], g[0], b[0], r[1], g[1], b[1], r[2], g[2], b[2]];
    let w = white(p);
    let inv = inverse(&m)?;
    let s = [0, 1, 2].map(|row| (0..3).map(|k| inv[row * 3 + k] * w[k]).sum::<f64>());
    Some([m[0] * s[0], m[1] * s[1], m[2] * s[2], m[3] * s[0], m[4] * s[1], m[5] * s[2], m[6] * s[0], m[7] * s[1], m[8] * s[2]])
}

fn xyz_to_rgb(p: &Chromaticities) -> Option<M3> {
    inverse(&rgb_to_xyz(p)?)
}

/// Bradford chromatic adaptation from white `from` to white `to` (XYZ).
fn adapt(from: [f64; 3], to: [f64; 3]) -> M3 {
    const B: M3 = [0.8951, 0.2664, -0.1614, -0.7502, 1.7135, 0.0367, 0.0389, -0.0685, 1.0296];
    let Some(bi) = inverse(&B) else { return [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0] };
    let cone = |w: [f64; 3]| [0, 1, 2].map(|r| (0..3).map(|k| B[r * 3 + k] * w[k]).sum::<f64>());
    let (s, d) = (cone(from), cone(to));
    let scale = [d[0] / s[0], 0.0, 0.0, 0.0, d[1] / s[1], 0.0, 0.0, 0.0, d[2] / s[2]];
    mul(&bi, &mul(&scale, &B))
}

/// The ST 2084 (PQ) EOTF: signal 0–1 to cd/m².
pub fn pq_eotf(e: f64) -> f64 {
    let (m1, m2, c1, c2, c3) = (0.1593017578125, 78.84375, 0.8359375, 18.8515625, 18.6875);
    let p = e.clamp(0.0, 1.0).powf(1.0 / m2);
    10000.0 * ((p - c1).max(0.0) / (c2 - c3 * p)).powf(1.0 / m1)
}

/// The fragment shader converting a surface into the composition space
/// (Smithay's custom texture shader conventions).
pub const SHADER: &str = r#"
#version 100
//_DEFINES
#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif
precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif
uniform float alpha;
varying vec2 v_coords;
#if defined(DEBUG_FLAGS)
uniform float tint;
#endif
uniform int tf_kind;
uniform float tf_power;
uniform float scale;
uniform mat3 to_bt709;
uniform vec3 luma;
uniform vec4 curve_r;
uniform vec3 curve_r2;
uniform vec4 curve_g;
uniform vec3 curve_g2;
uniform vec4 curve_b;
uniform vec3 curve_b2;

// ICC's parametric curve: (a v + b)^g + e above d, else c v + f.
float para(float v, vec4 p, vec3 q) {
    return v >= q.x ? pow(max(p.y * v + p.z, 0.0), p.x) + q.y : p.w * v + q.z;
}

float decode(float v) {
    float s = sign(v);
    float a = abs(v);
    if (tf_kind == 1) return s * pow(a, tf_power);
    if (tf_kind == 2) return s * (a <= 0.04045 ? a / 12.92 : pow((a + 0.055) / 1.055, 2.4));
    if (tf_kind == 3) return v;
    if (tf_kind == 6) return v <= 0.0 ? 0.0 : pow(10.0, tf_power * (min(v, 1.0) - 1.0));
    if (tf_kind == 5) {
        // HLG's inverse OETF: scene light 0-1.
        float x = clamp(v, 0.0, 1.0);
        return x <= 0.5 ? x * x / 3.0 : (exp((x - 0.55991073) / 0.17883277) + 0.28466892) / 12.0;
    }
    // PQ, to cd/m².
    float p = pow(clamp(v, 0.0, 1.0), 1.0 / 78.84375);
    return 10000.0 * pow(max(p - 0.8359375, 0.0) / (18.8515625 - 18.6875 * p), 1.0 / 0.1593017578125);
}

void main() {
    vec4 c = texture2D(tex, v_coords);
#if defined(NO_ALPHA)
    c.a = 1.0;
#endif
    vec3 rgb = c.a > 0.0 ? c.rgb / c.a : vec3(0.0);
    if (tf_kind == 7) {
        rgb = vec3(para(rgb.r, curve_r, curve_r2), para(rgb.g, curve_g, curve_g2), para(rgb.b, curve_b, curve_b2));
    } else {
        rgb = vec3(decode(rgb.r), decode(rgb.g), decode(rgb.b));
    }
    if (tf_kind == 5) {
        // HLG's OOTF: display light relative to the nominal peak.
        rgb *= pow(max(dot(luma, rgb), 1e-6), tf_power - 1.0);
    }
    rgb *= scale;
    // Above reference white is kept (an HDR monitor's frame holds it; an
    // 8-bit frame clips it).
    rgb = max(to_bt709 * rgb, 0.0);
    rgb = pow(rgb, vec3(1.0 / 2.2));
    gl_FragColor = vec4(rgb * c.a, c.a) * alpha;
#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        gl_FragColor = vec4(0.0, 0.2, 0.0, 0.2) + gl_FragColor * 0.8;
#endif
}
"#;

#[cfg(test)]
mod tests {

    /// A matrix/curve ICC profile with these D50 colorants and curve.
    fn icc(colorants: [[f64; 3]; 3], curve: &[u8]) -> Vec<u8> {
        let tags: Vec<(&[u8; 4], Vec<u8>)> = vec![
            (b"rXYZ", xyz_tag(colorants[0])),
            (b"gXYZ", xyz_tag(colorants[1])),
            (b"bXYZ", xyz_tag(colorants[2])),
            (b"rTRC", curve.to_vec()),
        ];
        let mut out = vec![0u8; 132 + 12 * tags.len()];
        out[16..20].copy_from_slice(b"RGB ");
        out[36..40].copy_from_slice(b"acsp");
        out[128..132].copy_from_slice(&(tags.len() as u32).to_be_bytes());
        for (i, (sig, data)) in tags.iter().enumerate() {
            let at = 132 + 12 * i;
            let offset = out.len() as u32;
            out[at..at + 4].copy_from_slice(*sig);
            out[at + 4..at + 8].copy_from_slice(&offset.to_be_bytes());
            out[at + 8..at + 12].copy_from_slice(&(data.len() as u32).to_be_bytes());
            out.extend_from_slice(data);
        }
        out
    }

    fn xyz_tag(v: [f64; 3]) -> Vec<u8> {
        let mut t = b"XYZ \0\0\0\0".to_vec();
        for c in v {
            t.extend_from_slice(&((c * 65536.0).round() as i32).to_be_bytes());
        }
        t
    }

    #[test]
    fn icc_profiles() {
        // sRGB's colorants adapted to D50 (as sRGB profiles carry them).
        let srgb = [[0.4361, 0.2225, 0.0139], [0.3851, 0.7169, 0.0971], [0.1431, 0.0606, 0.7141]];
        let mut gamma = b"curv\0\0\0\0".to_vec();
        gamma.extend_from_slice(&1u32.to_be_bytes());
        gamma.extend_from_slice(&((2.2 * 256.0) as u16).to_be_bytes());
        let c = from_icc(&icc(srgb, &gamma)).unwrap();
        assert!(matches!(c.tf, Tf::Parametric(p) if (p[0].g - 2.2).abs() < 0.01 && p[0].a == 1.0));
        // Back to BT.709 it is (close to) no change of color.
        let m = conversion(&c).unwrap().matrix;
        assert!(close(&m, &[1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0], 0.02), "{m:?}");
        let mut para = b"para\0\0\0\0\0\x03\0\0".to_vec();
        for v in [2.4, 1.0 / 1.055, 0.055 / 1.055, 1.0 / 12.92, 0.04045] {
            para.extend_from_slice(&((v * 65536.0f64).round() as i32).to_be_bytes());
        }
        let Tf::Parametric(p) = from_icc(&icc(srgb, &para)).unwrap().tf else { panic!() };
        assert!((p[0].eval(0.5) - 0.21404).abs() < 1e-3, "sRGB's curve exactly");
        // A LUT-based profile.
        let lut = from_icc(&crate::icc::tests::lut_profile()).unwrap();
        assert!(conversion(&lut).is_some());
        assert!(from_icc(b"not a profile").is_err());
    }

    #[test]
    fn hlg() {
        let c = conversion(&Colorimetry::named(BT2020_PRIMARIES, Tf::Named(tf::HLG))).unwrap();
        assert_eq!(c.tf_kind, kind::HLG);
        assert!((c.power - 1.2).abs() < 1e-6, "system gamma at 1000 cd/m²");
        assert!((c.scale - 1000.0 / 203.0).abs() < 1e-4);
        assert!((c.luma[1] - 0.678).abs() < 1e-3, "BT.2020 green's luminance share");
    }

    use super::*;

    fn close(a: &[f32], b: &[f32], eps: f32) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < eps)
    }

    #[test]
    fn plain_content_is_not_converted() {
        assert_eq!(conversion(&Colorimetry::srgb()), None);
        assert_eq!(conversion(&Colorimetry::named(SRGB_PRIMARIES, Tf::Named(tf::SRGB))), None);
    }

    #[test]
    fn bt2020_pq() {
        let c = conversion(&Colorimetry::named(BT2020_PRIMARIES, Tf::Named(tf::ST2084_PQ))).unwrap();
        assert_eq!(c.tf_kind, kind::PQ);
        // cd/m² over 203 cd/m² reference white.
        assert!((c.scale - 1.0 / 203.0).abs() < 1e-6);
        // The well-known BT.2020 → BT.709 matrix.
        assert!(close(&c.matrix, &[1.6605, -0.5876, -0.0728, -0.1246, 1.1329, -0.0083, -0.0182, -0.1006, 1.1187], 2e-3), "{:?}", c.matrix);
        // White stays white.
        let row_sums: Vec<f32> = c.matrix.chunks(3).map(|r| r.iter().sum()).collect();
        assert!(close(&row_sums, &[1.0, 1.0, 1.0], 1e-3));
    }

    #[test]
    fn scrgb_and_gamma() {
        let s = conversion(&Colorimetry::windows_scrgb()).unwrap();
        assert_eq!((s.tf_kind, s.scale), (kind::LINEAR, 1.0));
        assert!(close(&s.matrix, &[1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0], 1e-5));
        let g = conversion(&Colorimetry::named(SRGB_PRIMARIES, Tf::Named(tf::BT1886))).unwrap();
        assert_eq!((g.tf_kind, g.power), (kind::POWER, 2.4));
        // Display P3 has D65 white: white stays white without adaptation.
        let p3 = conversion(&Colorimetry::named(9, Tf::Named(tf::SRGB))).unwrap();
        let sums: Vec<f32> = p3.matrix.chunks(3).map(|r| r.iter().sum()).collect();
        assert!(close(&sums, &[1.0, 1.0, 1.0], 1e-3));
        // DCI-P3's white is adapted to D65.
        let dci = conversion(&Colorimetry::named(8, Tf::Named(tf::GAMMA22))).unwrap();
        let sums: Vec<f32> = dci.matrix.chunks(3).map(|r| r.iter().sum()).collect();
        assert!(close(&sums, &[1.0, 1.0, 1.0], 1e-3), "{sums:?}");
    }

    #[test]
    fn pq() {
        assert!(pq_eotf(0.0) < 1e-6);
        assert!((pq_eotf(1.0) - 10000.0).abs() < 1e-3);
        // 203 cd/m² is about 58 % signal.
        assert!((pq_eotf(0.5807) - 203.0).abs() < 2.0, "{}", pq_eotf(0.5807));
    }
}
