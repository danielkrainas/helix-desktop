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
         {\"time\":1795082500,\"agent\":\"/usr/bin/b\",\"name\":\"\",\"method\":\"Open\",\"target\":\"file:///tmp/x\",\"outcome\":\"refused:LevelTooLow\"}\n\
         {\"time\":1795082600,\"agent\":\"/usr/bin/b\",\"name\":\"\",\"method\":\"Input\",\"target\":\"window:3 (org.example.App)\",\"level\":\"screen-and-input\",\"outcome\":\"cancelled\",\"reason\":\"user-input\",\"done-count\":2}\n",
    )
    .unwrap();
    let all = json(&run(bus.address(), &home, &["log"], &[]));
    assert_eq!(all["entries"].as_array().unwrap().len(), 3);
    let b = json(&run(bus.address(), &home, &["log", "--agent", "/usr/bin/b"], &[]));
    assert_eq!(b["entries"][0]["method"], "Open");
    let later = json(&run(bus.address(), &home, &["log", "--since", "2026-11-19T10:01:00Z"], &[]));
    assert_eq!(later["entries"].as_array().unwrap().len(), 2);
    let text = run(bus.address(), &home, &["--text", "log"], &[]);
    assert!(String::from_utf8_lossy(&text.stdout).contains("refused:LevelTooLow"));
    assert!(String::from_utf8_lossy(&text.stdout).contains("cancelled  user-input"), "the reason after the outcome");
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
    // Reading is refused too.
    let out = cli(&["windows"]);
    assert_eq!((code(&out), json(&out)["error"]["code"].clone()), (10, Json::from("access-off")));
    let out = cli(&["wait", "window", "--app", "org.gnome.TextEditor", "--timeout", "1"]);
    assert_eq!(code(&out), 10);
    // Screenshots and input are refused like any call.
    let out = cli(&["screenshot"]);
    assert_eq!((code(&out), json(&out)["error"]["code"].clone()), (10, Json::from("access-off")));
    let out = cli(&["input", "type", "hello"]);
    assert_eq!((code(&out), json(&out)["error"]["code"].clone()), (10, Json::from("access-off")));
    // Driving apps is refused like any call.
    let out = cli(&["a11y", "tree", "1"]);
    assert_eq!((code(&out), json(&out)["error"]["code"].clone()), (10, Json::from("access-off")));
    // Questions are refused like any call, without showing anything.
    let out = cli(&["ask", "Ready?"]);
    assert_eq!((code(&out), json(&out)["error"]["code"].clone()), (10, Json::from("access-off")));
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

/// A Wayland client opening windows in the session.
mod client {
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use smithay_client_toolkit::reexports::client::globals::{registry_queue_init, GlobalListContents};
    use smithay_client_toolkit::reexports::client::protocol::{wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface};
    use smithay_client_toolkit::reexports::client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
    use smithay_client_toolkit::reexports::protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

    #[derive(Default)]
    pub struct State {
        configured: Vec<xdg_surface::XdgSurface>,
    }

