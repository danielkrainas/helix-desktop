//! Media players for agents (helixdesktop/spec
//! `contracts/agent-access/README.md`, "Records", Media player, and
//! "Media, power and stars"; helixdesktop/desktop#156): `GetMedia` reads
//! the session bus's MPRIS players when the call comes, and
//! `MediaControl` asks one of them to play, pause, skip, stop or seek.
//!
//! An excluded app's player (Settings', or one in `excluded-apps`) is
//! never listed, counted, made the active one or controlled: by its
//! `DesktopEntry`, by the process owning its bus name (its other windows,
//! its program, its systemd unit, as for the *Control apps* methods), or
//! without a `DesktopEntry` by its `Identity` being the app's name. A
//! player's `xesam:url` and art are never given.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use zbus::zvariant::{ObjectPath, OwnedValue};
use zbus::Connection;

use super::desktop::{Installed, Snapshot};
use super::policy::{self, AgentError, Config, Target};
use super::value;

type Record = HashMap<String, OwnedValue>;

pub const PREFIX: &str = "org.mpris.MediaPlayer2.";
pub const PATH: &str = "/org/mpris/MediaPlayer2";
const ROOT: &str = "org.mpris.MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
/// How long each player has to answer `GetMedia`.
const ANSWER: Duration = Duration::from_secs(1);
/// How long a player has to accept a `MediaControl` call.
const ACCEPT: Duration = Duration::from_secs(3);

/// A player as it answered.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Player {
    /// Its bus name (`org.mpris.MediaPlayer2.vlc`).
    pub bus_name: String,
    /// Its `DesktopEntry` (without `.desktop`), when it gives one.
    pub desktop_entry: Option<String>,
    /// Its `Identity`.
    pub identity: String,
    /// `PlaybackStatus` as it is (`Playing`, `Paused`, `Stopped`).
    pub status: String,
    pub can_control: Option<bool>,
    pub can_play: Option<bool>,
    pub can_pause: Option<bool>,
    pub can_go_next: Option<bool>,
    pub can_go_previous: Option<bool>,
    pub can_seek: Option<bool>,
    /// From `Metadata`.
    pub title: Option<String>,
    pub artist: Option<Vec<String>>,
    pub album: Option<String>,
    /// `mpris:length`, µs.
    pub length: Option<i64>,
    /// `mpris:trackid`.
    pub track_id: Option<String>,
    /// `Position`, µs.
    pub position: Option<i64>,
    /// The process owning its bus name (0: not known).
    pub pid: u32,
}

impl Player {
    /// Its `id`: the bus name without `org.mpris.MediaPlayer2.`.
    pub fn id(&self) -> &str {
        self.bus_name.strip_prefix(PREFIX).unwrap_or(&self.bus_name)
    }

    /// `status` as the Media player record has it.
    pub fn status_name(&self) -> &'static str {
        match self.status.as_str() {
            "Playing" => "playing",
            "Paused" => "paused",
            _ => "stopped",
        }
    }

    /// The activity log's `target`: `player:<id>`, and ` (<app id>)` when
    /// known.
    pub fn log_target(&self) -> String {
        match &self.desktop_entry {
            Some(app) => format!("player:{} ({app})", self.id()),
            None => format!("player:{}", self.id()),
        }
    }
}

fn text(map: &Record, key: &str) -> Option<String> {
    map.get(key).and_then(|v| String::try_from(v.clone()).ok())
}

fn flag(map: &Record, key: &str) -> Option<bool> {
    map.get(key).and_then(|v| bool::try_from(v.clone()).ok())
}

fn int(map: &Record, key: &str) -> Option<i64> {
    let v = map.get(key)?;
    i64::try_from(v.clone()).ok().or_else(|| u64::try_from(v.clone()).ok().and_then(|n| i64::try_from(n).ok())).or_else(|| i32::try_from(v.clone()).ok().map(i64::from))
}

