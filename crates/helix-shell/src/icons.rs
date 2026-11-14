//! Notification icons (helix-desktop-spec
//! `contracts/dbus/org.freedesktop.Notifications.md`, "Icons"): image
//! data, files, and icon names looked up in the icon theme (freedesktop
//! icon theme specification), decoded from PNG or SVG.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use helix_draw::Image;
use zbus::zvariant::Value;

/// Where a notification's icon comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum IconSource {
    Data(Image),
    File(PathBuf),
    /// Icon names, best first.
    Names(Vec<String>),
    /// Encoded image bytes (PNG or SVG).
    Bytes(Vec<u8>),
}

impl IconSource {
    /// An icon name, absolute path or `file://` URI.
    pub fn from_string(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.is_empty() {
            None
        } else if let Some(rest) = s.strip_prefix("file://") {
            use gio::prelude::*;
            let path = gio::File::for_uri(&format!("file://{rest}")).path()?;
            Some(Self::File(path))
        } else if s.starts_with('/') {
            Some(Self::File(PathBuf::from(s)))
        } else {
            Some(Self::Names(vec![s.to_owned()]))
        }
    }

    /// The `image-data` hint: `(iiibiiay)`.
    pub fn from_image_data(value: &Value<'_>) -> Option<Self> {
        let value = unwrap(value);
        let Value::Structure(s) = value else { return None };
        let [Value::I32(w), Value::I32(h), Value::I32(stride), Value::Bool(_alpha), Value::I32(bits), Value::I32(channels), Value::Array(data)] =
            s.fields()
        else {
            return None;
        };
        if *bits != 8 || !(3..=4).contains(channels) || *w <= 0 || *h <= 0 {
            return None;
        }
        let bytes: Vec<u8> = data.iter().filter_map(|v| if let Value::U8(b) = v { Some(*b) } else { None }).collect();
        let (w, h, stride, channels) = (*w as usize, *h as usize, *stride as usize, *channels as usize);
        if bytes.len() < stride * (h - 1) + w * channels {
            return None;
        }
        let mut rgba = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                let p = &bytes[y * stride + x * channels..];
                let a = if channels == 4 { p[3] } else { 255 };
                rgba.extend_from_slice(&premultiply([p[0], p[1], p[2], a]));
            }
        }
        Some(Self::Data(Image { width: w, height: h, rgba }))
    }

    /// A serialized `GIcon` (the org.gtk.Notifications `icon`).
    pub fn from_gicon(value: &Value<'_>) -> Option<Self> {
        let value = unwrap(value);
        match value {
            Value::Str(s) => Self::from_string(s),
            Value::Structure(s) => {
                let [Value::Str(kind), payload] = s.fields() else { return None };
                let payload = unwrap(payload);
                match (kind.as_str(), payload) {
                    ("themed", Value::Array(names)) => Some(Self::Names(
                        names.iter().filter_map(|n| if let Value::Str(s) = n { Some(s.to_string()) } else { None }).collect(),
                    )),
                    ("file", Value::Str(uri)) => Self::from_string(uri),
                    ("bytes", Value::Array(bytes)) => {
                        Some(Self::Bytes(bytes.iter().filter_map(|v| if let Value::U8(b) = v { Some(*b) } else { None }).collect()))
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// The icon of the application with desktop file id `id` (no
    /// `.desktop`).
    pub fn for_app(id: &str) -> Option<Self> {
        use gio::prelude::*;
        if id.is_empty() {
            return None;
        }
        let wanted = format!("{id}.desktop");
        let info = gio::AppInfo::all().into_iter().find(|a| a.id().is_some_and(|i| i == wanted))?;
        let icon = info.icon()?;
        if let Some(themed) = icon.downcast_ref::<gio::ThemedIcon>() {
            return Some(Self::Names(themed.names().iter().map(|n| n.to_string()).collect()));
        }
        if let Some(file) = icon.downcast_ref::<gio::FileIcon>() {
            return file.file().path().map(Self::File);
        }
        None
    }
}

/// The value inside any number of variant wrappers.
fn unwrap<'a>(mut value: &'a Value<'a>) -> &'a Value<'a> {
    while let Value::Value(inner) = value {
        value = inner.as_ref();
    }
    value
}

pub(crate) fn premultiply([r, g, b, a]: [u8; 4]) -> [u8; 4] {
    let m = |c: u8| ((u16::from(c) * u16::from(a) + 127) / 255) as u8;
    [m(r), m(g), m(b), a]
}

/// Decode PNG or SVG bytes at about `px` pixels.
pub fn decode(bytes: &[u8], px: u32) -> Option<Image> {
    if bytes.starts_with(b"\x89PNG") {
        decode_png(bytes)
    } else {
        decode_svg(bytes, px)
    }
}

fn decode_png(bytes: &[u8]) -> Option<Image> {
    let mut decoder = png::Decoder::new(bytes);
    decoder.set_transformations(png::Transformations::normalize_to_color8() | png::Transformations::ALPHA);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as usize, info.height as usize);
    let mut rgba = Vec::with_capacity(w * h * 4);
    let channels = info.color_type.samples();
    for y in 0..h {
        let row = &buf[y * info.line_size..];
        for x in 0..w {
            let p = &row[x * channels..];
            let px = match channels {
                4 => [p[0], p[1], p[2], p[3]],
                3 => [p[0], p[1], p[2], 255],
                2 => [p[0], p[0], p[0], p[1]],
                _ => [p[0], p[0], p[0], 255],
            };
            rgba.extend_from_slice(&premultiply(px));
        }
    }
    Some(Image { width: w, height: h, rgba })
}

fn decode_svg(bytes: &[u8], px: u32) -> Option<Image> {
    use resvg::{tiny_skia, usvg};
    let tree = usvg::Tree::from_data(bytes, &usvg::Options::default()).ok()?;
    let size = tree.size();
    let px = px.max(1);
    let scale = px as f32 / size.width().max(size.height());
    let mut pixmap = tiny_skia::Pixmap::new(px, px)?;
    let dx = (px as f32 - size.width() * scale) / 2.0;
    let dy = (px as f32 - size.height() * scale) / 2.0;
    resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale).post_translate(dx, dy), &mut pixmap.as_mut());
    Some(Image { width: px as usize, height: px as usize, rgba: pixmap.take() })
}

