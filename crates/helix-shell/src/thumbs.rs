//! Thumbnails for the file chooser's grid view (helixdesktop/spec
//! `contracts/dbus/org.freedesktop.impl.portal.md`, "FileChooser"),
//! following the freedesktop thumbnail specification: a fresh thumbnail
//! in `$XDG_CACHE_HOME/thumbnails/{normal,large,…}` is used as it is;
//! pictures without one are scaled down here and the result is written
//! there (`normal` or `large`, with `Thumb::URI` and `Thumb::MTime`), so
//! GNOME's file manager and later dialogs find it; other types go to the
//! installed thumbnailers (sandboxed, `helix_settings::thumbnails`).
//!
//! The UI asks for the thumbnails of the items on screen only
//! ([`Queue::want`]); worker threads make them, the newest wishes first,
//! dropping those scrolled away. The UI keeps what came in a
//! [`MemoryCache`] with a fixed budget.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};

use helix_settings::files::Entry;
use helix_settings::thumbnails;

/// Pictures larger than this are not thumbnailed here (a thumbnail
/// already in the cache is still used).
const PICTURE_MAX_BYTES: u64 = 32 << 20;
/// Threads making thumbnails.
const WORKERS: usize = 2;

/// Whether `entry` may have a thumbnail: any file with something in it
/// (one in the cache is used whatever its type; pictures and types an
/// installed thumbnailer handles get one made).
pub fn wanted(entry: &Entry) -> bool {
    !entry.dir && entry.size > 0
}

/// The thumbnail of `path` for showing it at most `px` pixels big
/// (premultiplied), if it can have one. Blocks: run it on a worker.
pub fn make(path: &Path, px: u32, sandbox: &dyn Fn() -> thumbnails::Sandbox) -> Option<helix_draw::Image> {
    use crate::wallpaper::{self, Limits};
    if thumbnails::in_cache(path) {
        return None;
    }
    let from_cache = |found: &Path| wallpaper::decode_within(found, Limits::ART).ok().map(|image| wallpaper::thumbnail(&image, px, px));
    if let Some(found) = thumbnails::cached_big_enough(path, px) {
        return from_cache(&found);
    }
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let mime = thumbnails::mime_of(path);
    if thumbnails::is_picture(&mime) {
        if meta.len() > PICTURE_MAX_BYTES {
            return None;
        }
        let image = wallpaper::decode_within(path, Limits::PREVIEW).ok()?;
        let (flavor, side) = thumbnails::flavor_to_make(px);
        let small = straight(&wallpaper::thumbnail(&image, side, side));
        // Only pictures larger than the flavor are worth keeping.
        if image.width > side || image.height > side {
            if let Err(err) = store(path, &small, flavor, &mime, &meta) {
                tracing::debug!(%err, file = %path.display(), "cannot write a thumbnail");
            }
        }
        return Some(wallpaper::thumbnail(&small, px, px));
    }
    // A smaller thumbnail is better than none.
    if let Some(found) = thumbnails::cached(path, px) {
        return from_cache(&found);
    }
    if thumbnails::has_thumbnailer(&mime) {
        let made = thumbnails::thumbnail(path, &sandbox())?;
        let image = wallpaper::decode_within(&made, Limits::ART).ok()?;
        return Some(wallpaper::thumbnail(&image, px, px));
    }
    None
}