    macro_rules! quiet {
        ($($iface:ty),*) => {$(
            impl Dispatch<$iface, ()> for State {
                fn event(_: &mut Self, _: &$iface, _: <$iface as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
            }
        )*};
    }
    quiet!(wl_compositor::WlCompositor, wl_surface::WlSurface, wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_buffer::WlBuffer, xdg_toplevel::XdgToplevel);

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
        fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
    }
    impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
        fn event(_: &mut Self, base: &xdg_wm_base::XdgWmBase, event: xdg_wm_base::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            if let xdg_wm_base::Event::Ping { serial } = event {
                base.pong(serial);
            }
        }
    }
    impl Dispatch<xdg_surface::XdgSurface, ()> for State {
        fn event(state: &mut Self, surface: &xdg_surface::XdgSurface, event: xdg_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            if let xdg_surface::Event::Configure { serial } = event {
                surface.ack_configure(serial);
                state.configured.push(surface.clone());
            }
        }
    }

    pub struct Client {
        queue: EventQueue<State>,
        qh: QueueHandle<State>,
        state: State,
        compositor: wl_compositor::WlCompositor,
        shm: wl_shm::WlShm,
        base: xdg_wm_base::XdgWmBase,
        scratch: PathBuf,
    }

    pub struct Window {
        surface: wl_surface::WlSurface,
        xdg: xdg_surface::XdgSurface,
        pub toplevel: xdg_toplevel::XdgToplevel,
    }

    impl Client {
        pub fn connect(display: &Path, scratch: &Path) -> Client {
            let conn = Connection::from_socket(UnixStream::connect(display).unwrap()).unwrap();
            let (globals, queue) = registry_queue_init::<State>(&conn).unwrap();
            let qh = queue.handle();
            Client {
                compositor: globals.bind(&qh, 4..=6, ()).unwrap(),
                shm: globals.bind(&qh, 1..=1, ()).unwrap(),
                base: globals.bind(&qh, 1..=6, ()).unwrap(),
                queue,
                qh,
                state: State::default(),
                scratch: scratch.to_path_buf(),
            }
        }

        pub fn roundtrip(&mut self) {
            self.queue.roundtrip(&mut self.state).unwrap();
        }

        /// A mapped toplevel of `app_id` titled `title`.
        pub fn window(&mut self, app_id: &str, title: &str) -> Window {
            let surface = self.compositor.create_surface(&self.qh, ());
            let xdg = self.base.get_xdg_surface(&surface, &self.qh, ());
            let toplevel = xdg.get_toplevel(&self.qh, ());
            toplevel.set_title(title.to_owned());
            toplevel.set_app_id(app_id.to_owned());
            surface.commit();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !self.state.configured.contains(&xdg) {
                assert!(Instant::now() < deadline, "no first configure");
                self.roundtrip();
                std::thread::sleep(Duration::from_millis(10));
            }
            let (w, h) = (320, 240);
            let bytes = vec![0x80u8; (w * h * 4) as usize];
            let path = self.scratch.join(format!("buffer-{}", app_id));
            std::fs::write(&path, &bytes).unwrap();
            let file = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
            let pool = self.shm.create_pool(file.as_fd(), bytes.len() as i32, &self.qh, ());
            let buffer = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, &self.qh, ());
            pool.destroy();
            surface.attach(Some(&buffer), 0, 0);
            surface.damage_buffer(0, 0, w, h);
            surface.commit();
            self.roundtrip();
            Window { surface, xdg, toplevel }
        }

        pub fn close(&mut self, window: Window) {
            window.toplevel.destroy();
            window.xdg.destroy();
            window.surface.destroy();
            self.roundtrip();
        }
    }
}