/// Directories icon themes live in.
fn icon_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(Path::new(&home).join(".icons"));
    }
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".local/share")));
    if let Some(d) = data_home {
        dirs.push(d.join("icons"));
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    dirs.extend(data_dirs.split(':').filter(|d| !d.is_empty()).map(|d| Path::new(d).join("icons")));
    dirs
}

/// One theme's subdirectories: (path, nominal size, scalable).
struct Theme {
    dirs: Vec<(PathBuf, u32, bool)>,
    inherits: Vec<String>,
}

fn load_theme(name: &str) -> Option<Theme> {
    let bases: Vec<PathBuf> = icon_dirs().into_iter().map(|d| d.join(name)).filter(|d| d.is_dir()).collect();
    let index = bases.iter().find_map(|b| std::fs::read_to_string(b.join("index.theme")).ok())?;
    let mut directories: Vec<String> = Vec::new();
    let mut inherits = Vec::new();
    let mut sections: HashMap<String, (u32, bool)> = HashMap::new();
    let mut section = String::new();
    for line in index.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.to_owned();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let (key, value) = (key.trim(), value.trim());
        if section == "Icon Theme" {
            match key {
                "Directories" | "ScaledDirectories" => directories.extend(value.split(',').map(|d| d.trim().to_owned()).filter(|d| !d.is_empty())),
                "Inherits" => inherits = value.split(',').map(|t| t.trim().to_owned()).filter(|t| !t.is_empty()).collect(),
                _ => {}
            }
        } else {
            let entry = sections.entry(section.clone()).or_insert((0, false));
            match key {
                "Size" => entry.0 = value.parse().unwrap_or(0),
                "Type" => entry.1 = value == "Scalable",
                _ => {}
            }
        }
    }
    let mut dirs = Vec::new();
    for dir in directories {
        let (size, scalable) = sections.get(&dir).copied().unwrap_or((0, false));
        for base in &bases {
            dirs.push((base.join(&dir), size, scalable));
        }
    }
    Some(Theme { dirs, inherits })
}

fn themes() -> &'static Mutex<HashMap<String, Option<std::sync::Arc<Theme>>>> {
    static THEMES: OnceLock<Mutex<HashMap<String, Option<std::sync::Arc<Theme>>>>> = OnceLock::new();
    THEMES.get_or_init(Default::default)
}

fn theme(name: &str) -> Option<std::sync::Arc<Theme>> {
    let mut cache = themes().lock().unwrap_or_else(|e| e.into_inner());
    cache.entry(name.to_owned()).or_insert_with(|| load_theme(name).map(std::sync::Arc::new)).clone()
}