/// A premultiplied image as straight RGBA (for writing a PNG).
fn straight(image: &helix_draw::Image) -> crate::wallpaper::Image {
    let mut rgba = image.rgba.clone();
    for p in rgba.as_chunks_mut::<4>().0 {
        let a = u32::from(p[3]);
        if a > 0 && a < 255 {
            for c in &mut p[..3] {
                *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    crate::wallpaper::Image { width: image.width as u32, height: image.height as u32, rgba }
}

/// The PNG of a thumbnail of `uri` (modified at `mtime`, Unix seconds;
/// `size` bytes), as the thumbnail specification has it.
pub fn encode(image: &crate::wallpaper::Image, uri: &str, mtime: u64, size: u64, mime: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, image.width, image.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        for (key, value) in [("Thumb::URI", uri.to_owned()), ("Thumb::MTime", mtime.to_string()), ("Thumb::Size", size.to_string()), ("Thumb::Mimetype", mime.to_owned()), ("Software", "Helix".to_owned())] {
            encoder.add_text_chunk(key.to_owned(), value).map_err(|e| e.to_string())?;
        }
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(&image.rgba).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

/// Write `image` as `path`'s thumbnail of `flavor` in the shared cache
/// (the folder made private, the file written whole then renamed in).
fn store(path: &Path, image: &crate::wallpaper::Image, flavor: &str, mime: &str, meta: &std::fs::Metadata) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let uri = thumbnails::uri(path);
    let mtime = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
    let bytes = encode(image, &uri, mtime, meta.len(), mime)?;
    let dir = gio::glib::user_cache_dir().join("thumbnails").join(flavor);
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).map_err(|e| e.to_string())?;
    let name = thumbnails::cache_name(&uri);
    let temp = dir.join(format!(".{name}.helix-{}", std::process::id()));
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp).map_err(|e| e.to_string())?;
    let written = file.write_all(&bytes).and_then(|()| std::fs::rename(&temp, dir.join(&name)));
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written.map_err(|e| e.to_string())
}

// ----- The workers --------------------------------------------------------

/// What the workers are asked: the paths wanted (first first) at a size,
/// and those being made.
#[derive(Default)]
struct Wishes {
    px: u32,
    pending: VecDeque<String>,
    making: Vec<String>,
}

/// Thumbnails asked of the worker threads; each made one goes to `done`.
pub struct Queue {
    wishes: Arc<(Mutex<Wishes>, Condvar)>,
}

impl Queue {
    /// Start the workers; `done` gets (path, size asked, thumbnail).
    pub fn start(done: impl Fn(String, u32, Option<helix_draw::Image>) + Send + Sync + 'static) -> Self {
        let wishes: Arc<(Mutex<Wishes>, Condvar)> = Arc::default();
        let done = Arc::new(done);
        for i in 0..WORKERS {
            let (wishes, done) = (wishes.clone(), done.clone());
            let spawned = std::thread::Builder::new().name(format!("thumbnails-{i}")).spawn(move || loop {
                let (path, px) = {
                    let (lock, ready) = &*wishes;
                    let mut w = lock.lock().unwrap_or_else(|e| e.into_inner());
                    loop {
                        if let Some(p) = w.pending.pop_front() {
                            w.making.push(p.clone());
                            break (p, w.px);
                        }
                        w = ready.wait(w).unwrap_or_else(|e| e.into_inner());
                    }
                };
                let image = make(Path::new(&path), px, &crate::ext_host::sandbox::thumbnailer_sandbox);
                wishes.0.lock().unwrap_or_else(|e| e.into_inner()).making.retain(|p| *p != path);
                done(path, px, image);
            });
            if let Err(err) = spawned {
                tracing::warn!(%err, "cannot start a thumbnail worker");
            }
        }
        Self { wishes }
    }

    /// The thumbnails wanted now, most wanted first, at `px`: these
    /// replace earlier wishes not started yet.
    pub fn want(&self, px: u32, paths: Vec<String>) {
        let (lock, ready) = &*self.wishes;
        let mut w = lock.lock().unwrap_or_else(|e| e.into_inner());
        w.px = px;
        let making = w.making.clone();
        w.pending = paths.into_iter().filter(|p| !making.contains(p)).collect();
        ready.notify_all();
    }
}

// ----- In memory ----------------------------------------------------------

/// Thumbnails in memory, by path: at most `budget` bytes of pixels, the
/// least recently shown going first. A file without a thumbnail is kept
/// as `None`.
#[derive(Debug, Default)]
pub struct MemoryCache {
    items: HashMap<String, (Option<Arc<helix_draw::Image>>, u64)>,
    /// When each was last shown (a counter).
    tick: u64,
    bytes: usize,
    budget: usize,
    /// The size the thumbnails are for (logical px × scale).
    pub px: u32,
}

/// The memory thumbnails may take (pixels, 4 bytes each).
pub const MEMORY_BUDGET: usize = 48 << 20;

impl MemoryCache {
    pub fn new(budget: usize) -> Self {
        Self { budget, ..Self::default() }
    }

    /// Forget everything (another size is wanted).
    pub fn clear(&mut self, px: u32) {
        self.items.clear();
        self.bytes = 0;
        self.px = px;
    }

    /// The thumbnail of `path` (`Some(None)`: it has none; `None`: not
    /// come), marked as just shown.
    pub fn get(&mut self, path: &str) -> Option<Option<Arc<helix_draw::Image>>> {
        self.tick += 1;
        let tick = self.tick;
        self.items.get_mut(path).map(|(image, seen)| {
            *seen = tick;
            image.clone()
        })
    }

    /// The thumbnail of `path`, if it came (not marking it shown).
    pub fn peek(&self, path: &str) -> Option<&Arc<helix_draw::Image>> {
        self.items.get(path).and_then(|(image, _)| image.as_ref())
    }

    pub fn contains(&self, path: &str) -> bool {
        self.items.contains_key(path)
    }

    /// `path`'s thumbnail came (or there is none); the least recently
    /// shown go while over the budget.
    pub fn put(&mut self, path: String, image: Option<helix_draw::Image>) {
        self.tick += 1;
        let size = image.as_ref().map_or(0, |i| i.rgba.len());
        if let Some((old, _)) = self.items.insert(path.clone(), (image.map(Arc::new), self.tick)) {
            self.bytes -= old.map_or(0, |i| i.rgba.len());
        }
        self.bytes += size;
        while self.bytes > self.budget {
            let Some(oldest) = self.items.iter().filter(|(p, (i, _))| i.is_some() && **p != path).min_by_key(|(_, (_, seen))| *seen).map(|(p, _)| p.clone()) else { break };
            if let Some((Some(gone), _)) = self.items.remove(&oldest) {
                self.bytes -= gone.rgba.len();
            }
        }
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// The paths of `entries` worth asking a thumbnail for, of those shown
/// (`shown`, indexes into `entries`), whose answer has not come.
pub fn to_ask(entries: &[Entry], shown: &[usize], cache: &MemoryCache) -> Vec<String> {
    shown.iter().map(|&i| &entries[i]).filter(|e| wanted(e)).map(|e| e.path.to_string_lossy().into_owned()).filter(|p| !cache.contains(p)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Where a test puts pictures (a scratch folder below the temp dir).
    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("helix-thumbs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn image(side: usize) -> helix_draw::Image {
        helix_draw::Image { width: side, height: side, rgba: vec![255; side * side * 4] }
    }

    #[test]
    fn memory_is_capped() {
        let one = 16 * 16 * 4;
        let mut cache = MemoryCache::new(3 * one);
        cache.put("/a".into(), None);
        assert_eq!(cache.get("/a"), Some(None), "it has none");
        assert_eq!(cache.get("/b"), None, "never asked");
        for p in ["/a", "/b", "/c"] {
            cache.put(p.into(), Some(image(16)));
        }
        cache.put("/none".into(), None);
        assert_eq!(cache.bytes(), 3 * one);
        // /a shown again: /b is the least recently shown and goes.
        assert!(cache.get("/a").unwrap().is_some());
        cache.put("/d".into(), Some(image(16)));
        assert_eq!(cache.bytes(), 3 * one);
        assert_eq!(cache.get("/b"), None);
        assert!(cache.get("/a").unwrap().is_some() && cache.get("/d").unwrap().is_some());
        assert_eq!(cache.get("/none"), Some(None), "no thumbnail is remembered (and costs nothing)");
        cache.clear(192);
        assert!(cache.is_empty() && cache.bytes() == 0 && cache.px == 192);
    }

    #[test]
    fn written_thumbnails_follow_the_spec() {
        let picture = crate::wallpaper::Image { width: 2, height: 1, rgba: vec![255, 0, 0, 255, 0, 0, 255, 128] };
        let png = encode(&picture, "file:///home/ada/a%20b.png", 1_700_000_000, 1234, "image/png").unwrap();
        let text = thumbnails::png_text(&png);
        let get = |k: &str| text.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
        assert_eq!(get("Thumb::URI").as_deref(), Some("file:///home/ada/a%20b.png"));
        assert_eq!(get("Thumb::MTime").as_deref(), Some("1700000000"));
        assert_eq!(get("Thumb::Size").as_deref(), Some("1234"));
        let back = crate::wallpaper::decode_bytes(&png, Path::new("t.png")).unwrap();
        assert_eq!((back.width, back.height, &back.rgba[..]), (2, 1, &picture.rgba[..]));
        let s = straight(&helix_draw::Image { width: 1, height: 1, rgba: vec![64, 0, 0, 128] });
        assert_eq!(s.rgba, [128, 0, 0, 128], "premultiplied back to straight");
    }

    /// A picture's thumbnail is made, written into the shared cache as the
    /// spec names it, and found there the next time (the scratch cache
    /// folder stands in for `$XDG_CACHE_HOME`).
    #[test]
    fn pictures_are_thumbnailed_and_cached() {
        let dir = scratch("cache");
        let cache = dir.join("cache");
        // GLib reads XDG_CACHE_HOME once: a test process of its own sets it.
        if std::env::var_os("HELIX_THUMBS_TEST_CHILD").is_none() {
            let exe = std::env::current_exe().unwrap();
            let status = std::process::Command::new(exe)
                .args(["--exact", "thumbs::tests::pictures_are_thumbnailed_and_cached", "--nocapture"])
                .env("HELIX_THUMBS_TEST_CHILD", "1")
                .env("XDG_CACHE_HOME", &cache)
                .status()
                .unwrap();
            assert!(status.success());
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let cache = gio::glib::user_cache_dir();
        let photo = dir.join("photo.png");
        let big = crate::wallpaper::Image { width: 600, height: 300, rgba: vec![200; 600 * 300 * 4] };
        std::fs::write(&photo, encode(&big, "x", 0, 0, "image/png").unwrap()).unwrap();
        let entry = Entry::at(photo.clone()).unwrap();
        assert!(wanted(&entry));
        let none = || thumbnails::Sandbox::default();
        let made = make(&photo, 96, &none).expect("a thumbnail");
        assert_eq!((made.width, made.height), (96, 48), "fits 96 px");
        let stored = cache.join("thumbnails/normal").join(thumbnails::cache_name(&thumbnails::uri(&photo)));
        let bytes = std::fs::read(&stored).expect("written to the normal cache");
        let back = crate::wallpaper::decode_bytes(&bytes, &stored).unwrap();
        assert_eq!((back.width, back.height), (128, 64));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&stored).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(stored.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(thumbnails::cached(&photo, 96), Some(stored.clone()), "found next time");
        // Bigger wanted: a large one is made beside it.
        let large = make(&photo, 200, &none).unwrap();
        assert_eq!((large.width, large.height), (200, 100));
        assert!(cache.join("thumbnails/large").join(thumbnails::cache_name(&thumbnails::uri(&photo))).is_file());
        // The file changed: the old thumbnail is stale.
        std::fs::write(&photo, encode(&crate::wallpaper::Image { width: 300, height: 300, rgba: vec![9; 300 * 300 * 4] }, "x", 0, 0, "image/png").unwrap()).unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options().write(true).open(&photo).unwrap().set_modified(later).unwrap();
        assert_eq!(thumbnails::cached(&photo, 96), None);
        assert_eq!(make(&photo, 96, &none).map(|i| (i.width, i.height)), Some((96, 96)));
        // Small pictures are shown but not stored; text has none.
        let tiny = dir.join("tiny.png");
        std::fs::write(&tiny, encode(&crate::wallpaper::Image { width: 20, height: 10, rgba: vec![1; 800] }, "x", 0, 0, "image/png").unwrap()).unwrap();
        assert_eq!(make(&tiny, 96, &none).map(|i| (i.width, i.height)), Some((20, 10)));
        assert!(!cache.join("thumbnails/normal").join(thumbnails::cache_name(&thumbnails::uri(&tiny))).exists());
        std::fs::write(dir.join("notes.txt"), "hello").unwrap();
        assert!(make(&dir.join("notes.txt"), 96, &none).is_none());
        // Thumbnails are not thumbnailed.
        assert!(make(&stored, 96, &none).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_new_paths_are_asked() {
        let entries: Vec<Entry> = ["a.png", "empty.txt", "c.jpg", "Folder"].iter().map(|n| Entry { name: (*n).into(), path: PathBuf::from("/p").join(n), dir: *n == "Folder", size: if *n == "empty.txt" { 0 } else { 10 }, ..Entry::default() }).collect();
        let mut cache = MemoryCache::new(MEMORY_BUDGET);
        assert_eq!(to_ask(&entries, &[0, 1, 2, 3], &cache), ["/p/a.png", "/p/c.jpg"], "not folders or empty files");
        cache.put("/p/a.png".into(), None);
        assert_eq!(to_ask(&entries, &[3, 2, 0], &cache), ["/p/c.jpg"], "in the order shown, not those answered");
    }
}
