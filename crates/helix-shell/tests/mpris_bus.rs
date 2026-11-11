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
