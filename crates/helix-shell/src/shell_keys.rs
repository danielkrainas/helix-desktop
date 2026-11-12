//! Screenshot, brightness and volume keys (helix-desktop-spec
//! `contracts/gsettings/shell-keys.md`): which accelerators to bind, and
//! what the brightness and volume keys do.
//!
//! Brightness goes through DisplayConfig's `Backlight` / `SetBacklight`;
//! volume through the sound server's PulseAudio interface (`pactl`, which
//! PipeWire also serves).

use std::collections::HashMap;

use gio::prelude::*;
use zbus::zvariant::OwnedValue;
use zbus::Connection;

use crate::extra_keys::{KbdKey, LaunchKey, SessionKey, TextKey, Toggle};
use crate::screenshot::ScreenshotKey;
use crate::Osd;

const SHELL_KEYS: &str = "org.gnome.shell.keybindings";
const MEDIA_KEYS: &str = "org.gnome.settings-daemon.plugins.media-keys";
const SOUND: &str = "org.gnome.desktop.sound";

/// Schemas read here (key tables in helix-desktop-spec).
pub const SCHEMAS: &[&str] = &[SHELL_KEYS, MEDIA_KEYS, SOUND];
/// Schemas whose changes rebind the shell's keys.
pub const KEY_SCHEMAS: &[&str] = &[SHELL_KEYS, MEDIA_KEYS, "org.gnome.desktop.wm.keybindings", "org.gnome.mutter.keybindings", "org.gnome.desktop.input-sources"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Brightness {
    Up,
    Down,
    Cycle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Volume {
    Up,
    Down,
    UpPrecise,
    DownPrecise,
    Mute,
    MicMute,
}

/// Accessibility keys (helix-desktop-spec
/// `contracts/gsettings/a11y-magnifier.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum A11yKey {
    Magnifier,
    ZoomIn,
    ZoomOut,
    ScreenReader,
    OnScreenKeyboard,
}

const A11Y_APPS: &str = "org.gnome.desktop.a11y.applications";
const MAGNIFIER: &str = "org.gnome.desktop.a11y.magnifier";

/// Act on an accessibility key: flip or step the settings.
pub fn run_a11y_key(key: A11yKey) {
    let flip = |name: &str| {
        if let Some(s) = settings_with(A11Y_APPS, name) {
            let _ = s.set_boolean(name, !s.boolean(name));
        }
    };
    match key {
        A11yKey::Magnifier => flip("screen-magnifier-enabled"),
        A11yKey::ScreenReader => flip("screen-reader-enabled"),
        A11yKey::OnScreenKeyboard => flip("screen-keyboard-enabled"),
        A11yKey::ZoomIn | A11yKey::ZoomOut => {
            let (Some(apps), Some(mag)) = (settings_with(A11Y_APPS, "screen-magnifier-enabled"), settings_with(MAGNIFIER, "mag-factor")) else { return };
            let (factor, enabled) = zoom_step(apps.boolean("screen-magnifier-enabled"), mag.double("mag-factor"), key == A11yKey::ZoomIn);
            let _ = mag.set_double("mag-factor", factor);
            let _ = apps.set_boolean("screen-magnifier-enabled", enabled);
        }
    }
    gio::Settings::sync();
}

/// The new `mag-factor` and `screen-magnifier-enabled` for a zoom key:
/// steps of 0.5, off at 1.
pub fn zoom_step(enabled: bool, factor: f64, zoom_in: bool) -> (f64, bool) {
    if zoom_in {
        if enabled {
            ((factor + 0.5).min(32.0), true)
        } else {
            (factor.max(1.5), true)
        }
    } else {
        let next = factor - 0.5;
        if next <= 1.0 {
            (1.0, false)
        } else {
            (next, enabled)
        }
    }
}

/// Playback keys: the MPRIS method each calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Playback {
    PlayPause,
    Pause,
    Stop,
    Next,
    Previous,
    Rewind,
    Forward,
    Repeat,
    Shuffle,
}

/// How far `playback-rewind` / `-forward` seek, in microseconds.
const SEEK_US: i64 = 10_000_000;

impl Playback {
    pub fn method(self) -> &'static str {
        match self {
            Playback::PlayPause => "PlayPause",
            Playback::Pause => "Pause",
            Playback::Stop => "Stop",
            Playback::Next => "Next",
            Playback::Previous => "Previous",
            Playback::Rewind | Playback::Forward => "Seek",
            Playback::Repeat => "LoopStatus",
            Playback::Shuffle => "Shuffle",
        }
    }
}

