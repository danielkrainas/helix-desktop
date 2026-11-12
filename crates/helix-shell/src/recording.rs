//! Screen recording: `org.gnome.Shell.Screencast` (helix-desktop-spec
//! `contracts/dbus/org.gnome.Shell.Screencast.md`) and the
//! `show-screen-recording-ui` key (`contracts/gsettings/shell-keys.md`).
//!
//! Frames come from an `org.gnome.Mutter.ScreenCast` stream; `gst-launch-1.0`
//! encodes them into the file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::io::AsyncBufReadExt;
use tokio::sync::{mpsc, oneshot, Mutex, OnceCell};
use zbus::fdo;
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{interface, Connection};

use crate::screenshot::{PickMode, Picked, Rect, Screens, Shot};
use crate::UiEvent;

pub const BUS_NAME: &str = "org.gnome.Shell.Screencast";
pub const OBJECT_PATH: &str = "/org/gnome/Shell/Screencast";
pub const ERROR_FAILED: &str = "org.gnome.Shell.Screencast.Error.Failed";

const SCREEN_CAST: &str = "org.gnome.Mutter.ScreenCast";
const SCREEN_CAST_PATH: &str = "/org/gnome/Mutter/ScreenCast";
/// The elements the default pipeline needs.
const ELEMENTS: [&str; 6] = ["pipewiresrc", "videorate", "videoconvert", "vp8enc", "webmmux", "filesink"];
/// How long GStreamer gets to finish the file.
const FINISH_TIMEOUT: Duration = Duration::from_secs(5);

/// What to record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Every monitor (the layout's bounding box).
    Screen,
    Area(Rect),
}

/// `options` of `Screencast` / `ScreencastArea`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub draw_cursor: bool,
    pub framerate: i32,
    pub pipeline: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self { draw_cursor: true, framerate: 30, pipeline: None }
    }
}

impl Options {
    pub fn from_dict(options: &HashMap<String, OwnedValue>) -> Self {
        let mut out = Self::default();
        if let Some(Value::Bool(b)) = options.get("draw-cursor").map(|v| &**v) {
            out.draw_cursor = *b;
        }
        if let Some(Value::I32(rate)) = options.get("framerate").map(|v| &**v) {
            if *rate > 0 {
                out.framerate = *rate;
            }
        }
        if let Some(Value::Str(p)) = options.get("pipeline").map(|v| &**v) {
            if !p.trim().is_empty() {
                out.pipeline = Some(p.to_string());
            }
        }
        out
    }
}

