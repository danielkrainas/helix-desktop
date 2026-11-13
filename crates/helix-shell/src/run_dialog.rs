//! The run dialog (helix-desktop-spec `contracts/wayland/README.md`, "Run
//! a command"): `panel-run-dialog` (Alt+F2) asks for a command and runs
//! it; `org.gnome.shell command-history` keeps what was run.

use tokio::sync::mpsc;

use crate::prompt::{self, Answer, Prompt};
use crate::UiEvent;

const SHELL: &str = "org.gnome.shell";
/// Commands kept in the history.
const HISTORY_MAX: usize = 30;

/// What to do with the text typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Nothing typed: just close.
    Nothing,
    /// Open a file, folder or URI with its default app.
    Open(String),
    /// Run a command line.
    Run(String),
}

/// `$HOME` for a leading `~`.
fn expand_home(text: &str, home: &str) -> String {
    match text.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("{home}{rest}"),
        _ => text.to_owned(),
    }
}

/// What `text` asks for.
pub fn plan(text: &str, home: &str) -> Plan {
    let text = text.trim();
    if text.is_empty() {
        return Plan::Nothing;
    }
    let expanded = expand_home(text, home);
    let uri_scheme = text.split_once("://").is_some_and(|(scheme, _)| !scheme.is_empty() && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c)));
    if uri_scheme || (expanded.starts_with('/') && !expanded.contains(' ') && std::path::Path::new(&expanded).is_dir()) {
        return Plan::Open(expanded);
    }
    Plan::Run(expanded)
}

/// The history after running `command`: added last, earlier copies
/// dropped, at most [`HISTORY_MAX`].
pub fn with_command(mut history: Vec<String>, command: &str) -> Vec<String> {
    history.retain(|c| c != command);
    history.push(command.to_owned());
    let extra = history.len().saturating_sub(HISTORY_MAX);
    history.drain(..extra);
    history
}

fn history() -> Vec<String> {
    use gio::prelude::*;
    if !helix_settings::has_schema(SHELL) {
        return Vec::new();
    }
    let s = gio::Settings::new(SHELL);
    if !s.settings_schema().is_some_and(|x| x.has_key("command-history")) {
        return Vec::new();
    }
    s.strv("command-history").iter().map(|c| c.to_string()).collect()
}

fn save_history(list: &[String]) {
    use gio::prelude::*;
    if helix_settings::has_schema(SHELL) {
        let s = gio::Settings::new(SHELL);
        if s.settings_schema().is_some_and(|x| x.has_key("command-history")) {
            let refs: Vec<&str> = list.iter().map(String::as_str).collect();
            let _ = s.set_strv("command-history", refs);
        }
    }
}

/// Runs what `text` asks for; an error message for the dialog otherwise.
fn execute(text: &str) -> Result<(), String> {
    use gio::prelude::*;
    let home = gio::glib::home_dir().to_string_lossy().into_owned();
    match plan(text, &home) {
        Plan::Nothing => Ok(()),
        Plan::Open(target) => {
            let file = if target.starts_with('/') { gio::File::for_path(&target) } else { gio::File::for_uri(&target) };
            gio::AppInfo::launch_default_for_uri(&file.uri(), None::<&gio::AppLaunchContext>).map_err(|e| e.message().to_owned())
        }
        Plan::Run(line) => {
            let argv = gio::glib::shell_parse_argv(&line).map_err(|e| e.message().to_owned())?;
            let program = argv.first().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            if gio::glib::find_program_in_path(&program).is_none() {
                return Err(format!("Command not found: {program}"));
            }
            let app = gio::AppInfo::create_from_commandline(&line, None, gio::AppInfoCreateFlags::NONE).map_err(|e| e.message().to_owned())?;
            app.launch(&[], None::<&gio::AppLaunchContext>).map_err(|e| e.message().to_owned())
        }
    }
}

