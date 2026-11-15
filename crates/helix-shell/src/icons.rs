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
        if id.is_empty() {
            return None;
        }
        apps().icons.get(&format!("{id}.desktop")).cloned().flatten()
    }
}

/// How long one reading of the installed apps is used: asking for every
/// app's icon (the app grid), or the dock updating for every window
/// opened, reads them (some hundred desktop files) once, not each time.
const APPS_FRESH: std::time::Duration = std::time::Duration::from_secs(5);

/// The installed apps, as GIO lists them.
#[derive(Default)]
pub struct Apps {
    /// Desktop file ids, in GIO's order.
    pub ids: Vec<String>,
    /// The ones shown in menus (`should_show`).
    pub shown: Vec<String>,
    /// Icons by desktop file id.
    icons: HashMap<String, Option<IconSource>>,
}

/// The installed apps (read again when older than [`APPS_FRESH`]).
pub fn apps() -> std::sync::Arc<Apps> {
    use gio::prelude::*;
    static APPS: Mutex<Option<(std::time::Instant, std::sync::Arc<Apps>)>> = Mutex::new(None);
    let mut cached = APPS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, apps)) = cached.as_ref().filter(|(read, _)| read.elapsed() < APPS_FRESH) {
        return apps.clone();
    }
    let mut apps = Apps::default();
    for info in gio::AppInfo::all() {
        let Some(id) = info.id().map(|i| i.to_string()) else { continue };
        let icon = info.icon().and_then(|icon| {
            if let Some(themed) = icon.downcast_ref::<gio::ThemedIcon>() {
                return Some(IconSource::Names(themed.names().iter().map(|n| n.to_string()).collect()));
            }
            icon.downcast_ref::<gio::FileIcon>().and_then(|file| file.file().path()).map(IconSource::File)
        });
        if info.should_show() {
            apps.shown.push(id.clone());
        }
        apps.ids.push(id.clone());
        apps.icons.insert(id, icon);
    }
    let apps = std::sync::Arc::new(apps);
    *cached = Some((std::time::Instant::now(), apps.clone()));
    apps
}

/// A key for `source`'s picture (the same picture gives the same key), to
/// cache it by.
pub fn source_key(source: &IconSource) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    match source {
        IconSource::Data(image) => (0u8, image.width, image.height, &image.rgba).hash(&mut h),
        IconSource::File(path) => (1u8, path).hash(&mut h),
        IconSource::Names(names) => (2u8, names).hash(&mut h),
        IconSource::Bytes(bytes) => (3u8, bytes).hash(&mut h),
    }
    format!("{:016x}", h.finish())
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
    /// Directory and its image size in pixels (`Size` × `Scale`). Only
    /// SVG files scale: a PNG is its directory's size even where the theme
    /// calls the directory scalable (as elementary-xfce does for its
    /// fixed-size PNGs), so `Type` is not read.
    dirs: Vec<(PathBuf, u32)>,
    inherits: Vec<String>,
}

