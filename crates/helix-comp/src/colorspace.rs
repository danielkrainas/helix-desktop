//! Color for HDR monitors and ICC profiles (helix-desktop-spec
//! `contracts/dbus/org.gnome.Mutter.DisplayConfig.md`, "Color modes" and
//! "Color profiles"): what a monitor's EDID says about HDR, the BT.2100
//! (PQ, BT.2020) encoding SDR content is converted to, the kernel's HDR
//! metadata, and the calibration curves (`vcgt`) of ICC profiles.
//!
//! Not yet tried with an HDR monitor or a calibrated profile on real
//! hardware: everything here is checked by unit tests with reference
//! values.

/// What an EDID's CTA-861 extension says about HDR.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct HdrCaps {
    /// The SMPTE ST 2084 (PQ) transfer function.
    pub pq: bool,
    /// Hybrid log-gamma.
    pub hlg: bool,
    /// BT.2020 RGB colorimetry.
    pub bt2020: bool,
    /// Desired content luminances (cd/m²): maximum, maximum frame
    /// average, minimum.
    pub max_luminance: Option<f64>,
    pub max_frame_average: Option<f64>,
    pub min_luminance: Option<f64>,
}

impl HdrCaps {
    /// Whether the monitor can be driven in BT.2100 (PQ with BT.2020).
    pub fn bt2100(&self) -> bool {
        self.pq && self.bt2020
    }
}

/// HDR capabilities from the CTA-861 extension blocks of `edid`.
pub fn hdr_caps(edid: &[u8]) -> HdrCaps {
    let mut caps = HdrCaps::default();
    for block in edid.as_chunks::<128>().0.iter().skip(1) {
        if block[0] != 0x02 {
            continue;
        }
        let end = usize::from(block[2]).clamp(4, 127);
        let mut at = 4;
        while at < end {
            let header = block[at];
            let (tag, len) = (header >> 5, usize::from(header & 0x1f));
            let payload = &block[(at + 1).min(end)..(at + 1 + len).min(end)];
            if tag == 7 && !payload.is_empty() {
                match payload[0] {
                    // Colorimetry.
                    0x05 if payload.len() >= 2 => caps.bt2020 |= payload[1] & 0x80 != 0,
                    // HDR static metadata.
                    0x06 if payload.len() >= 2 => {
                        caps.pq |= payload[1] & 0x04 != 0;
                        caps.hlg |= payload[1] & 0x08 != 0;
                        let code = |i: usize| payload.get(i).copied().filter(|v| *v != 0);
                        caps.max_luminance = code(3).map(|cv| 50.0 * 2f64.powf(f64::from(cv) / 32.0));
                        caps.max_frame_average = code(4).map(|cv| 50.0 * 2f64.powf(f64::from(cv) / 32.0));
                        caps.min_luminance = match (caps.max_luminance, code(5)) {
                            (Some(max), Some(cv)) => Some(max * (f64::from(cv) / 255.0).powi(2) / 100.0),
                            _ => None,
                        };
                    }
                    _ => {}
                }
            }
            at += 1 + len;
        }
    }
    caps
}

/// sRGB's electro-optical transfer: encoded 0–1 to linear 0–1.
pub fn srgb_eotf(v: f64) -> f64 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

const M1: f64 = 2610.0 / 16384.0;
const M2: f64 = 2523.0 / 4096.0 * 128.0;
const C1: f64 = 3424.0 / 4096.0;
const C2: f64 = 2413.0 / 4096.0 * 32.0;
const C3: f64 = 2392.0 / 4096.0 * 32.0;
/// PQ's peak luminance (cd/m²).
pub const PQ_PEAK: f64 = 10000.0;

/// PQ encoding (the inverse EOTF) of a luminance in cd/m².
pub fn pq_encode(nits: f64) -> f64 {
    let y = (nits / PQ_PEAK).clamp(0.0, 1.0);
    let p = y.powf(M1);
    ((C1 + C2 * p) / (1.0 + C3 * p)).powf(M2)
}

/// PQ decoding (the EOTF): luminance in cd/m² of an encoded value.
pub fn pq_decode(v: f64) -> f64 {
    let p = v.clamp(0.0, 1.0).powf(1.0 / M2);
    PQ_PEAK * ((p - C1).max(0.0) / (C2 - C3 * p)).powf(1.0 / M1)
}

