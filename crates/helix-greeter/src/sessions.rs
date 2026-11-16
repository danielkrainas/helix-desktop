//! The sessions offered (helix-desktop-spec `components/greeter.md`,
//! "Session" and "Starting the session"): the desktop entries in
//! `/usr/share/wayland-sessions`.

use std::path::Path;

use crate::greetd::Request;

pub const SESSIONS_DIR: &str = "/usr/share/wayland-sessions";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// The entry's file name without `.desktop`.
    pub id: String,
    pub name: String,
    pub exec: String,
    /// `DesktopNames`, as written (`;`-separated).
    pub desktop_names: Option<String>,
}

/// Undo a desktop entry string value's escapes (`\s`, `\n`, `\t`, `\r`,
/// `\\`).
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Parse a session entry. `None` when it is not to be offered: hidden,
/// `NoDisplay`, missing `Name` or `Exec`, or its `TryExec` program does not
/// exist (`exists` says whether a program can be found).
pub fn parse(id: &str, contents: &str, exists: impl Fn(&str) -> bool) -> Option<Session> {
    parse_in(id, contents, exists, &helix_i18n::locale_key_candidates(&helix_i18n::messages_locale()))
}

/// [`parse`] with the locale's `Name[…]` keys to look for, best first
/// (see `helix_i18n::locale_key_candidates`).
pub fn parse_in(id: &str, contents: &str, exists: impl Fn(&str) -> bool, locales: &[String]) -> Option<Session> {
    let mut in_entry = false;
    let (mut name, mut exec, mut try_exec, mut desktop_names) = (None, None, None, None);
    // The best localized name so far: (rank in `locales`, name).
    let mut localized: Option<(usize, String)> = None;
    let (mut hidden, mut no_display) = (false, false);
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = unescape(value.trim());
        let key = key.trim();
        // `Name[de]`: the name in the system's language.
        if let Some(lang) = key.strip_prefix("Name[").and_then(|k| k.strip_suffix(']')) {
            if let Some(rank) = locales.iter().position(|l| l == lang) {
                if !value.is_empty() && localized.as_ref().is_none_or(|(best, _)| rank < *best) {
                    localized = Some((rank, value));
                }
            }
            continue;
        }
        match key {
            "Name" => name = Some(value),
            "Exec" => exec = Some(value),
            "TryExec" => try_exec = Some(value),
            "DesktopNames" => desktop_names = Some(value),
            "Hidden" => hidden = value == "true",
            "NoDisplay" => no_display = value == "true",
            _ => {}
        }
    }
    if hidden || no_display {
        return None;
    }
    if let Some(program) = try_exec.filter(|p| !p.is_empty()) {
        if !exists(&program) {
            return None;
        }
    }
    let (name, exec) = (name.filter(|n| !n.is_empty())?, exec.filter(|e| !e.is_empty())?);
    let name = localized.map_or(name, |(_, n)| n);
    let desktop_names = desktop_names.filter(|d| !d.trim_matches(';').is_empty());
    Some(Session { id: id.to_owned(), name, exec, desktop_names })
}

/// Whether `program` (a path, or a name looked up in `PATH`) is an
/// executable file.
pub fn program_exists(program: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let executable = |p: &Path| std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    if program.contains('/') {
        return executable(Path::new(program));
    }
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into());
    std::env::split_paths(&path).any(|dir| executable(&dir.join(program)))
}

/// The sessions in `dir`, by name.
pub fn load(dir: &Path) -> Vec<Session> {
    let mut sessions = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        tracing::warn!(dir = %dir.display(), "no session directory");
        return sessions;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "desktop") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        match std::fs::read_to_string(&path) {
            Ok(contents) => sessions.extend(parse(id, &contents, program_exists)),
            Err(err) => tracing::warn!(%err, path = %path.display(), "cannot read session"),
        }
    }
    sessions.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.id.cmp(&b.id)));
    sessions
}

