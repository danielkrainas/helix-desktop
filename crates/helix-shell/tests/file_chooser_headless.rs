//! The file chooser end to end (helixdesktop/desktop#112–#117): a headless Helix
//! session (helix-comp with helix-shell, a scratch home with its own
//! folders, a private bus) is asked `org.helixdesktop.PortalDialog.ChooseFiles`
//! as helix-portals asks it, and driven through the compositor's test
//! input. Where to click comes from the shell's own log of the dialog's
//! parts (at debug level).
//!
//! Needs helix-comp built next to this test; skipped otherwise. The
//! sessions run one at a time.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use helix_test_support::headless::{sibling_binary, HeadlessSession, Options};
use zbus::zvariant::{OwnedValue, Value};

/// One session at a time (each runs a compositor, a shell and Xwayland).
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

const SETTINGS: &str = "[org/gnome/desktop/interface]\nenable-hot-corners=false\n";
const LOG: &str = "info,helix_shell::ui::file_chooser=debug";

/// The scratch home: standard folders (named in `user-dirs.dirs`), a
/// Documents folder with folders, files, a hidden file and one nobody
/// may read, and an empty Archive.
fn fill_home(home: &Path, huge: bool) {
    let dirs = "XDG_DESKTOP_DIR=\"$HOME/Desktop\"\nXDG_DOCUMENTS_DIR=\"$HOME/Documents\"\nXDG_DOWNLOAD_DIR=\"$HOME/Downloads\"\nXDG_MUSIC_DIR=\"$HOME/Music\"\nXDG_PICTURES_DIR=\"$HOME/Pictures\"\nXDG_VIDEOS_DIR=\"$HOME/Videos\"\n";
    std::fs::write(home.join(".config/user-dirs.dirs"), dirs).unwrap();
    for d in ["Desktop", "Downloads", "Music", "Pictures", "Videos", "Documents/Archive", "Documents/Invoices", "Documents/Drafts", "Documents/Slow", "Documents/Client — Contoso Ltd/2026"] {
        std::fs::create_dir_all(home.join(d)).unwrap();
    }
    let docs = home.join("Documents");
    for i in 1..=5 {
        std::fs::write(docs.join(format!("Invoice-2024{i}.pdf")), "%PDF-1.7\n").unwrap();
    }
    for f in ["notes.md", "photo.png", "report.txt", ".hidden-config", "Invoices/Invoice-1.pdf", "Client — Contoso Ltd/2026/plan.odt"] {
        std::fs::write(docs.join(f), "x").unwrap();
    }
    // Listed although nobody may read it: the listing reads no contents.
    std::fs::write(docs.join("secret.pdf"), "%PDF-1.7\n").unwrap();
    std::fs::set_permissions(docs.join("secret.pdf"), std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
    std::fs::write(home.join("Downloads/setup-1.deb"), "x").unwrap();
    if huge {
        let big = home.join("Downloads/Huge export");
        std::fs::create_dir_all(&big).unwrap();
        for i in 1..=6000 {
            std::fs::write(big.join(format!("record-{i:05}.csv")), "row\n").unwrap();
        }
    }
}

fn start(size: (u32, u32), settings: &str, env: Vec<(&str, &str)>, huge: bool) -> Option<HeadlessSession> {
    let Some(comp) = sibling_binary("helix-comp") else {
        eprintln!("helix-comp is not built next to this test; skipped");
        return None;
    };
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_helix-shell"));
    let mut env: Vec<(String, String)> = env.into_iter().map(|(k, v)| (k.to_owned(), v.to_owned())).collect();
    env.push(("RUST_LOG".into(), LOG.into()));
    let session = HeadlessSession::start_prepared(Options { comp: &comp, shell: Some(&shell), size, settings, env }, |home| fill_home(home, huge))?;
    assert!(session.wait_for_log("namespace=\"helix-panel\"", Duration::from_secs(30)), "the shell's panel:\n{}", session.log());
    Some(session)
}

fn v(value: Value<'_>) -> OwnedValue {
    OwnedValue::try_from(value).unwrap()
}

/// `ChooseFiles` as helix-portals calls it, on a thread; its answer
/// comes when the dialog is answered.
fn choose(session: &HeadlessSession, mode: &str, folder: &Path, name: &str) -> std::thread::JoinHandle<(u32, HashMap<String, OwnedValue>)> {
    let address = session.bus().address().to_owned();
    let filters = vec![
        ("Images".to_owned(), vec![(1u32, "image/png".to_owned()), (1, "image/jpeg".to_owned())]),
        ("PDF documents".to_owned(), vec![(0u32, "*.pdf".to_owned())]),
        ("All Files".to_owned(), vec![(0u32, "*".to_owned())]),
    ];
    let mut options: HashMap<String, OwnedValue> = HashMap::from([
        ("title".to_owned(), v(Value::from(if mode == "save" { "Save Document" } else { "Open Document" }))),
        ("mode".to_owned(), v(Value::from(mode))),
        ("folder".to_owned(), v(Value::from(folder.as_os_str().as_encoded_bytes().to_vec()))),
        ("filters".to_owned(), v(Value::from(filters))),
        ("filter".to_owned(), v(Value::from(2i32))),
    ]);
    if !name.is_empty() {
        options.insert("name".into(), v(Value::from(name)));
    }
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let conn = zbus::connection::Builder::address(address.as_str()).unwrap().build().await.unwrap();
            let reply = conn.call_method(Some("org.gnome.Shell"), "/org/helixdesktop/PortalDialog", Some("org.helixdesktop.PortalDialog"), "ChooseFiles", &(options,)).await.expect("ChooseFiles answered");
            reply.body().deserialize::<(u32, HashMap<String, OwnedValue>)>().unwrap()
        })
    })
}

