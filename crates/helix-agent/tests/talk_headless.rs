//! `helix-agent ask`, `confirm` and `remind` against whole headless
//! sessions (helixdesktop/spec `contracts/agent-access/README.md`, "Talk
//! to the user methods"; helixdesktop/desktop#148): the shell shows the
//! question in its dialog, which the session's synthetic input (the
//! headless input socket, the way remote desktop input arrives too)
//! cannot answer; an unanswered question times out (exit 21) and locking
//! the screen cancels one (exit 20); a reminder comes due as a
//! notification, and one left waiting when the session ends comes due in
//! the next session, from the file it was kept in.
//!
//! With `HELIX_AGENT_SNAPSHOTS=dir`, the dialogs and the notifications
//! are kept there as PNGs.
//!
//! Needs helix-comp and helix-shell built next to this test; skipped
//! otherwise.

use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use helix_test_support::headless::{sibling_binary, HeadlessSession, Options};
use serde_json::Value as Json;

const CLI: &str = env!("CARGO_BIN_EXE_helix-agent");

/// The CLI on `session`'s bus with its home as every XDG directory.
fn command(session: &HeadlessSession, args: &[&str]) -> Command {
    let home = session.home();
    let mut cmd = Command::new(CLI);
    cmd.args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("LANG", "C.UTF-8")
        .env("HELIX_AGENT_NAME", "Deploy bot")
        .env("DBUS_SESSION_BUS_ADDRESS", session.bus().address())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

fn run(session: &HeadlessSession, args: &[&str]) -> Output {
    command(session, args).output().expect("run helix-agent")
}

fn spawn(session: &HeadlessSession, args: &[&str]) -> Child {
    command(session, args).spawn().expect("start helix-agent")
}

fn json(out: &Output) -> Json {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(&out.stdout)))
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("exited")
}

fn keep(session: &HeadlessSession, name: &str) {
    if let Some(dir) = std::env::var_os("HELIX_AGENT_SNAPSHOTS") {
        let _ = std::fs::create_dir_all(&dir);
        session.keep_snapshot(&Path::new(&dir).join(name));
    }
}

/// The agent indicator's red (the question's band and quote bar).
fn red(p: &[u8]) -> bool {
    (p[0], p[1], p[2]) == (0xC0, 0x1C, 0x28)
}

/// Red pixels below the top bar (the question's band, not the indicator).
fn red_below_bar((w, h, data): &(u32, u32, Vec<u8>)) -> usize {
    (40..*h).flat_map(|y| (0..*w).map(move |x| (x, y))).filter(|(x, y)| red(&data[((y * w + x) * 4) as usize..])).count()
}

/// Wait for `child` to end, at most `secs` seconds.
fn finish(child: Child, secs: u64) -> Output {
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut child = child;
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "helix-agent did not end");
        std::thread::sleep(Duration::from_millis(50));
    }
    child.wait_with_output().unwrap()
}

/// Synthetic input at every part of the dialog: Enter, Escape, Tab,
/// Space, typing, and clicks all over.
fn try_to_answer(session: &mut HeadlessSession) {
    session.input("keysym 0xff0d 1\nkeysym 0xff0d 0\nkeysym 0xff1b 1\nkeysym 0xff1b 0\ntap 15\nkeysym 0x20 1\nkeysym 0x20 0\ntype yes\nkeysym 0xff0d 1\nkeysym 0xff0d 0");
    for y in (120..560).step_by(16) {
        for x in [150, 250, 400, 550, 650] {
            session.input(&format!("click {x} {y}"));
        }
    }
}