pub type Matrix = [[f64; 3]; 3];

/// Chromaticities: red, green, blue, white (x, y).
pub type Primaries = [(f64, f64); 4];

pub const SRGB: Primaries = [(0.64, 0.33), (0.30, 0.60), (0.15, 0.06), (0.3127, 0.3290)];
pub const BT2020: Primaries = [(0.708, 0.292), (0.170, 0.797), (0.131, 0.046), (0.3127, 0.3290)];

fn mul(a: &Matrix, b: &Matrix) -> Matrix {
    std::array::from_fn(|i| std::array::from_fn(|j| (0..3).map(|k| a[i][k] * b[k][j]).sum()))
}

pub fn apply(m: &Matrix, v: [f64; 3]) -> [f64; 3] {
    std::array::from_fn(|i| (0..3).map(|k| m[i][k] * v[k]).sum())
}

fn invert(m: &Matrix) -> Matrix {
    let [[a, b, c], [d, e, f], [g, h, i]] = *m;
    let det = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g);
    [
        [(e * i - f * h) / det, (c * h - b * i) / det, (b * f - c * e) / det],
        [(f * g - d * i) / det, (a * i - c * g) / det, (c * d - a * f) / det],
        [(d * h - e * g) / det, (b * g - a * h) / det, (a * e - b * d) / det],
    ]
}

/// The matrix from linear RGB with `p` primaries to CIE XYZ.
pub fn rgb_to_xyz(p: &Primaries) -> Matrix {
    let xyz = |(x, y): (f64, f64)| [x / y, 1.0, (1.0 - x - y) / y];
    let (r, g, b, w) = (xyz(p[0]), xyz(p[1]), xyz(p[2]), xyz(p[3]));
    let m = [[r[0], g[0], b[0]], [r[1], g[1], b[1]], [r[2], g[2], b[2]]];
    let s = apply(&invert(&m), w);
    std::array::from_fn(|i| std::array::from_fn(|j| m[i][j] * s[j]))
}

/// Linear RGB with `from` primaries to linear RGB with `to` (same white).
pub fn conversion(from: &Primaries, to: &Primaries) -> Matrix {
    mul(&invert(&rgb_to_xyz(to)), &rgb_to_xyz(from))
}

/// The luminance (cd/m²) SDR white is shown at on an HDR monitor.
pub const SDR_WHITE: f64 = 203.0;

/// SDR white for an `output-luminance` percentage (100: [`SDR_WHITE`]);
/// between 25 % and 300 %.
pub fn sdr_white(percent: Option<f64>) -> f64 {
    SDR_WHITE * percent.unwrap_or(100.0).clamp(25.0, 300.0) / 100.0
}

/// An sRGB-encoded color as BT.2100 PQ-encoded values, with SDR white at
/// `white` cd/m² (what the compositor's HDR pass computes on the GPU).
pub fn sdr_to_pq(rgb: [f64; 3], white: f64) -> [f64; 3] {
    let linear = rgb.map(srgb_eotf);
    let wide = apply(&conversion(&SRGB, &BT2020), linear);
    wide.map(|v| pq_encode(v.max(0.0) * white))
}

/// The fragment shader of the compositor's HDR pass (GLSL ES 1.00, in
/// the form Smithay's custom texture shaders take): an sRGB-encoded,
/// premultiplied frame to BT.2100 PQ, with SDR white at the `sdr_white`
/// uniform (cd/m² / 10000).
pub fn hdr_shader() -> String {
    let m = conversion(&SRGB, &BT2020);
    // GLSL matrices are given column by column.
    let columns: Vec<String> = (0..3).flat_map(|j| (0..3).map(move |i| (i, j))).map(|(i, j)| format!("{:.8}", m[i][j])).collect();
    format!(
        r#"#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
uniform float sdr_white;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

const mat3 SRGB_TO_BT2020 = mat3({matrix});

vec3 srgb_to_linear(vec3 c) {{
    vec3 low = c / 12.92;
    vec3 high = pow((c + 0.055) / 1.055, vec3(2.4));
    return mix(low, high, step(vec3(0.04045), c));
}}

vec3 pq_encode(vec3 y) {{
    vec3 p = pow(clamp(y, 0.0, 1.0), vec3({m1:.10}));
    return pow(({c1:.10} + {c2:.10} * p) / (1.0 + {c3:.10} * p), vec3({m2:.10}));
}}

void main() {{
    vec4 color = texture2D(tex, v_coords);
    vec3 rgb = color.a > 0.0 ? color.rgb / color.a : vec3(0.0);
    vec3 linear = SRGB_TO_BT2020 * srgb_to_linear(rgb);
    color = vec4(pq_encode(linear * sdr_white), 1.0);

#if defined(NO_ALPHA)
    color = color * alpha;
#else
    color = color * alpha;
#endif

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    gl_FragColor = color;
}}
"#,
        matrix = columns.join(", "),
        m1 = M1,
        m2 = M2,
        c1 = C1,
        c2 = C2,
        c3 = C3,
    )
}

