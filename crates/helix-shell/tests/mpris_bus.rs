//! Playback keys reach the right MPRIS player.

use std::sync::{Arc, Mutex};

use helix_shell::shell_keys::{control_player, Playback};
use helix_test_support::TestBus;

struct Player {
    status: &'static str,
    calls: Arc<Mutex<Vec<String>>>,
    name: &'static str,
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    async fn next(&self) {
        self.calls.lock().unwrap().push(format!("{} Next", self.name));
    }
    async fn play_pause(&self) {
        self.calls.lock().unwrap().push(format!("{} PlayPause", self.name));
    }
    #[zbus(property)]
    async fn playback_status(&self) -> String {
        self.status.into()
    }
    #[zbus(property)]
    async fn can_control(&self) -> bool {
        true
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn keys_go_to_the_playing_player() {
    let Some(bus) = TestBus::start() else { return };
    let conn = bus.connect().await;
    control_player(&conn, Playback::Next).await; // no player: nothing happens
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut players = Vec::new();
    for (name, status) in [("a", "Paused"), ("b", "Playing")] {
        let player = Player { status, calls: calls.clone(), name };
        players.push(
            bus.builder()
                .serve_at("/org/mpris/MediaPlayer2", player)
                .unwrap()
                .name(format!("org.mpris.MediaPlayer2.{name}"))
                .unwrap()
                .build()
                .await
                .unwrap(),
        );
    }
    control_player(&conn, Playback::Next).await;
    control_player(&conn, Playback::PlayPause).await;
    assert_eq!(*calls.lock().unwrap(), ["b Next", "b PlayPause"]);
}

/// A player that reports where it is (or not), and seeks.
struct Positioned {
    position: Arc<Mutex<Option<i64>>>,
    length: Option<i64>,
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl Positioned {
    #[zbus(property)]
    async fn playback_status(&self) -> String {
        "Playing".into()
    }
    #[zbus(property)]
    async fn can_control(&self) -> bool {
        true
    }
    #[zbus(property)]
    async fn metadata(&self) -> std::collections::HashMap<String, zbus::zvariant::OwnedValue> {
        let mut m = std::collections::HashMap::new();
        m.insert("xesam:title".to_owned(), zbus::zvariant::Value::from("Clair de Lune").try_into().unwrap());
        if let Some(length) = self.length {
            m.insert("mpris:length".to_owned(), zbus::zvariant::Value::from(length).try_into().unwrap());
        }
        m
    }
    #[zbus(property)]
    async fn position(&self) -> zbus::fdo::Result<i64> {
        (*self.position.lock().unwrap()).ok_or_else(|| zbus::fdo::Error::NotSupported("no position".into()))
    }
    #[zbus(signal)]
    async fn seeked(emitter: &zbus::object_server::SignalEmitter<'_>, position: i64) -> zbus::Result<()>;
}

#[tokio::test(flavor = "multi_thread")]
async fn position_and_length_and_seeks() {
    use std::time::{Duration, Instant};
    let Some(bus) = TestBus::start() else { return };
    let conn = bus.connect().await;
    let position = Arc::new(Mutex::new(Some(30_000_000i64)));
    let player = Positioned { position: position.clone(), length: Some(120_000_000) };
    let server = bus.builder().serve_at("/org/mpris/MediaPlayer2", player).unwrap().name("org.mpris.MediaPlayer2.music").unwrap().build().await.unwrap();
    let read = helix_shell::calendar::player(&conn).await.expect("the player");
    assert_eq!((read.title.as_str(), read.position, read.length), ("Clair de Lune", Some(30_000_000), Some(120_000_000)));
    let p = read.progress(Instant::now()).unwrap();
    assert!((0.25..0.27).contains(&p), "{p}");

    // A seek: the cards get the player again, at its new place.
    let (ui, mut events) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(helix_shell::calendar::follow_players(conn.clone(), ui));
    tokio::time::sleep(Duration::from_millis(200)).await;
    *position.lock().unwrap() = Some(90_000_000);
    let emitter = zbus::object_server::SignalEmitter::new(&server, "/org/mpris/MediaPlayer2").unwrap();
    Positioned::seeked(&emitter, 90_000_000).await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(5), events.recv()).await.expect("a re-read after the seek").unwrap();
    let helix_shell::UiEvent::MediaPlayer(Some(seeked)) = event else { panic!("not the player") };
    assert_eq!(seeked.position, Some(90_000_000));
}

#[tokio::test(flavor = "multi_thread")]
async fn no_bar_without_a_position() {
    let Some(bus) = TestBus::start() else { return };
    let conn = bus.connect().await;
    let player = Positioned { position: Arc::new(Mutex::new(None)), length: None };
    let _server = bus.builder().serve_at("/org/mpris/MediaPlayer2", player).unwrap().name("org.mpris.MediaPlayer2.radio").unwrap().build().await.unwrap();
    let read = helix_shell::calendar::player(&conn).await.expect("the player");
    assert_eq!((read.position, read.length), (None, None));
    assert_eq!(read.progress(std::time::Instant::now()), None);
}
