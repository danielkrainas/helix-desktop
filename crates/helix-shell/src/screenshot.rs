//! `org.gnome.Shell.Screenshot` (helix-desktop-spec
//! `contracts/dbus/org.gnome.Shell.Screenshot.md`) and the screenshot keys
//! (`contracts/gsettings/shell-keys.md`).
//!
//! The flows live here; the UI thread only provides primitives, asked for
//! with [`Job`]s: capture into a file (through the compositor), let the
//! user pick an area / a point / what to capture, and flash an area.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use gio::prelude::*;
use tokio::sync::{mpsc, oneshot};
use zbus::fdo::{self, DBusProxy};
use zbus::message::Header;
use zbus::zvariant::{OwnedValue, Value};
use zbus::{interface, Connection};

use crate::UiEvent;

pub const OBJECT_PATH: &str = "/org/gnome/Shell/Screenshot";

/// Well-known names whose owners may use the interface.
pub const ALLOWED_CALLERS: [&str; 2] = ["org.freedesktop.impl.portal.desktop.helix", "org.gnome.Screenshot"];

const CANCELLED: &str = "Operation was cancelled";

/// A rectangle in logical layout coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self { x, y, width, height }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Screen,
    /// The focused window, with the compositor's title bar or not.
    Window { frame: bool },
    /// The window at a point (layout coordinates), with its title bar.
    WindowAt(i32, i32),
    Area(Rect),
}