/// The best file for icon `name` at `px` in `theme_name`, its parents, then
/// `hicolor`, then `/usr/share/pixmaps`.
pub fn find_icon(name: &str, px: u32, theme_name: &str) -> Option<PathBuf> {
    let mut queue = vec![theme_name.to_owned()];
    let mut seen = Vec::new();
    while let Some(current) = queue.first().cloned() {
        queue.remove(0);
        if seen.contains(&current) {
            continue;
        }
        seen.push(current.clone());
        if let Some(theme) = theme(&current) {
            // Scalable first, then the smallest size at least `px`, then
            // the largest.
            let mut best: Option<(PathBuf, i64)> = None;
            for (dir, size, scalable) in &theme.dirs {
                for ext in ["svg", "png"] {
                    let path = dir.join(format!("{name}.{ext}"));
                    if !path.is_file() {
                        continue;
                    }
                    let score = if *scalable || ext == "svg" {
                        0
                    } else if *size >= px {
                        i64::from(*size - px) + 1
                    } else {
                        10_000 + i64::from(px - *size)
                    };
                    if best.as_ref().is_none_or(|(_, s)| score < *s) {
                        best = Some((path, score));
                    }
                }
            }
            if let Some((path, _)) = best {
                return Some(path);
            }
            queue.extend(theme.inherits.iter().cloned());
        }
        if queue.is_empty() && !seen.iter().any(|t| t == "hicolor") {
            queue.push("hicolor".into());
        }
    }
    ["png", "svg", "xpm"]
        .iter()
        .map(|ext| Path::new("/usr/share/pixmaps").join(format!("{name}.{ext}")))
        .find(|p| p.is_file() && !p.extension().is_some_and(|e| e == "xpm"))
}

fn current_theme() -> String {
    use gio::prelude::*;
    const SCHEMA: &str = "org.gnome.desktop.interface";
    if helix_settings::has_schema(SCHEMA) {
        gio::Settings::new(SCHEMA).string("icon-theme").to_string()
    } else {
        "hicolor".into()
    }
}

/// A symbolic icon from the theme (else Adwaita) at `px`, drawn in `rgb`.
pub fn symbolic(name: &str, px: u32, rgb: [u8; 3]) -> Option<Image> {
    let path = find_icon(name, px, &current_theme()).or_else(|| find_icon(name, px, "Adwaita"))?;
    let mut image = decode(&std::fs::read(path).ok()?, px)?;
    recolor(&mut image, rgb);
    Some(image)
}

/// Paint every pixel of `image` in `rgb`, keeping its coverage. Pixels
/// are premultiplied: a half-covered edge pixel gets half the color, or
/// edges come out too bright and the icon looks jagged.
fn recolor(image: &mut Image, rgb: [u8; 3]) {
    for p in image.rgba.as_chunks_mut::<4>().0 {
        let a = u16::from(p[3]);
        for (c, v) in p.iter_mut().zip(rgb) {
            *c = ((u16::from(v) * a + 127) / 255) as u8;
        }
    }
}

/// The icon at `px` pixels, if it can be found and decoded.
pub fn load(source: &IconSource, px: u32) -> Option<Image> {
    match source {
        IconSource::Data(image) => Some(image.clone()),
        IconSource::Bytes(bytes) => decode(bytes, px),
        IconSource::File(path) => decode(&std::fs::read(path).ok()?, px),
        IconSource::Names(names) => {
            let theme = current_theme();
            names.iter().find_map(|name| decode(&std::fs::read(find_icon(name, px, &theme)?).ok()?, px))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{Array, Structure};

    #[test]
    fn sources() {
        assert_eq!(IconSource::from_string(""), None);
        assert_eq!(IconSource::from_string("mail-unread"), Some(IconSource::Names(vec!["mail-unread".into()])));
        assert_eq!(IconSource::from_string("/tmp/x.png"), Some(IconSource::File("/tmp/x.png".into())));
        assert_eq!(IconSource::from_string("file:///tmp/a%20b.png"), Some(IconSource::File("/tmp/a b.png".into())));
        let themed = Value::Structure(Structure::from(("themed".to_owned(), Value::Value(Box::new(Value::from(vec!["a", "b"]))))));
        assert_eq!(IconSource::from_gicon(&themed), Some(IconSource::Names(vec!["a".into(), "b".into()])));
    }

    #[test]
    fn image_data() {
        // 1×1 RGB with a 4-byte rowstride.
        let data = Value::Structure(Structure::from((1i32, 1i32, 4i32, false, 8i32, 3i32, Array::from(vec![10u8, 20, 30, 0]))));
        let Some(IconSource::Data(image)) = IconSource::from_image_data(&data) else { panic!("not decoded") };
        assert_eq!(image.rgba, vec![10, 20, 30, 255]);
    }

    #[test]
    fn recolor_keeps_premultiplied() {
        let mut image = Image { width: 2, height: 1, rgba: vec![0, 0, 0, 255, 0, 0, 0, 128] };
        recolor(&mut image, [255, 200, 0]);
        assert_eq!(image.rgba, vec![255, 200, 0, 255, 128, 100, 0, 128]);
    }

    #[test]
    fn svg_and_png() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10" fill="red"/></svg>"#;
        let image = decode(svg, 16).unwrap();
        assert_eq!((image.width, image.height), (16, 16));
        assert_eq!(&image.rgba[0..4], &[255, 0, 0, 255]);
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.write_header().unwrap().write_image_data(&[0, 0, 255, 128]).unwrap();
        }
        assert_eq!(decode(&png, 16).unwrap().rgba, vec![0, 0, 128, 128], "premultiplied");
    }
}