fn load_theme(name: &str) -> Option<Theme> {
    let bases: Vec<PathBuf> = icon_dirs().into_iter().map(|d| d.join(name)).filter(|d| d.is_dir()).collect();
    let index = bases.iter().find_map(|b| std::fs::read_to_string(b.join("index.theme")).ok())?;
    let mut directories: Vec<String> = Vec::new();
    let mut inherits = Vec::new();
    // Size, scale.
    let mut sections: HashMap<String, (u32, u32)> = HashMap::new();
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
            let entry = sections.entry(section.clone()).or_insert((0, 1));
            match key {
                "Size" => entry.0 = value.parse().unwrap_or(0),
                "Scale" => entry.1 = value.parse().unwrap_or(1).max(1),
                _ => {}
            }
        }
    }
    let mut dirs = Vec::new();
    for dir in directories {
        let (size, scale) = sections.get(&dir).copied().unwrap_or((0, 1));
        for base in &bases {
            dirs.push((base.join(&dir), size * scale));
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
/// `hicolor`, then `/usr/share/pixmaps` (helix-desktop-spec
/// `contracts/wayland/README.md`, "App and theme icons").
pub fn find_icon(name: &str, px: u32, theme_name: &str) -> Option<PathBuf> {
    find_in(name, px, theme_name, &theme).or_else(|| {
        ["png", "svg", "xpm"]
            .iter()
            .map(|ext| Path::new("/usr/share/pixmaps").join(format!("{name}.{ext}")))
            .find(|p| p.is_file() && !p.extension().is_some_and(|e| e == "xpm"))
    })
}

/// Scores at least this mean the image is smaller than wanted.
const TOO_SMALL: i64 = 10_000;

/// [`find_icon`] through the themes `theme` gives (without the pixmaps
/// fallback). A theme whose best image is smaller than `px` does not end
/// the search: a later theme with a scalable or large enough image wins,
/// else that small image is used.
fn find_in(name: &str, px: u32, theme_name: &str, theme: &dyn Fn(&str) -> Option<std::sync::Arc<Theme>>) -> Option<PathBuf> {
    let mut queue = vec![theme_name.to_owned()];
    let mut seen = Vec::new();
    let mut small: Option<PathBuf> = None;
    while let Some(current) = queue.first().cloned() {
        queue.remove(0);
        if seen.contains(&current) {
            continue;
        }
        seen.push(current.clone());
        if let Some(theme) = theme(&current) {
            // SVG first, then the smallest PNG at least `px`, then
            // the largest.
            let mut best: Option<(PathBuf, i64)> = None;
            for (dir, size) in &theme.dirs {
                for ext in ["svg", "png"] {
                    let path = dir.join(format!("{name}.{ext}"));
                    if !path.is_file() {
                        continue;
                    }
                    let score = if ext == "svg" {
                        0
                    } else if *size >= px {
                        i64::from(*size - px) + 1
                    } else {
                        TOO_SMALL + i64::from(px - *size)
                    };
                    if best.as_ref().is_none_or(|(_, s)| score < *s) {
                        best = Some((path, score));
                    }
                }
            }
            match best {
                Some((path, score)) if score < TOO_SMALL => return Some(path),
                // Too small: remembered (the first theme's), and the
                // search goes on for a sharper one.
                Some((path, _)) => {
                    small.get_or_insert(path);
                }
                None => {}
            }
            queue.extend(theme.inherits.iter().cloned());
        }
        if queue.is_empty() && !seen.iter().any(|t| t == "hicolor") {
            queue.push("hicolor".into());
        }
    }
    small
}

/// Tells `ui` whenever `icon-theme` changes, for the rest of the process
/// (helix-desktop-spec `contracts/wayland/README.md`, "System status").
pub async fn follow(ui: tokio::sync::mpsc::UnboundedSender<crate::UiEvent>) {
    const SCHEMA: &str = "org.gnome.desktop.interface";
    if !helix_settings::has_schema(SCHEMA) {
        return;
    }
    let watched = helix_settings::GSettings::new(&[SCHEMA]);
    let (mut changes, _watch) = watched.watch();
    let mut theme = read_theme();
    while changes.recv().await.is_some() {
        let now = read_theme();
        if now != theme {
            theme = now;
            if ui.send(crate::UiEvent::IconTheme).is_err() {
                break;
            }
        }
    }
}

/// The icon theme in use (read once, until [`forget_theme`]).
fn current_theme() -> String {
    let mut theme = THEME.lock().unwrap_or_else(|e| e.into_inner());
    theme.get_or_insert_with(read_theme).clone()
}

static THEME: Mutex<Option<String>> = Mutex::new(None);

/// `icon-theme` changed: read it again.
pub fn forget_theme() {
    *THEME.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

fn read_theme() -> String {
    const SCHEMA: &str = "org.gnome.desktop.interface";
    if helix_settings::has_schema(SCHEMA) {
        helix_settings::Settings::new(SCHEMA).string("icon-theme").to_string()
    } else {
        "hicolor".into()
    }
}

/// A symbolic icon from the theme (else Adwaita) at `px`, drawn in `rgb`.
pub fn symbolic(name: &str, px: u32, rgb: [u8; 3]) -> Option<Image> {
    let path = find_icon(name, px, &current_theme()).or_else(|| find_icon(name, px, "Adwaita"))?;
    let mut image = decode_file(&path, px)?;
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
        IconSource::File(path) => decode_file(path, px),
        IconSource::Names(names) => {
            let theme = current_theme();
            names.iter().find_map(|name| decode_file(&find_icon(name, px, &theme)?, px))
        }
    }
}

/// The theme's generic application icon, for apps whose own icon cannot
/// be found.
pub const GENERIC_APP: &str = "application-x-executable";

/// App `id`'s icon (desktop file id without `.desktop`; also tried in
/// lower case) at `px` pixels, else the generic application icon.
pub fn app_icon(id: &str, px: u32) -> Option<Image> {
    let source = IconSource::for_app(id).or_else(|| IconSource::for_app(&id.to_lowercase()));
    or_generic(id, source.as_ref(), px, load, generic_app)
}

/// The icon from `source`, else (none, or not decodable) `generic`'s.
/// A missing icon is logged once per app.
fn or_generic(
    id: &str,
    source: Option<&IconSource>,
    px: u32,
    load: impl Fn(&IconSource, u32) -> Option<Image>,
    generic: impl Fn(u32) -> Option<Image>,
) -> Option<Image> {
    if let Some(image) = source.and_then(|s| load(s, px)) {
        return Some(image);
    }
    static LOGGED: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let mut logged = LOGGED.lock().unwrap_or_else(|e| e.into_inner());
    if !logged.iter().any(|l| l == id) {
        logged.push(id.to_owned());
        let named = match source {
            Some(IconSource::Names(names)) => names.join(","),
            Some(IconSource::File(path)) => path.display().to_string(),
            Some(_) => "image data".into(),
            None => String::new(),
        };
        tracing::debug!(app = id, icon = named, "no icon for the app; showing the generic one");
    }
    drop(logged);
    generic(px)
}

/// `application-x-executable` at `px`, else its symbolic form in light
/// grey; decoded once per size.
fn generic_app(px: u32) -> Option<Image> {
    static DONE: Mutex<Vec<(u32, Option<Image>)>> = Mutex::new(Vec::new());
    if let Some((_, image)) = DONE.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(p, _)| *p == px) {
        return image.clone();
    }
    let image = load(&IconSource::Names(vec![GENERIC_APP.into()]), px)
        .or_else(|| symbolic(&format!("{GENERIC_APP}-symbolic"), px, [0xDE, 0xDD, 0xDA]));
    DONE.lock().unwrap_or_else(|e| e.into_inner()).push((px, image.clone()));
    image
}

/// The icon file at `path` decoded at about `px` pixels; rendered SVGs
/// come from the disk cache when it has them.
pub fn decode_file(path: &Path, px: u32) -> Option<Image> {
    decode_file_with(disk_cache(), path, px)
}

fn decode_file_with(cache: Option<&DiskCache>, path: &Path, px: u32) -> Option<Image> {
    let meta = std::fs::metadata(path).ok()?;
    if let Some(found) = cache.and_then(|c| c.get(path, &meta, px)) {
        return Some(found);
    }
    let bytes = std::fs::read(path).ok()?;
    let image = decode(&bytes, px)?;
    // PNGs decode quickly (and at their own size): only rendered SVGs
    // are kept.
    if !bytes.starts_with(b"\x89PNG") {
        if let Some(cache) = cache {
            cache.put(path, &meta, px, &image);
        }
    }
    Some(image)
}

fn disk_cache() -> Option<&'static DiskCache> {
    static CACHE: OnceLock<Option<DiskCache>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            if cfg!(test) {
                // Unit tests use their own (`DiskCache::at`).
                return None;
            }
            let base = std::env::var_os("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".cache")))?;
            let cache = DiskCache::at(base.join("helix/icons"));
            cache.prune(DISK_CACHE_UNUSED);
            Some(cache)
        })
        .as_ref()
}