/// `file_template` with `%d` and `%t` replaced (`%%` is a `%`).
pub fn expand(template: &str, date: &str, time: &str) -> String {
    let mut out = String::new();
    let mut chars = template.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('d') => out.push_str(date),
            Some('t') => out.push_str(time),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

/// Where an expanded name is saved: absolute paths as they are, others in
/// `videos`; without an extension, `.webm` (`.mkv` with a pipeline).
pub fn output_path(name: &str, videos: &Path, custom_pipeline: bool) -> PathBuf {
    let path = Path::new(name);
    let mut path = if path.is_absolute() { path.to_path_buf() } else { videos.join(path) };
    if path.extension().is_none() {
        let mut file = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        file.push(if custom_pipeline { ".mkv" } else { ".webm" });
        path.set_file_name(file);
    }
    path
}

/// `$XDG_VIDEOS_DIR`, or the home directory if it doesn't exist.
pub fn videos_dir() -> PathBuf {
    gio::glib::user_special_dir(gio::glib::UserDirectory::Videos)
        .filter(|d| d.is_dir())
        .unwrap_or_else(gio::glib::home_dir)
}

/// The current local date and time as `%d` and `%t` put them.
fn stamp() -> (String, String) {
    let now = gio::glib::DateTime::now_local().ok();
    let format = |f: &str| now.as_ref().and_then(|n| n.format(f).ok()).map(|s| s.to_string()).unwrap_or_default();
    (format("%Y-%m-%d"), format("%H-%M-%S"))
}

/// A `gst-launch` property value, quoted.
fn quoted(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// `gst-launch-1.0` arguments recording PipeWire node `node` into `file`.
pub fn gst_args(node: u32, options: &Options, file: &Path) -> Vec<String> {
    let mut args: Vec<String> = ["-e", "pipewiresrc", &format!("path={node}"), "do-timestamp=true", "keepalive-time=1000", "!", "videorate", "!"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    args.push(format!("video/x-raw,framerate={}/1", options.framerate));
    args.extend(["!", "videoconvert", "!", "queue", "!"].map(String::from));
    match &options.pipeline {
        Some(pipeline) => {
            args.push(pipeline.clone());
            args.extend(["!", "queue", "!", "matroskamux"].map(String::from));
        }
        None => {
            args.extend(["vp8enc", "deadline=1", "cpu-used=8", "threads=4", "!", "queue", "!", "webmmux"].map(String::from));
        }
    }
    args.extend(["!", "filesink"].map(String::from));
    args.push(format!("location={}", quoted(&file.to_string_lossy())));
    args
}

/// A directory whose `gst-launch-1.0` has all of [`ELEMENTS`]: the system
/// one first, then those on `PATH`.
async fn find_gstreamer() -> Option<PathBuf> {
    let mut dirs = vec![PathBuf::from("/usr/bin")];
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    for dir in dirs {
        let (launch, inspect) = (dir.join("gst-launch-1.0"), dir.join("gst-inspect-1.0"));
        if !(launch.is_file() && inspect.is_file()) {
            continue;
        }
        let mut all = true;
        for element in ELEMENTS {
            let found = tokio::process::Command::new(&inspect)
                .args(["--exists", element])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await
                .is_ok_and(|s| s.success());
            if !found {
                all = false;
                break;
            }
        }
        if all {
            return Some(dir);
        }
    }
    None
}

async fn stop_session(conn: &Connection, session: &OwnedObjectPath) {
    let _ = conn
        .call_method(Some(SCREEN_CAST), session.as_str(), Some("org.gnome.Mutter.ScreenCast.Session"), "Stop", &())
        .await;
}

/// Create a ScreenCast session recording `target` and start it: (session,
/// PipeWire node).
async fn start_cast(conn: &Connection, target: Target, cursor: bool) -> Result<(OwnedObjectPath, u32), String> {
    let err = |e: zbus::Error| e.to_string();
    let reply = conn
        .call_method(Some(SCREEN_CAST), SCREEN_CAST_PATH, Some(SCREEN_CAST), "CreateSession", &HashMap::<String, Value>::new())
        .await
        .map_err(err)?;
    let session: OwnedObjectPath = reply.body().deserialize().map_err(err)?;
    let result = async {
        let mut properties = HashMap::<String, Value>::new();
        properties.insert("cursor-mode".into(), Value::U32(if cursor { 1 } else { 0 }));
        let session_iface = Some("org.gnome.Mutter.ScreenCast.Session");
        let reply = match target {
            // An area covering everything: clipped to the layout.
            Target::Screen => {
                let (x, y, w, h) = (i32::MIN / 4, i32::MIN / 4, i32::MAX / 2, i32::MAX / 2);
                conn.call_method(Some(SCREEN_CAST), session.as_str(), session_iface, "RecordArea", &(x, y, w, h, properties)).await
            }
            Target::Area(r) => {
                conn.call_method(Some(SCREEN_CAST), session.as_str(), session_iface, "RecordArea", &(r.x, r.y, r.width, r.height, properties))
                    .await
            }
        }
        .map_err(err)?;
        let stream: OwnedObjectPath = reply.body().deserialize().map_err(err)?;
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface("org.gnome.Mutter.ScreenCast.Stream")
            .map_err(err)?
            .member("PipeWireStreamAdded")
            .map_err(err)?
            .path(stream)
            .map_err(err)?
            .build();
        let mut added = zbus::MessageStream::for_match_rule(rule, conn, Some(1)).await.map_err(err)?;
        conn.call_method(Some(SCREEN_CAST), session.as_str(), session_iface, "Start", &()).await.map_err(err)?;
        let node = tokio::time::timeout(Duration::from_secs(5), added.next())
            .await
            .map_err(|_| "no PipeWire stream".to_owned())?
            .ok_or("no PipeWire stream")?
            .map_err(err)?;
        node.body().deserialize::<u32>().map_err(err)
    }
    .await;
    match result {
        Ok(node) => Ok((session, node)),
        Err(e) => {
            stop_session(conn, &session).await;
            Err(e)
        }
    }
}

/// A running recording.
struct Active {
    id: u64,
    stop: oneshot::Sender<()>,
    done: oneshot::Receiver<()>,
}

#[derive(Default)]
struct Inner {
    active: Option<Active>,
    next: u64,
}

/// Starts and stops recordings (one at a time).
#[derive(Clone)]
pub struct Recorder {
    inner: Arc<Mutex<Inner>>,
    gstreamer: Arc<OnceCell<Option<PathBuf>>>,
    ui: mpsc::UnboundedSender<UiEvent>,
    center: Option<crate::notifications::Center>,
}

impl Recorder {
    pub fn new(ui: mpsc::UnboundedSender<UiEvent>, center: Option<crate::notifications::Center>) -> Self {
        Self { inner: Default::default(), gstreamer: Default::default(), ui, center }
    }

    /// Whether recording works here (GStreamer with the needed elements).
    pub async fn supported(&self) -> bool {
        self.gstreamer().await.is_some()
    }

    async fn gstreamer(&self) -> Option<PathBuf> {
        self.gstreamer.get_or_init(find_gstreamer).await.clone()
    }

    pub async fn recording(&self) -> bool {
        self.inner.lock().await.active.is_some()
    }

    /// Start recording `target` into the file `template` names; returns
    /// the file.
    pub async fn start(&self, conn: &Connection, target: Target, template: &str, options: &Options) -> Result<PathBuf, String> {
        let mut inner = self.inner.lock().await;
        if inner.active.is_some() {
            return Err("already recording".into());
        }
        if let Target::Area(r) = target {
            if r.width <= 0 || r.height <= 0 {
                return Err("empty area".into());
            }
        }
        let gst = self.gstreamer().await.ok_or("GStreamer is not available")?;
        let (date, time) = stamp();
        let path = output_path(&expand(template, &date, &time), &videos_dir(), options.pipeline.is_some());
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let (path, file) = crate::screenshot::create_unique(&path).map_err(|e| e.to_string())?;
        drop(file);
        let (session, node) = match start_cast(conn, target, options.draw_cursor).await {
            Ok(started) => started,
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                return Err(e);
            }
        };
        let args = gst_args(node, options, &path);
        tracing::debug!(?args, "gst-launch");
        let child = tokio::process::Command::new(gst.join("gst-launch-1.0"))
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                stop_session(conn, &session).await;
                let _ = std::fs::remove_file(&path);
                return Err(e.to_string());
            }
        };
        // GStreamer's last words, for errors.
        let last = Arc::new(std::sync::Mutex::new(String::new()));
        for out in [child.stdout.take().map(|o| Box::new(o) as Box<dyn tokio::io::AsyncRead + Send + Unpin>), child.stderr.take().map(|e| Box::new(e) as _)]
            .into_iter()
            .flatten()
        {
            let last = last.clone();
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(out).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(line, "gst-launch");
                    if line.starts_with("ERROR") || line.contains("error") {
                        *last.lock().unwrap_or_else(|e| e.into_inner()) = line;
                    }
                }
            });
        }
        // Fails at once on a bad pipeline.
        if let Ok(status) = tokio::time::timeout(Duration::from_millis(500), child.wait()).await {
            stop_session(conn, &session).await;
            let _ = std::fs::remove_file(&path);
            let message = last.lock().unwrap_or_else(|e| e.into_inner()).clone();
            return Err(format!("GStreamer stopped ({status:?}): {message}"));
        }
        inner.next += 1;
        let id = inner.next;
        let (stop, stop_rx) = oneshot::channel();
        let (done_tx, done) = oneshot::channel();
        inner.active = Some(Active { id, stop, done });
        drop(inner);
        tracing::info!(path = %path.display(), ?target, "recording started");
        let _ = self.ui.send(UiEvent::Recording(true));
        let (me, conn, file) = (self.clone(), conn.clone(), path.clone());
        tokio::spawn(async move {
            let stopped = tokio::select! {
                _ = stop_rx => true,
                _ = child.wait() => false,
            };
            if stopped {
                // End of stream, so the file is finished.
                if let Some(pid) = child.id() {
                    // SAFETY: signalling our own child.
                    unsafe { libc::kill(pid as i32, libc::SIGINT) };
                }
                if tokio::time::timeout(FINISH_TIMEOUT, child.wait()).await.is_err() {
                    tracing::warn!("GStreamer did not finish the file in time");
                    let _ = child.kill().await;
                }
            }
            stop_session(&conn, &session).await;
            if !stopped {
                let message = last.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let message = if message.is_empty() { "the recording stopped".to_owned() } else { message };
                tracing::warn!(message, "recording failed");
                let mut inner = me.inner.lock().await;
                if inner.active.as_ref().is_some_and(|a| a.id == id) {
                    inner.active = None;
                }
                drop(inner);
                if let Ok(emitter) = SignalEmitter::new(&conn, OBJECT_PATH) {
                    let _ = Screencast::error(&emitter, ERROR_FAILED, &message).await;
                }
            }
            tracing::info!(path = %file.display(), "recording ended");
            let _ = me.ui.send(UiEvent::Recording(false));
            if stopped {
                if let Some(center) = &me.center {
                    center.screencast_recorded(file);
                }
            } else if std::fs::metadata(&file).is_ok_and(|m| m.len() == 0) {
                let _ = std::fs::remove_file(&file);
            }
            let _ = done_tx.send(());
        });
        Ok(path)
    }

    /// Stop the recording, once the file is finished; false without one.
    pub async fn stop(&self) -> bool {
        let Some(active) = self.inner.lock().await.active.take() else { return false };
        let _ = active.stop.send(());
        let _ = tokio::time::timeout(FINISH_TIMEOUT + Duration::from_secs(1), active.done).await;
        true
    }

    /// The `show-screen-recording-ui` key: stop a recording, or open the
    /// screenshot UI with Record chosen.
    pub async fn key(&self, conn: &Connection, screens: &Screens) {
        if self.stop().await {
            return;
        }
        self.screenshot_ui(conn, screens, true).await;
    }

    /// The screenshot UI from a key (recording allowed): take the
    /// screenshot or start the recording the user chose.
    pub async fn screenshot_ui(&self, conn: &Connection, screens: &Screens, record: bool) {
        let (shot, record, pointer) = match screens.pick(PickMode::Ui { record, allow_record: true }).await {
            Picked::Shot { target, record, pointer } => (target, record, pointer),
            _ => return,
        };
        if record {
            let target = match shot {
                Shot::Area(r) => Target::Area(r),
                Shot::Screen => Target::Screen,
                Shot::WindowAt(..) => return,
            };
            let options = Options { draw_cursor: pointer, ..Options::default() };
            if let Err(err) = self.start(conn, target, "Screencast From %d %t.webm", &options).await {
                tracing::warn!(err, "cannot record the screen");
            }
            return;
        }
        match screens.capture_to(shot.target(), pointer, true, &crate::screenshot::screenshot_path()).await {
            Ok(path) => screens.taken(&path),
            Err(err) => tracing::info!(?err, "no screenshot taken"),
        }
    }
}