/// A player's properties of `iface`: all at once, or one by one when it
/// does not answer `GetAll` (`names`).
async fn properties(conn: &Connection, bus_name: &str, iface: &str, names: &[&str]) -> Record {
    let props = "org.freedesktop.DBus.Properties";
    if let Ok(reply) = conn.call_method(Some(bus_name), PATH, Some(props), "GetAll", &(iface,)).await {
        if let Ok(all) = reply.body().deserialize::<Record>() {
            return all;
        }
    }
    let mut out = Record::new();
    for name in names {
        if let Ok(reply) = conn.call_method(Some(bus_name), PATH, Some(props), "Get", &(iface, *name)).await {
            if let Ok(v) = reply.body().deserialize::<OwnedValue>() {
                out.insert((*name).to_owned(), v);
            }
        }
    }
    out
}

/// Player `bus_name` as it answers now; `None` when it does not.
async fn read_one(conn: &Connection, bus_name: String) -> Option<Player> {
    let player = properties(conn, &bus_name, PLAYER, &["PlaybackStatus", "Metadata", "Position", "CanControl", "CanPlay", "CanPause", "CanGoNext", "CanGoPrevious", "CanSeek"]).await;
    let status = text(&player, "PlaybackStatus")?;
    let root = properties(conn, &bus_name, ROOT, &["Identity", "DesktopEntry"]).await;
    let metadata: Record = player.get("Metadata").and_then(|v| Record::try_from(v.clone()).ok()).unwrap_or_default();
    let artist = metadata.get("xesam:artist").and_then(|v| Vec::<String>::try_from(v.clone()).ok().or_else(|| String::try_from(v.clone()).ok().map(|a| vec![a])));
    let track_id = metadata.get("mpris:trackid").and_then(|v| zbus::zvariant::OwnedObjectPath::try_from(v.clone()).ok().map(|p| p.to_string()).or_else(|| String::try_from(v.clone()).ok()));
    let pid = match zbus::fdo::DBusProxy::new(conn).await {
        Ok(dbus) => match zbus::names::BusName::try_from(bus_name.clone()) {
            Ok(name) => dbus.get_connection_unix_process_id(name).await.unwrap_or(0),
            Err(_) => 0,
        },
        Err(_) => 0,
    };
    Some(Player {
        desktop_entry: text(&root, "DesktopEntry").map(|e| e.strip_suffix(".desktop").unwrap_or(&e).to_owned()).filter(|e| !e.is_empty()),
        identity: text(&root, "Identity").unwrap_or_default(),
        status,
        can_control: flag(&player, "CanControl"),
        can_play: flag(&player, "CanPlay"),
        can_pause: flag(&player, "CanPause"),
        can_go_next: flag(&player, "CanGoNext"),
        can_go_previous: flag(&player, "CanGoPrevious"),
        can_seek: flag(&player, "CanSeek"),
        title: text(&metadata, "xesam:title"),
        artist,
        album: text(&metadata, "xesam:album"),
        length: int(&metadata, "mpris:length").filter(|l| *l > 0),
        track_id,
        position: int(&player, "Position").filter(|p| *p >= 0),
        pid,
        bus_name,
    })
}

/// The players on `conn` now, by bus name; each has [`ANSWER`] to answer,
/// and those that do not are left out.
pub async fn read(conn: &Connection) -> Vec<Player> {
    let Ok(dbus) = zbus::fdo::DBusProxy::new(conn).await else { return Vec::new() };
    let Ok(names) = dbus.list_names().await else { return Vec::new() };
    let mut names: Vec<String> = names.iter().map(|n| n.to_string()).filter(|n| n.starts_with(PREFIX) && n.len() > PREFIX.len()).collect();
    names.sort();
    let asks = names.into_iter().map(|name| async move { tokio::time::timeout(ANSWER, read_one(conn, name)).await.ok().flatten() });
    futures_util::future::join_all(asks).await.into_iter().flatten().collect()
}

