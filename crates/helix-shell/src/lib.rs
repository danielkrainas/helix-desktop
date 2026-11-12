//! Helix shell.
//!
//! - [`ShellService`] serves `org.gnome.Shell` (contract:
//!   `helix-desktop-spec/contracts/dbus/org.gnome.Shell.xml` and `.md`).
//! - Requests that need UI (OSD, overview) are published as [`UiEvent`]s for
//!   the shell UI.

pub mod accel;
pub mod app_notify;
pub mod auth;
pub mod display_change;
pub mod end_dialog;
pub mod extra_keys;
pub mod favorites;
pub mod font;
pub mod geoclue_agent;
pub mod ibus;
pub mod idle;
pub mod input_sources;
pub mod keyring_prompt;
pub mod launcher;
pub mod location;
pub mod logind;
pub mod night_light;
pub mod bluetooth_agent;
pub mod obex;
pub mod osk_words;
pub mod calendar;
pub mod click_type;
pub mod ical;
pub mod icons;
pub mod markup;
pub mod network_agent;
pub mod vpn_secrets;
pub mod media;
pub mod a11y_menu;
pub mod dock;
pub mod desktop_icons;
pub mod launcher_entry;
pub mod mount_handler;
pub mod notifications;
pub mod polkit;
pub mod portal_dialog;
pub mod power;
pub mod privacy;
pub mod prompt;
pub mod notify_store;
pub mod recording;
pub mod screensaver;
pub mod search;
pub mod secret_exchange;
/// Event sounds live in `helix-settings`, shared with the compositor.
pub use helix_settings::sound;
pub mod screencast_chooser;
pub mod screenshot;
pub mod shell_keys;
pub mod shortcuts_inhibit;
pub mod state;
pub mod status;
pub mod text;
pub mod ui;
pub mod wallpaper;
pub mod weather;
pub mod wellbeing;
pub mod windows;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use zbus::fdo::{DBusProxy, RequestNameFlags, RequestNameReply};
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};
use zbus::{connection, interface, Connection};

use crate::state::ShellState;

pub const BUS_NAME: &str = "org.gnome.Shell";
/// The lock screen action mode of `GrabAccelerator`.
pub const ACTION_MODE_LOCK_SCREEN: u32 = 4;
/// `grabFlags`: notify on release as well as press.
pub const GRAB_TRIGGER_RELEASE: u32 = 128;
pub const OBJECT_PATH: &str = "/org/gnome/Shell";
/// The GNOME version whose surface Helix implements (see gaps.md).
pub const SHELL_VERSION: &str = "51.0";

/// An on-screen display request.
#[derive(Debug, Clone, PartialEq)]
pub struct Osd {
    pub icon: Option<String>,
    pub label: Option<String>,
    pub level: Option<f64>,
    pub max_level: f64,
    pub connector: Option<String>,
}

impl Osd {
    fn from_params(params: &HashMap<String, OwnedValue>) -> Self {
        let string = |k: &str| match params.get(k).map(|v| &**v) {
            Some(Value::Str(s)) => Some(s.to_string()),
            _ => None,
        };
        let double = |k: &str| match params.get(k).map(|v| &**v) {
            Some(Value::F64(d)) => Some(*d),
            _ => None,
        };
        Self {
            icon: string("icon"),
            label: string("label"),
            level: double("level"),
            max_level: double("max_level").unwrap_or(1.0),
            connector: string("connector"),
        }
    }
}