/// Cached icons not used for this long are removed (when the shell
/// starts using the cache).
const DISK_CACHE_UNUSED: std::time::Duration = std::time::Duration::from_secs(60 * 24 * 3600);

/// A used entry's time is renewed when it is older than this.
const DISK_CACHE_RENEW: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

/// Rendered icons kept between runs (`$XDG_CACHE_HOME/helix/icons`), so a
/// restarted shell does not render every SVG again. An entry is for one
/// icon file as it is (path, modification time and size) at one pixel
/// size (logical size × scale), named by a hash of those; it holds
/// `HXIC`, a version byte, width, height and key length (u32, little
/// endian), the key itself (checked on reading), then premultiplied RGBA.
/// An icon file that changes gets a new key; old entries go when unused.
pub struct DiskCache {
    dir: PathBuf,
}

const DISK_MAGIC: &[u8; 5] = b"HXIC\x01";

/// A file's age by its modification time.
fn age(meta: std::io::Result<std::fs::Metadata>) -> Option<std::time::Duration> {
    meta.and_then(|m| m.modified()).ok()?.elapsed().ok()
}

impl DiskCache {
    pub fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn key(path: &Path, meta: &std::fs::Metadata, px: u32) -> Option<String> {
        let mtime = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
        Some(format!("{}\n{}.{:09}\n{}\n{px}", path.display(), mtime.as_secs(), mtime.subsec_nanos(), meta.len()))
    }

    fn file(&self, key: &str) -> PathBuf {
        // FNV-1a: the same name in every build.
        let hash = key.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
        self.dir.join(format!("{hash:016x}.icon"))
    }