fn start(comp: &Path, shell: &Path, schemas: &Path, me: &str, reminders: Option<&str>) -> Option<HeadlessSession> {
    let settings = "[org/helixdesktop/agent-access]\nenabled=true\nallow-remote=true\naccess-level='read-only'\nremember-mode='remember'\n";
    // This test runs the CLI, so it is the agent; the user allowed it to
    // read always.
    let grants = format!("version = 1\n\n[[agent]]\nid = \"{me}\"\nexecutable = \"{me}\"\nname = \"Deploy bot\"\nlevel = \"read-only\"\ngranted = 1\n");
    let session = HeadlessSession::start_prepared(
        Options {
            comp,
            shell: Some(shell),
            size: (800, 600),
            settings,
            env: vec![
                ("GSETTINGS_SCHEMA_DIR".into(), schemas.to_string_lossy().into_owned()),
                ("RUST_LOG".into(), "info,helix_comp::compositor::remote=debug,helix_comp::compositor::headless_input=debug".into()),
            ],
        },
        |home| {
            std::fs::create_dir_all(home.join(".config/helix")).unwrap();
            std::fs::write(home.join(".config/helix/agent-grants.toml"), &grants).unwrap();
            if let Some(kept) = reminders {
                std::fs::create_dir_all(home.join(".local/state/helix")).unwrap();
                std::fs::write(home.join(".local/state/helix/agent-reminders.json"), kept).unwrap();
            }
        },
    )?;
    assert!(session.wait_for_log("shell ready", Duration::from_secs(30)), "the shell did not start:\n{}", session.log());
    Some(session)
}