/// The `LoopStatus` after `current`.
pub fn next_loop_status(current: &str) -> &'static str {
    match current {
        "None" => "Playlist",
        "Playlist" => "Track",
        _ => "None",
    }
}

/// What a shell key does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Screenshot(ScreenshotKey),
    /// `show-screen-recording-ui`: start or stop recording.
    Record,
    Brightness(Brightness),
    Volume(Volume),
    Playback(Playback),
    A11y(A11yKey),
    Launch(LaunchKey),
    Session(SessionKey),
    Text(TextKey),
    Touchpad(Toggle),
    KbdBacklight(KbdKey),
    Battery,
    /// A custom shortcut, by its place in `custom-keybindings`.
    Custom(usize),
    /// `rotate-video-lock`: flip the orientation lock.
    RotationLock,
    /// The power button (`power-button-action`).
    PowerButton,
}

impl KeyAction {
    /// Which keys work while locked (helix-desktop-spec `shell-keys.md`).
    pub fn works_when_locked(self) -> bool {
        match self {
            KeyAction::Brightness(_) | KeyAction::Volume(_) | KeyAction::Playback(_) | KeyAction::A11y(_) | KeyAction::KbdBacklight(_) | KeyAction::Battery => true,
            KeyAction::Session(key) => matches!(key, SessionKey::Suspend | SessionKey::Hibernate),
            KeyAction::PowerButton => true,
            _ => false,
        }
    }
}

