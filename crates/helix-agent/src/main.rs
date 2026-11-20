//! `helix-agent`: the command-line front end to Agent Access
//! (helixdesktop/spec `contracts/agent-access/cli.md`). A thin client of
//! the shell's `org.helixdesktop.Agent1` with no policy of its own: every
//! refusal comes from the shell, and becomes an exit code and a message.

mod a11y;
mod act;
mod cli;
mod clipboard;
mod complete;
mod errors;
mod json;
mod media;
mod read;
mod screen;
mod talk;
mod when;

use std::collections::HashMap;
use std::io::Write;
use std::time::Duration;

use errors::Failure;
use helix_i18n::{tr, trf};
use serde_json::Value as Json;
use zbus::zvariant::{OwnedValue, Value};

const BUS: &str = "org.gnome.Shell";
const PATH: &str = "/org/helixdesktop/Agent1";
const IFACE: &str = "org.helixdesktop.Agent1";
/// `--timeout`'s default (seconds).
pub(crate) const DEFAULT_TIMEOUT: f64 = 30.0;

/// What a command printed: JSON, and the text for people.
pub(crate) struct Done {
    pub json: Json,
    pub text: String,
}

fn main() {
    helix_i18n::init();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let terminal = unsafe { libc::isatty(1) } == 1;
    let parsed = cli::parse(&argv);
    // JSON unless asked for text, or on a terminal (also for usage errors).
    let json = match &parsed {
        Ok(p) => p.json.unwrap_or(!terminal),
        Err(_) => match argv.iter().rev().find(|a| *a == "--json" || *a == "--text") {
            Some(a) => a == "--json",
            None => !terminal,
        },
    };
    let result = match parsed {
        Ok(p) => run(&p),
        Err(message) => Err(Failure::usage(message)),
    };
    let code = match result {
        Ok(None) => 0,
        Ok(Some(done)) => {
            if json {
                println!("{}", done.json);
            } else if !done.text.is_empty() {
                print!("{}", done.text);
                if !done.text.ends_with('\n') {
                    println!();
                }
            }
            0
        }
        Err(failure) if failure.output.is_some() => {
            if json {
                println!("{}", failure.output.as_ref().unwrap_or(&Json::Null));
            }
            failure.exit_code()
        }
        Err(failure) => {
            if json {
                println!("{}", failure.to_json());
            } else {
                eprintln!("helix-agent: {}", failure.message);
                if failure.code == "usage" {
                    eprintln!("{}", tr("Run “helix-agent --help” for help."));
                }
            }
            failure.exit_code()
        }
    };
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}

/// Help and other output that is only text.
fn print_text(text: String) -> Result<Option<Done>, Failure> {
    print!("{text}");
    Ok(None)
}

fn run(p: &cli::Parsed) -> Result<Option<Done>, Failure> {
    if p.version {
        return version(p).map(Some);
    }
    if p.help {
        let path: Vec<&str> = if p.path.first() == Some(&"help") { p.args.iter().map(String::as_str).collect() } else { p.path.clone() };
        return help(&path);
    }
    let Some(_) = p.command() else { return print_text(cli::main_help()) };
    let path = p.path.join(" ");
    match path.as_str() {
        "help" => help(&p.args.iter().map(String::as_str).collect::<Vec<_>>()),
        "completions" => print_text(match p.args[0].as_str() {
            "bash" => complete::bash(),
            "zsh" => complete::zsh(),
            _ => complete::fish(),
        }),
        "log" => log(p).map(Some),
        "status" => status(p, call(p, "GetStatus", &())).map(Some),
        "request" => {
            let level = p.opt("level").unwrap_or("");
            status(p, call(p, "RequestAccess", &(level, options(p)))).map(Some)
        }
        "state" | "windows" | "apps" | "workspaces" | "monitors" | "notifications" | "files recent" | "files search" | "files starred" | "wait window" | "wait closed" => {
            read::run(p, &path).map(Some)
        }
        "media list" | "media play" | "media pause" | "media play-pause" | "media next" | "media previous" | "media stop" | "media seek" | "power status" => media::run(p, &path).map(Some),
        "open" | "launch" | "window" | "workspace switch" | "toggle" | "set" | "notify" => act::run(p, &path).map(Some),
        "ask" | "confirm" | "remind add" | "remind list" | "remind cancel" => talk::run(p, &path).map(Some),
        "clipboard get" | "clipboard types" | "clipboard set" => {
            // SAFETY: isatty on standard output.
            let json = p.json.unwrap_or(unsafe { libc::isatty(1) } != 1);
            clipboard::run(p, &path, json)
        }
        "screenshot" | "input key" | "input type" | "input click" | "input move" | "input scroll" | "input drag" | "input sequence" => {
            let json = p.json.unwrap_or(unsafe { libc::isatty(1) } != 1);
            screen::run(p, &path, json)
        }
        "a11y tree" | "a11y find" | "a11y wait" | "a11y do" | "a11y press" | "a11y focus" | "a11y select" | "a11y set" => a11y::run(p, &path).map(Some),
        // The shell's methods for the rest come with helixdesktop/desktop
        // #146–#151 (`Cmd::ready`); until then this session cannot do them.
        _ => Err(Failure::not_supported()),
    }
}