/// Refuse `p` when it is Settings' or an excluded app's (see the module
/// documentation), with the exclusion's error.
pub fn check(p: &Player, snapshot: &Snapshot, config: &Config, installed: &[Installed], proc_root: &Path) -> Result<(), AgentError> {
    match &p.desktop_entry {
        Some(entry) => policy::check_target(config, Target::App(entry))?,
        None if !p.identity.is_empty() => {
            for app in installed.iter().filter(|a| a.name == p.identity) {
                policy::check_target(config, Target::App(&app.id))?;
            }
        }
        None => {}
    }
    super::a11y::check_app_process(p.pid, snapshot, config, installed, proc_root)
}

/// The player the playback keys would control among `players`
/// (`contracts/dbus/org.mpris.MediaPlayer2.Player.md`): its bus name.
pub fn active<'a>(players: &[&'a Player]) -> Option<&'a str> {
    let candidates: Vec<(String, String)> = players.iter().filter(|p| p.can_control != Some(false)).map(|p| (p.bus_name.clone(), p.status.clone())).collect();
    let chosen = crate::shell_keys::choose_player(&candidates)?;
    players.iter().find(|p| p.bus_name == chosen).map(|p| p.bus_name.as_str())
}

/// Seconds from microseconds.
fn seconds(us: i64) -> f64 {
    us as f64 / 1_000_000.0
}

/// `p`'s Media player record. The one place that builds it: what it tells
/// of the track (title, artist, album) is decided here.
pub fn record(p: &Player, active: bool) -> Record {
    let mut r = Record::from([
        ("id".to_owned(), value(p.id())),
        ("name".to_owned(), value(p.identity.as_str())),
        ("status".to_owned(), value(p.status_name())),
        ("active".to_owned(), value(active)),
    ]);
    if let Some(app) = &p.desktop_entry {
        r.insert("app-id".to_owned(), value(app.as_str()));
    }
    if let Some(title) = &p.title {
        r.insert("title".to_owned(), value(title.as_str()));
    }
    if let Some(artist) = &p.artist {
        r.insert("artist".to_owned(), value(artist.clone()));
    }
    if let Some(album) = &p.album {
        r.insert("album".to_owned(), value(album.as_str()));
    }
    if let Some(length) = p.length {
        r.insert("length".to_owned(), value(seconds(length)));
    }
    if let Some(position) = p.position {
        r.insert("position".to_owned(), value(seconds(position)));
    }
    for (key, can) in [
        ("can-play", p.can_play),
        ("can-pause", p.can_pause),
        ("can-go-next", p.can_go_next),
        ("can-go-previous", p.can_go_previous),
        ("can-seek", p.can_seek),
        ("can-control", p.can_control),
    ] {
        if let Some(can) = can {
            r.insert(key.to_owned(), value(can));
        }
    }
    r
}

/// The Media player records of the `players` an agent may see: the active
/// one first, then by bus name.
pub fn records(players: &[&Player]) -> Vec<Record> {
    let active = active(players);
    let mut sorted: Vec<&Player> = players.to_vec();
    sorted.sort_by(|a, b| (Some(a.bus_name.as_str()) != active).cmp(&(Some(b.bus_name.as_str()) != active)).then_with(|| a.bus_name.cmp(&b.bus_name)));
    sorted.iter().map(|p| record(p, Some(p.bus_name.as_str()) == active)).collect()
}

/// Where `seek` goes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Seek {
    /// Seconds from where it is.
    Offset(f64),
    /// Seconds from the start.
    Position(f64),
}

/// What `MediaControl` asks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Play,
    Pause,
    PlayPause,
    Next,
    Previous,
    Stop,
    Seek(Seek),
}