/// (schema, key, defaults from the 51.0 schemas, action)
const KEYS: &[(&str, &str, &[&str], KeyAction)] = &[
    (SHELL_KEYS, "show-screenshot-ui", &["Print"], KeyAction::Screenshot(ScreenshotKey::Ui)),
    (SHELL_KEYS, "screenshot", &["<Shift>Print"], KeyAction::Screenshot(ScreenshotKey::Screen)),
    (SHELL_KEYS, "screenshot-window", &["<Alt>Print"], KeyAction::Screenshot(ScreenshotKey::Window)),
    (SHELL_KEYS, "show-screen-recording-ui", &["<Ctrl><Shift><Alt>R"], KeyAction::Record),
    (SHELL_KEYS, "screen-brightness-up", &["XF86MonBrightnessUp"], KeyAction::Brightness(Brightness::Up)),
    (SHELL_KEYS, "screen-brightness-up-monitor", &["<Shift>XF86MonBrightnessUp"], KeyAction::Brightness(Brightness::Up)),
    (SHELL_KEYS, "screen-brightness-down", &["XF86MonBrightnessDown"], KeyAction::Brightness(Brightness::Down)),
    (SHELL_KEYS, "screen-brightness-down-monitor", &["<Shift>XF86MonBrightnessDown"], KeyAction::Brightness(Brightness::Down)),
    (SHELL_KEYS, "screen-brightness-cycle", &["XF86MonBrightnessCycle"], KeyAction::Brightness(Brightness::Cycle)),
    (SHELL_KEYS, "screen-brightness-cycle-monitor", &["<Shift>XF86MonBrightnessCycle"], KeyAction::Brightness(Brightness::Cycle)),
    (MEDIA_KEYS, "volume-up", &[], KeyAction::Volume(Volume::Up)),
    (MEDIA_KEYS, "volume-up-static", &["XF86AudioRaiseVolume", "<Ctrl>XF86AudioRaiseVolume"], KeyAction::Volume(Volume::Up)),
    (MEDIA_KEYS, "volume-up-quiet", &[], KeyAction::Volume(Volume::Up)),
    (MEDIA_KEYS, "volume-up-quiet-static", &["<Alt>XF86AudioRaiseVolume", "<Alt><Ctrl>XF86AudioRaiseVolume"], KeyAction::Volume(Volume::Up)),
    (MEDIA_KEYS, "volume-up-precise", &[], KeyAction::Volume(Volume::UpPrecise)),
    (MEDIA_KEYS, "volume-up-precise-static", &["<Shift>XF86AudioRaiseVolume", "<Ctrl><Shift>XF86AudioRaiseVolume"], KeyAction::Volume(Volume::UpPrecise)),
    (MEDIA_KEYS, "volume-down", &[], KeyAction::Volume(Volume::Down)),
    (MEDIA_KEYS, "volume-down-static", &["XF86AudioLowerVolume", "<Ctrl>XF86AudioLowerVolume"], KeyAction::Volume(Volume::Down)),
    (MEDIA_KEYS, "volume-down-quiet", &[], KeyAction::Volume(Volume::Down)),
    (MEDIA_KEYS, "volume-down-quiet-static", &["<Alt>XF86AudioLowerVolume", "<Alt><Ctrl>XF86AudioLowerVolume"], KeyAction::Volume(Volume::Down)),
    (MEDIA_KEYS, "volume-down-precise", &[], KeyAction::Volume(Volume::DownPrecise)),
    (MEDIA_KEYS, "volume-down-precise-static", &["<Shift>XF86AudioLowerVolume", "<Ctrl><Shift>XF86AudioLowerVolume"], KeyAction::Volume(Volume::DownPrecise)),
    (MEDIA_KEYS, "volume-mute", &[], KeyAction::Volume(Volume::Mute)),
    (MEDIA_KEYS, "volume-mute-static", &["XF86AudioMute"], KeyAction::Volume(Volume::Mute)),
    (MEDIA_KEYS, "volume-mute-quiet", &[], KeyAction::Volume(Volume::Mute)),
    (MEDIA_KEYS, "volume-mute-quiet-static", &["<Alt>XF86AudioMute"], KeyAction::Volume(Volume::Mute)),
    (MEDIA_KEYS, "mic-mute", &[], KeyAction::Volume(Volume::MicMute)),
    (MEDIA_KEYS, "mic-mute-static", &["XF86AudioMicMute"], KeyAction::Volume(Volume::MicMute)),
    (MEDIA_KEYS, "play", &[], KeyAction::Playback(Playback::PlayPause)),
    (MEDIA_KEYS, "play-static", &["XF86AudioPlay", "<Ctrl>XF86AudioPlay"], KeyAction::Playback(Playback::PlayPause)),
    (MEDIA_KEYS, "pause", &[], KeyAction::Playback(Playback::Pause)),
    (MEDIA_KEYS, "pause-static", &["XF86AudioPause"], KeyAction::Playback(Playback::Pause)),
    (MEDIA_KEYS, "stop", &[], KeyAction::Playback(Playback::Stop)),
    (MEDIA_KEYS, "stop-static", &["XF86AudioStop"], KeyAction::Playback(Playback::Stop)),
    (MEDIA_KEYS, "next", &[], KeyAction::Playback(Playback::Next)),
    (MEDIA_KEYS, "next-static", &["XF86AudioNext", "<Ctrl>XF86AudioNext"], KeyAction::Playback(Playback::Next)),
    (MEDIA_KEYS, "previous", &[], KeyAction::Playback(Playback::Previous)),
    (MEDIA_KEYS, "previous-static", &["XF86AudioPrev", "<Ctrl>XF86AudioPrev"], KeyAction::Playback(Playback::Previous)),
    (MEDIA_KEYS, "playback-rewind", &[], KeyAction::Playback(Playback::Rewind)),
    (MEDIA_KEYS, "playback-rewind-static", &["XF86AudioRewind"], KeyAction::Playback(Playback::Rewind)),
    (MEDIA_KEYS, "playback-forward", &[], KeyAction::Playback(Playback::Forward)),
    (MEDIA_KEYS, "playback-forward-static", &["XF86AudioForward"], KeyAction::Playback(Playback::Forward)),
    (MEDIA_KEYS, "playback-repeat", &[], KeyAction::Playback(Playback::Repeat)),
    (MEDIA_KEYS, "playback-repeat-static", &["XF86AudioRepeat"], KeyAction::Playback(Playback::Repeat)),
    (MEDIA_KEYS, "playback-random", &[], KeyAction::Playback(Playback::Shuffle)),
    (MEDIA_KEYS, "playback-random-static", &["XF86AudioRandomPlay"], KeyAction::Playback(Playback::Shuffle)),
    (MEDIA_KEYS, "www", &[], KeyAction::Launch(LaunchKey::Www)),
    (MEDIA_KEYS, "www-static", &["XF86WWW"], KeyAction::Launch(LaunchKey::Www)),
    (MEDIA_KEYS, "email", &[], KeyAction::Launch(LaunchKey::Email)),
    (MEDIA_KEYS, "email-static", &["XF86Mail"], KeyAction::Launch(LaunchKey::Email)),
    (MEDIA_KEYS, "calculator", &[], KeyAction::Launch(LaunchKey::Calculator)),
    (MEDIA_KEYS, "calculator-static", &["XF86Calculator"], KeyAction::Launch(LaunchKey::Calculator)),
    (MEDIA_KEYS, "control-center", &[], KeyAction::Launch(LaunchKey::Settings)),
    (MEDIA_KEYS, "control-center-static", &["XF86Tools"], KeyAction::Launch(LaunchKey::Settings)),
    (MEDIA_KEYS, "home", &[], KeyAction::Launch(LaunchKey::Home)),
    (MEDIA_KEYS, "home-static", &["XF86Explorer"], KeyAction::Launch(LaunchKey::Home)),
    (MEDIA_KEYS, "media", &[], KeyAction::Launch(LaunchKey::Media)),
    (MEDIA_KEYS, "media-static", &["XF86AudioMedia"], KeyAction::Launch(LaunchKey::Media)),
    (MEDIA_KEYS, "help", &["", "<Super>F1"], KeyAction::Launch(LaunchKey::Help)),
    (MEDIA_KEYS, "search", &[], KeyAction::Launch(LaunchKey::Search)),
    (MEDIA_KEYS, "search-static", &["XF86Search"], KeyAction::Launch(LaunchKey::Search)),
    (MEDIA_KEYS, "screensaver", &["<Super>l"], KeyAction::Session(SessionKey::Lock)),
    (MEDIA_KEYS, "screensaver-static", &["XF86ScreenSaver"], KeyAction::Session(SessionKey::Lock)),
    (MEDIA_KEYS, "logout", &["<Control><Alt>Delete"], KeyAction::Session(SessionKey::Logout)),
    (MEDIA_KEYS, "shutdown", &[], KeyAction::Session(SessionKey::Shutdown)),
    (MEDIA_KEYS, "power", &[], KeyAction::PowerButton),
    (MEDIA_KEYS, "power-static", &["XF86PowerOff"], KeyAction::PowerButton),
    (MEDIA_KEYS, "reboot", &[], KeyAction::Session(SessionKey::Reboot)),
    (MEDIA_KEYS, "suspend", &[], KeyAction::Session(SessionKey::Suspend)),
    (MEDIA_KEYS, "suspend-static", &["XF86Sleep"], KeyAction::Session(SessionKey::Suspend)),
    (MEDIA_KEYS, "hibernate", &[], KeyAction::Session(SessionKey::Hibernate)),
    (MEDIA_KEYS, "hibernate-static", &["XF86Suspend", "XF86Hibernate"], KeyAction::Session(SessionKey::Hibernate)),
    (MEDIA_KEYS, "increase-text-size", &[], KeyAction::Text(TextKey::Bigger)),
    (MEDIA_KEYS, "decrease-text-size", &[], KeyAction::Text(TextKey::Smaller)),
    (MEDIA_KEYS, "toggle-contrast", &[], KeyAction::Text(TextKey::Contrast)),
    (MEDIA_KEYS, "touchpad-on", &[], KeyAction::Touchpad(Toggle::On)),
    (MEDIA_KEYS, "touchpad-on-static", &["XF86TouchpadOn"], KeyAction::Touchpad(Toggle::On)),
    (MEDIA_KEYS, "touchpad-off", &[], KeyAction::Touchpad(Toggle::Off)),
    (MEDIA_KEYS, "touchpad-off-static", &["XF86TouchpadOff"], KeyAction::Touchpad(Toggle::Off)),
    (MEDIA_KEYS, "touchpad-toggle", &[], KeyAction::Touchpad(Toggle::Flip)),
    (MEDIA_KEYS, "touchpad-toggle-static", &["XF86TouchpadToggle", "<Ctrl><Super>XF86TouchpadToggle"], KeyAction::Touchpad(Toggle::Flip)),
    (MEDIA_KEYS, "keyboard-brightness-up", &[], KeyAction::KbdBacklight(KbdKey::Up)),
    (MEDIA_KEYS, "keyboard-brightness-up-static", &["XF86KbdBrightnessUp"], KeyAction::KbdBacklight(KbdKey::Up)),
    (MEDIA_KEYS, "keyboard-brightness-down", &[], KeyAction::KbdBacklight(KbdKey::Down)),
    (MEDIA_KEYS, "keyboard-brightness-down-static", &["XF86KbdBrightnessDown"], KeyAction::KbdBacklight(KbdKey::Down)),
    (MEDIA_KEYS, "keyboard-brightness-toggle", &[], KeyAction::KbdBacklight(KbdKey::Toggle)),
    (MEDIA_KEYS, "keyboard-brightness-toggle-static", &["XF86KbdLightOnOff"], KeyAction::KbdBacklight(KbdKey::Toggle)),
    (MEDIA_KEYS, "rotate-video-lock", &[], KeyAction::RotationLock),
    (MEDIA_KEYS, "rotate-video-lock-static", &["<Super>o", "XF86RotationLockToggle"], KeyAction::RotationLock),
    (MEDIA_KEYS, "battery-status", &[], KeyAction::Battery),
    (MEDIA_KEYS, "battery-status-static", &["XF86Battery"], KeyAction::Battery),
    (MEDIA_KEYS, "magnifier", &["<Alt><Super>8"], KeyAction::A11y(A11yKey::Magnifier)),
    (MEDIA_KEYS, "magnifier-zoom-in", &["<Alt><Super>equal"], KeyAction::A11y(A11yKey::ZoomIn)),
    (MEDIA_KEYS, "magnifier-zoom-out", &["<Alt><Super>minus"], KeyAction::A11y(A11yKey::ZoomOut)),
    (MEDIA_KEYS, "screenreader", &["<Alt><Super>s"], KeyAction::A11y(A11yKey::ScreenReader)),
    (MEDIA_KEYS, "on-screen-keyboard", &[], KeyAction::A11y(A11yKey::OnScreenKeyboard)),
];

