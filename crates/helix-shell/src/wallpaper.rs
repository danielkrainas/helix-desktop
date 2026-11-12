//! The background picture (helix-desktop-spec
//! `contracts/gsettings/background.md`): settings, decoding, slideshows and
//! where the picture goes on a monitor.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gio::prelude::*;
use tokio::sync::mpsc;

use crate::UiEvent;

pub const BACKGROUND: &str = "org.gnome.desktop.background";
pub const SCREENSAVER: &str = "org.gnome.desktop.screensaver";
const INTERFACE: &str = "org.gnome.desktop.interface";
/// SVGs are drawn this large (longer side) at least (filters make larger
/// renders slow).
const SVG_SIZE: f32 = 1920.0;

/// `picture-options`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fit {
    None,
    Wallpaper,
    Centered,
    Scaled,
    Stretched,
    #[default]
    Zoom,
    Spanned,
}

impl Fit {
    pub fn parse(s: &str) -> Self {
        match s {
            "none" => Fit::None,
            "wallpaper" => Fit::Wallpaper,
            "centered" => Fit::Centered,
            "scaled" => Fit::Scaled,
            "stretched" => Fit::Stretched,
            "spanned" => Fit::Spanned,
            _ => Fit::Zoom,
        }
    }
}

/// `color-shading-type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Shading {
    #[default]
    Solid,
    Vertical,
    Horizontal,
}

/// A decoded picture: RGBA, not premultiplied.
#[derive(Debug, PartialEq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// What to draw: the settings and the picture.
#[derive(Debug, Clone, PartialEq)]
pub struct Background {
    pub fit: Fit,
    /// 0.0 to 1.0.
    pub opacity: f32,
    pub primary: [u8; 3],
    pub secondary: [u8; 3],
    pub shading: Shading,
    pub image: Option<Arc<Image>>,
}

impl Default for Background {
    fn default() -> Self {
        Self { fit: Fit::Zoom, opacity: 1.0, primary: [0x02, 0x3c, 0x88], secondary: [0x57, 0x89, 0xca], shading: Shading::Solid, image: None }
    }
}

/// `#rrggbb`.
pub fn parse_color(s: &str) -> Option<[u8; 3]> {
    let hex = s.trim().strip_prefix('#')?;
    if hex.len() != 6 || !hex.is_ascii() {
        return None;
    }
    let b = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some([b(0)?, b(2)?, b(4)?])
}

/// A `picture-uri` as a path.
pub fn uri_path(uri: &str) -> Option<PathBuf> {
    let uri = uri.trim();
    if uri.is_empty() {
        return None;
    }
    if uri.starts_with('/') {
        return Some(PathBuf::from(uri));
    }
    if uri.starts_with("file://") {
        return gio::File::for_uri(uri).path();
    }
    None
}

// ----- Decoding ---------------------------------------------------------

fn rgba_from(width: u32, height: u32, data: Vec<u8>, channels: usize) -> Result<Image, String> {
    let pixels = width as usize * height as usize;
    if data.len() < pixels * channels {
        return Err("short image data".into());
    }
    let rgba = match channels {
        4 => data,
        3 => data.as_chunks::<3>().0.iter().take(pixels).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        2 => data.as_chunks::<2>().0.iter().take(pixels).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        1 => data.iter().take(pixels).flat_map(|g| [*g, *g, *g, 255]).collect(),
        _ => return Err("unsupported channels".into()),
    };
    Ok(Image { width, height, rgba })
}

fn decode_png(data: &[u8]) -> Result<Image, String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    buf.truncate(info.buffer_size());
    rgba_from(info.width, info.height, buf, info.color_type.samples())
}

fn decode_jpeg(data: &[u8]) -> Result<Image, String> {
    let options = zune_jpeg::zune_core::options::DecoderOptions::default().jpeg_set_out_colorspace(zune_jpeg::zune_core::colorspace::ColorSpace::RGB);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(zune_jpeg::zune_core::bytestream::ZCursor::new(data), options);
    let pixels = decoder.decode().map_err(|e| format!("{e:?}"))?;
    let info = decoder.info().ok_or("no JPEG header")?;
    rgba_from(u32::from(info.width), u32::from(info.height), pixels, 3)
}