/// The served `org.gnome.Shell.Screencast` object.
pub struct Screencast {
    pub recorder: Recorder,
}

async fn check(conn: &Connection, header: &Header<'_>) -> fdo::Result<()> {
    if crate::screenshot::allowed(conn, header).await {
        Ok(())
    } else {
        Err(fdo::Error::AccessDenied("screen recording is restricted".into()))
    }
}

impl Screencast {
    async fn record(&self, conn: &Connection, target: Target, template: &str, options: &HashMap<String, OwnedValue>) -> (bool, String) {
        match self.recorder.start(conn, target, template, &Options::from_dict(options)).await {
            Ok(path) => (true, path.to_string_lossy().into_owned()),
            Err(err) => {
                tracing::info!(err, "no recording");
                (false, String::new())
            }
        }
    }
}

#[interface(name = "org.gnome.Shell.Screencast")]
impl Screencast {
    async fn screencast(
        &self,
        file_template: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<(bool, String)> {
        check(conn, &header).await?;
        Ok(self.record(conn, Target::Screen, file_template, &options).await)
    }

    #[allow(clippy::too_many_arguments)]
    async fn screencast_area(
        &self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        file_template: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<(bool, String)> {
        check(conn, &header).await?;
        Ok(self.record(conn, Target::Area(Rect::new(x, y, width, height)), file_template, &options).await)
    }

    async fn stop_screencast(&self, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> fdo::Result<bool> {
        check(conn, &header).await?;
        Ok(self.recorder.stop().await)
    }

    #[zbus(property)]
    async fn screencast_supported(&self) -> bool {
        self.recorder.supported().await
    }

    #[zbus(signal)]
    pub async fn error(emitter: &SignalEmitter<'_>, name: &str, message: &str) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates() {
        assert_eq!(expand("Screencast From %d %t", "2026-11-11", "10-00-00"), "Screencast From 2026-11-11 10-00-00");
        assert_eq!(expand("100%% %x %", "d", "t"), "100% %x %");
        let videos = Path::new("/home/u/Videos");
        assert_eq!(output_path("rec", videos, false), Path::new("/home/u/Videos/rec.webm"));
        assert_eq!(output_path("rec", videos, true), Path::new("/home/u/Videos/rec.mkv"));
        assert_eq!(output_path("/tmp/a.webm", videos, false), Path::new("/tmp/a.webm"));
    }

    #[test]
    fn options() {
        let owned = |v: Value<'_>| OwnedValue::try_from(v).unwrap();
        let o = Options::from_dict(&HashMap::from([
            ("draw-cursor".to_owned(), owned(Value::Bool(false))),
            ("framerate".to_owned(), owned(Value::I32(15))),
            ("pipeline".to_owned(), owned(Value::from("x264enc"))),
        ]));
        assert_eq!(o, Options { draw_cursor: false, framerate: 15, pipeline: Some("x264enc".into()) });
        assert_eq!(Options::from_dict(&HashMap::from([("framerate".to_owned(), owned(Value::I32(0)))])), Options::default());
    }

    #[test]
    fn pipelines() {
        let args = gst_args(42, &Options::default(), Path::new("/v/Screencast \"x\".webm")).join(" ");
        assert_eq!(
            args,
            "-e pipewiresrc path=42 do-timestamp=true keepalive-time=1000 ! videorate ! video/x-raw,framerate=30/1 ! videoconvert ! queue ! \
             vp8enc deadline=1 cpu-used=8 threads=4 ! queue ! webmmux ! filesink location=\"/v/Screencast \\\"x\\\".webm\""
        );
        let custom = Options { pipeline: Some("x264enc".into()), framerate: 10, ..Options::default() };
        assert!(gst_args(1, &custom, Path::new("/a.mkv")).join(" ").contains("framerate=10/1 ! videoconvert ! queue ! x264enc ! queue ! matroskamux ! filesink"));
    }
}
