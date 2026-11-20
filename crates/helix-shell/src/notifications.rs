//! Notification servers: `org.freedesktop.Notifications` and
//! `org.gtk.Notifications` (contracts in helixdesktop/spec), sharing one
//! queue of banners shown by the shell UI.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use gio::prelude::*;
use tokio::sync::mpsc;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};
use zbus::{fdo, interface, Connection};

use crate::icons::IconSource;
use crate::notify_store::{CloseReason, IgnoreTimeouts, Notification, Origin, Store, Urgency};
use crate::sound::Sound;
use crate::screensaver::{self, SharedStatus};
use crate::UiEvent;
use helix_i18n::tr;
use helix_i18n::{trf, trnf};

pub const FDO_BUS_NAME: &str = "org.freedesktop.Notifications";
pub const FDO_PATH: &str = "/org/freedesktop/Notifications";
pub const GTK_BUS_NAME: &str = "org.gtk.Notifications";
pub const GTK_PATH: &str = "/org/gtk/Notifications";
/// The user counts as active (banner timers run) with input this recent.
const ACTIVE_WINDOW: Duration = Duration::from_secs(5);
/// Helix's own settings: the ignore periods.
const HELIX_SCHEMA: &str = "org.helixdesktop.shell";
/// Longer summaries and bodies are cut (in characters).
pub const MAX_SUMMARY: usize = 1_000;
pub const MAX_BODY: usize = 10_000;

/// Cut `text` to at most `max` characters, with an ellipsis.
pub fn clip(text: &mut String, max: usize) {
    if let Some((at, _)) = text.char_indices().nth(max) {
        text.truncate(at);
        text.push('…');
    }
}

/// What the UI draws for the current banner.
#[derive(Debug, Clone, PartialEq)]
pub struct Banner {
    pub id: u32,
    pub app_name: String,
    /// See [`Notification::app_id`].
    pub app_id: String,
    pub summary: String,
    pub body: String,
    /// Buttons as (key, label).
    pub buttons: Vec<(String, String)>,
    pub icon: Option<IconSource>,
    /// `body` uses notification markup.
    pub markup: bool,
    /// The app's banners show their whole text (`force-expanded`).
    pub expanded: bool,
    /// When it arrived, Unix seconds (0: unknown).
    pub arrived: i64,
    /// Critical urgency: shown above full-screen windows, which hide the
    /// others.
    pub critical: bool,
}

impl From<&Notification> for Banner {
    fn from(n: &Notification) -> Self {
        Self {
            id: n.id,
            app_name: n.app_name.clone(),
            app_id: n.app_id.clone(),
            summary: n.summary.clone(),
            body: n.body.clone(),
            buttons: n.actions.iter().filter(|(k, _)| k != "default").cloned().collect(),
            icon: n.icon.clone(),
            markup: n.markup,
            expanded: crate::app_notify::force_expanded(&n.app_id),
            arrived: 0,
            critical: n.urgency == Urgency::Critical,
        }
    }
}

/// How long ago a notification arrived (`arrived`, Unix seconds) at
/// `now`: "now", "5 min", "2 h" (today), "Yesterday", a weekday within
/// the week, then a date ("14 Nov"; with the year when not this year).
/// `long`: the words for a screen reader ("just now", "5 minutes ago",
/// "2 hours ago", …). Empty for an unknown time.
pub fn arrival(arrived: i64, now: &gio::glib::DateTime, long: bool) -> String {
    let Some(then) = gio::glib::DateTime::from_unix_local(arrived).ok().filter(|_| arrived > 0) else { return String::new() };
    let secs = (now.to_unix() - arrived).max(0);
    let day = |d: &gio::glib::DateTime| (d.year(), d.day_of_year());
    let yesterday = now.add_days(-1).ok();
    if secs < 60 {
        return if long { tr("just now").into() } else { tr("now").into() };
    }
    if secs < 3600 {
        let m = secs / 60;
        // Translators: how long ago a notification came, for a screen reader.
        return if long { trnf!("{n} minute ago", "{n} minutes ago", m) } else { trf!("{m} min", m = m) };
    }
    if day(&then) == day(now) {
        let h = secs / 3600;
        // Translators: "h" is short for hours; how long ago a notification came.
        return if long { trnf!("{n} hour ago", "{n} hours ago", h) } else { trf!("{h} h", h = h) };
    }
    if yesterday.as_ref().is_some_and(|y| day(y) == day(&then)) {
        return tr("Yesterday").into();
    }
    // Translators: GLib date formats (strftime-like) for notifications
    // older than yesterday: a weekday, then day and month, then with the
    // year; the long forms are read out by screen readers.
    let format = if secs < 6 * 86_400 {
        tr("%A")
    } else if then.year() == now.year() {
        if long { tr("%-d %B") } else { tr("%-d %b") }
    } else if long {
        tr("%-d %B %Y")
    } else {
        tr("%-d %b %Y")
    };
    then.format(format).map(|s| s.to_string()).unwrap_or_default()
}

/// What the notification list shows.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct List {
    /// Newest first (buttons are not shown in the list).
    pub entries: Vec<Banner>,
    /// Posted since the list was last opened.
    pub unseen: bool,
}