/// The installed schema, if it has `key`.
fn settings_with(schema: &str, key: &str) -> Option<gio::Settings> {
    let source = gio::SettingsSchemaSource::default()?;
    let found = source.lookup(schema, true)?;
    found.has_key(key).then(|| gio::Settings::new(schema))
}

fn strv(schema: &str, key: &str, defaults: &[&str]) -> Vec<String> {
    match settings_with(schema, key) {
        Some(settings) => settings.strv(key).iter().map(|s| s.to_string()).collect(),
        None => defaults.iter().map(|s| (*s).to_owned()).collect(),
    }
}

/// An `org.gnome.shell.keybindings` key's accelerators (empty ones left out).
pub fn shell_strv(key: &str, defaults: &[&str]) -> Vec<String> {
    strv(SHELL_KEYS, key, defaults).into_iter().filter(|a| !a.is_empty()).collect()
}

/// The accelerators to bind and what they do; the first binding of an
/// accelerator wins.
pub fn accelerators() -> Vec<(String, KeyAction)> {
    let mut out: Vec<(String, KeyAction)> = Vec::new();
    for (schema, key, defaults, action) in KEYS {
        for accel in strv(schema, key, defaults) {
            if !accel.is_empty() && !out.iter().any(|(a, _)| *a == accel) {
                out.push((accel, *action));
            }
        }
    }
    for (index, (binding, command)) in crate::extra_keys::customs().into_iter().enumerate() {
        if !binding.is_empty() && !command.is_empty() && !out.iter().any(|(a, _)| *a == binding) {
            out.push((binding, KeyAction::Custom(index)));
        }
    }
    out
}