impl Action {
    /// `action` with its options; `NotSupported` for an unknown action,
    /// `InvalidArgs` for a `seek` without one of `offset` and `position`.
    pub fn parse(action: &str, options: &HashMap<String, OwnedValue>) -> Result<Action, AgentError> {
        Ok(match action {
            "play" => Action::Play,
            "pause" => Action::Pause,
            "play-pause" => Action::PlayPause,
            "next" => Action::Next,
            "previous" => Action::Previous,
            "stop" => Action::Stop,
            "seek" => {
                let number = |key: &str| -> Result<Option<f64>, AgentError> {
                    match options.get(key) {
                        None => Ok(None),
                        Some(v) => f64::try_from(v.clone()).ok().filter(|n| n.is_finite()).map(Some).ok_or_else(|| AgentError::invalid_args(format!("“{key}” must be a number of seconds (d)"))),
                    }
                };
                match (number("offset")?, number("position")?) {
                    (Some(offset), None) => Action::Seek(Seek::Offset(offset)),
                    (None, Some(position)) if position >= 0.0 => Action::Seek(Seek::Position(position)),
                    (None, Some(_)) => return Err(AgentError::invalid_args("“position” must not be negative")),
                    _ => return Err(AgentError::invalid_args("seek takes one of “offset” and “position”")),
                }
            }
            other => return Err(AgentError::NotSupported(format!("unknown action {other}"))),
        })
    }