/// The answer's paths.
fn paths(results: &HashMap<String, OwnedValue>) -> Vec<PathBuf> {
    let bytes: Vec<Vec<u8>> = results.get("paths").and_then(|v| Vec::<Vec<u8>>::try_from(v.clone()).ok()).unwrap_or_default();
    bytes.into_iter().map(|b| PathBuf::from(String::from_utf8(b).unwrap())).collect()
}

fn wait_answer(handle: std::thread::JoinHandle<(u32, HashMap<String, OwnedValue>)>) -> (u32, HashMap<String, OwnedValue>) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !handle.is_finished() {
        assert!(Instant::now() < deadline, "the chooser did not answer");
        std::thread::sleep(Duration::from_millis(50));
    }
    handle.join().unwrap()
}

/// How many log lines hold `text`.
fn count(session: &HeadlessSession, text: &str) -> usize {
    session.log().lines().filter(|l| l.contains(text)).count()
}

/// Wait until `n` log lines hold all of `texts`.
fn wait_lines(session: &HeadlessSession, texts: &[&str], n: usize) -> bool {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if session.log().lines().filter(|l| texts.iter().all(|t| l.contains(t))).count() >= n {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// The dialog's parts as last logged: name → (x, y, w, h).
fn parts(session: &HeadlessSession) -> HashMap<String, (i32, i32, i32, i32)> {
    let log = session.log();
    let Some(line) = log.lines().rev().find(|l| l.contains("file chooser parts")) else { return HashMap::new() };
    let Some(at) = line.find("hits=") else { return HashMap::new() };
    line[at + 5..]
        .split_whitespace()
        .filter_map(|t| {
            let (name, r) = t.rsplit_once('=')?;
            let n: Vec<i32> = r.split(',').filter_map(|x| x.parse().ok()).collect();
            (n.len() == 4).then(|| (name.to_owned(), (n[0], n[1], n[2], n[3])))
        })
        .collect()
}

/// Wait for part `name` and give its middle.
fn part(session: &HeadlessSession, name: &str) -> (i32, i32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some((x, y, w, h)) = parts(session).get(name) {
            return (x + w / 2, y + h / 2);
        }
        assert!(Instant::now() < deadline, "no {name} among the parts:\n{}", session.log());
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn click(session: &mut HeadlessSession, (x, y): (i32, i32)) {
    session.input(&format!("click {x} {y}"));
}

/// Click the file type button and wait for its menu to be drawn.
fn open_menu(session: &mut HeadlessSession) {
    let before = count(session, "file chooser parts");
    click_part(session, "Files(FilterButton)");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !(count(session, "file chooser parts") > before && parts(session).contains_key("Files(FilterItem(0))")) {
        assert!(Instant::now() < deadline, "the file type menu did not open:\n{}", session.log());
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Click the middle of part `name`.
fn click_part(session: &mut HeadlessSession, name: &str) {
    let at = part(session, name);
    click(session, at);
}

/// Folder changes, keys, places and the file type menu: one dialog
/// surface throughout (helixdesktop/desktop#112), the keyboard (#114), a click
/// where a place was before a folder change (#116), the file type in one
/// step (#115).
#[test]
fn browsing_keeps_one_dialog() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut s) = start((1280, 720), SETTINGS, Vec::new(), false) else { return };
    let docs = s.home().join("Documents");
    let answer = choose(&s, "open", &docs, "");
    assert!(wait_lines(&s, &["file chooser listed", "entries=15"], 1), "Documents listed with the unreadable file:\n{}", s.log());
    std::thread::sleep(Duration::from_millis(500));
    // Type-ahead: "inv" selects the folder Invoices (folders come first).
    s.input("type inv");
    assert!(wait_lines(&s, &["type-ahead typed=inv", "current=Some(\"Invoices\")"], 1), "type-ahead:\n{}", s.log());
    // Enter opens it; Backspace goes back up with it selected.
    s.input("tap 28");
    assert!(wait_lines(&s, &["file chooser folder", "Documents/Invoices"], 1), "into Invoices:\n{}", s.log());
    assert!(wait_lines(&s, &["file chooser listed", "entries=1 "], 1));
    s.input("tap 14");
    assert!(wait_lines(&s, &["file chooser listed", "current=Some(\"Invoices\")"], 1), "back up with Invoices selected:\n{}", s.log());
    // End: the last file.
    s.input("tap 107");
    assert!(wait_lines(&s, &["current row current=secret.pdf"], 1), "End:\n{}", s.log());
    // Ctrl+H shows hidden files: type-ahead finds one.
    s.input("tap 29 35");
    assert!(wait_lines(&s, &["hidden files", "hidden=true", "rows=15"], 1), "Ctrl+H:\n{}", s.log());
    std::thread::sleep(Duration::from_millis(1700));
    s.input("type .hid");
    assert!(wait_lines(&s, &["type-ahead", "current=Some(\".hidden-config\")"], 1), "the hidden file:\n{}", s.log());
    s.input("tap 29 35");
    assert!(wait_lines(&s, &["hidden files", "hidden=false"], 1));
    // The file type menu: PDF documents in one step.
    open_menu(&mut s);
    click_part(&mut s, "Files(FilterItem(1))");
    assert!(wait_lines(&s, &["file chooser filtered", "rows=11"], 1), "five folders and six PDFs:\n{}", s.log());
    open_menu(&mut s);
    click_part(&mut s, "Files(FilterItem(2))");
    assert!(wait_lines(&s, &["file chooser filtered", "rows=14"], 1));
    // Into the empty Archive, then a click where Downloads was.
    let downloads = part(&s, "Files(Place(4))");
    // (The file type button has the keyboard now: back to the list.)
    click_part(&mut s, "Files(Row(1))");
    std::thread::sleep(Duration::from_millis(1700));
    s.input("type arc");
    assert!(wait_lines(&s, &["type-ahead typed=arc", "current=Some(\"Archive\")"], 1));
    s.input("tap 28");
    assert!(wait_lines(&s, &["file chooser listed", "entries=0 "], 1), "the empty Archive:\n{}", s.log());
    click(&mut s, downloads);
    assert!(wait_lines(&s, &["file chooser folder", "/Downloads"], 1), "the click opened Downloads:\n{}", s.log());
    assert!(wait_lines(&s, &["file chooser listed", "entries=1 "], 2));
    // One dialog surface the whole time.
    assert_eq!(count(&s, "new layer surface namespace=\"helix-dialog\""), 1, "{}", s.log());
    s.input("tap 1");
    let (response, _) = wait_answer(answer);
    assert_eq!(response, 1, "Escape cancels");
}

/// A folder that takes two seconds to read: the dialog stays, says it is
/// loading, and its places and Cancel work meanwhile (helixdesktop/desktop#112).
#[test]
fn slow_folder_keeps_the_dialog() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut s) = start((1280, 720), SETTINGS, vec![("HELIX_TEST_LIST_DELAY", "/Slow:2500")], false) else { return };
    let answer = choose(&s, "open", &s.home().join("Documents/Slow"), "");
    assert!(s.wait_for_log("file chooser loading", Duration::from_secs(10)), "a loading indicator:\n{}", s.log());
    assert_eq!(count(&s, "file chooser listed"), 0, "still loading");
    // Keys typed meanwhile stay in the dialog.
    s.input("type zz");
    let clicked = Instant::now();
    click_part(&mut s, "Files(Place(4))");
    assert!(wait_lines(&s, &["file chooser listed", "entries=1 "], 1), "Downloads while Slow loads:\n{}", s.log());
    assert!(clicked.elapsed() < Duration::from_millis(2000), "the place answered before the slow folder: {:?}", clicked.elapsed());
    click_part(&mut s, "Cancel");
    let (response, _) = wait_answer(answer);
    assert_eq!(response, 1);
    std::thread::sleep(Duration::from_millis(2600));
    assert_eq!(count(&s, "file chooser listed"), 1, "the slow folder's late listing is dropped:\n{}", s.log());
    assert_eq!(count(&s, "new layer surface namespace=\"helix-dialog\""), 1);
}

/// The wheel and Page Down scroll; the 6,000-file folder goes to its end
/// drawing only the rows that show, as fast as a small one
/// (helixdesktop/desktop#113, #114).
#[test]
fn large_folder_scrolls() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut s) = start((1280, 720), SETTINGS, Vec::new(), true) else { return };
    let answer = choose(&s, "open", &s.home().join("Downloads"), "");
    assert!(wait_lines(&s, &["file chooser listed", "entries=2 "], 1), "{}", s.log());
    std::thread::sleep(Duration::from_millis(500));
    // A few paints of the small folder.
    s.input("tap 108\ntap 103\ntap 108\ntap 103");
    std::thread::sleep(Duration::from_millis(800));
    let small = paint_times(&s);
    s.input("type h");
    assert!(wait_lines(&s, &["type-ahead", "current=Some(\"Huge export\")"], 1));
    s.input("tap 28");
    assert!(wait_lines(&s, &["file chooser listed", "rows=6000"], 1), "{}", s.log());
    let before = paint_times(&s).len();
    // The wheel over the list.
    let body = part(&s, "Files(List)");
    s.input(&format!("move {} {}", body.0, body.1));
    s.input("scroll 0 960");
    assert!(wait_lines(&s, &["file chooser scrolled", "scroll=960"], 1), "the wheel scrolls the list:\n{}", s.log());
    // Page Down twice from the first row: a page is the rows that show.
    s.input("tap 109");
    s.input("tap 109");
    assert!(wait_lines(&s, &["current row current=record-000"], 2));
    let paged = s.log().lines().rev().find(|l| l.contains("current row current=")).unwrap().to_owned();
    assert!(!paged.contains("record-00001.csv") && !paged.contains("record-00003.csv"), "Page Down moves by a page: {paged}");
    // End, then the wheel back up.
    s.input("tap 107");
    assert!(wait_lines(&s, &["current row current=record-06000.csv"], 1), "End:\n{}", s.log());
    s.input("scroll 0 -2000");
    assert!(wait_lines(&s, &["file chooser scrolled"], 2));
    std::thread::sleep(Duration::from_millis(800));
    let huge: Vec<(u64, u64)> = paint_times(&s).into_iter().skip(before).collect();
    assert!(!huge.is_empty() && !small.is_empty());
    let drawn_max = huge.iter().map(|p| p.1).max().unwrap();
    assert!(drawn_max <= 20, "only the rows that show are drawn: {huge:?}");
    let worst = huge.iter().map(|p| p.0).max().unwrap();
    let typical_small = {
        let mut t: Vec<u64> = small.iter().map(|p| p.0).collect();
        t.sort_unstable();
        t[t.len() / 2]
    };
    assert!(worst <= typical_small * 2 + 100_000, "a paint of 6,000 rows ({worst} µs) costs about what one of two does ({typical_small} µs)");
    eprintln!("paint µs: small median {typical_small}, huge worst {worst}");
    s.input("tap 1");
    assert_eq!(wait_answer(answer).0, 1);
}