/// Executable names on `$PATH` (for completion).
pub fn path_commands() -> Vec<String> {
    use std::os::unix::fs::PermissionsExt;
    let mut out: Vec<String> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .flatten()
        .filter(|e| e.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Tab completion: the first word of `text` extended to the longest
/// prefix all matching `commands` share (unchanged without a match or
/// after the first word).
pub fn complete(text: &str, commands: &[String]) -> String {
    if text.is_empty() || text.contains(char::is_whitespace) {
        return text.to_owned();
    }
    let matching: Vec<&String> = commands.iter().filter(|c| c.starts_with(text)).collect();
    let Some(first) = matching.first() else { return text.to_owned() };
    let mut prefix = first.as_str();
    for m in &matching[1..] {
        let common = prefix.chars().zip(m.chars()).take_while(|(a, b)| a == b).count();
        prefix = &prefix[..prefix.char_indices().nth(common).map_or(prefix.len(), |(i, _)| i)];
    }
    let mut out = prefix.to_owned();
    if matching.len() == 1 {
        out.push(' ');
    }
    out
}

/// Tab completion of extension command titles (any case, spaces
/// allowed): the longest prefix the matching titles share, or `None`
/// when no title starts with `text`.
pub fn complete_title(text: &str, titles: &[String]) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    let lower = text.to_lowercase();
    let matching: Vec<&String> = titles.iter().filter(|t| t.to_lowercase().starts_with(&lower)).collect();
    let first = matching.first()?;
    let mut len = first.chars().count();
    for m in &matching[1..] {
        len = len.min(first.chars().zip(m.chars()).take_while(|(a, b)| a.to_lowercase().eq(b.to_lowercase())).count());
    }
    Some(first.chars().take(len).collect())
}

/// The extension command whose title `text` is (any case).
pub fn command_for(text: &str, commands: &[(String, String)]) -> Option<String> {
    let text = text.trim().to_lowercase();
    commands.iter().find(|(_, title)| title.to_lowercase() == text).map(|(id, _)| id.clone())
}

/// Shows the dialog until a command runs or it is cancelled.
pub async fn run(ui: mpsc::UnboundedSender<UiEvent>, extensions: crate::ext_host::ExtHost) {
    if crate::locked_down("disable-command-line") {
        tracing::info!("the run dialog is locked down");
        return;
    }
    let _turn = prompt::turn().await;
    let mut text = String::new();
    let mut warning = String::new();
    loop {
        let p = Prompt {
            title: "Run a Command".into(),
            entry: Some(("Enter a Command".into(), text.clone())),
            history: history(),
            complete_commands: true,
            warning: warning.clone(),
            continue_label: "Run".into(),
            cancel_label: "Cancel".into(),
            ..Default::default()
        };
        let Answer::Continue { entry: Some(typed), .. } = prompt::ask(&ui, p).await else { break };
        // An extension command, by its title.
        let commands: Vec<(String, String)> = extensions.list().into_iter().filter(|i| i.enabled || i.kind == crate::ext_host::store::Kind::Development).flat_map(|i| i.commands.into_iter().map(move |(c, title, _)| (format!("{}/{c}", i.id), title))).collect();
        let result = match command_for(&typed, &commands) {
            Some(command) => extensions.run_command(&command).await,
            None => execute(&typed),
        };
        match result {
            Ok(()) => {
                let typed = typed.trim();
                if !typed.is_empty() {
                    tracing::info!(command = typed, "run dialog");
                    save_history(&with_command(history(), typed));
                }
                break;
            }
            Err(err) => {
                warning = err;
                text = typed;
            }
        }
    }
    prompt::done(&ui);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_commands() {
        let titles = vec!["Toggle Caffeine".to_owned(), "Toggle Counter".to_owned()];
        assert_eq!(complete_title("tog", &titles).as_deref(), Some("Toggle C"));
        assert_eq!(complete_title("toggle ca", &titles).as_deref(), Some("Toggle Caffeine"));
        assert_eq!(complete_title("gedit", &titles), None);
        let commands = vec![("org.x.C/toggle".to_owned(), "Toggle Caffeine".to_owned())];
        assert_eq!(command_for("  toggle CAFFEINE ", &commands).as_deref(), Some("org.x.C/toggle"));
        assert_eq!(command_for("toggle", &commands), None);
    }

    #[test]
    fn plans() {
        assert_eq!(plan("  ", "/home/u"), Plan::Nothing);
        assert_eq!(plan("gedit notes.txt", "/home/u"), Plan::Run("gedit notes.txt".into()));
        assert_eq!(plan("https://example.org", "/home/u"), Plan::Open("https://example.org".into()));
        assert_eq!(plan("/", "/home/u"), Plan::Open("/".into()));
        assert_eq!(plan("~/bin/tool --x", "/home/u"), Plan::Run("/home/u/bin/tool --x".into()));
        assert_eq!(plan("~user", "/home/u"), Plan::Run("~user".into()));
    }

    #[test]
    fn history_keeps_the_newest() {
        let h = with_command(vec!["a".into(), "b".into()], "a");
        assert_eq!(h, ["b", "a"]);
        let long: Vec<String> = (0..30).map(|i| i.to_string()).collect();
        let h = with_command(long, "new");
        assert_eq!(h.len(), 30);
        assert_eq!(h.first().map(String::as_str), Some("1"));
        assert_eq!(h.last().map(String::as_str), Some("new"));
    }

    #[test]
    fn completion() {
        let cmds: Vec<String> = ["gedit", "gnome-calculator", "gnome-terminal", "xterm"].iter().map(|s| s.to_string()).collect();
        assert_eq!(complete("gn", &cmds), "gnome-");
        assert_eq!(complete("xt", &cmds), "xterm ");
        assert_eq!(complete("zz", &cmds), "zz");
        assert_eq!(complete("gedit fi", &cmds), "gedit fi");
    }
}