fn help(path: &[&str]) -> Result<Option<Done>, Failure> {
    match path {
        [] => print_text(cli::main_help()),
        ["exit-codes"] => print_text(cli::exit_codes_help()),
        _ => match cli::command_help(path) {
            Some(text) => print_text(text),
            None => Err(Failure::usage(trf!("unknown command “{command}”", command = path.join(" ")))),
        },
    }
}

/// The common options every method with `options` takes.
pub(crate) fn options(p: &cli::Parsed) -> HashMap<&'static str, Value<'static>> {
    let mut out = HashMap::new();
    out.insert("interactive", Value::from(!p.no_ask));
    if p.sensitive {
        out.insert("sensitive", Value::from(true));
    }
    out
}

/// Call `method` on the shell's `Agent1`, waiting at most `--timeout`.
fn call<B>(p: &cli::Parsed, method: &str, body: &B) -> Result<HashMap<String, OwnedValue>, Failure>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    call_for::<_, (HashMap<String, OwnedValue>,)>(p, method, body, p.timeout.unwrap_or(DEFAULT_TIMEOUT)).map(|(map,)| map)
}

/// Call `method`, waiting at most `timeout` seconds (0: no limit), and
/// read its reply as `R`.
pub(crate) fn call_for<B, R>(_p: &cli::Parsed, method: &str, body: &B, timeout: f64) -> Result<R, Failure>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
    R: serde::de::DeserializeOwned + zbus::zvariant::Type,
{
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| Failure::new("failed", e.to_string()))?;
    runtime.block_on(async {
        let work = async {
            let conn = zbus::Connection::session().await.map_err(|_| Failure::no_shell())?;
            let reply = conn.call_method(Some(BUS), PATH, Some(IFACE), method, body).await.map_err(errors::from_zbus)?;
            let out: R = reply.body().deserialize().map_err(|e| Failure::new("failed", e.to_string()))?;
            Ok(out)
        };
        if timeout == 0.0 {
            work.await
        } else {
            tokio::time::timeout(Duration::from_secs_f64(timeout), work).await.unwrap_or_else(|_| Err(Failure::timed_out()))
        }
    })
}

fn text(map: &HashMap<String, OwnedValue>, key: &str) -> String {
    map.get(key).and_then(|v| String::try_from(v.clone()).ok()).unwrap_or_default()
}

fn flag(map: &HashMap<String, OwnedValue>, key: &str) -> bool {
    map.get(key).and_then(|v| bool::try_from(v.clone()).ok()).unwrap_or(false)
}