// ----- Brightness -------------------------------------------------------

/// The next value for a backlight in `min..=max`.
pub fn brightness_step(min: i32, max: i32, value: i32, step: Brightness) -> i32 {
    let delta = ((max - min) / 20).max(1);
    let value = value.clamp(min, max);
    match step {
        Brightness::Up => (value + delta).min(max),
        Brightness::Down => (value - delta).max(min),
        Brightness::Cycle if value >= max => min,
        Brightness::Cycle => (value + delta).min(max),
    }
}

type Props = HashMap<String, OwnedValue>;

fn int(entry: &Props, key: &str) -> Option<i32> {
    entry.get(key).and_then(|v| i32::try_from(v.clone()).ok())
}

/// Step every controllable backlight; the OSD for the result.
pub async fn change_brightness(conn: &Connection, step: Brightness) -> Option<Osd> {
    let proxy = zbus::Proxy::new(
        conn,
        "org.gnome.Mutter.DisplayConfig",
        "/org/gnome/Mutter/DisplayConfig",
        "org.gnome.Mutter.DisplayConfig",
    )
    .await
    .ok()?;
    let (serial, entries): (u32, Vec<Props>) = match proxy.get_property("Backlight").await {
        Ok(backlight) => backlight,
        Err(err) => {
            tracing::info!(%err, "no backlight to change");
            return None;
        }
    };
    let mut level = None;
    for entry in &entries {
        let (Some(min), Some(max), Some(value)) = (int(entry, "min"), int(entry, "max"), int(entry, "value")) else { continue };
        let Some(connector) = entry.get("connector").and_then(|v| String::try_from(v.clone()).ok()) else { continue };
        let new = brightness_step(min, max, value, step);
        if let Err(err) = proxy.call_method("SetBacklight", &(serial, connector.as_str(), new)).await {
            tracing::warn!(%err, connector, "cannot set the backlight");
            continue;
        }
        if max > min {
            level = Some(f64::from(new - min) / f64::from(max - min));
        }
    }
    level.map(|level| Osd {
        icon: Some("display-brightness-symbolic".into()),
        label: None,
        level: Some(level),
        max_level: 1.0,
        connector: None,
    })
}

// ----- Playback (MPRIS) -------------------------------------------------

