//! What the greeter remembers between runs: the last user and session
//! (`$XDG_STATE_HOME/helix-greeter/state.json` of the greeter user).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub last_user: Option<String>,
    /// The session's id (its file name without `.desktop`).
    pub last_session: Option<String>,
}

/// The state file under `xdg_state_home`, else under `home`'s
/// `.local/state`.
pub fn path_from(xdg_state_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    let base = match xdg_state_home.filter(|d| d.starts_with('/')) {
        Some(dir) => PathBuf::from(dir),
        None => Path::new(home.filter(|h| !h.is_empty())?).join(".local/state"),
    };
    Some(base.join("helix-greeter/state.json"))
}

pub fn path() -> Option<PathBuf> {
    path_from(std::env::var("XDG_STATE_HOME").ok().as_deref(), std::env::var("HOME").ok().as_deref())
}

impl State {
    pub fn load_from(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|err| {
                tracing::warn!(%err, path = %path.display(), "ignoring unreadable state");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn load() -> Self {
        path().map(|p| Self::load_from(&p)).unwrap_or_default()
    }

    /// Write atomically (a new file renamed over the old).
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(&tmp, path)
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = path().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "neither XDG_STATE_HOME nor HOME is set"))?;
        self.save_to(&path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(path_from(Some("/s"), Some("/h")), Some(PathBuf::from("/s/helix-greeter/state.json")));
        assert_eq!(path_from(None, Some("/var/lib/greetd")), Some(PathBuf::from("/var/lib/greetd/.local/state/helix-greeter/state.json")));
        assert_eq!(path_from(Some("relative"), Some("/h")), Some(PathBuf::from("/h/.local/state/helix-greeter/state.json")), "relative paths are ignored");
        assert_eq!(path_from(None, None), None);
    }

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!("helix-greeter-state-{}", std::process::id()));
        let path = dir.join("helix-greeter/state.json");
        assert_eq!(State::load_from(&path), State::default(), "missing file");
        let state = State { last_user: Some("ann".into()), last_session: Some("helix".into()) };
        state.save_to(&path).unwrap();
        assert_eq!(State::load_from(&path), state);
        assert!(std::fs::read_to_string(&path).unwrap().contains("\"last_user\""));
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(State::load_from(&path), State::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