/// Shared by both D-Bus objects, the expiry ticker and the UI bridge.
#[derive(Clone)]
pub struct Center {
    store: Arc<Mutex<Store>>,
    /// org.gtk action parameters, by notification id and action key.
    targets: Arc<Mutex<HashMap<(u32, String), OwnedValue>>>,
    ui: mpsc::UnboundedSender<UiEvent>,
    lock: SharedStatus,
    /// `org.gnome.desktop.notifications show-banners` (false = do not disturb).
    pub show_banners: Arc<AtomicBool>,
    shown: Arc<Mutex<Option<u32>>>,
    listed: Arc<Mutex<Option<List>>>,
    /// Freedesktop notifications closed for the list limit, to signal.
    overflowed: Arc<Mutex<Vec<u32>>>,
    /// The banner the pointer is over or the keyboard is in: its timer
    /// waits.
    held: Arc<Mutex<Option<u32>>>,
}

impl Center {
    pub fn new(ui: mpsc::UnboundedSender<UiEvent>, lock: SharedStatus) -> Self {
        Self {
            store: Default::default(),
            targets: Default::default(),
            ui,
            lock,
            show_banners: Arc::new(AtomicBool::new(true)),
            shown: Default::default(),
            listed: Default::default(),
            overflowed: Default::default(),
            held: Default::default(),
        }
    }

    /// The pointer entered or left banner `id`, or keyboard focus moved
    /// in or out of it.
    pub fn hold_banner(&self, id: u32, held: bool) {
        let mut current = self.held.lock().unwrap_or_else(|e| e.into_inner());
        if held {
            *current = Some(id);
        } else if *current == Some(id) {
            *current = None;
        }
    }

    /// New ignore periods (`banner-ignore-timeout` and its critical twin).
    pub fn set_ignore_timeouts(&self, ignore: IgnoreTimeouts) {
        self.store().ignore = ignore;
    }

    fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Tell the UI what to show now (if it changed).
    pub fn refresh(&self) {
        let locked = screensaver::status(&self.lock).locked();
        let banners = self.show_banners.load(Ordering::Relaxed);
        let banner = {
            let mut store = self.store();
            if locked {
                store.requeue_current();
                None
            } else {
                let shown = store.current(Instant::now(), |n| banners || n.urgency == Urgency::Critical).map(Banner::from);
                shown.map(|b| Banner { arrived: store.arrived(b.id), ..b })
            }
        };
        {
            let mut shown = self.shown.lock().unwrap_or_else(|e| e.into_inner());
            let id = banner.as_ref().map(|b| b.id);
            if *shown != id || banner.is_some() {
                *shown = id;
                let _ = self.ui.send(UiEvent::Banner(banner));
            }
        }
        let list = {
            let store = self.store();
            List { entries: store.list().into_iter().map(|n| Banner { arrived: store.arrived(n.id), ..Banner::from(n) }).collect(), unseen: store.unseen }
        };
        let mut listed = self.listed.lock().unwrap_or_else(|e| e.into_inner());
        if listed.as_ref() != Some(&list) {
            *listed = Some(list.clone());
            let _ = self.ui.send(UiEvent::Notifications(list));
        }
    }

    /// The list as agents read it (helixdesktop/desktop#146): newest
    /// first, each with when it arrived (Unix seconds).
    pub fn listed(&self) -> Vec<(Notification, i64)> {
        let store = self.store();
        store.list().into_iter().map(|n| (n.clone(), store.arrived(n.id))).collect()
    }

    /// The user opened the list: nothing is unseen any more.
    pub fn mark_seen(&self) {
        self.store().unseen = false;
        self.refresh();
    }

    /// "Clear": dismiss everything in the list.
    pub async fn clear(&self, conn: &Connection) -> zbus::Result<()> {
        let ids: Vec<u32> = self.store().list().iter().map(|n| n.id).collect();
        for id in ids {
            self.close(conn, id, CloseReason::Dismissed).await?;
        }
        Ok(())
    }

    /// "Screenshot captured", for a screenshot the user took.
    pub fn screenshot_captured(&self, path: std::path::PathBuf) -> u32 {
        self.post(Notification {
            id: 0,
            app_name: tr("Screenshot").into(),
            summary: tr("Screenshot captured").into(),
            body: tr("You can paste the image from the clipboard.").into(),
            actions: vec![("default".into(), tr("Open").into()), ("show-in-files".into(), tr("Show in Files").into())],
            has_default: true,
            urgency: Urgency::Normal,
            timeout: Notification::timeout_from(-1, Urgency::Normal),
            icon: Some(IconSource::File(path.clone())),
            origin: Origin::Screenshot { path },
            markup: false,
            transient: false,
            app_id: String::new(),
        }, None)
    }

    /// Whether the session is locked.
    pub fn locked(&self) -> bool {
        screensaver::status(&self.lock).locked()
    }

    /// Take back one of the shell's own notifications.
    pub fn withdraw(&self, id: u32) {
        self.remove(id);
    }

    /// About removable media; with `root`, "Open with Files" and "Eject"
    /// buttons for that mount.
    pub fn media_notice(&self, summary: &str, body: &str, root: Option<String>) -> u32 {
        let actions = if root.is_some() { vec![("open".into(), tr("Open with Files").into()), ("eject".into(), tr("Eject").into())] } else { Vec::new() };
        self.post(Notification {
            id: 0,
            app_name: tr("Removable Media").into(),
            summary: summary.into(),
            body: body.into(),
            actions,
            has_default: false,
            urgency: Urgency::Normal,
            timeout: Notification::timeout_from(-1, Urgency::Normal),
            icon: Some(IconSource::Names(vec!["media-removable-symbolic".into()])),
            origin: Origin::Media { root },
            markup: false,
            transient: false,
            app_id: String::new(),
        }, None)
    }