    /// Its name, as the activity log's `action`.
    pub fn name(self) -> &'static str {
        match self {
            Action::Play => "play",
            Action::Pause => "pause",
            Action::PlayPause => "play-pause",
            Action::Next => "next",
            Action::Previous => "previous",
            Action::Stop => "stop",
            Action::Seek(_) => "seek",
        }
    }

    /// Whether `p` can do it: `NotSupported` when it says it cannot.
    pub fn supported(self, p: &Player) -> Result<(), AgentError> {
        let cannot = |why: &str| Err(AgentError::NotSupported(format!("the player {why}")));
        if p.can_control == Some(false) {
            return cannot("cannot be controlled");
        }
        let (can, why) = match self {
            Action::Play => (p.can_play, "cannot play"),
            Action::Pause => (p.can_pause, "cannot pause"),
            Action::PlayPause => (if p.status == "Playing" { p.can_pause } else { p.can_play }, "cannot play or pause"),
            Action::Next => (p.can_go_next, "cannot skip to the next track"),
            Action::Previous => (p.can_go_previous, "cannot go back to the previous track"),
            Action::Stop => (None, ""),
            Action::Seek(_) => (p.can_seek, "cannot seek"),
        };
        if can == Some(false) {
            return cannot(why);
        }
        if let Action::Seek(Seek::Position(_)) = self {
            if p.track_id.as_deref().is_none_or(|t| ObjectPath::try_from(t).is_err()) {
                return cannot("does not tell its track");
            }
        }
        Ok(())
    }

    /// Ask `p` to do it; returns once it accepted the call.
    pub async fn send(self, conn: &Connection, p: &Player) -> Result<(), AgentError> {
        let micro = |s: f64| (s * 1_000_000.0).round() as i64;
        let name = p.bus_name.as_str();
        let call = async {
            match self {
                Action::Seek(Seek::Offset(offset)) => conn.call_method(Some(name), PATH, Some(PLAYER), "Seek", &(micro(offset),)).await,
                Action::Seek(Seek::Position(position)) => {
                    let track = p.track_id.as_deref().and_then(|t| ObjectPath::try_from(t).ok()).ok_or_else(|| zbus::Error::Failure("no track".into()))?;
                    conn.call_method(Some(name), PATH, Some(PLAYER), "SetPosition", &(track, micro(position))).await
                }
                other => {
                    let method = match other {
                        Action::Play => "Play",
                        Action::Pause => "Pause",
                        Action::PlayPause => "PlayPause",
                        Action::Next => "Next",
                        Action::Previous => "Previous",
                        _ => "Stop",
                    };
                    conn.call_method(Some(name), PATH, Some(PLAYER), method, &()).await
                }
            }
        };
        match tokio::time::timeout(ACCEPT, call).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(err)) => Err(AgentError::Failed(format!("the player refused: {err}"))),
            Err(_) => Err(AgentError::Failed("the player did not answer in time".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn player(name: &str, status: &str) -> Player {
        Player { bus_name: format!("{PREFIX}{name}"), status: status.into(), identity: name.into(), ..Player::default() }
    }

    #[test]
    fn active_and_order() {
        let (a, b, c) = (player("a", "Paused"), player("b", "Stopped"), player("c", "Playing"));
        let all = [&a, &b, &c];
        assert_eq!(active(&all), Some("org.mpris.MediaPlayer2.c"));
        let ids: Vec<String> = records(&all).iter().map(|r| String::try_from(r["id"].clone()).unwrap()).collect();
        assert_eq!(ids, ["c", "a", "b"], "the active one first, then by bus name");
        // A player that cannot be controlled is never the active one.
        let c2 = Player { can_control: Some(false), ..c.clone() };
        assert_eq!(active(&[&a, &b, &c2]), Some("org.mpris.MediaPlayer2.a"));
        assert_eq!(active(&[]), None);
    }

    #[test]
    fn record_keys() {
        let p = Player {
            desktop_entry: Some("org.videolan.VLC".into()),
            title: Some("Song".into()),
            artist: Some(vec!["A".into(), "B".into()]),
            length: Some(90_500_000),
            position: Some(1_000_000),
            can_seek: Some(true),
            ..player("vlc", "Playing")
        };
        let r = record(&p, true);
        assert_eq!(String::try_from(r["status"].clone()).unwrap(), "playing");
        assert_eq!(f64::try_from(r["length"].clone()).unwrap(), 90.5);
        assert_eq!(f64::try_from(r["position"].clone()).unwrap(), 1.0);
        assert_eq!(Vec::<String>::try_from(r["artist"].clone()).unwrap(), ["A", "B"]);
        assert!(!r.contains_key("album") && !r.contains_key("can-play"));
        assert_eq!(p.log_target(), "player:vlc (org.videolan.VLC)");
        assert_eq!(player("x", "Paused").log_target(), "player:x");
    }

    #[test]
    fn actions() {
        let none = HashMap::new();
        assert_eq!(Action::parse("play-pause", &none).unwrap(), Action::PlayPause);
        assert_eq!(Action::parse("rewind", &none).unwrap_err().kind(), "NotSupported");
        assert_eq!(Action::parse("seek", &none).unwrap_err().kind(), "InvalidArgs");
        let opts = |pairs: &[(&str, f64)]| pairs.iter().map(|(k, v)| ((*k).to_owned(), value(*v))).collect::<HashMap<_, _>>();
        assert_eq!(Action::parse("seek", &opts(&[("offset", -10.0)])).unwrap(), Action::Seek(Seek::Offset(-10.0)));
        assert_eq!(Action::parse("seek", &opts(&[("position", 5.0)])).unwrap(), Action::Seek(Seek::Position(5.0)));
        assert_eq!(Action::parse("seek", &opts(&[("offset", 1.0), ("position", 5.0)])).unwrap_err().kind(), "InvalidArgs");
        assert_eq!(Action::parse("seek", &opts(&[("position", -1.0)])).unwrap_err().kind(), "InvalidArgs");
        let p = Player { can_seek: Some(true), ..player("a", "Playing") };
        assert!(Action::Seek(Seek::Offset(1.0)).supported(&p).is_ok());
        assert_eq!(Action::Seek(Seek::Position(1.0)).supported(&p).unwrap_err().kind(), "NotSupported", "no track id");
        let p = Player { track_id: Some("/org/example/track/1".into()), ..p };
        assert!(Action::Seek(Seek::Position(1.0)).supported(&p).is_ok());
        let p = Player { can_control: Some(false), ..p };
        assert_eq!(Action::Stop.supported(&p).unwrap_err().kind(), "NotSupported");
        let p = Player { can_go_next: Some(false), ..player("a", "Playing") };
        assert_eq!(Action::Next.supported(&p).unwrap_err().kind(), "NotSupported");
        assert!(Action::Previous.supported(&p).is_ok(), "not said: tried");
    }
}
