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

use crate::screenshot::ScreenshotKey;
use crate::Osd;

const SHELL_KEYS: &str = "org.gnome.shell.keybindings";
const MEDIA_KEYS: &str = "org.gnome.settings-daemon.plugins.media-keys";
const SOUND: &str = "org.gnome.desktop.sound";

/// Schemas read here (key tables in helix-desktop-spec).
pub const SCHEMAS: &[&str] = &[SHELL_KEYS, MEDIA_KEYS, SOUND];

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

/// What a shell key does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Screenshot(ScreenshotKey),
    Brightness(Brightness),
    Volume(Volume),
}

impl KeyAction {
    /// Volume and brightness keys work while locked.
    pub fn works_when_locked(self) -> bool {
        !matches!(self, KeyAction::Screenshot(_))
    }
}

/// (schema, key, defaults from the 51.0 schemas, action)
const KEYS: &[(&str, &str, &[&str], KeyAction)] = &[
    (SHELL_KEYS, "show-screenshot-ui", &["Print"], KeyAction::Screenshot(ScreenshotKey::Ui)),
    (SHELL_KEYS, "screenshot", &["<Shift>Print"], KeyAction::Screenshot(ScreenshotKey::Screen)),
    (SHELL_KEYS, "screenshot-window", &["<Alt>Print"], KeyAction::Screenshot(ScreenshotKey::Window)),
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