    /// A notice from the system itself (battery warnings).
    pub fn system_notice(&self, summary: &str, body: &str, critical: bool) -> u32 {
        let icon = if critical { "battery-caution-symbolic" } else { "battery-low-symbolic" };
        self.notice(tr("Power"), summary, body, icon, critical)
    }

    /// One of the shell's own notices, as `app` with an icon by name.
    pub fn notice(&self, app: &str, summary: &str, body: &str, icon: &str, critical: bool) -> u32 {
        self.notice_with(0, app, summary, body, icon, critical)
    }

    /// [`Center::notice`] in place of notice `replaces` (0: a new one).
    pub fn notice_replacing(&self, replaces: u32, app: &str, summary: &str, body: &str, icon: &str) -> u32 {
        self.notice_with(replaces, app, summary, body, icon, false)
    }

    fn notice_with(&self, id: u32, app: &str, summary: &str, body: &str, icon: &str, critical: bool) -> u32 {
        let urgency = if critical { Urgency::Critical } else { Urgency::Normal };
        self.post(Notification {
            id,
            app_name: app.into(),
            summary: summary.into(),
            body: body.into(),
            actions: Vec::new(),
            has_default: false,
            urgency,
            timeout: Notification::timeout_from(-1, urgency),
            icon: Some(IconSource::Names(vec![icon.into()])),
            origin: Origin::Freedesktop { resident: false },
            markup: false,
            transient: false,
            app_id: String::new(),
        }, None)
    }

    /// A notification an agent shows (helixdesktop/desktop#147), under
    /// "Agent: <agent>" (`agent` the label the indicator gives it) in place
    /// of an app's name, so it cannot pass for the system's or an app's.
    /// Plain text, never critical, no buttons.
    pub fn agent_notice(&self, agent: &str, summary: &str, body: &str, icon: Option<&str>, low: bool) -> u32 {
        let urgency = if low { Urgency::Low } else { Urgency::Normal };
        let plain = |text: &str| text.chars().filter(|c| !c.is_control() || *c == '\n').collect::<String>();
        self.post(Notification {
            id: 0,
            app_name: trf!("Agent: {agent}", agent = agent),
            summary: plain(summary).replace('\n', " "),
            body: plain(body),
            actions: Vec::new(),
            has_default: false,
            urgency,
            timeout: Notification::timeout_from(-1, urgency),
            icon: Some(IconSource::Names(vec![icon.unwrap_or("dialog-information-symbolic").into(), "dialog-information-symbolic".into()])),
            origin: Origin::Freedesktop { resident: false },
            markup: false,
            transient: false,
            app_id: String::new(),
        }, None)
    }

    /// "<title> is ready": window `id` of app `app_id` wants attention.
    pub fn window_ready(&self, id: u32, title: &str, app_id: &str) -> u32 {
        use gio::prelude::*;
        let wanted = format!("{app_id}.desktop");
        let info = gio::AppInfo::all().into_iter().find(|a| a.id().is_some_and(|i| i.eq_ignore_ascii_case(&wanted)));
        let app_name = info.as_ref().map_or_else(|| app_id.to_owned(), |i| i.name().to_string());
        let icon = info.as_ref().and_then(|i| i.icon()).and_then(|i| i.downcast::<gio::ThemedIcon>().ok()).map(|t| t.names().iter().map(|n| n.to_string()).collect::<Vec<_>>());
        let title = if title.is_empty() { app_name.clone() } else { title.to_owned() };
        self.post_for(
            Notification {
                id: 0,
                app_name,
                summary: trf!("{title} is ready", title = title),
                body: String::new(),
                actions: Vec::new(),
                has_default: true,
                urgency: Urgency::Normal,
                timeout: Notification::timeout_from(-1, Urgency::Normal),
                icon: Some(IconSource::Names(icon.unwrap_or_else(|| vec!["dialog-information-symbolic".into()]))),
                origin: Origin::Window { id },
                markup: false,
                transient: false,
                app_id: String::new(),
            },
            None,
            info.is_some().then_some(app_id),
        )
    }

    /// "Screencast recorded", naming the file.
    pub fn screencast_recorded(&self, path: std::path::PathBuf) -> u32 {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        self.post(Notification {
            id: 0,
            app_name: tr("Screencast").into(),
            summary: tr("Screencast recorded").into(),
            body: trf!("Saved as {name}.", name = name),
            actions: vec![("default".into(), tr("Open").into()), ("show-in-files".into(), tr("Show in Files").into())],
            has_default: true,
            urgency: Urgency::Normal,
            timeout: Notification::timeout_from(-1, Urgency::Normal),
            icon: None,
            origin: Origin::Screenshot { path },
            markup: false,
            transient: false,
            app_id: String::new(),
        }, None)
    }

    /// Show (or, with do not disturb, only list) a notification; play its
    /// sound if it gets a banner. Closes the oldest beyond the list's limit.
    fn post(&self, mut notification: Notification, sound: Option<Sound>) -> u32 {
        clip(&mut notification.summary, MAX_SUMMARY);
        clip(&mut notification.body, MAX_BODY);
        // The app's own settings (none for the shell's own).
        let app = match &notification.origin {
            Origin::Gtk { app_id, .. } => Some(app_id.clone()),
            _ => None,
        };
        self.post_for(notification, sound, app.as_deref())
    }