/// Requests for the shell UI.
#[derive(Debug, Clone, PartialEq)]
pub enum UiEvent {
    Overview(bool),
    /// The accessibility switches changed (or were read at start).
    A11y(a11y_menu::Status),
    /// Whether the dwell click type window should show.
    ClickTypeWindow(bool),
    /// The compositor's dwell click type.
    ClickType(String),
    /// The dock's settings changed (or were read at start).
    Dock(dock::DockSettings),
    /// An app's launcher entry (count, progress, urgency); `None` when it
    /// went away.
    LauncherEntry(String, Option<launcher_entry::Entry>),
    /// Mounted drives (for the dock).
    Mounts(Vec<media::MountInfo>),
    /// The desktop icons to show (`None`: none).
    DesktopIcons(Option<Vec<desktop_icons::DesktopItem>>),
    /// Whether the trash holds anything.
    Trash(bool),
    Osd(Osd),
    MonitorLabels(HashMap<String, i32>),
    HideMonitorLabels,
    /// Ask the compositor to deliver this accelerator for `action` (also
    /// while locked, with `when_locked`).
    Bind { action: u32, accelerator: accel::Accelerator, when_locked: bool },
    Unbind { action: u32 },
    /// Lock the session now.
    Lock,
    /// Show the application launcher (`ShowApplications`, `FocusSearch`).
    Launcher,
    /// The notification banner to show, or none.
    Banner(Option<notifications::Banner>),
    /// The notification list changed.
    Notifications(notifications::List),
    /// The message tray's calendar data.
    Calendar(Box<calendar::CalendarData>),
    /// The media player for the lock screen.
    LockMedia(Option<calendar::Player>),
    /// Show (or update, or with `None` hide) the break screen.
    Break(Option<wellbeing::BreakView>),
    /// `org.gnome.desktop.notifications show-banners` changed.
    DoNotDisturb(bool),
    /// The end-session confirmation dialog to show, or none.
    EndDialog(Option<end_dialog::EndDialog>),
    /// Which power actions the Power menu may offer.
    PowerAvailability(logind::Availability),
    /// Ask whether to keep a display change (countdown seconds), or stop
    /// asking.
    DisplayChange(Option<u32>),
    /// Screenshot work for the UI (capture, picking, flash).
    Screenshot(screenshot::JobSlot),
    /// Ask which monitor to share with an app.
    ShareScreen(screencast_chooser::QuestionSlot),
    /// Ask whether an app may control input (remote desktop).
    AllowControl(screencast_chooser::ControlSlot),
    /// The open windows (for remembered screen casts).
    Windows(screencast_chooser::WindowsSlot),
    /// Whether the screen is being shared (panel indicator).
    Sharing(bool),
    /// Whether the screen is being recorded (panel indicator).
    Recording(bool),
    /// Key settings changed: bind the shell's keys again.
    RebindKeys,
    /// Ask for a password (or update the prompt shown).
    Prompt(prompt::PromptSlot),
    /// Close the prompt dialog.
    PromptDone,
    /// A search provider answered: its results for search `serial`.
    SearchResults { serial: u64, provider: usize, hits: Vec<launcher::SearchHit> },
    /// `org.gnome.desktop.a11y.interface` `high-contrast`.
    HighContrast(bool),
    /// `show-status-shapes` and `keyboard-focus-visible-timeout`.
    InterfaceA11y { shapes: bool, focus_timeout: i32 },
    /// System status (panel icons, quick settings switches).
    Status(Box<status::Status>),
    /// The desktop and lock screen backgrounds to show.
    Background { desktop: Box<wallpaper::Background>, lock: Box<wallpaper::Background> },
    /// From IBus, for the input method.
    Ime(ibus::ImEvent),
    /// Quick settings values: volume (percent, muted, max) and brightness
    /// (percent), each when available.
    QuickSettings { volume: Option<(u32, bool, u32)>, brightness: Option<u32> },
    /// The remembered answer for a window asking for all shortcuts.
    ShortcutsAnswer { id: u32, allowed: bool },
    /// Ask whether a window may have all shortcuts (no answer stored).
    AskShortcuts { id: u32, app_id: String },
}

/// What the quick settings menu changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuickChange {
    /// Nothing: just read the values.
    Refresh,
    Volume(u32),
    Mute,
    Brightness(u32),
    /// Wi-Fi, Bluetooth, airplane mode or the power profile.
    Toggle(status::Toggle),
}

/// Power menu entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    Lock,
    Suspend,
    LogOut,
    Restart,
    PowerOff,
}

/// Reports from the shell UI back to the D-Bus side.
#[derive(Debug, Clone, PartialEq)]
pub enum UiRequest {
    /// The UI opened/closed the launcher itself.
    Overview(bool),
    /// The dock's "Empty Trash": ask, then empty it.
    EmptyTrash,
    /// An entry of an app's quicklist was chosen.
    QuickItem(launcher_entry::Quicklist, i32),
    /// The dwell click type was chosen in its window.
    SetClickType(String),
    /// A bound accelerator was pressed or released.
    Accelerator { action: u32, pressed: bool, time: u32 },
    /// The compositor refused (or cannot offer) the binding.
    GrabFailed { action: u32 },
    /// The lock is in place.
    Locked,
    /// The user unlocked.
    Unlocked,
    /// The lock was refused or taken away.
    LockGone,
    /// A banner was clicked (`action` None) or one of its buttons pressed.
    NotificationActivated { id: u32, action: Option<String> },
    /// A banner's (or list entry's) close button was pressed.
    NotificationDismissed { id: u32 },
    /// The notification list was opened.
    NotificationListOpened,
    /// "Clear" in the notification list.
    NotificationsCleared,
    /// The Do Not Disturb switch was flipped (true = on).
    SetDoNotDisturb(bool),
    /// A link in a notification was clicked.
    OpenUri(String),
    /// The break screen's buttons.
    BreakAnswer(wellbeing::Answer),
    /// The lock screen wants the player (after pressing a button).
    LockMedia(Option<shell_keys::Playback>),
    /// The apps with windows (app id → 1 running, 2 focused).
    RunningApps(HashMap<String, u32>),
    /// The tray's calendar shows `[from, to)`: read its data.
    Calendar { from: i64, to: i64 },
    /// A media button in the tray; then read the calendar data again.
    Media { command: shell_keys::Playback, from: i64, to: i64 },
    /// The user answered the end-session dialog.
    EndDialogAnswer(end_dialog::Choice),
    /// A power menu entry was chosen.
    Power(PowerAction),
    /// The user kept or reverted a display change (or time ran out).
    DisplayChangeAnswer { keep: bool },
    /// A screenshot, brightness or volume key was pressed.
    ShellKey(shell_keys::KeyAction),
    /// For IBus, from the input method.
    Ime(ibus::ImRequest),
    /// Quick settings: change something (or nothing), then report.
    QuickSettings(QuickChange),
    /// Window `id` of app `app_id` asks for all keyboard shortcuts: look
    /// up the remembered answer.
    ShortcutsInhibit { id: u32, app_id: String },
    /// Remember the user's answer for `app_id`.
    StoreShortcutsAnswer { app_id: String, allowed: bool },
    /// The panel's "Recording" indicator was clicked.
    StopRecording,
    /// Ask the search providers (`query` empty: the launcher opened).
    Search { serial: u64, query: String },
    /// A search provider's result was chosen.
    SearchActivate { provider: usize, id: String, query: String },
    /// Open the provider's app with the search.
    SearchLaunch { provider: usize, query: String },
}