/// What the CLI reads from a session with real windows (helixdesktop/desktop#146):
/// windows (with the document the app tells), the state, workspaces,
/// monitors, apps, notifications (redacted without the switch), recent
/// files, file search and the waits; and the refusals for excluded apps,
/// Settings and the lock.
#[test]
fn reads_a_session() {
    let (Some(comp), Some(shell)) = (sibling_binary("helix-comp"), sibling_binary("helix-shell")) else {
        eprintln!("helix-comp or helix-shell not built; skipping");
        return;
    };
    let schemas = std::env::temp_dir().join(format!("helix-agent-cli-read-schemas-{}", std::process::id()));
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
    let grants = format!("version = 1\n\n[[agent]]\nid = \"{me}\"\nexecutable = \"{me}\"\nname = \"\"\nlevel = \"read-only\"\ngranted = 1\n");
    let base = "[org/helixdesktop/agent-access]\nenabled=true\nallow-remote=true\naccess-level='read-only'\nremember-mode='remember'\n";
    let Some(session) = HeadlessSession::start_prepared(
        Options {
            comp: &comp,
            shell: Some(&shell),
            size: (1024, 768),
            settings: base,
            env: vec![
                ("GSETTINGS_SCHEMA_DIR".into(), schemas.to_string_lossy().into_owned()),
                // No sound server for the volume.
                ("PULSE_SERVER".into(), "unix:/nonexistent/helix-test".into()),
            ],
        },
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
    let app = "org.helixdesktop.TestAgentRead";

    // A file the test app used, as GTK records it.
    let file = home.join("agent-test.txt");
    std::fs::write(&file, "hello").unwrap();
    let file_uri = format!("file://{}", file.display());
    std::fs::write(
        home.join(".local/share/recently-used.xbel"),
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<xbel version=\"1.0\" xmlns:bookmark=\"http://www.freedesktop.org/standards/desktop-bookmarks\" xmlns:mime=\"http://www.freedesktop.org/standards/shared-mime-info\">\n<bookmark href=\"{file_uri}\" added=\"2026-11-19T09:00:00Z\" modified=\"2026-11-19T09:00:00Z\" visited=\"2026-11-19T09:00:00Z\"><info><metadata owner=\"http://freedesktop.org\"><mime:mime-type type=\"text/plain\"/><bookmark:applications><bookmark:application name=\"{app}\" exec=\"&apos;{app} %u&apos;\" modified=\"2026-11-19T09:00:00Z\" count=\"1\"/></bookmark:applications></metadata></info></bookmark>\n</xbel>\n"
        ),
    )
    .unwrap();

    let mut client = client::Client::connect(&session.wayland_display, &home.join("tmp"));
    let secret = client.window("org.helixdesktop.TestAgentSecret", "Secret things");
    let settings_window = client.window("org.helixdesktop.Settings", "Agent Access");
    let window = client.window(app, "agent-test.txt - Test");

    // The windows, the newest focused, with the document its app tells.
    let out = until("the window was not listed", || cli(&["windows"]), |o| code(o) == 0 && json(o)["windows"].as_array().is_some_and(|w| w.iter().any(|w| w["app-id"] == app && w["focused"] == true && w["width"].as_i64() > Some(0))));
    let windows = json(&out)["windows"].as_array().unwrap().clone();
    let ids: Vec<&str> = windows.iter().filter_map(|w| w["app-id"].as_str()).collect();
    assert!(ids.contains(&"org.helixdesktop.TestAgentSecret"));
    assert!(!ids.contains(&"org.helixdesktop.Settings"), "Settings is never listed");
    let w = windows.iter().find(|w| w["app-id"] == app).unwrap().clone();
    let id = w["id"].as_u64().unwrap();
    assert_eq!((w["title"].clone(), w["workspace"].clone(), w["state"].clone()), (Json::from("agent-test.txt - Test"), Json::from(0), Json::from("normal")));
    assert_eq!(w["document"], file_uri.as_str());
    assert!(w["width"].as_i64().unwrap() > 0 && w["monitor"].as_str().is_some_and(|m| !m.is_empty()), "{w}");
    let text = String::from_utf8_lossy(&cli(&["--text", "windows", "--app", app]).stdout).into_owned();
    assert!(text.contains(app) && text.contains("(focused)") && text.contains(&format!("Window {id} shows {file_uri}")), "{text}");
    assert_eq!(json(&cli(&["windows", "--focused"]))["windows"].as_array().unwrap().len(), 1);
    // Settings' windows cannot be asked for.
    let out = cli(&["windows", "--app", "org.helixdesktop.Settings"]);
    assert_eq!((code(&out), json(&out)["error"]["message"].clone()), (14, Json::from("Agents can't use Settings")));

    // The state, workspaces, monitors and apps.
    let state = json(&cli(&["state"]));
    assert_eq!(state["focused-window"].as_u64(), Some(id));
    assert_eq!(state["active-workspace"], 0);
    assert!(state.get("dark-style").is_some() && state.get("volume").is_none(), "{state}");
    let spaces = json(&cli(&["workspaces"]));
    assert!(spaces["workspaces"][0]["windows"].as_u64().unwrap() >= 2, "{spaces}");
    assert_eq!(spaces["workspaces"][0]["active"], true);
    let monitors = json(&cli(&["monitors"]));
    let m = &monitors["monitors"][0];
    assert_eq!((m["width"].clone(), m["height"].clone(), m["primary"].clone()), (Json::from(1024), Json::from(768), Json::from(true)), "{monitors}");
    let apps = json(&cli(&["apps", "--running"]));
    assert!(apps["apps"].as_array().unwrap().iter().any(|a| a["id"] == app && a["running"] == true && a["windows"] == 1), "{apps}");

    // Maximized, as the compositor says.
    window.toplevel.set_maximized();
    client.roundtrip();
    until("the window was not maximized", || cli(&["windows", "--app", app]), |o| json(o)["windows"][0]["state"] == "maximized");

    // Notifications: only app and time without the switch.
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let conn = session.bus().connect().await;
        let hints: std::collections::HashMap<&str, zbus::zvariant::Value<'_>> = std::collections::HashMap::from([("desktop-entry", zbus::zvariant::Value::from(app))]);
        conn.call_method(Some("org.freedesktop.Notifications"), "/org/freedesktop/Notifications", Some("org.freedesktop.Notifications"), "Notify", &("Test", 0u32, "", "Build finished", "All green", Vec::<&str>::new(), hints, -1i32))
            .await
            .unwrap();
    });
    let notes = json(&cli(&["notifications"]));
    let n = &notes["notifications"][0];
    assert_eq!((n["app-id"].clone(), n["redacted"].clone()), (Json::from(app), Json::from(true)), "{notes}");
    assert!(n.get("summary").is_none() && n.get("body").is_none());
    let text = String::from_utf8_lossy(&cli(&["--text", "notifications"]).stdout).into_owned();
    assert!(text.contains("Notifications access is turned off") && !text.contains("Build finished"), "{text}");
    std::fs::write(&keyfile, format!("{base}notifications=true\n")).unwrap();
    let out = until("notifications was not followed", || cli(&["notifications", "--limit", "1"]), |o| json(o)["notifications"][0]["summary"] == "Build finished");
    assert_eq!(json(&out)["notifications"][0]["body"], "All green");

    // Recent files and search.
    let recent = json(&cli(&["files", "recent", "--type", "text/*"]));
    assert_eq!(recent["files"][0]["uri"], file_uri.as_str(), "{recent}");
    let found = json(&cli(&["files", "search", "agent-test", "--in", &home.to_string_lossy(), "--since", "-1d"]));
    assert_eq!(found["files"][0]["name"], "agent-test.txt", "{found}");
    assert_eq!(code(&cli(&["files", "recent", "--limit", "0"])), 2);

    // Waits.
    let out = cli(&["wait", "window", "--app", app, "--timeout", "5"]);
    assert_eq!((code(&out), json(&out)["id"].as_u64()), (0, Some(id)));
    let out = cli(&["wait", "window", "--app", "org.example.Nothing", "--timeout", "1"]);
    assert_eq!((code(&out), json(&out)["error"]["code"].clone()), (21, Json::from("timed-out")));
    let out = cli(&["wait", "closed", "999999", "--timeout", "1"]);
    assert_eq!(code(&out), 22);
    let waiter = Command::new(CLI)
        .args(["wait", "closed", &id.to_string(), "--timeout", "20"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .env("DBUS_SESSION_BUS_ADDRESS", &bus)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    client.close(window);
    let out = waiter.wait_with_output().unwrap();
    assert_eq!((code(&out), json(&out)), (0, serde_json::json!({})));
    let left = json(&cli(&["windows"]));
    assert!(!left["windows"].as_array().unwrap().iter().any(|w| w["app-id"] == app));

    // An excluded app: left out, and refused by name.
    std::fs::write(&keyfile, format!("{base}excluded-apps=['org.helixdesktop.TestAgentSecret']\n")).unwrap();
    until("excluded-apps was not followed", || cli(&["windows"]), |o| json(o)["windows"].as_array().is_some_and(|w| w.is_empty()));
    let out = cli(&["windows", "--app", "org.helixdesktop.TestAgentSecret"]);
    assert_eq!(code(&out), 14);
    assert_eq!(json(&out)["error"]["message"], "org.helixdesktop.TestAgentSecret is excluded from agent access");
    let out = cli(&["--text", "wait", "window", "--app", "org.helixdesktop.TestAgentSecret.desktop"]);
    assert_eq!(code(&out), 14);
    assert!(String::from_utf8_lossy(&out.stderr).contains("excluded from agent access"));

    // Locked: nothing.
    rt.block_on(async {
        let conn = session.bus().connect().await;
        conn.call_method(Some("org.gnome.ScreenSaver"), "/org/gnome/ScreenSaver", Some("org.gnome.ScreenSaver"), "Lock", &()).await.unwrap();
    });
    let out = until("the lock was not followed", || cli(&["state"]), |o| code(o) != 0);
    assert_eq!((code(&out), json(&out)["error"]["code"].clone()), (15, Json::from("locked")));
    assert_eq!(code(&cli(&["windows"])), 15);
    drop((secret, settings_window));
    let _ = std::fs::remove_dir_all(&schemas);
}

/// What the CLI does to a session with real windows (helixdesktop/desktop#147):
/// focus, tile and move windows and switch workspaces, and the windows
/// and workspaces show it; quick settings in the scratch settings; a
/// notification under the agent's name; the refusals of what is not
/// there, unsupported addresses, unknown apps and a sensitive action
/// without asking; all in the activity log.
#[test]
fn acts_on_a_session() {
    let (Some(comp), Some(shell)) = (sibling_binary("helix-comp"), sibling_binary("helix-shell")) else {
        eprintln!("helix-comp or helix-shell not built; skipping");
        return;
    };
    let schemas = std::env::temp_dir().join(format!("helix-agent-cli-act-schemas-{}", std::process::id()));
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
    let grants = format!("version = 1\n\n[[agent]]\nid = \"{me}\"\nexecutable = \"{me}\"\nname = \"\"\nlevel = \"desktop\"\ngranted = 1\n");
    let base = "[org/helixdesktop/agent-access]\nenabled=true\nallow-remote=true\naccess-level='desktop'\nremember-mode='remember'\n";
    let Some(session) = HeadlessSession::start_prepared(
        Options {
            comp: &comp,
            shell: Some(&shell),
            size: (1024, 768),
            settings: base,
            env: vec![
                ("GSETTINGS_SCHEMA_DIR".into(), schemas.to_string_lossy().into_owned()),
                // No sound server for the volume.
                ("PULSE_SERVER".into(), "unix:/nonexistent/helix-test".into()),
            ],
        },
        |home| {
            std::fs::create_dir_all(home.join(".config/helix")).unwrap();
            std::fs::write(home.join(".config/helix/agent-grants.toml"), &grants).unwrap();
        },
    ) else {
        return;
    };
    assert!(session.wait_for_log("shell ready", Duration::from_secs(30)), "the shell did not start:\n{}", session.log());
    let (bus, home) = (session.bus().address().to_owned(), session.home().to_owned());
    let cli = |args: &[&str]| run(&bus, &home, args, &[("HELIX_AGENT_NAME", "CLI test"), ("PULSE_SERVER", "unix:/nonexistent/helix-test")]);
    let keyfile = home.join(".config/glib-2.0/settings/keyfile");
    let window_of = |app: &str| -> Json {
        let out = cli(&["windows", "--app", app]);
        assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stdout));
        json(&out)["windows"][0].clone()
    };

    let mut client = client::Client::connect(&session.wayland_display, &home.join("tmp"));
    let first = client.window("org.helixdesktop.TestAgentFirst", "First");
    let second = client.window("org.helixdesktop.TestAgentSecond", "Second");
    until("the windows were not listed", || cli(&["windows"]), |o| json(o)["windows"].as_array().is_some_and(|w| w.len() == 2 && w.iter().any(|w| w["app-id"] == "org.helixdesktop.TestAgentSecond" && w["focused"] == true)));
    let a = window_of("org.helixdesktop.TestAgentFirst")["id"].as_u64().unwrap().to_string();

    // Focus: the first window has the focus at once.
    let out = cli(&["window", "focus", &a]);
    assert_eq!((code(&out), json(&out)), (0, serde_json::json!({})), "{}", String::from_utf8_lossy(&out.stdout));
    assert_eq!(window_of("org.helixdesktop.TestAgentFirst")["focused"], true);
    assert_eq!(json(&cli(&["state"]))["focused-window"].as_u64().map(|n| n.to_string()), Some(a.clone()));
    // Tile, then move (which ends the tile).
    assert_eq!(code(&cli(&["window", "tile-left", &a])), 0);
    let w = window_of("org.helixdesktop.TestAgentFirst");
    assert_eq!((w["state"].clone(), w["x"].clone()), (Json::from("tiled-left"), Json::from(0)), "{w}");
    assert_eq!(code(&cli(&["--text", "window", "move", &a, "200", "150"])), 0);
    let w = window_of("org.helixdesktop.TestAgentFirst");
    assert_eq!((w["state"].clone(), w["x"].clone(), w["y"].clone()), (Json::from("normal"), Json::from(200), Json::from(150)), "{w}");
    assert_eq!(code(&cli(&["window", "maximize", &a])), 0);
    assert_eq!(window_of("org.helixdesktop.TestAgentFirst")["state"], "maximized");
    assert_eq!(code(&cli(&["window", "restore", &a])), 0);
    assert_eq!(window_of("org.helixdesktop.TestAgentFirst")["state"], "normal");
    // Usage errors never reach the shell.
    assert_eq!(code(&cli(&["window", "move", &a, "200"])), 2);
    assert_eq!(code(&cli(&["window", "wiggle", &a])), 2);
    assert_eq!(code(&cli(&["window", "focus", "999999"])), 22);

    // Workspaces: there is always an empty one after the last.
    let spaces = json(&cli(&["workspaces"]))["workspaces"].as_array().unwrap().len();
    assert!(spaces >= 2, "{spaces} workspaces");
    assert_eq!(code(&cli(&["window", "to-workspace", &a, "1"])), 0);
    assert_eq!(window_of("org.helixdesktop.TestAgentFirst")["workspace"], 1);
    assert_eq!(code(&cli(&["workspace", "switch", "1"])), 0);
    let spaces = json(&cli(&["workspaces"]));
    assert_eq!(spaces["workspaces"][1]["active"], true, "{spaces}");
    assert_eq!(json(&cli(&["state"]))["active-workspace"], 1);
    assert_eq!(code(&cli(&["workspace", "switch", "0"])), 0);
    assert_eq!(code(&cli(&["workspace", "switch", "99"])), 22);

    // Closing is sensitive: without asking, it needs confirmation.
    let out = cli(&["--no-ask", "window", "close", &a]);
    assert_eq!((code(&out), json(&out)["error"]["code"].clone()), (18, Json::from("needs-confirmation")));
    let out = cli(&["--no-ask", "toggle", "wifi", "off"]);
    assert_eq!(code(&out), 18);

    // Quick settings, in the session's own settings file.
    let out = cli(&["toggle", "dark-style", "on"]);
    assert_eq!((code(&out), json(&out)), (0, serde_json::json!({"name": "dark-style", "on": true})), "{}", String::from_utf8_lossy(&out.stdout));
    until("dark style was not written", || cli(&["state"]), |o| json(o)["dark-style"] == true);
    assert!(std::fs::read_to_string(&keyfile).unwrap().contains("color-scheme='prefer-dark'"));
    let out = cli(&["--text", "toggle", "night-light"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "night-light: on", "flipped from off");
    // What is not there: no sound server, no NetworkManager.
    let out = cli(&["set", "volume", "50"]);
    assert_eq!((code(&out), json(&out)["error"]["code"].clone()), (23, Json::from("not-supported")));
    assert_eq!(code(&cli(&["toggle", "mute"])), 23, "nothing to flip");
    assert_eq!(code(&cli(&["set", "volume", "loud"])), 2);
    // Opening: refused addresses and apps, never started here.
    assert_eq!(code(&cli(&["open", "ssh://example.org"])), 23);
    assert_eq!(code(&cli(&["open", "/nonexistent/helix-test"])), 22);
    assert_eq!(code(&cli(&["launch", "org.example.Nothing"])), 22);
    let out = cli(&["launch", "org.helixdesktop.Settings"]);
    assert_eq!((code(&out), json(&out)["error"]["message"].clone()), (14, Json::from("Agents can't use Settings")));

    // A notification, under the agent's name.
    let out = cli(&["notify", "Build finished", "All green"]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stdout));
    let id = json(&out)["id"].as_u64().unwrap();
    let notes = json(&cli(&["notifications"]));
    let n = notes["notifications"].as_array().unwrap().iter().find(|n| n["id"].as_u64() == Some(id)).cloned().unwrap();
    assert!(n["app-name"].as_str().unwrap().starts_with("Agent: CLI test ("), "{n}");

    // Recorded, without the text.
    let log = std::fs::read_dir(home.join(".local/state/helix/agent-activity")).unwrap().flatten().map(|e| std::fs::read_to_string(e.path()).unwrap()).collect::<String>();
    let lines: Vec<Json> = log.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let has = |method: &str, outcome: &str, extra: &dyn Fn(&Json) -> bool| lines.iter().any(|l| l["method"] == method && l["outcome"] == outcome && l["agent"] == me.as_str() && extra(l));
    assert!(has("WindowAction", "allowed", &|l| l["action"] == "tile-left" && l["target"] == format!("window:{a} (org.helixdesktop.TestAgentFirst)").as_str()), "{log}");
    assert!(has("WindowAction", "refused:NeedsConfirmation", &|l| l["action"] == "close"));
    assert!(has("SwitchWorkspace", "allowed", &|l| l["target"] == "workspace:1"));
    assert!(has("SetToggle", "allowed", &|l| l["target"] == "dark-style" && l["state"] == "on"));
    assert!(has("SetValue", "allowed", &|l| l["error"] == "NotSupported"));
    assert!(has("Launch", "refused:Excluded", &|l| l["target"] == "org.helixdesktop.Settings"));
    assert!(has("Notify", "allowed", &|l| l["title-length"] == 14));
    assert!(!log.contains("Build finished") && !log.contains("All green"));
    drop((first, second));
    let _ = std::fs::remove_dir_all(&schemas);
}