/// (µs, rows drawn) of each paint logged.
fn paint_times(s: &HeadlessSession) -> Vec<(u64, u64)> {
    let num = |l: &str, key: &str| l.split(key).nth(1).and_then(|r| r.split_whitespace().next()).and_then(|n| n.parse().ok());
    s.log().lines().filter(|l| l.contains("file chooser painted")).filter_map(|l| Some((num(l, "us=")?, num(l, "drawn=")?))).collect()
}

/// A second click after the double-click time selects but does not
/// open; a quick one opens (helixdesktop/desktop#114).
#[test]
fn double_click_timing() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut s) = start((1280, 720), SETTINGS, Vec::new(), false) else { return };
    let docs = s.home().join("Documents");
    let answer = choose(&s, "open", &docs, "");
    assert!(wait_lines(&s, &["file chooser listed"], 1));
    // Rows: Archive, Client — Contoso Ltd, Drafts, Invoices, Slow, the
    // five invoices, then notes.md (row 10).
    let notes = part(&s, "Files(Row(10))");
    click(&mut s, notes);
    std::thread::sleep(Duration::from_millis(900));
    click(&mut s, notes);
    std::thread::sleep(Duration::from_millis(900));
    assert!(!answer.is_finished(), "a slow second click does not open");
    assert_eq!(count(&s, "double-click"), 0);
    click(&mut s, notes);
    click(&mut s, notes);
    let (response, results) = wait_answer(answer);
    assert_eq!((response, paths(&results)), (0, vec![docs.join("notes.md")]), "a double-click opens it");
}