/// The kernel's `struct hdr_output_metadata` (type 0, static metadata
/// type 1) for PQ with BT.2020 primaries: `max_luminance` and
/// `min_luminance` in cd/m², and content light levels.
pub fn hdr_output_metadata(max_luminance: f64, min_luminance: f64, max_cll: f64, max_fall: f64) -> [u8; 32] {
    let mut out = [0u8; 32];
    // metadata_type (u32) = 0, then the infoframe.
    const EOTF_ST2084: u8 = 2;
    out[4] = EOTF_ST2084;
    out[5] = 0; // static metadata descriptor type 1
    let chroma = |v: f64| ((v * 50000.0).round() as u16).to_ne_bytes();
    let mut at = 6;
    for (x, y) in BT2020 {
        out[at..at + 2].copy_from_slice(&chroma(x));
        out[at + 2..at + 4].copy_from_slice(&chroma(y));
        at += 4;
    }
    let u16v = |v: f64| (v.round().clamp(0.0, 65535.0) as u16).to_ne_bytes();
    out[22..24].copy_from_slice(&u16v(max_luminance));
    out[24..26].copy_from_slice(&u16v(min_luminance * 10000.0));
    out[26..28].copy_from_slice(&u16v(max_cll));
    out[28..30].copy_from_slice(&u16v(max_fall));
    out
}