/// What the user is asked to pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickMode {
    /// Drag out a rectangle.
    Area,
    /// Click a pixel.
    Point,
    /// The screenshot UI: an area, or Enter for the whole screen.
    Screenshot,
    /// Click a window.
    Window,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Picked {
    Cancelled,
    /// Another pick is in progress.
    Busy,
    Area(Rect),
    Point(i32, i32),
    Screen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureError {
    Locked,
    NoWindow,
    Invalid,
    Failed,
    /// No compositor capture (not started by the compositor).
    Unavailable,
}

/// Work for the UI thread.
#[derive(Debug)]
pub enum Job {
    /// Write a PNG of `target` into `file`; reply with the rectangle captured.
    Capture { target: Target, cursor: bool, file: File, reply: oneshot::Sender<Result<Rect, CaptureError>> },
    Pick { mode: PickMode, reply: oneshot::Sender<Picked> },
    Flash(Rect),
    /// Put the PNG image in this file on the clipboard.
    Copy(File),
}

/// A [`Job`] that can travel in a (`Clone`, `PartialEq`) [`UiEvent`].
#[derive(Debug, Clone)]
pub struct JobSlot(Arc<Mutex<Option<Job>>>);

impl JobSlot {
    pub fn new(job: Job) -> Self {
        Self(Arc::new(Mutex::new(Some(job))))
    }

    pub fn take(&self) -> Option<Job> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

impl PartialEq for JobSlot {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// Hands jobs to the UI.
#[derive(Clone)]
pub struct Screens {
    ui: mpsc::UnboundedSender<UiEvent>,
    /// Where "Screenshot captured" goes (none in some tests).
    center: Option<crate::notifications::Center>,
}

impl Screens {
    pub fn new(ui: mpsc::UnboundedSender<UiEvent>) -> Self {
        Self { ui, center: None }
    }

    pub fn with_notifications(ui: mpsc::UnboundedSender<UiEvent>, center: crate::notifications::Center) -> Self {
        Self { ui, center: Some(center) }
    }

    /// A screenshot the user took: on the clipboard, and announced.
    pub fn taken(&self, path: &Path) {
        match File::open(path) {
            Ok(file) => {
                self.send(Job::Copy(file));
            }
            Err(err) => tracing::warn!(%err, "cannot copy the screenshot"),
        }
        if let Some(center) = &self.center {
            center.screenshot_captured(path.to_path_buf());
        }
    }

    fn send(&self, job: Job) -> bool {
        self.ui.send(UiEvent::Screenshot(JobSlot::new(job))).is_ok()
    }

    pub async fn capture(&self, target: Target, cursor: bool, file: File) -> Result<Rect, CaptureError> {
        let (reply, answer) = oneshot::channel();
        if !self.send(Job::Capture { target, cursor, file, reply }) {
            return Err(CaptureError::Unavailable);
        }
        answer.await.unwrap_or(Err(CaptureError::Unavailable))
    }

    pub async fn pick(&self, mode: PickMode) -> Picked {
        let (reply, answer) = oneshot::channel();
        if !self.send(Job::Pick { mode, reply }) {
            return Picked::Cancelled;
        }
        answer.await.unwrap_or(Picked::Cancelled)
    }

    pub fn flash(&self, area: Rect) {
        self.send(Job::Flash(area));
    }

    /// Capture into a new file at `path` (made unique); flash if asked.
    /// Returns the path written.
    pub async fn capture_to(&self, target: Target, cursor: bool, flash: bool, path: &Path) -> Result<PathBuf, CaptureError> {
        let (path, file) = create_unique(path).map_err(|err| {
            tracing::warn!(%err, path = %path.display(), "cannot create screenshot file");
            CaptureError::Failed
        })?;
        match self.capture(target, cursor, file).await {
            Ok(rect) => {
                if flash {
                    self.flash(rect);
                }
                tracing::info!(path = %path.display(), "screenshot saved");
                Ok(path)
            }
            Err(err) => {
                let _ = std::fs::remove_file(&path);
                tracing::warn!(?err, "screenshot failed");
                Err(err)
            }
        }
    }

    /// The screenshot UI: what the user picks is saved in the Screenshots
    /// folder. `Ok(None)` when cancelled.
    pub async fn interactive(&self) -> Result<Option<PathBuf>, CaptureError> {
        let target = match self.pick(PickMode::Screenshot).await {
            Picked::Screen => Target::Screen,
            Picked::Area(rect) => Target::Area(rect),
            Picked::Busy => return Err(CaptureError::Failed),
            _ => return Ok(None),
        };
        let path = self.capture_to(target, false, true, &screenshot_path()).await?;
        self.taken(&path);
        Ok(Some(path))
    }

    /// Let the user click a pixel; its sRGB color.
    pub async fn pick_color(&self) -> Result<Option<(f64, f64, f64)>, CaptureError> {
        let (x, y) = match self.pick(PickMode::Point).await {
            Picked::Point(x, y) => (x, y),
            Picked::Busy => return Err(CaptureError::Failed),
            _ => return Ok(None),
        };
        let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let (path, file) = create_unique(&dir.join("helix-color.png")).map_err(|_| CaptureError::Failed)?;
        let result = self.capture(Target::Area(Rect::new(x, y, 1, 1)), false, file).await;
        let bytes = std::fs::read(&path);
        let _ = std::fs::remove_file(&path);
        result?;
        let rgb = bytes.ok().and_then(|b| first_pixel(&b)).ok_or(CaptureError::Failed)?;
        Ok(Some(rgb))
    }
}

/// The top-left pixel of a PNG image, as sRGB values from 0 to 1.
pub fn first_pixel(png_bytes: &[u8]) -> Option<(f64, f64, f64)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let channels = info.color_type.samples();
    if info.bit_depth != png::BitDepth::Eight || channels < 3 || buf.len() < 3 {
        return None;
    }
    let c = |i: usize| f64::from(buf[i]) / 255.0;
    Some((c(0), c(1), c(2)))
}

/// `$XDG_PICTURES_DIR`, or the home directory if it doesn't exist.
pub fn pictures_dir() -> PathBuf {
    gio::glib::user_special_dir(gio::glib::UserDirectory::Pictures)
        .filter(|d| d.is_dir())
        .unwrap_or_else(gio::glib::home_dir)
}

/// Where a `filename` argument is saved: absolute paths as they are, base
/// names in `pictures`; always ending in `.png`.
pub fn resolve(filename: &str, pictures: &Path) -> PathBuf {
    let path = Path::new(filename);
    let mut path = if path.is_absolute() { path.to_path_buf() } else { pictures.join(path) };
    let is_png = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("png"));
    if !is_png {
        let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        name.push(".png");
        path.set_file_name(name);
    }
    path
}

/// Create `path`, or `name (2).png`, `name (3).png`, … if it is taken.
pub fn create_unique(path: &Path) -> std::io::Result<(PathBuf, File)> {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    for n in 1..1000 {
        let candidate = if n == 1 { path.to_path_buf() } else { path.with_file_name(format!("{stem} ({n}){ext}")) };
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&candidate) {
            Ok(file) => return Ok((candidate, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(std::io::Error::other("no free file name"))
}

/// `Screenshot From YYYY-MM-DD HH-MM-SS.png` in the Screenshots folder of
/// the pictures directory (created if needed).
pub fn screenshot_path() -> PathBuf {
    let dir = pictures_dir().join("Screenshots");
    if let Err(err) = std::fs::create_dir_all(&dir) {
        tracing::warn!(%err, dir = %dir.display(), "cannot create the Screenshots folder");
    }
    let stamp = gio::glib::DateTime::now_local()
        .and_then(|now| now.format("%Y-%m-%d %H-%M-%S"))
        .map(|s| s.to_string())
        .unwrap_or_else(|_| "unknown time".into());
    dir.join(screenshot_name(&stamp))
}

pub fn screenshot_name(stamp: &str) -> String {
    format!("Screenshot From {stamp}.png")
}

/// Whether the caller owns one of [`ALLOWED_CALLERS`].
async fn allowed(conn: &Connection, header: &Header<'_>) -> bool {
    let Some(sender) = header.sender() else { return false };
    let Ok(dbus) = DBusProxy::new(conn).await else { return false };
    for name in ALLOWED_CALLERS {
        let Ok(name) = zbus::names::BusName::try_from(name) else { continue };
        if dbus.get_name_owner(name).await.is_ok_and(|owner| owner.as_str() == sender.as_str()) {
            return true;
        }
    }
    false
}

async fn check(conn: &Connection, header: &Header<'_>) -> fdo::Result<()> {
    if allowed(conn, header).await {
        Ok(())
    } else {
        Err(fdo::Error::AccessDenied("screenshots are restricted".into()))
    }
}

fn positive(width: i32, height: i32) -> fdo::Result<()> {
    if width > 0 && height > 0 {
        Ok(())
    } else {
        Err(fdo::Error::InvalidArgs(format!("invalid size {width}x{height}")))
    }
}

/// The served `org.gnome.Shell.Screenshot` object.
pub struct Screenshot {
    pub screens: Screens,
}

impl Screenshot {
    async fn save(&self, target: Target, cursor: bool, flash: bool, filename: &str) -> (bool, String) {
        let path = resolve(filename, &pictures_dir());
        match self.screens.capture_to(target, cursor, flash, &path).await {
            Ok(path) => (true, path.to_string_lossy().into_owned()),
            Err(_) => (false, String::new()),
        }
    }
}

#[interface(name = "org.gnome.Shell.Screenshot")]
impl Screenshot {
    async fn interactive_screenshot(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<(bool, String)> {
        check(conn, &header).await?;
        match self.screens.interactive().await {
            Ok(Some(path)) => Ok((true, gio::File::for_path(&path).uri().to_string())),
            Ok(None) => Ok((false, String::new())),
            Err(_) => Err(fdo::Error::Failed("cannot take a screenshot now".into())),
        }
    }

    async fn screenshot(
        &self,
        include_cursor: bool,
        flash: bool,
        filename: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<(bool, String)> {
        check(conn, &header).await?;
        Ok(self.save(Target::Screen, include_cursor, flash, filename).await)
    }

    async fn screenshot_window(
        &self,
        include_frame: bool,
        include_cursor: bool,
        flash: bool,
        filename: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<(bool, String)> {
        check(conn, &header).await?;
        Ok(self.save(Target::Window { frame: include_frame }, include_cursor, flash, filename).await)
    }

    #[allow(clippy::too_many_arguments)]
    async fn screenshot_area(
        &self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        flash: bool,
        filename: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<(bool, String)> {
        check(conn, &header).await?;
        positive(width, height)?;
        Ok(self.save(Target::Area(Rect::new(x, y, width, height)), false, flash, filename).await)
    }

    async fn pick_color(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<std::collections::HashMap<String, OwnedValue>> {
        check(conn, &header).await?;
        match self.screens.pick_color().await {
            Ok(Some((r, g, b))) => {
                let color = OwnedValue::try_from(Value::from((r, g, b))).map_err(|e| fdo::Error::Failed(e.to_string()))?;
                Ok(std::collections::HashMap::from([("color".to_owned(), color)]))
            }
            Ok(None) => Err(fdo::Error::Failed(CANCELLED.into())),
            Err(_) => Err(fdo::Error::Failed("cannot pick a color now".into())),
        }
    }

    async fn flash_area(
        &self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<()> {
        check(conn, &header).await?;
        positive(width, height)?;
        self.screens.flash(Rect::new(x, y, width, height));
        Ok(())
    }

    async fn select_area(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<(i32, i32, i32, i32)> {
        check(conn, &header).await?;
        match self.screens.pick(PickMode::Area).await {
            Picked::Area(r) => Ok((r.x, r.y, r.width, r.height)),
            Picked::Busy => Err(fdo::Error::Failed("another selection is in progress".into())),
            _ => Err(fdo::Error::Failed(CANCELLED.into())),
        }
    }
}

/// Helix's window pick for the Screenshot portal (helix-desktop-spec
/// `org.gnome.Shell.Screenshot.md`, "Picking a window"), at the same object.
pub struct WindowPicker {
    pub screens: Screens,
}

#[interface(name = "org.helix.ScreenshotWindowPicker")]
impl WindowPicker {
    /// Let the user click a window and capture it: (success, file used).
    async fn pick_window(
        &self,
        include_cursor: bool,
        flash: bool,
        filename: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<(bool, String)> {
        check(conn, &header).await?;
        let path = resolve(filename, &pictures_dir());
        loop {
            let (x, y) = match self.screens.pick(PickMode::Window).await {
                Picked::Point(x, y) => (x, y),
                Picked::Busy => return Err(fdo::Error::Failed("another selection is in progress".into())),
                _ => return Err(fdo::Error::Failed(CANCELLED.into())),
            };
            match self.screens.capture_to(Target::WindowAt(x, y), include_cursor, flash, &path).await {
                Ok(path) => return Ok((true, path.to_string_lossy().into_owned())),
                // No window there: pick again.
                Err(CaptureError::NoWindow) => continue,
                Err(_) => return Ok((false, String::new())),
            }
        }
    }
}

/// Screenshot keys of `org.gnome.shell.keybindings`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenshotKey {
    /// `show-screenshot-ui`
    Ui,
    /// `screenshot`
    Screen,
    /// `screenshot-window`
    Window,
}

/// Act on a screenshot key.
pub async fn run_key(screens: &Screens, key: ScreenshotKey) {
    let result = match key {
        ScreenshotKey::Ui => screens.interactive().await.map(drop),
        ScreenshotKey::Screen => screens.capture_to(Target::Screen, false, true, &screenshot_path()).await.map(|p| screens.taken(&p)),
        ScreenshotKey::Window => screens.capture_to(Target::Window { frame: true }, false, true, &screenshot_path()).await.map(|p| screens.taken(&p)),
    };
    if let Err(err) = result {
        tracing::info!(?err, ?key, "no screenshot taken");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names() {
        let pictures = Path::new("/home/u/Pictures");
        assert_eq!(resolve("shot", pictures), Path::new("/home/u/Pictures/shot.png"));
        assert_eq!(resolve("shot.PNG", pictures), Path::new("/home/u/Pictures/shot.PNG"));
        assert_eq!(resolve("a.b", pictures), Path::new("/home/u/Pictures/a.b.png"));
        assert_eq!(resolve("/tmp/x.png", pictures), Path::new("/tmp/x.png"));
        assert_eq!(screenshot_name("2026-11-11 10-00-00"), "Screenshot From 2026-11-11 10-00-00.png");
    }

    #[test]
    fn never_overwrites() {
        let dir = std::env::temp_dir().join(format!("helix-shot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shot.png");
        let (first, _) = create_unique(&path).unwrap();
        let (second, _) = create_unique(&path).unwrap();
        let (third, _) = create_unique(&path).unwrap();
        assert_eq!(first, path);
        assert_eq!(second, dir.join("shot (2).png"));
        assert_eq!(third, dir.join("shot (3).png"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn color_of_first_pixel() {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 2, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[255, 0, 51, 255, 0, 0, 0, 255]).unwrap();
        }
        assert_eq!(first_pixel(&bytes), Some((1.0, 0.0, 0.2)));
        assert_eq!(first_pixel(b"not a png"), None);
    }
}