/// Save: typing replaces the base name and keeps ".odt"; with a folder
/// selected in the list, Enter still saves under the name typed
/// (helixdesktop/desktop#114, #115).
#[test]
fn save_name_and_enter() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut s) = start((1280, 720), SETTINGS, Vec::new(), false) else { return };
    let docs = s.home().join("Documents");
    let answer = choose(&s, "save", &docs, "Untitled document.odt");
    assert!(wait_lines(&s, &["file chooser listed"], 1));
    std::thread::sleep(Duration::from_millis(500));
    s.input("type Q3 report");
    click_part(&mut s, "Files(Row(3))");
    std::thread::sleep(Duration::from_millis(300));
    s.input("tap 28");
    let (response, results) = wait_answer(answer);
    assert_eq!((response, paths(&results)), (0, vec![docs.join("Q3 report.odt")]));
    assert_eq!(count(&s, "new layer surface namespace=\"helix-dialog\""), 1);
}

/// The path bar takes you up the branch; New Folder makes a folder and
/// goes into it (helixdesktop/desktop#115).
#[test]
fn path_bar_and_new_folder() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut s) = start((1280, 720), SETTINGS, Vec::new(), false) else { return };
    let deep = s.home().join("Documents/Client — Contoso Ltd/2026");
    let answer = choose(&s, "save", &deep, "plan.odt");
    assert!(wait_lines(&s, &["file chooser listed", "entries=1 "], 1), "{}", s.log());
    // Home, Documents, Client — Contoso Ltd, 2026: Documents.
    click_part(&mut s, "Files(Crumb(1))");
    assert!(wait_lines(&s, &["file chooser listed", "current=Some(\"Client — Contoso Ltd\")"], 1), "Documents, the branch's folder selected:\n{}", s.log());
    assert!(s.log().lines().any(|l| l.contains("file chooser folder") && l.ends_with("/Documents")), "{}", s.log());
    click_part(&mut s, "Files(NewFolder)");
    std::thread::sleep(Duration::from_millis(300));
    s.input("type Drafts 2027");
    s.input("tap 28");
    assert!(wait_lines(&s, &["file chooser made a folder"], 1), "{}", s.log());
    assert!(wait_lines(&s, &["file chooser listed", "entries=0 "], 1));
    assert!(s.home().join("Documents/Drafts 2027").is_dir());
    // Saved there, under the name kept.
    s.input("tap 28");
    let (response, results) = wait_answer(answer);
    assert_eq!((response, paths(&results)), (0, vec![s.home().join("Documents/Drafts 2027/plan.odt")]));
}