/// Calibration curves from an ICC profile's `vcgt` tag, resampled to
/// `size` entries per channel (16-bit). `None` without a usable tag.
pub fn icc_vcgt(icc: &[u8], size: usize) -> Option<(Vec<u16>, Vec<u16>, Vec<u16>)> {
    let be32 = |at: usize| icc.get(at..at + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    let be16 = |at: usize| icc.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]]));
    if icc.len() < 132 || &icc[36..40] != b"acsp" || size < 2 {
        return None;
    }
    let count = be32(128)? as usize;
    let (offset, len) = (0..count.min(1000)).find_map(|i| {
        let at = 132 + i * 12;
        (icc.get(at..at + 4)? == b"vcgt").then(|| Some((be32(at + 4)? as usize, be32(at + 8)? as usize)))?
    })?;
    let tag = icc.get(offset..offset.checked_add(len)?)?;
    if tag.get(0..4)? != b"vcgt" {
        return None;
    }
    let at = |i: usize| offset + i;
    let x = |i: usize| i as f64 / (size - 1) as f64;
    let ramp = |f: &dyn Fn(f64) -> f64| (0..size).map(|i| (f(x(i)).clamp(0.0, 1.0) * 65535.0).round() as u16).collect::<Vec<u16>>();
    match be32(at(8))? {
        // A table: channels, entries, entry size, then each channel.
        0 => {
            let (channels, entries, width) = (usize::from(be16(at(12))?), usize::from(be16(at(14))?), usize::from(be16(at(16))?));
            if !(channels == 1 || channels == 3) || entries < 2 || !(width == 1 || width == 2) {
                return None;
            }
            let value = |c: usize, i: usize| -> Option<f64> {
                let p = at(18) + (c * entries + i) * width;
                Some(if width == 1 { f64::from(*icc.get(p)?) / 255.0 } else { f64::from(be16(p)?) / 65535.0 })
            };
            // Every value must be there.
            value(channels - 1, entries - 1)?;
            let channel = |c: usize| {
                let c = if channels == 1 { 0 } else { c };
                ramp(&|v: f64| {
                    let pos = v * (entries - 1) as f64;
                    let (i, frac) = (pos.floor() as usize, pos.fract());
                    let a = value(c, i).unwrap_or(0.0);
                    let b = value(c, (i + 1).min(entries - 1)).unwrap_or(a);
                    a + (b - a) * frac
                })
            };
            Some((channel(0), channel(1), channel(2)))
        }
        // A formula per channel: gamma, minimum, maximum (s15Fixed16).
        1 => {
            let fixed = |p: usize| be32(p).map(|v| f64::from(v as i32) / 65536.0);
            let channel = |c: usize| -> Option<Vec<u16>> {
                let base = at(12) + c * 12;
                let (gamma, min, max) = (fixed(base)?, fixed(base + 4)?, fixed(base + 8)?);
                Some(ramp(&|v: f64| min + (max - min) * v.powf(gamma)))
            };
            Some((channel(0)?, channel(1)?, channel(2)?))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() <= eps
    }

    /// An EDID with a CTA extension holding the given data blocks.
    fn edid_with(blocks: &[&[u8]]) -> Vec<u8> {
        let mut e = vec![0u8; 256];
        e[128] = 0x02;
        e[129] = 0x03;
        let mut at = 132;
        for b in blocks {
            e[at..at + b.len()].copy_from_slice(b);
            at += b.len();
        }
        e[130] = at as u8 - 128;
        e
    }

    #[test]
    fn edid_hdr() {
        // Colorimetry (extended tag 5): BT2020RGB; HDR static metadata
        // (extended tag 6): SDR + PQ + HLG, type 1, luminance codes.
        let colorimetry: &[u8] = &[(7 << 5) | 3, 0x05, 0x80, 0x00];
        let hdr: &[u8] = &[(7 << 5) | 6, 0x06, 0x0d, 0x01, 0x80, 0x60, 0x40];
        let caps = hdr_caps(&edid_with(&[colorimetry, hdr]));
        assert!(caps.pq && caps.hlg && caps.bt2020 && caps.bt2100());
        assert!(close(caps.max_luminance.unwrap(), 50.0 * 2f64.powf(4.0), 1e-9), "{:?}", caps.max_luminance);
        assert!(close(caps.max_frame_average.unwrap(), 50.0 * 2f64.powi(3), 1e-9));
        assert!(close(caps.min_luminance.unwrap(), 800.0 * (64.0f64 / 255.0).powi(2) / 100.0, 1e-9));
        let sdr = hdr_caps(&edid_with(&[&[(7 << 5) | 3, 0x06, 0x01, 0x01]]));
        assert!(!sdr.pq && !sdr.bt2100());
        assert!(!hdr_caps(&edid_with(&[hdr])).bt2100(), "PQ without BT.2020");
        assert_eq!(hdr_caps(&[0u8; 128]), HdrCaps::default(), "no extension");
    }

    #[test]
    fn pq() {
        assert!(close(pq_encode(10000.0), 1.0, 1e-9));
        assert!(close(pq_encode(0.0), 0.0, 1e-6));
        // 100 cd/m² is about 0.508, 203 about 0.58, 1000 about 0.752.
        assert!(close(pq_encode(100.0), 0.5081, 1e-3), "{}", pq_encode(100.0));
        assert!(close(pq_encode(203.0), 0.5806, 1e-3), "{}", pq_encode(203.0));
        assert!(close(pq_encode(1000.0), 0.7518, 1e-3));
        for nits in [0.1, 5.0, 203.0, 4000.0] {
            assert!(close(pq_decode(pq_encode(nits)), nits, nits * 1e-6 + 1e-6));
        }
        assert!(close(srgb_eotf(0.5), 0.2140, 1e-4));
    }

    #[test]
    fn primaries() {
        let m = conversion(&SRGB, &BT2020);
        let expected = [[0.6274, 0.3293, 0.0433], [0.0691, 0.9195, 0.0114], [0.0164, 0.0880, 0.8956]];
        for i in 0..3 {
            for j in 0..3 {
                assert!(close(m[i][j], expected[i][j], 2e-4), "{i},{j}: {}", m[i][j]);
            }
        }
        let y = rgb_to_xyz(&SRGB)[1];
        assert!(close(y[0], 0.2126, 1e-4) && close(y[1], 0.7152, 1e-4) && close(y[2], 0.0722, 1e-4));
        // White stays white: SDR white at 203 cd/m².
        let white = sdr_to_pq([1.0, 1.0, 1.0], SDR_WHITE);
        assert!(white.iter().all(|v| close(*v, pq_encode(203.0), 1e-6)));
        assert_eq!(sdr_to_pq([0.0, 0.0, 0.0], SDR_WHITE).map(|v| (v * 1e6).round()), [0.0; 3].map(|v: f64| (pq_encode(v) * 1e6).round()));
        assert_eq!((sdr_white(None), sdr_white(Some(200.0)), sdr_white(Some(1000.0))), (203.0, 406.0, 609.0));
    }

    #[test]
    fn metadata_blob() {
        let b = hdr_output_metadata(1000.0, 0.005, 1000.0, 400.0);
        assert_eq!(&b[0..4], &0u32.to_ne_bytes(), "metadata type 0");
        assert_eq!(b[4], 2, "ST 2084");
        let u16_at = |i: usize| u16::from_ne_bytes([b[i], b[i + 1]]);
        assert_eq!((u16_at(6), u16_at(8)), (35400, 14600), "red 0.708, 0.292");
        assert_eq!((u16_at(18), u16_at(20)), (15635, 16450), "white 0.3127, 0.3290");
        assert_eq!((u16_at(22), u16_at(24), u16_at(26), u16_at(28)), (1000, 50, 1000, 400));
    }

    #[test]
    fn shader() {
        let source = hdr_shader();
        assert!(source.lines().any(|l| l == "//_DEFINES_"), "Smithay's defines marker");
        assert!(source.contains("uniform float sdr_white;"));
        // The first column of the matrix (red's share in each channel).
        assert!(source.contains("mat3(0.627"), "{source}");
        assert_eq!(source.matches('{').count(), source.matches('}').count());
    }

    fn icc_with(tag: &[u8]) -> Vec<u8> {
        let mut icc = vec![0u8; 132 + 12];
        icc[36..40].copy_from_slice(b"acsp");
        icc[128..132].copy_from_slice(&1u32.to_be_bytes());
        icc[132..136].copy_from_slice(b"vcgt");
        let offset = icc.len() as u32;
        icc[136..140].copy_from_slice(&offset.to_be_bytes());
        icc[140..144].copy_from_slice(&(tag.len() as u32).to_be_bytes());
        icc.extend_from_slice(tag);
        icc
    }

    #[test]
    fn vcgt_table() {
        // Three channels of three 16-bit entries: red halved, green
        // identity, blue inverted.
        let mut tag = b"vcgt".to_vec();
        tag.extend([0, 0, 0, 0]);
        tag.extend(0u32.to_be_bytes());
        tag.extend(3u16.to_be_bytes());
        tag.extend(3u16.to_be_bytes());
        tag.extend(2u16.to_be_bytes());
        for v in [0u16, 16384, 32767, 0, 32768, 65535, 65535, 32768, 0] {
            tag.extend(v.to_be_bytes());
        }
        let (r, g, b) = icc_vcgt(&icc_with(&tag), 5).unwrap();
        assert_eq!(r, [0, 8192, 16384, 24576, 32767]);
        assert_eq!(g, [0, 16384, 32768, 49152, 65535]);
        assert_eq!((b[0], b[4]), (65535, 0));
        assert_eq!(icc_vcgt(&icc_with(&tag[..20]), 5), None, "cut short");
        assert_eq!(icc_vcgt(&[0u8; 200], 5), None, "not a profile");
    }

    #[test]
    fn vcgt_formula() {
        let mut tag = b"vcgt".to_vec();
        tag.extend([0, 0, 0, 0]);
        tag.extend(1u32.to_be_bytes());
        let fixed = |v: f64| ((v * 65536.0).round() as i32).to_be_bytes();
        for (gamma, min, max) in [(1.0, 0.0, 1.0), (2.0, 0.0, 1.0), (1.0, 0.1, 0.9)] {
            tag.extend(fixed(gamma));
            tag.extend(fixed(min));
            tag.extend(fixed(max));
        }
        let (r, g, b) = icc_vcgt(&icc_with(&tag), 3).unwrap();
        assert_eq!(r, [0, 32768, 65535]);
        assert_eq!(g, [0, 16384, 65535]);
        assert_eq!(b, [6554, 32768, 58981], "0.9 as s15Fixed16 is a little under");
    }
}