const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
pub const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
pub const MPRIS_PLAYER: &str = "org.mpris.MediaPlayer2.Player";

/// The player to control among (bus name, status), in bus-name order:
/// the first playing, else the first paused, else the first.
pub fn choose_player(players: &[(String, String)]) -> Option<&str> {
    let mut sorted: Vec<&(String, String)> = players.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    sorted
        .iter()
        .find(|(_, s)| s == "Playing")
        .or_else(|| sorted.iter().find(|(_, s)| s == "Paused"))
        .or_else(|| sorted.first())
        .map(|(name, _)| name.as_str())
}

/// The MPRIS player the playback keys control, by bus name.
pub async fn chosen_player(conn: &Connection) -> Option<String> {
    let names = match zbus::fdo::DBusProxy::new(conn).await {
        Ok(dbus) => dbus.list_names().await.unwrap_or_default(),
        Err(_) => return None,
    };
    let mut players = Vec::new();
    for name in names.iter().filter(|n| n.starts_with(MPRIS_PREFIX)) {
        let Ok(proxy) = zbus::Proxy::new(conn, name.to_string(), MPRIS_PATH, MPRIS_PLAYER).await else { continue };
        if proxy.get_property::<bool>("CanControl").await == Ok(false) {
            continue;
        }
        let status = proxy.get_property::<String>("PlaybackStatus").await.unwrap_or_default();
        players.push((name.to_string(), status));
    }
    choose_player(&players).map(str::to_owned)
}

/// Send a playback key to the chosen MPRIS player.
pub async fn control_player(conn: &Connection, command: Playback) {
    let Some(player) = chosen_player(conn).await else {
        tracing::info!("no media player to control");
        return;
    };
    let player = player.as_str();
    tracing::debug!(player, method = command.method(), "media key");
    let result = match command {
        Playback::Rewind | Playback::Forward => {
            let offset = if command == Playback::Rewind { -SEEK_US } else { SEEK_US };
            conn.call_method(Some(player), MPRIS_PATH, Some(MPRIS_PLAYER), "Seek", &(offset,)).await.map(drop)
        }
        Playback::Repeat | Playback::Shuffle => match zbus::Proxy::new(conn, player.to_owned(), MPRIS_PATH, MPRIS_PLAYER).await {
            Ok(proxy) => {
                let set = if command == Playback::Repeat {
                    let current: String = proxy.get_property("LoopStatus").await.unwrap_or_else(|_| "None".into());
                    proxy.set_property("LoopStatus", next_loop_status(&current)).await
                } else {
                    let current: bool = proxy.get_property("Shuffle").await.unwrap_or(false);
                    proxy.set_property("Shuffle", !current).await
                };
                set.map_err(zbus::Error::from)
            }
            Err(err) => Err(err),
        },
        _ => conn.call_method(Some(player), MPRIS_PATH, Some(MPRIS_PLAYER), command.method(), &()).await.map(drop),
    };
    if let Err(err) = result {
        tracing::warn!(%err, player, "media player refused the key");
    }
}

// ----- Volume -----------------------------------------------------------

/// Volume settings: step in percent, highest volume in percent.
pub fn volume_limits() -> (u32, u32) {
    let step = settings_with(MEDIA_KEYS, "volume-step").map(|s| s.int("volume-step")).unwrap_or(6).clamp(1, 20) as u32;
    let loud = settings_with(SOUND, "allow-volume-above-100-percent").is_some_and(|s| s.boolean("allow-volume-above-100-percent"));
    (step, if loud { 150 } else { 100 })
}

/// The output's new (percent, muted) after `action`.
pub fn volume_change(percent: u32, muted: bool, action: Volume, step: u32, max: u32) -> (u32, bool) {
    let percent = percent.min(max);
    match action {
        Volume::Up => ((percent + step).min(max), false),
        Volume::UpPrecise => ((percent + 1).min(max), false),
        Volume::Down => (percent.saturating_sub(step), muted),
        Volume::DownPrecise => (percent.saturating_sub(1), muted),
        Volume::Mute => (percent, !muted),
        Volume::MicMute => (percent, muted),
    }
}

pub fn volume_icon(percent: u32, muted: bool) -> &'static str {
    match percent {
        _ if muted => "audio-volume-muted-symbolic",
        0 => "audio-volume-muted-symbolic",
        1..=33 => "audio-volume-low-symbolic",
        34..=66 => "audio-volume-medium-symbolic",
        _ => "audio-volume-high-symbolic",
    }
}

