//! Notification sounds (helix-desktop-spec
//! `contracts/dbus/org.freedesktop.Notifications.md`, "Sounds"): a file,
//! or a name from the freedesktop sound theme, played with PipeWire's
//! `pw-play` (or `paplay`).

use std::path::{Path, PathBuf};

const SCHEMA: &str = "org.gnome.desktop.sound";
const EXTENSIONS: [&str; 3] = ["oga", "ogg", "wav"];

/// Sound theme directories (`$XDG_DATA_HOME/sounds`, then
/// `$XDG_DATA_DIRS/sounds`).
fn sound_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".local/share")));
    if let Some(d) = data_home {
        dirs.push(d.join("sounds"));
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    dirs.extend(data_dirs.split(':').filter(|d| !d.is_empty()).map(|d| Path::new(d).join("sounds")));
    dirs
}

/// The file for sound `name` in `theme`, then `freedesktop`. Names may be
/// shortened at dashes (`message-new-instant` → `message-new` →
/// `message`), as the sound naming specification allows.
pub fn find_sound(name: &str, theme: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    let mut candidates = vec![name.to_owned()];
    let mut short = name;
    while let Some((head, _)) = short.rsplit_once('-') {
        candidates.push(head.to_owned());
        short = head;
    }
    for theme in [theme, "freedesktop"] {
        for candidate in &candidates {
            for dir in dirs {
                for sub in ["stereo", ""] {
                    for ext in EXTENSIONS {
                        let path = dir.join(theme).join(sub).join(format!("{candidate}.{ext}"));
                        if path.is_file() {
                            return Some(path);
                        }
                    }
                }
            }
        }
    }
    None
}

/// What a notification asks to play.
#[derive(Debug, Clone, PartialEq)]
pub enum Sound {
    File(PathBuf),
    Name(String),
}

/// Whether controls should make sounds when used: `event-sounds` and
/// `input-feedback-sounds` both on.
pub fn input_feedback() -> bool {
    use gio::prelude::*;
    crate::has_schema(SCHEMA) && {
        let s = gio::Settings::new(SCHEMA);
        s.boolean("event-sounds") && s.settings_schema().is_some_and(|x| x.has_key("input-feedback-sounds")) && s.boolean("input-feedback-sounds")
    }
}

/// Play `sound` in the background, unless event sounds are off.
pub fn play(sound: &Sound) {
    use gio::prelude::*;
    let (enabled, theme) = if crate::has_schema(SCHEMA) {
        let settings = gio::Settings::new(SCHEMA);
        (settings.boolean("event-sounds"), settings.string("theme-name").to_string())
    } else {
        (true, "freedesktop".to_owned())
    };
    if !enabled {
        return;
    }
    let path = match sound {
        Sound::File(path) => Some(path.clone()),
        Sound::Name(name) => find_sound(name, &theme, &sound_dirs()),
    };
    let Some(path) = path else {
        tracing::debug!(?sound, "no such sound");
        return;
    };
    for player in ["pw-play", "paplay"] {
        match std::process::Command::new(player)
            .arg(&path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                // Reap it when it finishes.
                std::thread::spawn(move || child.wait());
                return;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                tracing::warn!(%err, player, "cannot play a sound");
                return;
            }
        }
    }
    tracing::warn!("no sound player (pw-play or paplay) found");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup() {
        let dir = std::env::temp_dir().join(format!("helix-sounds-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("freedesktop/stereo")).unwrap();
        std::fs::create_dir_all(dir.join("custom")).unwrap();
        std::fs::write(dir.join("freedesktop/stereo/message.oga"), b"").unwrap();
        std::fs::write(dir.join("custom/bell.wav"), b"").unwrap();
        let dirs = vec![dir.clone()];
        assert_eq!(find_sound("message-new-instant", "custom", &dirs), Some(dir.join("freedesktop/stereo/message.oga")));
        assert_eq!(find_sound("bell", "custom", &dirs), Some(dir.join("custom/bell.wav")));
        assert_eq!(find_sound("nothing", "custom", &dirs), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