/// `status` and `request`: the status dictionary.
fn status(_p: &cli::Parsed, map: Result<HashMap<String, OwnedValue>, Failure>) -> Result<Done, Failure> {
    let map = map?;
    let on_off = |on: bool| if on { tr("on") } else { tr("off") };
    let mut lines = vec![
        trf!("Agent access: {state}", state = on_off(flag(&map, "enabled"))),
        trf!("Access level: {level}", level = cli::level_name(&text(&map, "access-level"))),
    ];
    let name = text(&map, "name");
    let agent = text(&map, "agent");
    lines.push(if name.is_empty() { trf!("This agent: {agent}", agent = agent) } else { trf!("This agent: {name} ({agent})", name = name, agent = agent) });
    let granted = cli::level_name(&text(&map, "granted-level"));
    lines.push(match text(&map, "grant").as_str() {
        "session" => trf!("Allowed for this session: {level}", level = granted),
        "always" => trf!("Always allowed: {level}", level = granted),
        "denied" => tr("Denied by the user for this session").to_owned(),
        _ => tr("Not allowed yet: the user is asked on first use").to_owned(),
    });
    if !flag(&map, "identified") {
        lines.push(tr("This agent could not be told apart from the program that started it, so it can only be allowed for this session.").to_owned());
    }
    if flag(&map, "remote") {
        lines.push(if flag(&map, "allow-remote") { tr("Running in a remote session (allowed)").to_owned() } else { tr("Running in a remote session; agent access from remote sessions is turned off").to_owned() });
    }
    if flag(&map, "locked") {
        lines.push(tr("The screen is locked").to_owned());
    }
    lines.push(trf!(
        "Notifications access: {notifications}, Clipboard access: {clipboard}, Confirm sensitive actions: {confirm}",
        notifications = on_off(flag(&map, "notifications")),
        clipboard = on_off(flag(&map, "clipboard")),
        confirm = on_off(flag(&map, "confirm-sensitive"))
    ));
    // An older shell does not say.
    if map.contains_key("read-text-fields") {
        lines.push(trf!("Reading text fields: {state}", state = on_off(flag(&map, "read-text-fields"))));
    }
    if !flag(&map, "enabled") {
        lines.push(tr("Agent access is controlled in Settings › Privacy & Security › Agent Access.").to_owned());
    }
    Ok(Done { json: json::record(&map), text: lines.join("\n") })
}

/// `-V`: the CLI's version, and the shell's Agent Access version when it
/// answers.
fn version(p: &cli::Parsed) -> Result<Done, Failure> {
    let shell = call(p, "GetStatus", &()).ok().and_then(|m| m.get("version").and_then(|v| u32::try_from(v.clone()).ok()));
    let text = match shell {
        Some(v) => format!("helix-agent {}\n{}", env!("CARGO_PKG_VERSION"), trf!("Agent Access version {version}", version = v)),
        None => format!("helix-agent {}\n{}", env!("CARGO_PKG_VERSION"), tr("Agent Access version unknown (cannot reach the Helix shell)")),
    };
    Ok(Done { json: serde_json::json!({ "version": env!("CARGO_PKG_VERSION"), "agent-access": shell }), text })
}

/// Where the activity log is (`README.md`, "Activity log").
fn log_dir() -> std::path::PathBuf {
    let state = std::env::var_os("XDG_STATE_HOME").filter(|d| !d.is_empty()).map(std::path::PathBuf::from);
    let state = state.unwrap_or_else(|| std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"));
    state.join("helix/agent-activity")
}

/// An entry's time in Unix seconds (a number, or an ISO 8601 text).
fn entry_time(entry: &Json) -> Option<i64> {
    match entry.get("time")? {
        Json::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Json::String(s) => when::parse(s, 0).ok(),
        _ => None,
    }
}

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// `log`: the user's own activity log, read directly.
fn log(p: &cli::Parsed) -> Result<Done, Failure> {
    let since = p.opt("since").map(|w| when::parse(w, now())).transpose().map_err(Failure::usage)?;
    let agent = p.opt("agent");
    let dir = log_dir();
    let mut files: Vec<std::path::PathBuf> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "jsonl")).collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(Failure::new("failed", trf!("Cannot read the activity log: {error}", error = e))),
    };
    files.sort();
    let mut entries = Vec::new();
    for file in files {
        let Ok(content) = std::fs::read_to_string(&file) else { continue };
        for line in content.lines().filter(|l| !l.trim().is_empty()) {
            let Ok(entry) = serde_json::from_str::<Json>(line) else { continue };
            if agent.is_some_and(|a| entry.get("agent").and_then(Json::as_str) != Some(a)) {
                continue;
            }
            if since.is_some_and(|s| entry_time(&entry).is_none_or(|t| t < s)) {
                continue;
            }
            entries.push(entry);
        }
    }
    let field = |e: &Json, k: &str| e.get(k).and_then(Json::as_str).unwrap_or("").to_owned();
    let text = if entries.is_empty() {
        tr("No agent activity recorded.").to_owned()
    } else {
        entries
            .iter()
            .map(|e| {
                let time = entry_time(e).and_then(|t| gio::glib::DateTime::from_unix_local(t).ok()).and_then(|d| d.format("%Y-%m-%d %H:%M:%S").ok()).map(|s| s.to_string()).unwrap_or_default();
                let name = field(e, "name");
                let who = if name.is_empty() { field(e, "agent") } else { name };
                [time, who, field(e, "method"), field(e, "target"), field(e, "outcome"), field(e, "reason")].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("  ")
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(Done { json: serde_json::json!({ "entries": entries }), text })
}