/// The exported `org.gnome.Shell` object.
pub struct Shell {
    state: Arc<Mutex<ShellState>>,
    ui: mpsc::UnboundedSender<UiEvent>,
    /// Whether the session is locked (the lock screen action mode).
    lock: screensaver::SharedStatus,
}

impl Shell {
    fn state(&self) -> MutexGuard<'_, ShellState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn bind_in_compositor(&self, action: u32) {
        if action == 0 {
            return;
        }
        let grab = self.state().grab_for(action).map(|g| (g.accelerator.clone(), g.mode_flags));
        if let Some((accelerator, mode_flags)) = grab {
            let when_locked = mode_flags & ACTION_MODE_LOCK_SCREEN != 0;
            let _ = self.ui.send(UiEvent::Bind { action, accelerator, when_locked });
        }
    }

    /// Emit AcceleratorActivated/Deactivated if the grab applies in the
    /// current action mode.
    pub async fn accelerator_event(
        &self,
        action: u32,
        pressed: bool,
        time: u32,
        emitter: &SignalEmitter<'_>,
    ) -> zbus::Result<()> {
        let locked = screensaver::status(&self.lock).locked();
        let (applies, mode, on_release) = {
            let state = self.state();
            let mode = if locked { ACTION_MODE_LOCK_SCREEN } else { state.action_mode() };
            let grab = state.grab_for(action);
            let applies = grab.is_some_and(|g| g.mode_flags & mode != 0);
            let on_release = grab.is_some_and(|g| g.grab_flags & GRAB_TRIGGER_RELEASE != 0);
            (applies, mode, on_release)
        };
        // Releases only for grabs that ask for them.
        if !applies || (!pressed && !on_release) {
            return Ok(());
        }
        let params = HashMap::from([
            ("device-id", Value::from(0u32)),
            ("timestamp", Value::from(time)),
            ("action-mode", Value::from(mode)),
            ("device-node", Value::from("")),
        ]);
        tracing::debug!(action, pressed, "accelerator");
        if pressed {
            Self::accelerator_activated(emitter, action, params).await
        } else {
            Self::accelerator_deactivated(emitter, action, params).await
        }
    }

    /// The compositor could not bind the accelerator; forget the grab.
    pub fn grab_failed(&self, action: u32) {
        if self.state().ungrab(action) {
            tracing::warn!(action, "accelerator could not be bound by the compositor; grab dropped");
        }
    }

    /// Overview state changed by the shell UI itself (no echo to the UI).
    pub async fn set_overview_from_ui(&self, active: bool, emitter: &SignalEmitter<'_>) -> zbus::Result<()> {
        if self.state().set_overview_active(active) {
            self.overview_active_changed(emitter).await?;
        }
        Ok(())
    }

    /// Show the launcher (part of the overview state, `OverviewActive`).
    async fn show_overview(&self, emitter: &SignalEmitter<'_>) -> zbus::Result<()> {
        let _ = self.ui.send(UiEvent::Launcher);
        if self.state().set_overview_active(true) {
            self.overview_active_changed(emitter).await?;
        }
        Ok(())
    }
}

fn sender(header: &Header<'_>) -> Option<String> {
    header.sender().map(|s| s.to_string())
}

#[interface(name = "org.gnome.Shell")]
impl Shell {
    async fn eval(&self, _script: &str) -> (bool, String) {
        // Eval is only available in unsafe mode, which Helix does not have.
        (false, String::new())
    }

    async fn focus_search(&self, #[zbus(signal_emitter)] e: SignalEmitter<'_>) -> zbus::fdo::Result<()> {
        Ok(self.show_overview(&e).await?)
    }

    #[zbus(name = "ShowOSD")]
    async fn show_osd(&self, params: HashMap<String, OwnedValue>) {
        let osd = Osd::from_params(&params);
        tracing::info!(?osd, "OSD");
        let _ = self.ui.send(UiEvent::Osd(osd));
    }

