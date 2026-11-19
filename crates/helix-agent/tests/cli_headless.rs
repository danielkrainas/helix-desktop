//! `helix-agent` against a whole headless session (helixdesktop/spec
//! `contracts/agent-access/cli.md`): its JSON and text output, the exit
//! codes and messages of the shell's refusals, the agent the shell sees
//! behind it, and the commands that need the shell no more.
//!
//! Needs helix-comp and helix-shell built next to this test; skipped
//! otherwise.

use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use helix_test_support::headless::{sibling_binary, HeadlessSession, Options};
use helix_test_support::TestBus;
use serde_json::Value as Json;

const CLI: &str = env!("CARGO_BIN_EXE_helix-agent");

/// Run the CLI on `bus` with `home` as every XDG directory.
fn run(bus: &str, home: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    Command::new(CLI)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("LANG", "C.UTF-8")
        .env("DBUS_SESSION_BUS_ADDRESS", bus)
        .envs(env.iter().copied())
        .output()
        .expect("run helix-agent")
}

fn json(out: &Output) -> Json {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(&out.stdout)))
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("exited")
}

/// Run until `ok` holds for the output (settings take a moment).
fn until(what: &str, mut f: impl FnMut() -> Output, ok: impl Fn(&Output) -> bool) -> Output {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let out = f();
        if ok(&out) {
            return out;
        }
        assert!(Instant::now() < deadline, "{what}: {}", String::from_utf8_lossy(&out.stdout));
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn without_a_shell() {
    let Some(bus) = TestBus::start() else { return };
    let home = std::env::temp_dir().join(format!("helix-agent-cli-noshell-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let out = run(bus.address(), &home, &["status"], &[]);
    assert_eq!(code(&out), 3);
    assert_eq!(json(&out)["error"]["code"], "no-shell");
    assert!(out.stderr.is_empty(), "JSON failures say nothing on standard error");
    // Text: the message on standard error.
    let out = run(bus.address(), &home, &["--text", "status"], &[]);
    assert_eq!(code(&out), 3);
    assert_eq!(String::from_utf8_lossy(&out.stderr).trim(), "helix-agent: Cannot reach the Helix shell (is a Helix session running?)");
    // Usage errors, help and completions need no shell.
    assert_eq!(code(&run(bus.address(), &home, &["frobnicate"], &[])), 2);
    let help = run(bus.address(), &home, &["files", "search", "--help"], &[]);
    assert_eq!(code(&help), 0);
    assert!(String::from_utf8_lossy(&help.stdout).contains("Usage: helix-agent files search [OPTIONS] <QUERY>"));
    for shell in ["bash", "zsh", "fish"] {
        let out = run(bus.address(), &home, &["completions", shell], &[]);
        assert_eq!(code(&out), 0);
        // The script parses, where the shell is installed.
        let script = home.join(format!("complete.{shell}"));
        std::fs::write(&script, &out.stdout).unwrap();
        if let Ok(check) = Command::new(shell).arg("-n").arg(&script).output() {
            assert!(check.status.success(), "{shell}: {}", String::from_utf8_lossy(&check.stderr));
        }
    }
    // The log is the user's own file: no shell needed.
    let log = home.join(".local/state/helix/agent-activity");
    std::fs::create_dir_all(&log).unwrap();
    std::fs::write(
        log.join("2026-11-19.jsonl"),
        "{\"time\":1795082400,\"agent\":\"/usr/bin/a\",\"name\":\"A\",\"method\":\"GetState\",\"outcome\":\"allowed\"}\n\
         {\"time\":1795082500,\"agent\":\"/usr/bin/b\",\"name\":\"\",\"method\":\"Open\",\"target\":\"file:///tmp/x\",\"outcome\":\"refused:LevelTooLow\"}\n",
    )
    .unwrap();
    let all = json(&run(bus.address(), &home, &["log"], &[]));
    assert_eq!(all["entries"].as_array().unwrap().len(), 2);
    let b = json(&run(bus.address(), &home, &["log", "--agent", "/usr/bin/b"], &[]));
    assert_eq!(b["entries"][0]["method"], "Open");
    let later = json(&run(bus.address(), &home, &["log", "--since", "2026-11-19T10:01:00Z"], &[]));
    assert_eq!(later["entries"].as_array().unwrap().len(), 1);
    let text = run(bus.address(), &home, &["--text", "log"], &[]);
    assert!(String::from_utf8_lossy(&text.stdout).contains("refused:LevelTooLow"));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn against_a_session() {
    let (Some(comp), Some(shell)) = (sibling_binary("helix-comp"), sibling_binary("helix-shell")) else {
        eprintln!("helix-comp or helix-shell not built; skipping");
        return;
    };
    let schemas = std::env::temp_dir().join(format!("helix-agent-cli-schemas-{}", std::process::id()));
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
    // This test runs the CLI, so the shell sees this test as the agent
    // (helix-agent itself is skipped); the user allowed it to read.
    let me = std::env::current_exe().unwrap().to_string_lossy().into_owned();
    let grants = format!("version = 1\n\n[[agent]]\nid = \"{me}\"\nexecutable = \"{me}\"\nname = \"\"\nlevel = \"read-only\"\ngranted = 1\n");
    let Some(session) = HeadlessSession::start_prepared(
        Options { comp: &comp, shell: Some(&shell), size: (800, 600), settings: "", env: vec![("GSETTINGS_SCHEMA_DIR".into(), schemas.to_string_lossy().into_owned())] },
        |home| {
            std::fs::create_dir_all(home.join(".config/helix")).unwrap();
            std::fs::write(home.join(".config/helix/agent-grants.toml"), &grants).unwrap();
        },
    ) else {
        return;
    };
    assert!(session.wait_for_log("shell ready", Duration::from_secs(30)), "the shell did not start:\n{}", session.log());
    let (bus, home) = (session.bus().address().to_owned(), session.home().to_owned());
    let cli = |args: &[&str]| run(&bus, &home, args, &[("HELIX_AGENT_NAME", "CLI test")]);
    let keyfile = home.join(".config/glib-2.0/settings/keyfile");

    // Off by default: status answers, everything else is refused with the
    // contract's words.
    let out = cli(&["status"]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stdout));
    let status = json(&out);
    assert_eq!(status["enabled"], false);
    assert_eq!(status["version"], 1);
    assert_eq!(status["agent"], me.as_str(), "the agent is who runs helix-agent");
    assert_eq!(status["name"], "CLI test");
    let out = cli(&["request"]);
    assert_eq!(code(&out), 10);
    assert_eq!(
        json(&out),
        serde_json::json!({"error": {"code": "access-off", "exit-code": 10, "message": "Agent access is turned off (Settings › Privacy & Security › Agent Access)", "dbus-error": "org.helixdesktop.Agent1.Error.AccessOff"}})
    );
    let out = cli(&["--text", "request"]);
    assert_eq!(String::from_utf8_lossy(&out.stderr).trim(), "helix-agent: Agent access is turned off (Settings › Privacy & Security › Agent Access)");
    let text = String::from_utf8_lossy(&cli(&["--text", "status"]).stdout).into_owned();
    assert!(text.contains("Agent access: off"), "{text}");
    // Commands whose shell methods come later.
    let out = cli(&["windows"]);
    assert_eq!((code(&out), json(&out)["error"]["code"].clone()), (23, Json::from("not-supported")));
    let out = cli(&["wait", "window", "--app", "org.gnome.TextEditor", "--timeout", "1"]);
    assert_eq!(code(&out), 23);
    // -V asks the shell for its version.
    let version = json(&cli(&["--version"]));
    assert_eq!(version["agent-access"], 1);

    // On; without logind the CLI's caller counts as remote.
    std::fs::write(&keyfile, "[org/helixdesktop/agent-access]\nenabled=true\naccess-level='desktop'\nremember-mode='remember'\n").unwrap();
    let out = until("enabled was not followed", || cli(&["--no-ask", "request"]), |o| code(o) != 10);
    assert_eq!(code(&out), 16);
    assert_eq!(json(&out)["error"]["message"], "Agent access from remote sessions is turned off (Settings › Privacy & Security › Agent Access)");

    std::fs::write(&keyfile, "[org/helixdesktop/agent-access]\nenabled=true\nallow-remote=true\naccess-level='desktop'\nremember-mode='remember'\n").unwrap();
    // The remembered grant covers reading; more needs the user, who
    // cannot be asked with --no-ask.
    let out = until("allow-remote was not followed", || cli(&["--no-ask", "request", "--level", "read-only"]), |o| code(o) != 16);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stdout));
    let status = json(&out);
    assert_eq!((status["grant"].clone(), status["granted-level"].clone()), (Json::from("always"), Json::from("read-only")));
    let out = cli(&["--no-ask", "request", "--level", "desktop"]);
    assert_eq!(code(&out), 12);
    assert_eq!(json(&out)["error"]["code"], "not-granted");
    // A level above the setting.
    let out = cli(&["--no-ask", "request", "--level", "apps"]);
    assert_eq!(code(&out), 11);
    assert_eq!(json(&out)["error"]["message"], "This needs the “Control apps” access level (Settings › Privacy & Security › Agent Access)");
    // Asking shows the consent prompt; nobody answers it here, so the
    // CLI's own timeout ends the wait.
    let out = cli(&["--timeout", "2", "request", "--level", "desktop"]);
    assert_eq!(code(&out), 21, "{}", String::from_utf8_lossy(&out.stdout));
    assert!(session.wait_for_log("agent prompt shown", Duration::from_secs(5)));
    let _ = std::fs::remove_dir_all(&schemas);
}