    /// [`Center::post`] for a notification of app `app_id`.
    fn post_for(&self, mut notification: Notification, sound: Option<Sound>, app_id: Option<&str>) -> u32 {
        notification.app_id = app_id.unwrap_or_default().to_owned();
        let policy = app_id.map(crate::app_notify::policy).unwrap_or_default();
        let critical = notification.urgency == Urgency::Critical;
        if !policy.enable {
            // Dropped: an id, and nothing else.
            let mut store = self.store();
            let id = store.add(notification, false);
            store.close(id);
            tracing::debug!(id, app_id, "notifications of this app are off");
            return id;
        }
        let banner = critical || (self.show_banners.load(Ordering::Relaxed) && policy.banners);
        let (id, overflow) = {
            let mut store = self.store();
            let id = store.add(notification, banner);
            (id, store.overflow())
        };
        if let Some(sound) = sound.filter(|_| banner && policy.sounds) {
            crate::sound::play(&sound);
        }
        for old in overflow {
            self.expire_quietly(old);
        }
        self.refresh();
        id
    }

    /// Close `id` as expired from a context without a connection (the
    /// list limit): the signal goes out from the ticker's connection.
    fn expire_quietly(&self, id: u32) {
        if let Some(n) = self.remove(id) {
            if matches!(n.origin, Origin::Freedesktop { .. }) {
                self.overflowed.lock().unwrap_or_else(|e| e.into_inner()).push(id);
            }
        }
    }

    /// Remove a notification; returns it so callers can signal.
    fn remove(&self, id: u32) -> Option<Notification> {
        let n = self.store().close(id);
        self.targets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(nid, _), _| *nid != id);
        if n.is_some() {
            self.refresh();
        }
        n
    }

    /// Close and tell freedesktop clients why.
    pub async fn close(&self, conn: &Connection, id: u32, reason: CloseReason) -> zbus::Result<()> {
        if let Some(n) = self.remove(id) {
            if matches!(n.origin, Origin::Freedesktop { .. }) {
                let emitter = SignalEmitter::new(conn, FDO_PATH)?;
                FdoNotifications::notification_closed(&emitter, id, reason as u32).await?;
            }
        }
        Ok(())
    }

    /// Run an org.gtk notification's action `key` (`default` or
    /// `button-N`), with the user's reply if it asked for one.
    async fn gtk_activate(&self, conn: &Connection, id: u32, n: &Notification, key: &str, response: Option<String>) -> zbus::Result<()> {
        let Origin::Gtk { app_id, id: gtk_id, default_action, buttons, .. } = &n.origin else { return Ok(()) };
        let action = if key == "default" {
            default_action.clone()
        } else {
            key.strip_prefix("button-").and_then(|i| i.parse::<usize>().ok()).and_then(|i| buttons.get(i).cloned())
        };
        let target = self.targets.lock().unwrap_or_else(|e| e.into_inner()).get(&(id, key.to_owned())).and_then(|v| v.try_clone().ok());
        let parameter = action_parameter(target, response.as_deref());
        // For apps reached by signal (the portal's): the reply also in the
        // platform data, where the portal backend looks for it.
        let mut platform: HashMap<&str, Value<'_>> = HashMap::new();
        if let Some(text) = response.as_deref() {
            platform.insert(RESPONSE_KEY, Value::from(text.to_owned()));
        }
        // The app may present a window: with the shell's token it gets
        // focus (`activation.rs`).
        let token = crate::activation::token_async(Some(app_id.clone())).await;
        if let Some(token) = &token {
            platform.insert("activation-token", Value::from(token.clone()));
        }
        let signal = |name: &str| {
            let parameter: Vec<Value<'_>> = parameter.iter().filter_map(|v| v.try_clone().ok()).map(Value::from).collect();
            (name.to_owned(), parameter)
        };
        match action.as_deref() {
            // Not an `app.` action: the app hears about it by signal.
            Some(name) if !name.starts_with("app.") => {
                let emitter = SignalEmitter::new(conn, GTK_PATH)?;
                let (name, parameter) = signal(name);
                GtkNotifications::action_invoked(&emitter, app_id, gtk_id, &name, parameter, platform).await?;
            }
            _ => {
                let owned: Vec<OwnedValue> = parameter.iter().filter_map(|v| v.try_clone().ok()).collect();
                if let Err(err) = activate_app(conn, app_id, action.as_deref(), owned, token.as_deref()).await {
                    tracing::info!(%err, app_id, "cannot activate application");
                    // An app that cannot be reached (not on the bus, as
                    // portal clients may be) hears by signal.
                    if let (Some(name), true) = (action.as_deref(), unreachable(&err)) {
                        let emitter = SignalEmitter::new(conn, GTK_PATH)?;
                        let (name, parameter) = signal(name);
                        GtkNotifications::action_invoked(&emitter, app_id, gtk_id, &name, parameter, platform).await?;
                    }
                }
            }
        }
        self.remove(id);
        Ok(())
    }

    /// The user clicked the banner (`action` = None means the default
    /// action) or a button (`Some(key)`).
    pub async fn activate(&self, conn: &Connection, id: u32, action: Option<String>) -> zbus::Result<()> {
        let Some(n) = self.store().get(id).cloned() else { return Ok(()) };
        let key = action.unwrap_or_else(|| "default".to_owned());
        match &n.origin {
            Origin::Freedesktop { resident } => {
                if key != "default" || n.has_default {
                    let emitter = SignalEmitter::new(conn, FDO_PATH)?;
                    // As GNOME: the token first, for the window the app
                    // presents (`activation.rs`).
                    if let Some(token) = crate::activation::token_async(None).await {
                        FdoNotifications::activation_token(&emitter, id, &token).await?;
                    }
                    FdoNotifications::action_invoked(&emitter, id, &key).await?;
                }
                if !resident {
                    self.close(conn, id, CloseReason::Dismissed).await?;
                }
            }
            Origin::Gtk { reply: Some(r), .. } if key == format!("button-{r}") => {
                // Ask for the reply first, without holding up other requests.
                let (center, conn) = (self.clone(), conn.clone());
                tokio::spawn(async move {
                    let prompt = crate::prompt::Prompt {
                        title: trf!("Reply to {}", n.app_name),
                        message: n.summary.clone(),
                        description: crate::markup::plain(&n.body),
                        entry: Some((tr("Reply").into(), String::new())),
                        continue_label: tr("Send").into(),
                        cancel_label: tr("Cancel").into(),
                        ..Default::default()
                    };
                    let answer = crate::prompt::ask(&center.ui, prompt).await;
                    crate::prompt::done(&center.ui);
                    let crate::prompt::Answer::Continue { entry: Some(text), .. } = answer else { return };
                    if let Err(err) = center.gtk_activate(&conn, id, &n, &key, Some(text)).await {
                        tracing::info!(%err, "cannot send the reply");
                    }
                });
            }
            Origin::Gtk { .. } => self.gtk_activate(conn, id, &n, &key, None).await?,
            Origin::Media { root } => {
                match (key.as_str(), root) {
                    ("open", Some(root)) => crate::media::request(root.clone(), crate::media::Request::Open),
                    ("eject", Some(root)) => crate::media::request(root.clone(), crate::media::Request::Eject),
                    _ => {}
                }
                self.remove(id);
            }
            Origin::Window { id: window } => {
                let _ = self.ui.send(UiEvent::ActivateWindow(*window));
                self.remove(id);
            }
            Origin::Screenshot { path } => {
                if key == "show-in-files" {
                    show_in_files(conn, path).await;
                } else {
                    open_uri(&gio::File::for_path(path).uri());
                }
                self.remove(id);
            }
        }
        Ok(())
    }

    /// End banners whose time is up: transient notifications close, the
    /// rest stay in the list.
    pub async fn tick(&self, conn: &Connection) -> zbus::Result<()> {
        let shown = *self.shown.lock().unwrap_or_else(|e| e.into_inner());
        let expired = match shown {
            Some(id) => {
                // Asked only while a banner is up.
                let active = user_active(conn).await;
                let held = *self.held.lock().unwrap_or_else(|e| e.into_inner()) == Some(id);
                let mut store = self.store();
                store.advance(Instant::now(), active, held);
                store.expired()
            }
            None => None,
        };
        if let Some(id) = expired {
            let transient = self.store().get(id).is_some_and(|n| n.transient);
            if transient {
                self.close(conn, id, CloseReason::Expired).await?;
            } else {
                self.store().banner_done(id);
                self.refresh();
            }
        }
        let overflowed = std::mem::take(&mut *self.overflowed.lock().unwrap_or_else(|e| e.into_inner()));
        if !overflowed.is_empty() {
            let emitter = SignalEmitter::new(conn, FDO_PATH)?;
            for id in overflowed {
                FdoNotifications::notification_closed(&emitter, id, CloseReason::Expired as u32).await?;
            }
        }
        Ok(())
    }
}

