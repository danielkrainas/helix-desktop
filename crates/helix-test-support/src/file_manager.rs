//! A fake file manager for tests: `org.freedesktop.FileManager1` on a
//! private bus, writing down what it was asked to show.

use std::sync::{Arc, Mutex};

use zbus::interface;

use crate::TestBus;

/// What `ShowItems` was called with: the URIs and the startup id.
pub type Shown = Arc<Mutex<Vec<(Vec<String>, String)>>>;

struct FileManager(Shown);

#[interface(name = "org.freedesktop.FileManager1")]
impl FileManager {
    fn show_items(&self, uris: Vec<String>, startup_id: String) {
        self.0.lock().unwrap().push((uris, startup_id));
    }
    fn show_folders(&self, uris: Vec<String>, startup_id: String) {
        self.0.lock().unwrap().push((uris, startup_id));
    }
}

/// Serve the fake on `bus` as `org.freedesktop.FileManager1`; it goes
/// when the connection is dropped.
pub async fn serve(bus: &TestBus) -> (zbus::Connection, Shown) {
    let shown: Shown = Default::default();
    let conn = bus
        .builder()
        .serve_at("/org/freedesktop/FileManager1", FileManager(shown.clone()))
        .unwrap()
        .name("org.freedesktop.FileManager1")
        .unwrap()
        .build()
        .await
        .unwrap();
    (conn, shown)
}