    async fn show_monitor_labels(&self, params: HashMap<String, OwnedValue>) {
        let labels = params
            .iter()
            .filter_map(|(connector, v)| match &**v {
                Value::I32(n) => Some((connector.clone(), *n)),
                Value::Value(inner) => match **inner {
                    Value::I32(n) => Some((connector.clone(), n)),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        let _ = self.ui.send(UiEvent::MonitorLabels(labels));
    }

    async fn hide_monitor_labels(&self) {
        let _ = self.ui.send(UiEvent::HideMonitorLabels);
    }

    async fn focus_app(&self, id: &str, #[zbus(signal_emitter)] e: SignalEmitter<'_>) -> zbus::fdo::Result<()> {
        tracing::info!(id, "FocusApp");
        Ok(self.show_overview(&e).await?)
    }

    async fn show_applications(&self, #[zbus(signal_emitter)] e: SignalEmitter<'_>) -> zbus::fdo::Result<()> {
        Ok(self.show_overview(&e).await?)
    }

    async fn grab_accelerator(
        &self,
        accelerator: &str,
        mode_flags: u32,
        grab_flags: u32,
        #[zbus(header)] header: Header<'_>,
    ) -> u32 {
        let action = self
            .state()
            .grab(accelerator, mode_flags, grab_flags, sender(&header).as_deref());
        tracing::info!(accelerator, action, "GrabAccelerator");
        self.bind_in_compositor(action);
        action
    }

    async fn grab_accelerators(
        &self,
        accelerators: Vec<(String, u32, u32)>,
        #[zbus(header)] header: Header<'_>,
    ) -> Vec<u32> {
        let owner = sender(&header);
        let actions: Vec<u32> = {
            let mut state = self.state();
            accelerators
                .iter()
                .map(|(accel, modes, flags)| state.grab(accel, *modes, *flags, owner.as_deref()))
                .collect()
        };
        for action in &actions {
            self.bind_in_compositor(*action);
        }
        actions
    }

    /// False if `action` was not grabbed.
    async fn ungrab_accelerator(&self, action: u32) -> bool {
        let found = self.state().ungrab(action);
        if found {
            let _ = self.ui.send(UiEvent::Unbind { action });
        }
        found
    }

    /// Releases every listed grab; false if any of them was not grabbed.
    async fn ungrab_accelerators(&self, actions: Vec<u32>) -> bool {
        let mut state = self.state();
        let mut all_found = true;
        for action in actions {
            // No short-circuit: release the rest even after a miss.
            let found = state.ungrab(action);
            if found {
                let _ = self.ui.send(UiEvent::Unbind { action });
            }
            all_found &= found;
        }
        all_found
    }

    async fn screen_transition(&self) {
        // No transition effect yet.
    }

    #[zbus(property)]
    async fn mode(&self) -> String {
        "user".to_owned()
    }

    #[zbus(property)]
    async fn overview_active(&self) -> bool {
        self.state().overview_active()
    }

    #[zbus(property)]
    async fn set_overview_active(&self, active: bool) {
        if self.state().set_overview_active(active) {
            let _ = self.ui.send(UiEvent::Overview(active));
        }
    }

    #[zbus(property)]
    async fn shell_version(&self) -> String {
        SHELL_VERSION.to_owned()
    }

    #[zbus(signal)]
    pub async fn accelerator_activated(
        emitter: &SignalEmitter<'_>,
        action: u32,
        parameters: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn accelerator_deactivated(
        emitter: &SignalEmitter<'_>,
        action: u32,
        parameters: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<()>;
}

/// The running `org.gnome.Shell` service.
pub struct ShellService {
    conn: Connection,
    state: Arc<Mutex<ShellState>>,
    watcher: JoinHandle<()>,
    auth: Arc<dyn auth::Authenticator>,
    center: notifications::Center,
    ticker: JoinHandle<()>,
    ui: mpsc::UnboundedSender<UiEvent>,
    night_light: night_light::Handle,
    ibus: Arc<tokio::sync::Mutex<ibus::Ibus>>,
    recorder: recording::Recorder,
}

impl ShellService {
    /// Serve `org.gnome.Shell`; UI requests arrive on the returned receiver.
    pub async fn start(
        builder: connection::Builder<'_>,
    ) -> zbus::Result<(Self, mpsc::UnboundedReceiver<UiEvent>)> {
        let state = Arc::new(Mutex::new(ShellState::new()));
        let (ui, ui_rx) = mpsc::unbounded_channel();
        let ui_for_watcher = ui.clone();
        let auth = auth::authenticator();
        let lock_status: screensaver::SharedStatus = Default::default();
        let saver = screensaver::ScreenSaver {
            status: lock_status.clone(),
            ui: ui.clone(),
            auth: auth.clone(),
        };
        let center = notifications::Center::new(ui.clone(), lock_status.clone());
        let night_light = night_light::Handle::default();
        let recorder = recording::Recorder::new(ui.clone(), Some(center.clone()));
        let prompter = keyring_prompt::Prompter::new(ui.clone());
        let conn = builder
            .serve_at(OBJECT_PATH, Shell { state: state.clone(), ui: ui.clone(), lock: lock_status.clone() })?
            .serve_at(screensaver::OBJECT_PATH, saver)?
            .serve_at(notifications::FDO_PATH, notifications::FdoNotifications { center: center.clone() })?
            .serve_at(notifications::GTK_PATH, notifications::GtkNotifications { center: center.clone() })?
            .serve_at(
                end_dialog::OBJECT_PATH,
                end_dialog::EndSessionDialog { ui: ui.clone(), open_kind: Default::default() },
            )?
            .serve_at(
                screenshot::OBJECT_PATH,
                screenshot::Screenshot { screens: screenshot::Screens::with_notifications(ui.clone(), center.clone()) },
            )?
            .serve_at(
                screenshot::OBJECT_PATH,
                screenshot::WindowPicker { screens: screenshot::Screens::new(ui.clone()) },
            )?
            .serve_at(recording::OBJECT_PATH, recording::Screencast { recorder: recorder.clone() })?
            .serve_at(keyring_prompt::OBJECT_PATH, prompter.clone())?
            .serve_at(night_light::OBJECT_PATH, night_light::ColorService { handle: night_light.clone() })?
            .serve_at(
                screencast_chooser::OBJECT_PATH,
                screencast_chooser::ScreenCastChooser { ui: ui.clone(), busy: Default::default() },
            )?
            .serve_at(
                display_change::OBJECT_PATH,
                display_change::DisplayChangeDialog { ui: ui.clone(), open: Default::default() },
            )?
            .build()
            .await?;
        match conn
            .request_name_with_flags(BUS_NAME, RequestNameFlags::DoNotQueue.into())
            .await?
        {
            RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner => {}
            _ => return Err(zbus::Error::NameTaken),
        }
        tracing::info!(name = BUS_NAME, "shell ready");
        match conn
            .request_name_with_flags(screensaver::BUS_NAME, RequestNameFlags::DoNotQueue.into())
            .await
        {
            Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {
                if let Err(err) = auth.available() {
                    tracing::warn!(%err, "screen locking unavailable");
                }
            }
            other => tracing::warn!(?other, "cannot own org.gnome.ScreenSaver; screen locking unavailable"),
        }
        for name in [notifications::FDO_BUS_NAME, notifications::GTK_BUS_NAME, night_light::BUS_NAME, recording::BUS_NAME] {
            match conn.request_name_with_flags(name, RequestNameFlags::DoNotQueue.into()).await {
                Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {}
                other => tracing::warn!(name, ?other, "cannot own service name"),
            }
        }
        // gcr's prompter: taken over from a running gcr prompter.
        match conn
            .request_name_with_flags(keyring_prompt::BUS_NAME, RequestNameFlags::ReplaceExisting | RequestNameFlags::DoNotQueue)
            .await
        {
            Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => prompter.follow_clients(conn.clone()),
            other => tracing::warn!(?other, "cannot own the keyring prompter name; keyring prompts use gcr's own dialog"),
        }
        let ticker = notifications::spawn_ticker(center.clone(), conn.clone());

        let mut changes = DBusProxy::new(&conn).await?.receive_name_owner_changed().await?;
        let watched = state.clone();
        let watcher_ui = ui_for_watcher;
        let watcher = tokio::spawn(async move {
            while let Some(signal) = changes.next().await {
                let Ok(args) = signal.args() else { continue };
                let name = args.name().as_str();
                if name.starts_with(':') && args.new_owner().is_none() {
                    let removed = watched
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove_owner(name);
                    if !removed.is_empty() {
                        tracing::debug!(name, ?removed, "released accelerators of departed client");
                    }
                    for action in removed {
                        let _ = watcher_ui.send(UiEvent::Unbind { action });
                    }
                }
            }
        });
        let ibus = Arc::new(tokio::sync::Mutex::new(ibus::Ibus::new(ui.clone())));
        Ok((Self { conn, state, watcher, auth, center, ticker, ui, night_light, ibus, recorder }, ui_rx))
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// The password checker the lock screen uses.
    pub fn authenticator(&self) -> Arc<dyn auth::Authenticator> {
        self.auth.clone()
    }

    /// Lock now (e.g. the compositor restarted us while locked).
    pub async fn lock(&self) -> zbus::Result<()> {
        lock_via(&self.conn).await
    }

    /// Accelerators currently grabbed, as (action, normalized accelerator).
    pub fn grabbed(&self) -> Vec<(u32, String)> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .grabs()
            .map(|(id, g)| (*id, g.accelerator.to_string()))
            .collect()
    }

    /// Night Light's state, shared with `org.gnome.SettingsDaemon.Color`.
    pub fn night_light(&self) -> night_light::Handle {
        self.night_light.clone()
    }

    /// Where UI events go (for tasks that report to the UI).
    pub fn ui_sender(&self) -> mpsc::UnboundedSender<UiEvent> {
        self.ui.clone()
    }

    /// Lets the UI report back (overview changes, key presses).
    pub fn ui_bridge(&self) -> UiBridge {
        UiBridge {
            conn: self.conn.clone(),
            center: self.center.clone(),
            ui: self.ui.clone(),
            ibus: self.ibus.clone(),
            recorder: self.recorder.clone(),
            search: search::Searcher::default(),
        }
    }

    /// The notification center (e.g. to follow settings).
    pub fn notifications(&self) -> notifications::Center {
        self.center.clone()
    }

    pub async fn shutdown(self) -> zbus::Result<()> {
        self.ticker.abort();
        self.watcher.abort();
        self.conn.release_name(BUS_NAME).await?;
        Ok(())
    }
}

/// See [`ShellService::ui_bridge`].
#[derive(Clone)]
pub struct UiBridge {
    conn: Connection,
    center: notifications::Center,
    ui: mpsc::UnboundedSender<UiEvent>,
    ibus: Arc<tokio::sync::Mutex<ibus::Ibus>>,
    recorder: recording::Recorder,
    search: search::Searcher,
}

impl UiBridge {
    pub async fn handle(&self, request: UiRequest) -> zbus::Result<()> {
        let iface = self
            .conn
            .object_server()
            .interface::<_, Shell>(OBJECT_PATH)
            .await?;
        let shell = iface.get().await;
        let emitter = iface.signal_emitter();
        match request {
            UiRequest::Overview(active) => shell.set_overview_from_ui(active, emitter).await,
            UiRequest::Accelerator { action, pressed, time } => {
                shell.accelerator_event(action, pressed, time, emitter).await
            }
            UiRequest::GrabFailed { action } => {
                shell.grab_failed(action);
                Ok(())
            }
            UiRequest::Locked | UiRequest::Unlocked | UiRequest::LockGone => {
                let saver = self
                    .conn
                    .object_server()
                    .interface::<_, screensaver::ScreenSaver>(screensaver::OBJECT_PATH)
                    .await?;
                let locked = request == UiRequest::Locked;
                let object = saver.get().await;
                let result = object.set_locked(locked, saver.signal_emitter()).await;
                // Banners wait while locked and come back after.
                self.center.refresh();
                result
            }
            UiRequest::NotificationActivated { id, action } => self.center.activate(&self.conn, id, action).await,
            UiRequest::EndDialogAnswer(choice) => {
                let dialog = self
                    .conn
                    .object_server()
                    .interface::<_, end_dialog::EndSessionDialog>(end_dialog::OBJECT_PATH)
                    .await?;
                let object = dialog.get().await;
                let result = object.answer(choice, dialog.signal_emitter()).await;
                result
            }
            UiRequest::ShortcutsInhibit { id, app_id } => {
                let event = match shortcuts_inhibit::lookup(&self.conn, &app_id).await {
                    Some(allowed) => UiEvent::ShortcutsAnswer { id, allowed },
                    None => UiEvent::AskShortcuts { id, app_id },
                };
                let _ = self.ui.send(event);
                Ok(())
            }
            UiRequest::StoreShortcutsAnswer { app_id, allowed } => {
                if let Err(err) = shortcuts_inhibit::store(&self.conn, &app_id, allowed).await {
                    tracing::warn!(%err, "cannot remember the shortcuts answer");
                }
                Ok(())
            }
            UiRequest::DisplayChangeAnswer { keep } => {
                let dialog = self
                    .conn
                    .object_server()
                    .interface::<_, display_change::DisplayChangeDialog>(display_change::OBJECT_PATH)
                    .await?;
                let object = dialog.get().await;
                let result = object.answer(keep, dialog.signal_emitter()).await;
                result
            }
            UiRequest::Power(action) => {
                let call = |member: &'static str, body: Option<u32>| {
                    let conn = self.conn.clone();
                    async move {
                        let (iface, path, name) = ("org.gnome.SessionManager", "/org/gnome/SessionManager", "org.gnome.SessionManager");
                        match body {
                            Some(mode) => conn.call_method(Some(name), path, Some(iface), member, &(mode,)).await,
                            None => conn.call_method(Some(name), path, Some(iface), member, &()).await,
                        }
                        .map(|_| ())
                    }
                };
                let result = match action {
                    PowerAction::Lock => lock_via(&self.conn).await,
                    PowerAction::Suspend => call("Suspend", None).await,
                    PowerAction::LogOut if locked_down("disable-log-out") => Err(zbus::Error::Failure("logging out is disabled".into())),
                    PowerAction::LogOut => call("Logout", Some(0)).await,
                    PowerAction::Restart => call("Reboot", None).await,
                    PowerAction::PowerOff => call("Shutdown", None).await,
                };
                if let Err(err) = &result {
                    tracing::warn!(%err, ?action, "power action failed");
                }
                Ok(())
            }
            UiRequest::Search { serial, query } => {
                self.search.search(&self.conn, &self.ui, serial, &query).await;
                Ok(())
            }
            UiRequest::SearchLaunch { provider, query } => {
                let (search, conn) = (self.search.clone(), self.conn.clone());
                tokio::spawn(async move { search.launch(&conn, provider, &query).await });
                Ok(())
            }
            UiRequest::SearchActivate { provider, id, query } => {
                let (search, conn) = (self.search.clone(), self.conn.clone());
                tokio::spawn(async move { search.activate(&conn, provider, &id, &query).await });
                Ok(())
            }
            UiRequest::StopRecording => {
                let recorder = self.recorder.clone();
                tokio::spawn(async move {
                    recorder.stop().await;
                });
                Ok(())
            }
            UiRequest::QuickSettings(change) => {
                let (conn, ui, center) = (self.conn.clone(), self.ui.clone(), self.center.clone());
                tokio::spawn(async move {
                    match change {
                        QuickChange::Refresh => {}
                        QuickChange::Volume(percent) => shell_keys::set_volume(percent).await,
                        QuickChange::Mute => shell_keys::toggle_mute().await,
                        QuickChange::Brightness(percent) => shell_keys::set_brightness_percent(&conn, percent).await,
                        QuickChange::Toggle(toggle) => match zbus::Connection::system().await {
                            Ok(system) => {
                                if let Err(err) = status::apply(&system, toggle.clone()).await {
                                    tracing::warn!(%err, ?toggle, "cannot change the system setting");
                                    if let status::Toggle::BluetoothPair { name, .. } = &toggle {
                                        let reason = match &err {
                                            zbus::Error::MethodError(_, Some(text), _) => text.clone(),
                                            other => other.to_string(),
                                        };
                                        center.notice("Bluetooth", &format!("Cannot pair with {name}"), &reason, "bluetooth-active-symbolic", false);
                                    }
                                }
                            }
                            Err(err) => tracing::warn!(%err, "no system bus"),
                        },
                    }
                    status::changed();
                    let (_, max) = shell_keys::volume_limits();
                    let volume = shell_keys::volume_state().await.map(|(p, m)| (p, m, max));
                    let brightness = shell_keys::brightness_percent(&conn).await;
                    let _ = ui.send(UiEvent::QuickSettings { volume, brightness });
                });
                Ok(())
            }
            UiRequest::ShellKey(action) => {
                // Runs on its own: the screenshot UI waits for the user.
                let (conn, ui, center, recorder) = (self.conn.clone(), self.ui.clone(), self.center.clone(), self.recorder.clone());
                tokio::spawn(async move {
                    let osd = match action {
                        shell_keys::KeyAction::Record => {
                            recorder.key(&conn, &screenshot::Screens::with_notifications(ui.clone(), center)).await;
                            None
                        }
                        shell_keys::KeyAction::Screenshot(screenshot::ScreenshotKey::Ui) => {
                            recorder.screenshot_ui(&conn, &screenshot::Screens::with_notifications(ui.clone(), center), false).await;
                            None
                        }
                        shell_keys::KeyAction::Screenshot(key) => {
                            screenshot::run_key(&screenshot::Screens::with_notifications(ui.clone(), center), key).await;
                            None
                        }
                        shell_keys::KeyAction::Brightness(step) => shell_keys::change_brightness(&conn, step).await,
                        shell_keys::KeyAction::Volume(change) => {
                            let osd = shell_keys::change_volume(change).await;
                            status::changed();
                            osd
                        }
                        shell_keys::KeyAction::Playback(command) => {
                            shell_keys::control_player(&conn, command).await;
                            None
                        }
                        shell_keys::KeyAction::A11y(key) => {
                            tracing::info!(?key, "accessibility key");
                            shell_keys::run_a11y_key(key);
                            None
                        }
                        shell_keys::KeyAction::Launch(key) => {
                            tracing::info!(?key, "launch key");
                            if !extra_keys::launch(key) {
                                let _ = ui.send(UiEvent::Launcher);
                            }
                            None
                        }
                        shell_keys::KeyAction::Session(extra_keys::SessionKey::Lock) => {
                            if let Err(err) = lock_via(&conn).await {
                                tracing::warn!(%err, "cannot lock");
                            }
                            None
                        }
                        shell_keys::KeyAction::Session(key) => {
                            tracing::info!(?key, "session key");
                            extra_keys::session(&conn, key).await;
                            None
                        }
                        shell_keys::KeyAction::Text(key) => {
                            extra_keys::text(key);
                            None
                        }
                        shell_keys::KeyAction::Touchpad(key) => extra_keys::touchpad(key),
                        shell_keys::KeyAction::KbdBacklight(key) => match zbus::Connection::system().await {
                            Ok(system) => extra_keys::keyboard_backlight(&system, key).await,
                            Err(_) => None,
                        },
                        shell_keys::KeyAction::Battery => match zbus::Connection::system().await {
                            Ok(system) => extra_keys::battery(&system).await,
                            Err(_) => None,
                        },
                        shell_keys::KeyAction::PowerButton => {
                            power::act(&conn, power::read().button).await;
                            None
                        }
                        shell_keys::KeyAction::RotationLock => {
                            extra_keys::toggle_orientation_lock();
                            let locked = extra_keys::orientation_locked();
                            Some(Osd {
                                icon: Some(if locked { "rotation-locked-symbolic" } else { "rotation-allowed-symbolic" }.into()),
                                label: Some(if locked { "Rotation Locked" } else { "Auto Rotate" }.into()),
                                level: None,
                                max_level: 1.0,
                                connector: None,
                            })
                        }
                        shell_keys::KeyAction::Custom(index) => {
                            extra_keys::run_custom(index);
                            None
                        }
                        shell_keys::KeyAction::Eject => {
                            media::eject_drive();
                            Some(Osd { icon: Some("media-eject-symbolic".into()), label: None, level: None, max_level: 1.0, connector: None })
                        }
                        shell_keys::KeyAction::Rfkill(bluetooth_only) => match zbus::Connection::system().await {
                            Ok(system) => {
                                let osd = extra_keys::rfkill(&system, bluetooth_only).await;
                                status::changed();
                                osd
                            }
                            Err(_) => None,
                        },
                    };
                    if let Some(osd) = osd {
                        let _ = ui.send(UiEvent::Osd(osd));
                    }
                });
                Ok(())
            }
            UiRequest::Ime(request) => {
                self.ibus.lock().await.handle(request).await;
                Ok(())
            }
            UiRequest::NotificationDismissed { id } => {
                self.center
                    .close(&self.conn, id, notify_store::CloseReason::Dismissed)
                    .await
            }
            UiRequest::NotificationListOpened => {
                self.center.mark_seen();
                Ok(())
            }
            UiRequest::NotificationsCleared => self.center.clear(&self.conn).await,
            UiRequest::SetDoNotDisturb(on) => {
                notifications::set_do_not_disturb(on);
                Ok(())
            }
            UiRequest::RunningApps(apps) => {
                portal_dialog::set_running(&self.conn, apps).await;
                Ok(())
            }
            UiRequest::LockMedia(command) => {
                let (conn, ui) = (self.conn.clone(), self.ui.clone());
                tokio::spawn(async move {
                    if let Some(command) = command {
                        shell_keys::control_player(&conn, command).await;
                        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    }
                    let _ = ui.send(UiEvent::LockMedia(calendar::player(&conn).await));
                });
                Ok(())
            }
            UiRequest::BreakAnswer(answer) => {
                wellbeing::answer(answer);
                Ok(())
            }
            UiRequest::Calendar { from, to } => {
                let (conn, ui) = (self.conn.clone(), self.ui.clone());
                tokio::spawn(async move {
                    let data = calendar::read(&conn, from, to).await;
                    let _ = ui.send(UiEvent::Calendar(Box::new(data)));
                });
                Ok(())
            }
            UiRequest::Media { command, from, to } => {
                let (conn, ui) = (self.conn.clone(), self.ui.clone());
                tokio::spawn(async move {
                    shell_keys::control_player(&conn, command).await;
                    // Players update their state a moment later.
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    let data = calendar::read(&conn, from, to).await;
                    let _ = ui.send(UiEvent::Calendar(Box::new(data)));
                });
                Ok(())
            }
            UiRequest::SetClickType(name) => {
                let conn = self.conn.clone();
                tokio::spawn(async move { click_type::set(&conn, &name).await });
                Ok(())
            }
            UiRequest::QuickItem(list, id) => {
                let conn = self.conn.clone();
                tokio::spawn(async move {
                    if let Err(err) = launcher_entry::activate_quick_item(&conn, &list, id).await {
                        tracing::info!(%err, "the app did not take the quicklist choice");
                    }
                });
                Ok(())
            }
            UiRequest::OpenUri(uri) => {
                notifications::open_uri(&uri);
                Ok(())
            }
            UiRequest::EmptyTrash => {
                let ui = self.ui.clone();
                tokio::spawn(async move {
                    let prompt = prompt::Prompt {
                        title: "Empty Trash?".into(),
                        message: "All items in the Trash will be permanently deleted.".into(),
                        continue_label: "Empty Trash".into(),
                        cancel_label: "Cancel".into(),
                        ..prompt::Prompt::default()
                    };
                    let _turn = prompt::turn().await;
                    let answer = prompt::ask(&ui, prompt).await;
                    prompt::done(&ui);
                    if matches!(answer, prompt::Answer::Continue { .. }) {
                        media::trash_request(true);
                    }
                });
                Ok(())
            }
        }
    }
}

/// Ask our own ScreenSaver object to lock.
/// Lock, then wait (up to `timeout`) until the lock screen is shown.
/// Returns whether the session is locked.
pub async fn lock_and_wait(conn: &Connection, timeout: std::time::Duration) -> bool {
    if let Err(err) = lock_via(conn).await {
        tracing::warn!(%err, "cannot lock");
        return false;
    }
    let Ok(saver) = conn
        .object_server()
        .interface::<_, screensaver::ScreenSaver>(screensaver::OBJECT_PATH)
        .await
    else {
        return false;
    };
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if screensaver::status(&saver.get().await.status).locked() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!("the lock screen did not appear in time");
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Whether the administrator turned `key` of `org.gnome.desktop.lockdown`
/// on (helix-desktop-spec `contracts/gsettings/org.gnome.desktop.lockdown.md`).
pub fn locked_down(key: &str) -> bool {
    const LOCKDOWN: &str = "org.gnome.desktop.lockdown";
    use gio::prelude::SettingsExt;
    helix_settings::has_schema(LOCKDOWN) && gio::Settings::new(LOCKDOWN).boolean(key)
}

pub async fn lock_via(conn: &Connection) -> zbus::Result<()> {
    let saver = conn
        .object_server()
        .interface::<_, screensaver::ScreenSaver>(screensaver::OBJECT_PATH)
        .await?;
    let object = saver.get().await;
    let result = object
        .request_lock()
        .map_err(|e| zbus::Error::Failure(format!("cannot lock: {e}")));
    result
}