pub fn open_uri(uri: &str) {
    if let Err(err) = gio::AppInfo::launch_default_for_uri(uri, Some(&crate::app_scope::context(None))) {
        tracing::warn!(%err, uri, "cannot open");
    }
}

/// Show `path` in the file manager, or open its folder.
async fn show_in_files(conn: &Connection, path: &std::path::Path) {
    let uri = gio::File::for_path(path).uri().to_string();
    let shown = conn
        .call_method(
            Some("org.freedesktop.FileManager1"),
            "/org/freedesktop/FileManager1",
            Some("org.freedesktop.FileManager1"),
            "ShowItems",
            &(vec![uri.as_str()], ""),
        )
        .await;
    if let Err(err) = shown {
        tracing::info!(%err, "no file manager service; opening the folder");
        if let Some(dir) = path.parent() {
            open_uri(&gio::File::for_path(dir).uri());
        }
    }
}

/// Whether a call failed because nobody has (or may start) the app's name.
fn unreachable(err: &zbus::Error) -> bool {
    match err {
        zbus::Error::MethodError(name, _, _) => matches!(
            name.as_str(),
            "org.freedesktop.DBus.Error.ServiceUnknown" | "org.freedesktop.DBus.Error.NameHasNoOwner" | "org.freedesktop.DBus.Error.InvalidArgs"
        ),
        zbus::Error::InterfaceNotFound | zbus::Error::Names(_) => true,
        _ => false,
    }
}

/// `org.gnome.Calculator` → `/org/gnome/Calculator`.
pub fn app_object_path(app_id: &str) -> String {
    format!("/{}", app_id.replace('.', "/").replace('-', "_"))
}

/// Activate `app.NAME` (or the app itself) over org.freedesktop.Application.
async fn activate_app(
    conn: &Connection,
    app_id: &str,
    action: Option<&str>,
    parameter: Vec<OwnedValue>,
    token: Option<&str>,
) -> zbus::Result<()> {
    let path = app_object_path(app_id);
    let mut platform_data: HashMap<&str, Value<'_>> = HashMap::new();
    if let Some(token) = token {
        platform_data.insert("activation-token", Value::from(token.to_owned()));
        platform_data.insert("desktop-startup-id", Value::from(token.to_owned()));
    }
    match action.and_then(|a| a.strip_prefix("app.")) {
        Some(name) => {
            let parameter: Vec<Value<'_>> = parameter.into_iter().map(Value::from).collect();
            conn.call_method(
                Some(app_id),
                path.as_str(),
                Some("org.freedesktop.Application"),
                "ActivateAction",
                &(name, parameter, platform_data),
            )
            .await?;
        }
        None => {
            conn.call_method(Some(app_id), path.as_str(), Some("org.freedesktop.Application"), "Activate", &(platform_data,))
                .await?;
        }
    }
    Ok(())
}