#[test]
fn questions_and_reminders_in_a_session() {
    let (Some(comp), Some(shell)) = (sibling_binary("helix-comp"), sibling_binary("helix-shell")) else {
        eprintln!("helix-comp or helix-shell not built; skipping");
        return;
    };
    let schemas = std::env::temp_dir().join(format!("helix-agent-talk-schemas-{}", std::process::id()));
    std::fs::create_dir_all(&schemas).unwrap();
    for schema in ["org.helixdesktop.agent-access", "org.helixdesktop.shell", "org.helixdesktop.extensions"] {
        let data = format!("{}/../../data/{schema}.gschema.xml", env!("CARGO_MANIFEST_DIR"));
        std::fs::copy(data, schemas.join(format!("{schema}.gschema.xml"))).unwrap();
    }
    let Ok(compiled) = Command::new("glib-compile-schemas").arg(&schemas).status() else {
        eprintln!("glib-compile-schemas is not installed; skipping");
        return;
    };
    assert!(compiled.success());
    let me = std::env::current_exe().unwrap().to_string_lossy().into_owned();
    let Some(mut session) = start(&comp, &shell, &schemas, &me, None) else { return };
    let quiet = session.fresh_snapshot();
    assert_eq!(red_below_bar(&quiet), 0);

    // A typed answer, unanswered: the dialog shows, synthetic input does
    // not reach it, and the time runs out (exit 21).
    let asking = spawn(&session, &["ask", "Which branch should I deploy to production tonight?", "--default", "feature/login", "--timeout", "8"]);
    assert!(session.wait_for_log("agent question shown", Duration::from_secs(10)), "no question:\n{}", session.log());
    std::thread::sleep(Duration::from_millis(1500));
    let shown = session.fresh_snapshot();
    keep(&session, "agent-question-text.png");
    assert!(red_below_bar(&shown) > 200, "the question's red band");
    try_to_answer(&mut session);
    let out = finish(asking, 20);
    assert_eq!((code(&out), json(&out)["error"]["code"].clone()), (21, Json::from("timed-out")), "{}", session.log());
    let log = session.log();
    assert!(log.contains("an agent prompt is shown"), "the compositor dropped the input:\n{log}");
    assert!(!log.contains("agent question answered"), "synthetic input answered it:\n{log}");
    assert!(log.contains("agent question closed"), "{log}");

    // Choices: shown, unanswered.
    let asking = spawn(&session, &["ask", "Where should the release go?", "--choice", "production", "--choice", "staging", "--choice", "nowhere yet", "--default", "staging", "--timeout", "4"]);
    assert!(session.wait_for_log("agent question shown", Duration::from_secs(10)));
    std::thread::sleep(Duration::from_millis(1500));
    keep(&session, "agent-question-choices.png");
    let out = finish(asking, 20);
    assert_eq!(code(&out), 21);

    // A reminder a few seconds ahead: said to be set, then shown.
    let out = run(&session, &["remind", "add", "+3s", "Stand up and stretch"]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stdout));
    let id = json(&out)["id"].as_str().unwrap().to_owned();
    std::thread::sleep(Duration::from_millis(700));
    keep(&session, "agent-reminder-set.png");
    assert!(session.wait_for_log("agent reminder shown", Duration::from_secs(15)), "{}", session.log());
    std::thread::sleep(Duration::from_millis(500));
    keep(&session, "agent-reminder-due.png");
    let list = json(&run(&session, &["remind", "list"]));
    assert!(list["reminders"].as_array().unwrap().iter().all(|r| r["id"] != id.as_str()), "shown ones are gone: {list}");

    // Confirm: shown; locking the screen cancels it (exit 20, the error).
    let confirming = spawn(&session, &["confirm", "Delete the 3 old backups?", "--yes", "Delete", "--no", "Keep", "--timeout", "60"]);
    assert!(session.wait_for_log("agent question shown", Duration::from_secs(10)));
    std::thread::sleep(Duration::from_millis(1500));
    keep(&session, "agent-question-confirm.png");
    // Reminders for the next session: one due soon after it starts, and
    // one an hour on.
    let soon = json(&run(&session, &["remind", "add", "+12s", "Check the deploy"]))["id"].as_str().unwrap().to_owned();
    let later = json(&run(&session, &["remind", "add", "+1h", "Write the release notes"]))["id"].as_str().unwrap().to_owned();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let conn = session.bus().connect().await;
        conn.call_method(Some("org.gnome.ScreenSaver"), "/org/gnome/ScreenSaver", Some("org.gnome.ScreenSaver"), "Lock", &()).await.unwrap();
    });
    let out = finish(confirming, 20);
    assert_eq!(code(&out), 20, "{}", String::from_utf8_lossy(&out.stdout));
    assert_eq!(json(&out)["error"]["code"], "no");
    assert_eq!(json(&out)["error"]["message"], "The user cancelled the question");
    let kept = std::fs::read_to_string(session.home().join(".local/state/helix/agent-reminders.json")).unwrap();
    assert!(kept.contains(&soon) && kept.contains(&later) && !kept.contains(&id), "{kept}");
    drop(session);

    // The next session: the reminders are still there; the one now due
    // is shown.
    let Some(session) = start(&comp, &shell, &schemas, &me, Some(&kept)) else { return };
    let list = json(&run(&session, &["remind", "list"]));
    let ids: Vec<&str> = list["reminders"].as_array().unwrap().iter().filter_map(|r| r["id"].as_str()).collect();
    assert!(ids.contains(&later.as_str()), "{list}");
    assert!(session.wait_for_log("agent reminder shown", Duration::from_secs(30)), "{}", session.log());
    let left = std::fs::read_to_string(session.home().join(".local/state/helix/agent-reminders.json")).unwrap();
    assert!(!left.contains(&soon) && left.contains(&later), "{left}");
    // Cancelled by the agent: gone.
    let out = run(&session, &["remind", "cancel", &later]);
    assert_eq!(code(&out), 0);
    assert_eq!(code(&run(&session, &["remind", "cancel", &later])), 22);
    let text = String::from_utf8_lossy(&run(&session, &["--text", "remind", "list"]).stdout).into_owned();
    assert_eq!(text.trim(), "No reminders.");
    // The activity log has the calls, never the texts.
    let dir = session.home().join(".local/state/helix/agent-activity");
    let all: String = std::fs::read_dir(&dir).unwrap().flatten().map(|e| std::fs::read_to_string(e.path()).unwrap()).collect();
    assert!(all.contains("\"method\":\"Reminder\"") && all.contains("\"method\":\"CancelReminder\""), "{all}");
    for secret in ["release", "deploy", "Check", "branch"] {
        assert!(!all.contains(secret), "{secret} in the log:\n{all}");
    }
    let _ = std::fs::remove_dir_all(&schemas);
}