    /// The icon kept for `path` (as `meta` says it is now) at `px`.
    pub fn get(&self, path: &Path, meta: &std::fs::Metadata, px: u32) -> Option<Image> {
        let key = Self::key(path, meta, px)?;
        let file = self.file(&key);
        let data = std::fs::read(&file).ok()?;
        let rest = data.strip_prefix(DISK_MAGIC.as_slice())?;
        let u32_at = |i: usize| rest.get(i..i + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
        let (w, h, key_len) = (u32_at(0)?, u32_at(4)?, u32_at(8)?);
        let stored = rest.get(12..12 + key_len)?;
        let rgba = rest.get(12 + key_len..)?;
        if stored != key.as_bytes() || w == 0 || h == 0 || rgba.len() != w.checked_mul(h)?.checked_mul(4)? {
            return None;
        }
        // Still in use: not to be pruned.
        if age(std::fs::metadata(&file)).is_some_and(|a| a > DISK_CACHE_RENEW) {
            let _ = std::fs::File::options().write(true).open(&file).and_then(|f| f.set_modified(std::time::SystemTime::now()));
        }
        Some(Image { width: w, height: h, rgba: rgba.to_vec() })
    }

    /// Keep `image`, rendered from `path` at `px`.
    pub fn put(&self, path: &Path, meta: &std::fs::Metadata, px: u32, image: &Image) {
        static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let Some(key) = Self::key(path, meta, px) else { return };
        let (Ok(w), Ok(h), Ok(key_len)) = (u32::try_from(image.width), u32::try_from(image.height), u32::try_from(key.len())) else { return };
        let mut data = Vec::with_capacity(DISK_MAGIC.len() + 12 + key.len() + image.rgba.len());
        data.extend_from_slice(DISK_MAGIC);
        for v in [w, h, key_len] {
            data.extend_from_slice(&v.to_le_bytes());
        }
        data.extend_from_slice(key.as_bytes());
        data.extend_from_slice(&image.rgba);
        let file = self.file(&key);
        // Written whole, then renamed: no reader sees half a file.
        let n = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = file.with_extension(format!("tmp-{}-{n}", std::process::id()));
        let written = std::fs::create_dir_all(&self.dir).and_then(|()| std::fs::write(&tmp, &data)).and_then(|()| std::fs::rename(&tmp, &file));
        if let Err(err) = written {
            let _ = std::fs::remove_file(&tmp);
            tracing::debug!(%err, dir = %self.dir.display(), "cannot keep a rendered icon");
        }
    }

    /// Remove entries untouched for `unused` (and temporary files left
    /// by a shell that stopped while writing).
    pub fn prune(&self, unused: std::time::Duration) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return };
        for entry in entries.flatten() {
            let Some(age) = age(entry.metadata()) else { continue };
            let tmp = entry.file_name().to_string_lossy().contains(".tmp-");
            if age > unused || (tmp && age > std::time::Duration::from_secs(3600)) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apps_without_an_icon_get_the_generic_one() {
        let image = |v: u8| Image { width: 1, height: 1, rgba: vec![v; 4] };
        let load = |s: &IconSource, _| match s {
            IconSource::Data(i) => Some(i.clone()),
            _ => None,
        };
        let generic = |_| Some(image(9));
        let own = IconSource::Data(image(1));
        assert_eq!(or_generic("a", Some(&own), 48, load, generic).unwrap().rgba[0], 1, "its own icon");
        assert_eq!(or_generic("b", None, 48, load, generic).unwrap().rgba[0], 9, "no icon named");
        let broken = IconSource::File("/nonexistent/icon.png".into());
        assert_eq!(or_generic("c", Some(&broken), 48, load, generic).unwrap().rgba[0], 9, "the named icon is missing");
        assert!(or_generic("d", None, 48, load, |_| None).is_none(), "no generic icon in the theme either");
    }

    #[test]
    fn small_theme_images_give_way_to_sharper_ones() {
        let root = std::env::temp_dir().join(format!("helix-icon-lookup-{}", std::process::id()));
        let dir = |theme: &str, sub: &str| {
            let d = root.join(theme).join(sub);
            std::fs::create_dir_all(&d).unwrap();
            d
        };
        let file = |d: &Path, name: &str| {
            std::fs::write(d.join(name), b"x").unwrap();
            d.join(name)
        };
        let small = dir("styled", "apps/48");
        let claimed = dir("styled", "apps/32");
        let big = dir("styled", "apps/128");
        let doubled = dir("styled", "apps/48@2");
        let hicolor = dir("hicolor", "256x256/apps");
        let styled_small = file(&small, "blurry.png");
        let hicolor_big = file(&hicolor, "blurry.png");
        let styled_big = file(&big, "fine.png");
        file(&hicolor, "fine.png");
        let only_small = file(&small, "lonely.png");
        let styled_doubled = file(&doubled, "retina.png");
        file(&hicolor, "retina.png");
        file(&claimed, "claims.png");
        let hicolor_claims = file(&hicolor, "claims.png");
        let themes = |name: &str| -> Option<std::sync::Arc<Theme>> {
            Some(std::sync::Arc::new(match name {
                "styled" => Theme { dirs: vec![(small.clone(), 48), (big.clone(), 128), (doubled.clone(), 96), (claimed.clone(), 32)], inherits: vec![] },
                "hicolor" => Theme { dirs: vec![(hicolor.clone(), 256)], inherits: vec![] },
                _ => return None,
            }))
        };
        assert_eq!(find_in("blurry", 64, "styled", &themes), Some(hicolor_big), "the theme's 48 would be stretched");
        assert_eq!(find_in("blurry", 48, "styled", &themes), Some(styled_small.clone()), "large enough: the theme's own");
        assert_eq!(find_in("fine", 64, "styled", &themes), Some(styled_big), "the theme has a large one");
        assert_eq!(find_in("lonely", 64, "styled", &themes), Some(only_small), "nothing sharper: the small one");
        assert_eq!(find_in("retina", 64, "styled", &themes), Some(styled_doubled), "a @2 directory counts double");
        assert_eq!(find_in("claims", 64, "styled", &themes), Some(hicolor_claims), "a PNG in a 'Scalable' directory is still its size");
        let _ = std::fs::remove_dir_all(&root);
    }
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

    fn red_or_blue(fill: &str) -> Vec<u8> {
        format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10" fill="{fill}"/></svg>"#).into_bytes()
    }

    #[test]
    fn rendered_svgs_are_kept_on_disk_until_the_file_changes() {
        let root = std::env::temp_dir().join(format!("helix-icon-disk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let cache = DiskCache::at(root.join("cache"));
        let icon = root.join("app.svg");
        std::fs::write(&icon, red_or_blue("red")).unwrap();
        let entries = || std::fs::read_dir(root.join("cache")).map_or(0, |d| d.count());

        let first = decode_file_with(Some(&cache), &icon, 16).unwrap();
        assert_eq!(&first.rgba[0..4], &[255, 0, 0, 255]);
        assert_eq!(entries(), 1, "kept");
        // Read back from the cache, not rendered again: what the cache
        // holds is what comes back.
        let meta = std::fs::metadata(&icon).unwrap();
        let marked = Image { width: 1, height: 1, rgba: vec![1, 2, 3, 4] };
        cache.put(&icon, &meta, 16, &marked);
        assert_eq!(decode_file_with(Some(&cache), &icon, 16), Some(marked));
        // Another size is another entry.
        assert_eq!(decode_file_with(Some(&cache), &icon, 32).map(|i| i.width), Some(32));
        assert_eq!(entries(), 2);

        // The file changes (new contents, a later time): rendered again.
        std::fs::write(&icon, red_or_blue("blue")).unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options().write(true).open(&icon).unwrap().set_modified(later).unwrap();
        let changed = decode_file_with(Some(&cache), &icon, 16).unwrap();
        assert_eq!(&changed.rgba[0..4], &[0, 0, 255, 255], "not the old picture");
        assert_eq!(entries(), 3);

        // A damaged entry is ignored (and replaced).
        for entry in std::fs::read_dir(root.join("cache")).unwrap().flatten() {
            std::fs::write(entry.path(), b"HXIC\x01junk").unwrap();
        }
        assert_eq!(&decode_file_with(Some(&cache), &icon, 16).unwrap().rgba[0..4], &[0, 0, 255, 255]);

        // Pruning removes what was not used for that long.
        cache.prune(std::time::Duration::ZERO);
        assert_eq!(entries(), 0);
        // PNGs are not kept.
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.write_header().unwrap().write_image_data(&[0, 0, 255, 255]).unwrap();
        }
        std::fs::write(root.join("app.png"), &png).unwrap();
        assert!(decode_file_with(Some(&cache), &root.join("app.png"), 16).is_some());
        assert_eq!(entries(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn source_keys_follow_the_picture() {
        let a = IconSource::Names(vec!["a".into()]);
        assert_eq!(source_key(&a), source_key(&IconSource::Names(vec!["a".into()])));
        assert_ne!(source_key(&a), source_key(&IconSource::Names(vec!["b".into()])));
        assert_ne!(source_key(&IconSource::Bytes(vec![1])), source_key(&IconSource::Bytes(vec![2])));
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