/// The session selected at start: the last one used, else Helix, else the
/// first.
pub fn default_index(sessions: &[Session], last: Option<&str>) -> usize {
    last.and_then(|id| sessions.iter().position(|s| s.id == id)).or_else(|| sessions.iter().position(|s| s.id == "helix")).unwrap_or(0)
}

/// Where `systemd-cat` is looked for.
const SYSTEMD_CAT: &str = "/usr/bin/systemd-cat";

/// The `start_session` request for `session`.
pub fn start_request(session: &Session) -> Request {
    start_request_with(session, std::path::Path::new(SYSTEMD_CAT).is_file())
}

/// [`start_request`], with or without `systemd-cat`. greetd does not keep a
/// session's output, so when `systemd-cat` exists the session runs under it:
/// what the session and everything it starts print goes to the journal
/// under the session's id (as display managers like GDM do).
fn start_request_with(session: &Session, systemd_cat: bool) -> Request {
    let shell = ["/bin/sh".to_owned(), "-c".to_owned(), format!("exec {}", session.exec)];
    let cmd = if systemd_cat {
        [SYSTEMD_CAT.to_owned(), format!("--identifier={}", session.id), "--".to_owned()].into_iter().chain(shell).collect()
    } else {
        shell.to_vec()
    };
    let mut env = vec![
        "XDG_SESSION_TYPE=wayland".to_owned(),
        format!("XDG_SESSION_DESKTOP={}", session.id),
        format!("DESKTOP_SESSION={}", session.id),
    ];
    if let Some(names) = &session.desktop_names {
        env.push(format!("XDG_CURRENT_DESKTOP={}", names.replace(';', ":").trim_end_matches(':')));
    }
    Request::StartSession { cmd, env }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELIX: &str = "[Desktop Entry]\nName=Helix\nComment=Helix desktop session\nExec=helix-session\nType=Application\nDesktopNames=Helix;GNOME;\n";

    #[test]
    fn parses_entries() {
        let s = parse("helix", HELIX, |_| true).unwrap();
        assert_eq!(s, Session { id: "helix".into(), name: "Helix".into(), exec: "helix-session".into(), desktop_names: Some("Helix;GNOME;".into()) });
        let ubuntu = "[Desktop Entry]\nName=Ubuntu\nName[de]=Ubuntu DE\nExec=/usr/bin/gnome-session --session=ubuntu\nTryExec=/usr/bin/gnome-session\nDesktopNames=ubuntu;GNOME\n[Desktop Action x]\nName=Other\n";
        let s = parse("ubuntu", ubuntu, |p| p == "/usr/bin/gnome-session").unwrap();
        assert_eq!(s.name, "Ubuntu");
        assert_eq!(s.exec, "/usr/bin/gnome-session --session=ubuntu");
        // The name in the system's language: the best `Name[…]` there is.
        let de = |locale: &str| parse_in("ubuntu", ubuntu, |_| true, &helix_i18n::locale_key_candidates(locale)).unwrap().name;
        assert_eq!(de("de_DE.UTF-8"), "Ubuntu DE");
        assert_eq!(de("fr_FR.UTF-8"), "Ubuntu");
        let regional = "[Desktop Entry]\nName=Colour\nName[en_US]=Color\nName[en]=Colour (en)\nName[sr@latin]=Boja\nExec=x\n";
        let name = |locale: &str| parse_in("x", regional, |_| true, &helix_i18n::locale_key_candidates(locale)).unwrap().name;
        assert_eq!(name("en_US.UTF-8"), "Color", "lang_COUNTRY before lang");
        assert_eq!(name("en_GB.UTF-8"), "Colour (en)");
        assert_eq!(name("sr_RS.UTF-8@latin"), "Boja");
        assert_eq!(name("C"), "Colour");
        assert_eq!(parse("ubuntu", ubuntu, |_| false), None, "TryExec missing");
        assert_eq!(parse("h", "[Desktop Entry]\nName=H\nExec=h\nHidden=true\n", |_| true), None);
        assert_eq!(parse("h", "[Desktop Entry]\nName=H\nExec=h\nNoDisplay=true\n", |_| true), None);
        assert!(parse("h", "[Desktop Entry]\nName=H\nExec=h\nNoDisplay=false\n", |_| true).is_some());
        assert_eq!(parse("h", "[Desktop Entry]\nExec=h\n", |_| true), None, "no name");
        assert_eq!(parse("h", "[Desktop Entry]\nName=H\n", |_| true), None, "no exec");
        assert_eq!(parse("h", "[Other]\nName=H\nExec=h\n", |_| true), None, "keys outside the entry");
        let s = parse("e", "[Desktop Entry]\nName=E\nExec=env\\sA=1 sway\n", |_| true).unwrap();
        assert_eq!(s.exec, "env A=1 sway");
        assert_eq!(s.desktop_names, None);
    }

    #[test]
    fn start_request_as_specified() {
        let helix = parse("helix", HELIX, |_| true).unwrap();
        assert_eq!(
            start_request_with(&helix, false),
            Request::StartSession {
                cmd: vec!["/bin/sh".into(), "-c".into(), "exec helix-session".into()],
                env: vec!["XDG_SESSION_TYPE=wayland".into(), "XDG_SESSION_DESKTOP=helix".into(), "DESKTOP_SESSION=helix".into(), "XDG_CURRENT_DESKTOP=Helix:GNOME".into()],
            }
        );
        let plain = Session { id: "sway".into(), name: "Sway".into(), exec: "env FOO='a b' sway --unsupported-gpu".into(), desktop_names: None };
        assert_eq!(
            start_request_with(&plain, false),
            Request::StartSession {
                cmd: vec!["/bin/sh".into(), "-c".into(), "exec env FOO='a b' sway --unsupported-gpu".into()],
                env: vec!["XDG_SESSION_TYPE=wayland".into(), "XDG_SESSION_DESKTOP=sway".into(), "DESKTOP_SESSION=sway".into()],
            }
        );
    }

    #[test]
    fn start_request_logs_to_the_journal() {
        let helix = parse("helix", HELIX, |_| true).unwrap();
        let Request::StartSession { cmd, env } = start_request_with(&helix, true) else { panic!("not a start request") };
        assert_eq!(cmd, ["/usr/bin/systemd-cat", "--identifier=helix", "--", "/bin/sh", "-c", "exec helix-session"]);
        assert!(env.contains(&"XDG_SESSION_TYPE=wayland".to_owned()));
    }

    #[test]
    fn default_session() {
        let s = |id: &str| Session { id: id.into(), name: id.into(), exec: "x".into(), desktop_names: None };
        let sessions = vec![s("a"), s("helix"), s("ubuntu")];
        assert_eq!(default_index(&sessions, Some("ubuntu")), 2);
        assert_eq!(default_index(&sessions, Some("gone")), 1);
        assert_eq!(default_index(&sessions, None), 1);
        assert_eq!(default_index(&[s("a"), s("b")], None), 0);
    }

    #[test]
    fn loads_a_directory() {
        let dir = std::env::temp_dir().join(format!("helix-greeter-sessions-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("zed.desktop"), "[Desktop Entry]\nName=Zed\nExec=zed\n").unwrap();
        std::fs::write(dir.join("helix.desktop"), HELIX).unwrap();
        std::fs::write(dir.join("gone.desktop"), "[Desktop Entry]\nName=Gone\nExec=gone\nTryExec=/nonexistent/gone\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "[Desktop Entry]\nName=Txt\nExec=x\n").unwrap();
        let names: Vec<String> = load(&dir).into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["Helix", "Zed"]);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(program_exists("sh"));
        assert!(!program_exists("/nonexistent/program"));
    }
}
