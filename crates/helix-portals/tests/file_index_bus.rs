//! The file chooser's search through a file index (helix-desktop-spec
//! `org.freedesktop.impl.portal.md`, FileChooser): a private Tracker
//! endpoint, named as localsearch, on a test bus. Skipped without
//! `tracker3`.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use gio::prelude::*;
use helix_portals::file_chooser;
use helix_portals::file_index;
use helix_test_support::TestBus;

fn installed(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(program).is_file()))
}

/// `tracker3` with the test bus and a scratch home.
fn tracker(bus: &TestBus, scratch: &Path) -> Command {
    let mut c = Command::new("tracker3");
    c.env("DBUS_SESSION_BUS_ADDRESS", bus.address())
        .env("HOME", scratch)
        // No GVfs: it would mount on the real runtime directory.
        .env("GIO_USE_VFS", "local")
        .env("XDG_RUNTIME_DIR", scratch)
        .env("XDG_CACHE_HOME", scratch.join("cache"))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    c
}

#[tokio::test(flavor = "multi_thread")]
async fn index_search() {
    if !installed("tracker3") {
        eprintln!("tracker3 is not installed; skipping");
        return;
    }
    let Some(bus) = TestBus::start() else { return };
    let conn = bus.connect().await;
    let scratch = std::env::temp_dir().join(format!("helix-index-{}", std::process::id()));
    let dir = scratch.join("My Files");
    std::fs::create_dir_all(dir.join("sub/.hidden")).unwrap();
    std::fs::create_dir_all(scratch.join("elsewhere")).unwrap();
    for f in ["Report.txt", "sub/budget-notes.md", "sub/.hidden/budget.tmp", "sub/photo.png"] {
        std::fs::write(dir.join(f), "").unwrap();
    }
    std::fs::write(scratch.join("elsewhere/budget.txt"), "").unwrap();

    // No index: the chooser scans.
    assert_eq!(file_index::search(&conn, &dir, "budg").await, None);
    let scanned = file_chooser::find(&conn, &dir, "budget", None).await;
    assert_eq!(scanned, [dir.join("sub/budget-notes.md")]);

    let mut endpoint = tracker(&bus, &scratch)
        .args(["endpoint", "--session", "--ontology", "nepomuk", "-b", file_index::SERVICES[0], "-d"])
        .arg(scratch.join("db"))
        .spawn()
        .unwrap();
    let dbus = zbus::fdo::DBusProxy::new(&conn).await.unwrap();
    for _ in 0..100 {
        if dbus.name_has_owner(file_index::SERVICES[0].try_into().unwrap()).await.unwrap_or(false) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // What localsearch would have found: names, and a text's contents.
    let uri = |p: &Path| gio::File::for_path(p).uri().to_string();
    let file = |p: &Path, name: &str, extra: &str| {
        format!("<{u}> a nfo:FileDataObject ; nie:url \"{u}\" ; nfo:fileName \"{name}\" {extra}.\n", u = uri(p))
    };
    let report = dir.join("Report.txt");
    let data = format!(
        "INSERT DATA {{\n{}{}{}{}<urn:helix:1> a nfo:PlainTextDocument ; nie:isStoredAs <{}> ; nie:plainTextContent \"quarterly Budget figures\" .\n}}",
        file(&report, "Report.txt", "; nie:interpretedAs <urn:helix:1> "),
        file(&dir.join("sub/budget-notes.md"), "budget-notes.md", ""),
        file(&dir.join("sub/.hidden/budget.tmp"), "budget.tmp", ""),
        file(&scratch.join("elsewhere/budget.txt"), "budget.txt", ""),
        uri(&report),
    );
    let rq = scratch.join("insert.rq");
    std::fs::write(&rq, data).unwrap();
    let inserted = tracker(&bus, &scratch).args(["sparql", "--dbus-service", file_index::SERVICES[0], "-u", "-f"]).arg(&rq).status().unwrap();
    assert!(inserted.success());

    let mut found = file_index::search(&conn, &dir, "budg").await.expect("an index runs");
    found.sort();
    assert_eq!(found, [dir.join("Report.txt"), dir.join("sub/.hidden/budget.tmp"), dir.join("sub/budget-notes.md")], "below the folder only");
    // The chooser: hidden folders left out, names before contents.
    assert_eq!(file_chooser::find(&conn, &dir, "budget", None).await, [dir.join("sub/budget-notes.md"), dir.join("Report.txt")]);
    // Nothing in the index: the chooser scans.
    assert_eq!(file_chooser::find(&conn, &dir, "photo", None).await, [dir.join("sub/photo.png")]);

    let _ = endpoint.kill();
    let _ = endpoint.wait();
    let _ = std::fs::remove_dir_all(&scratch);
}