/// The name of app `app_id` from its desktop file, else the id.
fn app_display_name(app_id: &str) -> String {
    if app_id.is_empty() {
        return tr("Application").into();
    }
    let wanted = format!("{app_id}.desktop");
    gio::AppInfo::all().into_iter().find(|a| a.id().is_some_and(|i| i == wanted)).map_or_else(|| app_id.to_owned(), |a| a.display_name().to_string())
}

fn hint_str(hints: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    match hints.get(key).map(|v| &**v) {
        Some(Value::Str(s)) => Some(s.to_string()),
        _ => None,
    }
}

/// org.freedesktop.Notifications.
pub struct FdoNotifications {
    pub center: Center,
}

#[interface(name = "org.freedesktop.Notifications")]
impl FdoNotifications {
    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> u32 {
        let urgency = match hints.get("urgency").map(|v| &**v) {
            Some(Value::U8(0)) => Urgency::Low,
            Some(Value::U8(2)) => Urgency::Critical,
            _ => Urgency::Normal,
        };
        let flag = |key: &str| matches!(hints.get(key).map(|v| &**v), Some(Value::Bool(true)));
        let resident = flag("resident");
        let transient = flag("transient");
        let icon = ["image-data", "image_data", "icon_data"]
            .iter()
            .find_map(|k| hints.get(*k).and_then(|v| IconSource::from_image_data(v)))
            .or_else(|| ["image-path", "image_path"].iter().find_map(|k| hint_str(&hints, k).and_then(|p| IconSource::from_string(&p))))
            .or_else(|| IconSource::from_string(&app_icon))
            .or_else(|| hint_str(&hints, "desktop-entry").and_then(|d| IconSource::for_app(&d)));
        let sound = if flag("suppress-sound") {
            None
        } else {
            hint_str(&hints, "sound-file")
                .map(|f| Sound::File(f.strip_prefix("file://").unwrap_or(&f).into()))
                .or_else(|| hint_str(&hints, "sound-name").map(Sound::Name))
        };
        let pairs: Vec<(String, String)> = actions
            .chunks(2)
            .filter(|c| c.len() == 2)
            .map(|c| (c[0].clone(), c[1].clone()))
            .collect();
        let has_default = pairs.iter().any(|(k, _)| k == "default");
        let app_name = if app_name.is_empty() {
            hint_str(&hints, "desktop-entry").unwrap_or_default()
        } else {
            app_name
        };
        let app_id = hint_str(&hints, "desktop-entry").map(|e| crate::app_notify::app_id_of_entry(&e).to_owned());
        let id = self.center.post_for(Notification {
            id: replaces_id,
            app_name,
            summary,
            body,
            actions: pairs,
            has_default,
            urgency,
            timeout: Notification::timeout_from(expire_timeout, urgency),
            origin: Origin::Freedesktop { resident },
            icon,
            markup: true,
            transient,
            app_id: String::new(),
        }, sound, app_id.as_deref());
        tracing::debug!(id, "notification");
        id
    }

    async fn close_notification(&self, id: u32, #[zbus(connection)] conn: &Connection) -> fdo::Result<()> {
        Ok(self.center.close(conn, id, CloseReason::Closed).await?)
    }

    async fn get_capabilities(&self) -> Vec<String> {
        ["actions", "body", "body-hyperlinks", "body-markup", "icon-static", "persistence", "sound"]
            .map(String::from)
            .to_vec()
    }

    #[zbus(out_args("name", "vendor", "version", "spec_version"))]
    async fn get_server_information(&self) -> (String, String, String, String) {
        ("Helix".into(), "Helix".into(), env!("CARGO_PKG_VERSION").into(), "1.2".into())
    }