fn decode_webp(data: &[u8]) -> Result<Image, String> {
    let mut decoder = image_webp::WebPDecoder::new(std::io::Cursor::new(data)).map_err(|e| e.to_string())?;
    let (width, height) = decoder.dimensions();
    let channels = if decoder.has_alpha() { 4 } else { 3 };
    let mut buf = vec![0; decoder.output_buffer_size().ok_or("WebP too large")?];
    decoder.read_image(&mut buf).map_err(|e| e.to_string())?;
    rgba_from(width, height, buf, channels)
}

fn decode_jxl(data: &[u8]) -> Result<Image, String> {
    let image = jxl_oxide::JxlImage::builder().read(std::io::Cursor::new(data)).map_err(|e| e.to_string())?;
    let render = image.render_frame(0).map_err(|e| e.to_string())?;
    let mut stream = render.stream();
    let (width, height, channels) = (stream.width(), stream.height(), stream.channels() as usize);
    let mut buf = vec![0u8; width as usize * height as usize * channels];
    stream.write_to_buffer(&mut buf);
    rgba_from(width, height, buf, channels)
}

fn decode_svg(data: &[u8], base: &Path) -> Result<Image, String> {
    let options = resvg::usvg::Options { resources_dir: base.parent().map(Path::to_path_buf), ..Default::default() };
    let tree = resvg::usvg::Tree::from_data(data, &options).map_err(|e| e.to_string())?;
    let size = tree.size();
    let scale = (SVG_SIZE / size.width().max(size.height())).max(1.0);
    let (w, h) = ((size.width() * scale).ceil() as u32, (size.height() * scale).ceil() as u32);
    let mut pixmap = resvg::tiny_skia::Pixmap::new(w.max(1), h.max(1)).ok_or("SVG too large")?;
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    // tiny-skia is premultiplied.
    let mut rgba = pixmap.take();
    for px in rgba.as_chunks_mut::<4>().0 {
        let a = u32::from(px[3]);
        if a > 0 && a < 255 {
            for c in &mut px[..3] {
                *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    Ok(Image { width: w, height: h, rgba })
}

/// Decode a picture file, by its content.
pub fn decode(path: &Path) -> Result<Image, String> {
    let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let head = &data[..data.len().min(512)];
    if head.starts_with(b"\x89PNG") {
        decode_png(&data)
    } else if head.starts_with(&[0xFF, 0xD8]) {
        decode_jpeg(&data)
    } else if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        decode_webp(&data)
    } else if head.starts_with(&[0xFF, 0x0A]) || head.starts_with(&[0, 0, 0, 0x0C, b'J', b'X', b'L', b' ']) {
        decode_jxl(&data)
    } else if String::from_utf8_lossy(head).contains("<svg") {
        decode_svg(&data, path)
    } else {
        Err(format!("{}: unknown picture format", path.display()))
    }
}

// ----- Slideshows -------------------------------------------------------

/// A slideshow part: (seconds, picture now, picture in the second half).
#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    pub seconds: f64,
    pub first: PathBuf,
    pub second: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Slideshow {
    /// Start, in local seconds since the epoch.
    pub start: i64,
    pub parts: Vec<Part>,
}

/// A `<file>` (or `<from>`/`<to>`): a path, or the largest `<size>`.
fn file_of(node: roxmltree::Node<'_, '_>) -> Option<PathBuf> {
    let sizes: Vec<(u64, &str)> = node
        .children()
        .filter(|c| c.has_tag_name("size"))
        .filter_map(|c| {
            let w: u64 = c.attribute("width")?.parse().ok()?;
            let h: u64 = c.attribute("height")?.parse().ok()?;
            Some((w * h, c.text()?.trim()))
        })
        .collect();
    if let Some((_, path)) = sizes.iter().max_by_key(|(area, _)| *area) {
        return Some(PathBuf::from(path));
    }
    node.text().map(|t| PathBuf::from(t.trim())).filter(|p| !p.as_os_str().is_empty())
}

/// Parse GNOME's background XML.
pub fn parse_slideshow(xml: &str) -> Option<Slideshow> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let root = doc.root_element();
    if !root.has_tag_name("background") {
        return None;
    }
    let child_text = |n: roxmltree::Node<'_, '_>, tag: &str| n.children().find(|c| c.has_tag_name(tag)).and_then(|c| c.text()).map(|t| t.trim().to_owned());
    let start = root.children().find(|c| c.has_tag_name("starttime")).and_then(|st| {
        let get = |t: &str| child_text(st, t).and_then(|v| v.parse::<i32>().ok());
        let dt = gio::glib::DateTime::from_local(get("year")?, get("month")?, get("day")?, get("hour").unwrap_or(0), get("minute").unwrap_or(0), f64::from(get("second").unwrap_or(0))).ok()?;
        Some(dt.to_unix())
    })?;
    let mut parts = Vec::new();
    for node in root.children().filter(|c| c.is_element()) {
        let seconds = child_text(node, "duration").and_then(|d| d.parse::<f64>().ok()).unwrap_or(0.0);
        if node.has_tag_name("static") {
            let file = file_of(node.children().find(|c| c.has_tag_name("file"))?)?;
            parts.push(Part { seconds, first: file.clone(), second: file });
        } else if node.has_tag_name("transition") {
            let from = file_of(node.children().find(|c| c.has_tag_name("from"))?)?;
            let to = file_of(node.children().find(|c| c.has_tag_name("to"))?)?;
            parts.push(Part { seconds, first: from, second: to });
        }
    }
    (!parts.is_empty() && parts.iter().any(|p| p.seconds > 0.0)).then_some(Slideshow { start, parts })
}

impl Slideshow {
    /// The picture at `now` (unix seconds) and how long until it changes.
    pub fn at(&self, now: i64) -> (PathBuf, Duration) {
        let total: f64 = self.parts.iter().map(|p| p.seconds.max(0.0)).sum();
        let mut t = ((now - self.start) as f64).rem_euclid(total);
        for part in &self.parts {
            let len = part.seconds.max(0.0);
            if t < len {
                let half = len / 2.0;
                let (path, left) = if part.first == part.second {
                    (&part.first, len - t)
                } else if t < half {
                    (&part.first, half - t)
                } else {
                    (&part.second, len - t)
                };
                return (path.clone(), Duration::from_secs_f64(left.max(1.0)));
            }
            t -= len;
        }
        (self.parts[0].first.clone(), Duration::from_secs(60))
    }
}

/// The picture a file names now, and when to look again (slideshows).
pub fn resolve(path: &Path, now: i64) -> (PathBuf, Option<Duration>) {
    let Ok(text) = std::fs::read(path) else { return (path.to_path_buf(), None) };
    let head = String::from_utf8_lossy(&text[..text.len().min(512)]).to_string();
    if !head.contains("<background") {
        return (path.to_path_buf(), None);
    }
    match parse_slideshow(&String::from_utf8_lossy(&text)) {
        Some(show) => {
            let (picture, next) = show.at(now);
            (picture, Some(next))
        }
        None => (path.to_path_buf(), None),
    }
}

// ----- Placement --------------------------------------------------------

/// The layout's bounding box (x, y, w, h) and a monitor's offset in it.
pub type Span = ((f64, f64, f64, f64), (f64, f64));

/// Where the picture goes, in a monitor's logical coordinates: the
/// rectangle one copy covers (x, y, w, h), and whether it repeats.
pub fn place(fit: Fit, image: (f64, f64), monitor: (f64, f64), span: Option<Span>) -> Option<((f64, f64, f64, f64), bool)> {
    let (iw, ih) = image;
    let (mw, mh) = monitor;
    if iw <= 0.0 || ih <= 0.0 {
        return None;
    }
    let centered = |w: f64, h: f64| ((mw - w) / 2.0, (mh - h) / 2.0, w, h);
    Some(match fit {
        Fit::None => return None,
        Fit::Wallpaper => ((0.0, 0.0, iw, ih), true),
        Fit::Centered => (centered(iw, ih), false),
        Fit::Scaled => {
            let s = (mw / iw).min(mh / ih);
            (centered(iw * s, ih * s), false)
        }
        Fit::Stretched => ((0.0, 0.0, mw, mh), false),
        Fit::Zoom => {
            let s = (mw / iw).max(mh / ih);
            (centered(iw * s, ih * s), false)
        }
        Fit::Spanned => match span {
            // The layout's box, and this monitor's offset in it.
            Some(((_, _, bw, bh), (ox, oy))) => {
                let s = (bw / iw).max(bh / ih);
                let (w, h) = (iw * s, ih * s);
                (((bw - w) / 2.0 - ox, (bh - h) / 2.0 - oy, w, h), false)
            }
            None => {
                let s = (mw / iw).max(mh / ih);
                (centered(iw * s, ih * s), false)
            }
        },
    })
}

// ----- Settings and following them -------------------------------------

fn settings(schema: &str) -> Option<gio::Settings> {
    gio::SettingsSchemaSource::default()?.lookup(schema, true).map(|_| gio::Settings::new(schema))
}

fn dark() -> bool {
    settings(INTERFACE).is_some_and(|s| s.settings_schema().is_some_and(|x| x.has_key("color-scheme")) && s.string("color-scheme") == "prefer-dark")
}

/// The background settings of `schema` (without the picture) and the
/// picture file it names.
pub fn read(schema: &str) -> (Background, Option<PathBuf>) {
    let d = Background::default();
    let Some(s) = settings(schema) else { return (d, None) };
    let has = |k: &str| s.settings_schema().is_some_and(|x| x.has_key(k));
    let string = |k: &str| if has(k) { s.string(k).to_string() } else { String::new() };
    let mut uri = string("picture-uri");
    if schema == BACKGROUND && dark() && !string("picture-uri-dark").is_empty() {
        uri = string("picture-uri-dark");
    }
    let shading = match string("color-shading-type").as_str() {
        "vertical" => Shading::Vertical,
        "horizontal" => Shading::Horizontal,
        _ => Shading::Solid,
    };
    let background = Background {
        fit: Fit::parse(&string("picture-options")),
        opacity: if has("picture-opacity") { (s.int("picture-opacity").clamp(0, 100) as f32) / 100.0 } else { 1.0 },
        primary: parse_color(&string("primary-color")).unwrap_or(d.primary),
        secondary: parse_color(&string("secondary-color")).unwrap_or(d.secondary),
        shading,
        image: None,
    };
    (background, uri_path(&uri))
}

/// Decoded pictures, kept while in use.
#[derive(Default)]
struct Cache {
    entries: Vec<(PathBuf, Arc<Image>)>,
    /// Pictures that could not be read (not tried again).
    failed: Vec<PathBuf>,
}

impl Cache {
    async fn get(&mut self, path: &Path) -> Option<Arc<Image>> {
        if let Some((_, image)) = self.entries.iter().find(|(p, _)| p == path) {
            return Some(image.clone());
        }
        if self.failed.iter().any(|p| p == path) {
            return None;
        }
        let owned = path.to_path_buf();
        match tokio::task::spawn_blocking(move || decode(&owned)).await {
            Ok(Ok(image)) => {
                let image = Arc::new(image);
                self.entries.push((path.to_path_buf(), image.clone()));
                if self.entries.len() > 4 {
                    self.entries.remove(0);
                }
                Some(image)
            }
            Ok(Err(err)) => {
                tracing::warn!(err, "cannot show the background picture");
                self.failed.push(path.to_path_buf());
                None
            }
            Err(_) => None,
        }
    }
}

/// The background of `schema` now, and when to look again.
async fn current(cache: &mut Cache, schema: &str) -> (Background, Option<Duration>) {
    let (mut background, file) = read(schema);
    let mut next = None;
    if let Some(file) = file.filter(|_| background.fit != Fit::None) {
        let now = gio::glib::DateTime::now_local().map(|d| d.to_unix()).unwrap_or(0);
        let (picture, again) = resolve(&file, now);
        next = again;
        background.image = cache.get(&picture).await;
    }
    (background, next)
}

/// Keep the UI's desktop and lock screen backgrounds current.
pub async fn follow(ui: mpsc::UnboundedSender<UiEvent>) {
    let watched = helix_settings::GSettings::new(&[BACKGROUND, SCREENSAVER, INTERFACE]);
    let (mut changes, _watch) = watched.watch();
    let mut cache = Cache::default();
    loop {
        let (desktop, next_desktop) = current(&mut cache, BACKGROUND).await;
        let (lock, next_lock) = current(&mut cache, SCREENSAVER).await;
        let _ = ui.send(UiEvent::Background { desktop: Box::new(desktop), lock: Box::new(lock) });
        let wait = [next_desktop, next_lock].into_iter().flatten().min();
        let timer = async {
            match wait {
                Some(d) => tokio::time::sleep(d).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            change = changes.recv() => {
                if change.is_none() {
                    return;
                }
                // Several keys change together; take them at once.
                tokio::time::sleep(Duration::from_millis(100)).await;
                while changes.try_recv().is_ok() {}
            }
            _ = timer => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placement() {
        let m = (1000.0, 500.0);
        assert_eq!(place(Fit::Zoom, (500.0, 500.0), m, None), Some(((0.0, -250.0, 1000.0, 1000.0), false)));
        assert_eq!(place(Fit::Scaled, (500.0, 500.0), m, None), Some(((250.0, 0.0, 500.0, 500.0), false)));
        assert_eq!(place(Fit::Centered, (100.0, 100.0), m, None), Some(((450.0, 200.0, 100.0, 100.0), false)));
        assert_eq!(place(Fit::Stretched, (100.0, 100.0), m, None), Some(((0.0, 0.0, 1000.0, 500.0), false)));
        assert_eq!(place(Fit::Wallpaper, (64.0, 64.0), m, None), Some(((0.0, 0.0, 64.0, 64.0), true)));
        assert_eq!(place(Fit::None, (64.0, 64.0), m, None), None);
        // Spanned over two 1000x500 monitors side by side: the right one
        // shows the right half.
        let span = Some(((0.0, 0.0, 2000.0, 500.0), (1000.0, 0.0)));
        assert_eq!(place(Fit::Spanned, (2000.0, 1000.0), m, span), Some(((-1000.0, -250.0, 2000.0, 1000.0), false)));
    }

    #[test]
    fn slideshows() {
        let xml = r#"<background>
          <starttime><year>2009</year><month>08</month><day>04</day><hour>00</hour><minute>00</minute><second>00</second></starttime>
          <static><duration>100.0</duration><file>/a.jpg</file></static>
          <transition type="overlay"><duration>10.0</duration><from>/a.jpg</from><to>/b.jpg</to></transition>
          <static><duration>100.0</duration><file><size width="800" height="600">/b-small.jpg</size><size width="1920" height="1080">/b.jpg</size></file></static>
        </background>"#;
        let show = parse_slideshow(xml).unwrap();
        assert_eq!(show.parts.len(), 3);
        assert_eq!(show.parts[2].first, PathBuf::from("/b.jpg"), "the largest size");
        let s = show.start;
        assert_eq!(show.at(s + 10).0, PathBuf::from("/a.jpg"));
        assert_eq!(show.at(s + 103).0, PathBuf::from("/a.jpg"), "first half of the transition");
        assert_eq!(show.at(s + 107).0, PathBuf::from("/b.jpg"));
        assert_eq!(show.at(s + 210 + 5).0, PathBuf::from("/a.jpg"), "loops");
        assert_eq!(show.at(s + 10).1, Duration::from_secs(90));
        assert!(parse_slideshow("<other/>").is_none());
    }

    #[test]
    fn colors_and_uris() {
        assert_eq!(parse_color("#023c88"), Some([2, 60, 136]));
        assert_eq!(parse_color("blue"), None);
        assert_eq!(uri_path("file:///usr/share/a%20b.png"), Some(PathBuf::from("/usr/share/a b.png")));
        assert_eq!(uri_path("/x.png"), Some(PathBuf::from("/x.png")));
        assert_eq!(uri_path(""), None);
    }

    #[test]
    fn decodes_host_pictures() {
        for path in ["/usr/share/backgrounds/Clouds_by_Tibor_Mokanszki.jpg", "/usr/share/backgrounds/Northan_lights_by_mizuno.webp", "/usr/share/backgrounds/greybird.svg", "/usr/share/backgrounds/warty-final-ubuntu.png"] {
            let path = Path::new(path);
            if !path.exists() {
                continue;
            }
            let image = decode(path).unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(image.rgba.len(), image.width as usize * image.height as usize * 4, "{}", path.display());
        }
    }
}