/// The first channel's percentage in `pactl get-sink-volume` output.
pub fn parse_volume(output: &str) -> Option<u32> {
    output.split('/').find_map(|part| part.trim().strip_suffix('%')?.trim().parse().ok())
}

/// `pactl get-*-mute` output.
pub fn parse_mute(output: &str) -> Option<bool> {
    match output.trim().strip_prefix("Mute:")?.trim() {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}

async fn pactl(args: &[&str]) -> Option<String> {
    let output = tokio::process::Command::new("pactl").env("LC_ALL", "C").args(args).output().await;
    match output {
        Ok(out) if out.status.success() => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(out) => {
            tracing::warn!(?args, stderr = %String::from_utf8_lossy(&out.stderr).trim(), "pactl failed");
            None
        }
        Err(err) => {
            tracing::warn!(%err, "cannot run pactl; volume keys unavailable");
            None
        }
    }
}

const SINK: &str = "@DEFAULT_SINK@";
const SOURCE: &str = "@DEFAULT_SOURCE@";

/// The default output's (percent, muted), if there is a sound server.
pub async fn volume_state() -> Option<(u32, bool)> {
    let percent = parse_volume(&pactl(&["get-sink-volume", SINK]).await?)?;
    let muted = parse_mute(&pactl(&["get-sink-mute", SINK]).await?)?;
    Some((percent, muted))
}

/// Set the default output's volume (and unmute it).
pub async fn set_volume(percent: u32) {
    let (_, max) = volume_limits();
    let _ = pactl(&["set-sink-volume", SINK, &format!("{}%", percent.min(max))]).await;
    let _ = pactl(&["set-sink-mute", SINK, "0"]).await;
}

pub async fn toggle_mute() {
    let _ = pactl(&["set-sink-mute", SINK, "toggle"]).await;
}

async fn backlight(conn: &Connection) -> Option<(zbus::Proxy<'static>, u32, Vec<Props>)> {
    let proxy = zbus::Proxy::new(conn, "org.gnome.Mutter.DisplayConfig", "/org/gnome/Mutter/DisplayConfig", "org.gnome.Mutter.DisplayConfig").await.ok()?;
    let (serial, entries): (u32, Vec<Props>) = proxy.get_property("Backlight").await.ok()?;
    Some((proxy, serial, entries))
}

/// The first controllable backlight's level in percent.
pub async fn brightness_percent(conn: &Connection) -> Option<u32> {
    let (_, _, entries) = backlight(conn).await?;
    entries.iter().find_map(|e| {
        let (min, max, value) = (int(e, "min")?, int(e, "max")?, int(e, "value")?);
        (max > min).then(|| ((value - min) * 100 / (max - min)) as u32)
    })
}

/// Set every controllable backlight to `percent` of its range.
pub async fn set_brightness_percent(conn: &Connection, percent: u32) {
    let Some((proxy, serial, entries)) = backlight(conn).await else { return };
    for e in &entries {
        let (Some(min), Some(max)) = (int(e, "min"), int(e, "max")) else { continue };
        let Some(connector) = e.get("connector").and_then(|v| String::try_from(v.clone()).ok()) else { continue };
        let value = min + (max - min) * percent.min(100) as i32 / 100;
        if let Err(err) = proxy.call_method("SetBacklight", &(serial, connector.as_str(), value)).await {
            tracing::warn!(%err, connector, "cannot set the backlight");
        }
    }
}

/// Apply a volume key; the OSD for the result.
pub async fn change_volume(action: Volume) -> Option<Osd> {
    if action == Volume::MicMute {
        pactl(&["set-source-mute", SOURCE, "toggle"]).await?;
        let muted = parse_mute(&pactl(&["get-source-mute", SOURCE]).await?)?;
        let icon = if muted { "microphone-sensitivity-muted-symbolic" } else { "microphone-sensitivity-high-symbolic" };
        return Some(Osd { icon: Some(icon.into()), label: None, level: None, max_level: 1.0, connector: None });
    }
    let (step, max) = volume_limits();
    let percent = parse_volume(&pactl(&["get-sink-volume", SINK]).await?)?;
    let muted = parse_mute(&pactl(&["get-sink-mute", SINK]).await?)?;
    let (new_percent, new_muted) = volume_change(percent, muted, action, step, max);
    if new_percent != percent {
        pactl(&["set-sink-volume", SINK, &format!("{new_percent}%")]).await?;
    }
    if new_muted != muted {
        pactl(&["set-sink-mute", SINK, if new_muted { "1" } else { "0" }]).await?;
    }
    Some(Osd {
        icon: Some(volume_icon(new_percent, new_muted).into()),
        label: None,
        level: Some(f64::from(new_percent) / 100.0),
        max_level: f64::from(max) / 100.0,
        connector: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_keys() {
        assert_eq!(zoom_step(false, 2.0, true), (2.0, true), "turns on at the stored factor");
        assert_eq!(zoom_step(false, 1.0, true), (1.5, true));
        assert_eq!(zoom_step(true, 2.0, true), (2.5, true));
        assert_eq!(zoom_step(true, 32.0, true), (32.0, true));
        assert_eq!(zoom_step(true, 2.0, false), (1.5, true));
        assert_eq!(zoom_step(true, 1.5, false), (1.0, false), "off at 1");
    }

    #[test]
    fn brightness_steps() {
        assert_eq!(brightness_step(1, 1000, 400, Brightness::Up), 449);
        assert_eq!(brightness_step(1, 1000, 990, Brightness::Up), 1000);
        assert_eq!(brightness_step(1, 1000, 30, Brightness::Down), 1);
        assert_eq!(brightness_step(1, 15, 7, Brightness::Up), 8, "at least 1");
        assert_eq!(brightness_step(1, 1000, 1000, Brightness::Cycle), 1, "wraps");
        assert_eq!(brightness_step(1, 1000, 500, Brightness::Cycle), 549);
    }

    #[test]
    fn volume_steps() {
        assert_eq!(volume_change(40, false, Volume::Up, 6, 100), (46, false));
        assert_eq!(volume_change(98, true, Volume::Up, 6, 100), (100, false), "raising unmutes");
        assert_eq!(volume_change(98, false, Volume::Up, 6, 150), (104, false));
        assert_eq!(volume_change(120, false, Volume::Down, 6, 100), (94, false), "clamped first");
        assert_eq!(volume_change(3, true, Volume::Down, 6, 100), (0, true));
        assert_eq!(volume_change(50, false, Volume::UpPrecise, 6, 100), (51, false));
        assert_eq!(volume_change(50, false, Volume::DownPrecise, 6, 100), (49, false));
        assert_eq!(volume_change(50, false, Volume::Mute, 6, 100), (50, true));
        assert_eq!(volume_change(50, true, Volume::Mute, 6, 100), (50, false));
    }

    #[test]
    fn icons() {
        assert_eq!(volume_icon(50, true), "audio-volume-muted-symbolic");
        assert_eq!(volume_icon(0, false), "audio-volume-muted-symbolic");
        assert_eq!(volume_icon(33, false), "audio-volume-low-symbolic");
        assert_eq!(volume_icon(34, false), "audio-volume-medium-symbolic");
        assert_eq!(volume_icon(67, false), "audio-volume-high-symbolic");
    }

    #[test]
    fn pactl_output() {
        let out = "Volume: front-left: 26214 /  40% / -23.88 dB,   front-right: 26214 /  40% / -23.88 dB\n        balance 0.00\n";
        assert_eq!(parse_volume(out), Some(40));
        assert_eq!(parse_volume("nothing"), None);
        assert_eq!(parse_mute("Mute: yes\n"), Some(true));
        assert_eq!(parse_mute("Mute: no"), Some(false));
        assert_eq!(parse_mute("Stummschaltung: ja"), None);
    }

    #[test]
    fn player_choice() {
        let p = |n: &str, s: &str| (n.to_owned(), s.to_owned());
        assert_eq!(choose_player(&[]), None);
        let players = [p("org.mpris.MediaPlayer2.z", "Paused"), p("org.mpris.MediaPlayer2.b", "Stopped"), p("org.mpris.MediaPlayer2.c", "Playing")];
        assert_eq!(choose_player(&players), Some("org.mpris.MediaPlayer2.c"));
        assert_eq!(choose_player(&players[..2]), Some("org.mpris.MediaPlayer2.z"), "paused beats stopped");
        assert_eq!(choose_player(&players[1..2]), Some("org.mpris.MediaPlayer2.b"));
    }

    #[test]
    fn defaults_cover_every_key_group() {
        let actions: Vec<KeyAction> = KEYS.iter().filter(|k| !k.2.is_empty()).map(|k| k.3).collect();
        for action in [
            KeyAction::Screenshot(ScreenshotKey::Ui),
            KeyAction::Brightness(Brightness::Cycle),
            KeyAction::Volume(Volume::MicMute),
            KeyAction::Volume(Volume::DownPrecise),
        ] {
            assert!(actions.contains(&action), "{action:?} has a default");
        }
        assert!(!KeyAction::Screenshot(ScreenshotKey::Screen).works_when_locked());
        assert!(KeyAction::Volume(Volume::Up).works_when_locked());
    }
}