    #[zbus(signal)]
    pub async fn notification_closed(emitter: &SignalEmitter<'_>, id: u32, reason: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn action_invoked(emitter: &SignalEmitter<'_>, id: u32, action_key: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn activation_token(emitter: &SignalEmitter<'_>, id: u32, activation_token: &str) -> zbus::Result<()>;
}

/// org.gtk.Notifications (GLib's GNotification).
pub struct GtkNotifications {
    pub center: Center,
}

fn dict_str(d: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    match d.get(key).map(|v| &**v) {
        Some(Value::Str(s)) => Some(s.to_string()),
        _ => None,
    }
}

fn unwrap_variant(v: &Value<'_>) -> Option<OwnedValue> {
    let inner = match v {
        Value::Value(inner) => &**inner,
        other => other,
    };
    inner.try_to_owned().ok()
}

/// The button purpose that asks for a text reply.
pub const REPLY_PURPOSE: &str = "im.reply-with-text";

/// The button purposes the shell understands (the portal's
/// `SupportedOptions` `button-purpose`).
pub const BUTTON_PURPOSES: &[&str] = &[REPLY_PURPOSE, "call.accept", "call.decline", "call.hang-up", "call.enable-speakerphone", "call.disable-speakerphone"];

/// A button's label: its own, else one for its purpose; `None` for a
/// button not to show (no label and no purpose Helix names, such as
/// `system.custom-alert`).
pub fn button_label(label: Option<&str>, purpose: &str) -> Option<String> {
    if let Some(label) = label.map(str::trim).filter(|l| !l.is_empty()) {
        return Some(label.to_owned());
    }
    let named = match purpose {
        REPLY_PURPOSE => tr("Reply"),
        "call.accept" => tr("Accept"),
        "call.decline" => tr("Decline"),
        "call.hang-up" => tr("Hang Up"),
        "call.enable-speakerphone" => tr("Speaker On"),
        "call.disable-speakerphone" => tr("Speaker Off"),
        _ => return None,
    };
    Some(named.to_owned())
}

/// A notification action's parameter: its target (if any), then the
/// user's reply (if any).
pub fn action_parameter(target: Option<OwnedValue>, response: Option<&str>) -> Vec<OwnedValue> {
    let mut out: Vec<OwnedValue> = target.into_iter().collect();
    if let Some(text) = response {
        out.push(OwnedValue::try_from(Value::from(text)).expect("string"));
    }
    out
}

/// Helix's key in `ActionInvoked`'s platform data carrying the reply
/// (the portal backend moves it into the parameter).
pub const RESPONSE_KEY: &str = "x-helix-response";

#[interface(name = "org.gtk.Notifications")]
impl GtkNotifications {
    async fn add_notification(&self, app_id: String, id: String, notification: HashMap<String, OwnedValue>) {
        let priority = dict_str(&notification, "priority");
        let urgency = match priority.as_deref() {
            Some("low") => Urgency::Low,
            Some("urgent") => Urgency::Critical,
            _ => Urgency::Normal,
        };
        let default_action = dict_str(&notification, "default-action");
        let mut actions = Vec::new();
        let mut button_actions = Vec::new();
        let mut targets = Vec::new();
        if let Some(target) = notification.get("default-action-target").and_then(|v| unwrap_variant(v)) {
            targets.push(("default".to_owned(), target));
        }
        let mut reply = None;
        if let Some(Value::Array(buttons)) = notification.get("buttons").map(|v| &**v) {
            for button in buttons.iter() {
                let Ok(owned) = button.try_to_owned() else { continue };
                let Ok(dict) = HashMap::<String, OwnedValue>::try_from(owned) else { continue };
                let purpose = dict_str(&dict, "purpose").unwrap_or_default();
                let Some(label) = button_label(dict_str(&dict, "label").as_deref(), &purpose) else { continue };
                let action = dict_str(&dict, "action").unwrap_or_default();
                let i = button_actions.len();
                if purpose == REPLY_PURPOSE && reply.is_none() {
                    reply = Some(i);
                }
                let key = format!("button-{i}");
                if let Some(target) = dict.get("target").and_then(|v| unwrap_variant(v)) {
                    targets.push((key.clone(), target));
                }
                actions.push((key, label));
                button_actions.push(action);
            }
        }
        let existing = self.center.store().find_gtk(&app_id, &id).unwrap_or(0);
        // The portal's notifications may have a body with markup, and
        // Helix's own keys for what GTK's notifications lack.
        let markup_body = dict_str(&notification, "markup-body");
        let transient = matches!(notification.get("x-helix-transient").map(|v| &**v), Some(Value::Bool(true)));
        let sound = dict_str(&notification, "x-helix-sound").map(|s| if s.starts_with('/') { Sound::File(s.into()) } else { Sound::Name(s) });
        let nid = self.center.post(Notification {
            id: existing,
            app_name: app_display_name(&app_id),
            summary: dict_str(&notification, "title").unwrap_or_default(),
            body: markup_body.clone().or_else(|| dict_str(&notification, "body")).unwrap_or_default(),
            actions,
            has_default: true,
            urgency,
            timeout: Notification::timeout_from(-1, urgency),
            icon: notification.get("icon").and_then(|v| IconSource::from_gicon(v)).or_else(|| IconSource::for_app(&app_id)),
            origin: Origin::Gtk { app_id, id, default_action, buttons: button_actions, reply },
            markup: markup_body.is_some(),
            transient,
            app_id: String::new(),
        }, sound);
        let mut map = self.center.targets.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|(n, _), _| *n != nid);
        for (key, target) in targets {
            map.insert((nid, key), target);
        }
    }

    async fn remove_notification(&self, app_id: String, id: String) {
        let found = self.center.store().find_gtk(&app_id, &id);
        if let Some(nid) = found {
            self.center.remove(nid);
        }
    }

    #[zbus(signal)]
    pub async fn action_invoked(
        emitter: &SignalEmitter<'_>,
        app_id: &str,
        id: &str,
        action: &str,
        parameter: Vec<Value<'_>>,
        platform_data: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<()>;
}

const SETTINGS_SCHEMA: &str = "org.gnome.desktop.notifications";

/// Whether the user has given input in the last [`ACTIVE_WINDOW`], by the
/// compositor's idle monitor. Without one, the user counts as active.
async fn user_active(conn: &Connection) -> bool {
    let reply = conn
        .call_method(
            Some("org.gnome.Mutter.IdleMonitor"),
            "/org/gnome/Mutter/IdleMonitor/Core",
            Some("org.gnome.Mutter.IdleMonitor"),
            "GetIdletime",
            &(),
        )
        .await;
    match reply.ok().and_then(|r| r.body().deserialize::<u64>().ok()) {
        Some(idle_ms) => Duration::from_millis(idle_ms) < ACTIVE_WINDOW,
        None => true,
    }
}

/// Follow `org.helixdesktop.shell banner-ignore-timeout` and
/// `critical-banner-ignore-timeout`.
pub async fn follow_ignore_timeouts(center: Center) {
    center.set_ignore_timeouts(read_ignore_timeouts());
    if !helix_settings::has_schema(HELIX_SCHEMA) {
        return;
    }
    let settings = helix_settings::GSettings::new(&[HELIX_SCHEMA]);
    let (mut changes, _watch) = settings.watch();
    while changes.recv().await.is_some() {
        let ignore = read_ignore_timeouts();
        tracing::debug!(?ignore, "banner ignore periods changed");
        center.set_ignore_timeouts(ignore);
    }
}

/// The banner ignore periods from `org.helixdesktop.shell`; each one an older
/// installed schema lacks keeps its default.
pub fn read_ignore_timeouts() -> IgnoreTimeouts {
    let s = helix_settings::Schema::open(HELIX_SCHEMA);
    let d = IgnoreTimeouts::default();
    IgnoreTimeouts {
        normal: s.get::<u32>("banner-ignore-timeout").map_or(d.normal, |v| IgnoreTimeouts::from_seconds(v, 0).normal),
        critical: s.get::<u32>("critical-banner-ignore-timeout").map_or(d.critical, |v| IgnoreTimeouts::from_seconds(0, v).critical),
    }
}

/// Turn do not disturb on or off (`show-banners`, inverted).
pub fn set_do_not_disturb(on: bool) {
    if !helix_settings::has_schema(SETTINGS_SCHEMA) {
        return;
    }
    let settings = helix_settings::Settings::new(SETTINGS_SCHEMA);
    if let Err(err) = settings.set_boolean("show-banners", !on) {
        tracing::warn!(%err, "cannot change show-banners");
    }
    gio::Settings::sync();
}

/// Follow `org.gnome.desktop.notifications show-banners` (do not disturb).
pub async fn follow_settings(center: Center) {
    const SCHEMA: &str = SETTINGS_SCHEMA;
    if !helix_settings::has_schema(SCHEMA) {
        return;
    }
    let read = || {
        helix_settings::Settings::new(SCHEMA).boolean("show-banners")
    };
    center.show_banners.store(read(), Ordering::Relaxed);
    let _ = center.ui.send(UiEvent::DoNotDisturb(!read()));
    center.refresh();
    let settings = helix_settings::GSettings::new(&[SCHEMA]);
    let (mut changes, _watch) = settings.watch();
    while changes.recv().await.is_some() {
        let on = read();
        tracing::info!(show_banners = on, "notification banners setting changed");
        center.show_banners.store(on, Ordering::Relaxed);
        let _ = center.ui.send(UiEvent::DoNotDisturb(!on));
        center.refresh();
    }
}

/// Expire banners on time.
pub fn spawn_ticker(center: Center, conn: Connection) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(200)).await;
            if let Err(err) = center.tick(&conn).await {
                tracing::warn!(%err, "notification expiry failed");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_text_is_clipped() {
        let mut short = "Hello".to_owned();
        clip(&mut short, 5);
        assert_eq!(short, "Hello");
        let mut long = "ééééé".repeat(3);
        clip(&mut long, 4);
        assert_eq!(long, "éééé…");
    }

    #[test]
    fn purposes() {
        assert_eq!(button_label(Some("Answer"), "call.accept").as_deref(), Some("Answer"));
        assert_eq!(button_label(None, "call.accept").as_deref(), Some("Accept"));
        assert_eq!(button_label(Some("  "), REPLY_PURPOSE).as_deref(), Some("Reply"));
        assert_eq!(button_label(None, "system.custom-alert"), None, "not shown");
        assert_eq!(button_label(None, ""), None);
        let o = |v: Value<'_>| OwnedValue::try_from(v).unwrap();
        let p = action_parameter(Some(o(Value::from(3u32))), Some("hi"));
        assert_eq!((u32::try_from(p[0].clone()).unwrap(), String::try_from(p[1].clone()).unwrap().as_str()), (3, "hi"));
        assert!(action_parameter(None, None).is_empty());
    }

    #[test]
    fn arrival_words() {
        let now = gio::glib::DateTime::from_local(2026, 11, 15, 14, 30, 0.0).unwrap();
        let at = |y, mo, d, h, mi| gio::glib::DateTime::from_local(y, mo, d, h, mi, 0.0).unwrap().to_unix();
        let both = |t| (arrival(t, &now, false), arrival(t, &now, true));
        assert_eq!(both(now.to_unix() - 20), ("now".into(), "just now".into()));
        assert_eq!(both(at(2026, 11, 15, 14, 25)), ("5 min".into(), "5 minutes ago".into()));
        assert_eq!(both(at(2026, 11, 15, 14, 29)), ("1 min".into(), "1 minute ago".into()));
        assert_eq!(both(at(2026, 11, 15, 12, 10)), ("2 h".into(), "2 hours ago".into()));
        assert_eq!(both(at(2026, 11, 14, 23, 50)), ("Yesterday".into(), "Yesterday".into()));
        assert_eq!(arrival(at(2026, 11, 12, 9, 0), &now, false), "Thursday");
        assert_eq!(both(at(2026, 10, 2, 9, 0)), ("2 Oct".into(), "2 October".into()));
        assert_eq!(arrival(at(2025, 10, 2, 9, 0), &now, false), "2 Oct 2025");
        assert_eq!(arrival(0, &now, false), "", "unknown");
    }

    #[test]
    fn object_paths() {
        assert_eq!(app_object_path("org.gnome.Calculator"), "/org/gnome/Calculator");
        assert_eq!(app_object_path("org.example.my-app"), "/org/example/my_app");
    }
}