/// Large Text on a 1280×800 screen: the title, the path bar and both
/// buttons are on screen and work (helixdesktop/desktop#116).
#[test]
fn large_text_fits() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let settings = format!("{SETTINGS}text-scaling-factor=1.5\n");
    let Some(mut s) = start((1280, 800), &settings, Vec::new(), false) else { return };
    let answer = choose(&s, "save", &s.home().join("Documents"), "Untitled document.odt");
    assert!(wait_lines(&s, &["file chooser listed"], 1));
    let _ = part(&s, "Confirm");
    let p = parts(&s);
    for name in ["Cancel", "Confirm", "Files(Back)", "Files(Name)", "Files(Place(0))"] {
        let (x, y, w, h) = *p.get(name).unwrap_or_else(|| panic!("no {name}: {p:?}"));
        assert!(x >= 0 && y >= 0 && x + w <= 1280 && y + h <= 800, "{name} at {x},{y} {w}×{h} is off the 1280×800 screen");
    }
    let (_, cy, _, ch) = p["Confirm"];
    assert!(ch >= 50, "the buttons grew with the text: {ch}");
    assert!(cy + ch <= 800 - 16);
    click_part(&mut s, "Confirm");
    let (response, results) = wait_answer(answer);
    assert_eq!((response, paths(&results)), (0, vec![s.home().join("Documents/Untitled document.odt")]));
}
