//! Who an Agent Access caller is (helixdesktop/spec
//! `contracts/agent-access/README.md`, "Who is calling"): the agent behind
//! the D-Bus caller, found by walking up its parent processes past the CLI
//! and shells, and the display name it declares (shown, never trusted).

use std::path::{Path, PathBuf};

/// Processes skipped on the way up: the CLI, D-Bus tools, shells and
/// wrappers that only pass a command on.
const SKIPPED: &[&str] = &[
    "helix-agent", "gdbus", "busctl", "dbus-send", "sh", "dash", "bash", "zsh", "fish", "ksh", "mksh", "tcsh", "csh", "env",
    "timeout", "nice", "ionice", "nohup", "setsid", "stdbuf", "xargs", "flock", "time", "script", "taskset", "chrt",
];

/// The longest walk up the process tree.
const MAX_STEPS: usize = 32;

/// The longest display name, in characters.
pub const MAX_NAME: usize = 64;

/// The agent behind a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    /// Its id: the executable, or for an interpreter the executable and
    /// the script.
    pub id: String,
    pub executable: String,
    /// The self-declared display name (`HELIX_AGENT_NAME`), or "".
    pub name: String,
    /// Whether the agent could be told apart from the CLI (only then can
    /// it be remembered).
    pub identified: bool,
    /// The agent's process.
    pub pid: u32,
    /// The D-Bus caller's process.
    pub caller_pid: u32,
    /// The caller's or the agent's environment says it came over SSH.
    pub ssh: bool,
}

/// One process, as `/proc` shows it.
struct Process {
    exe: PathBuf,
    ppid: u32,
    uid: Option<u32>,
}

fn read_process(proc_root: &Path, pid: u32) -> Option<Process> {
    let dir = proc_root.join(pid.to_string());
    let exe = std::fs::read_link(dir.join("exe")).ok()?;
    let exe = match exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(trimmed) => PathBuf::from(trimmed),
        None => exe,
    };
    let stat = std::fs::read_to_string(dir.join("stat")).ok()?;
    // "pid (comm) state ppid …": comm may hold spaces and parentheses.
    let ppid = stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()?;
    let uid = std::fs::read_to_string(dir.join("status"))
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("Uid:").and_then(|v| v.split_whitespace().next()?.parse().ok())));
    Some(Process { exe, ppid, uid })
}

fn file_name(path: &Path) -> &str {
    path.file_name().and_then(|n| n.to_str()).unwrap_or("")
}

fn nul_split(data: &[u8]) -> Vec<String> {
    data.split(|b| *b == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect()
}

fn environment(proc_root: &Path, pid: u32) -> Vec<String> {
    std::fs::read(proc_root.join(pid.to_string()).join("environ")).map(|d| nul_split(&d)).unwrap_or_default()
}

fn env_value<'a>(env: &'a [String], key: &str) -> Option<&'a str> {
    env.iter().find_map(|e| e.strip_prefix(key)?.strip_prefix('='))
}

/// Whether `name` (an executable's file name) is an interpreter whose
/// script tells agents apart.
pub fn is_interpreter(name: &str) -> bool {
    let python = name.strip_prefix("python").is_some_and(|v| v.is_empty() || v == "3" || v.strip_prefix("3.").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())));
    python || matches!(name, "pypy3" | "node" | "nodejs" | "bun" | "deno" | "perl" | "ruby" | "php")
}

/// The script an interpreter runs, from its command line (`args[0]` is
/// the interpreter): the first argument not starting with `-`, made
/// absolute against `cwd`; `-m module` for Python modules. `None` for
/// inline code (`-c`, `-e`) or standard input.
pub fn script_of(args: &[String], cwd: Option<&Path>) -> Option<String> {
    let mut rest = args.iter().skip(1);
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "-c" | "-e" | "--eval" | "-p" | "--print" | "-" => return None,
            "-m" => return rest.next().map(|m| format!("-m {m}")),
            a if a.starts_with('-') => continue,
            a => {
                let path = Path::new(a);
                return Some(match cwd {
                    Some(cwd) if path.is_relative() => cwd.join(path).to_string_lossy().into_owned(),
                    _ => a.to_owned(),
                });
            }
        }
    }
    None
}

/// A display name as shown: control characters removed, at most
/// [`MAX_NAME`] characters, trimmed.
pub fn clean_name(name: &str) -> String {
    let kept: String = name.chars().filter(|c| !c.is_control()).take(MAX_NAME).collect();
    kept.trim().to_owned()
}

/// The launcher an agent was started through: its command (`args[0]`),
/// looked up like a shell would (against `cwd` when it holds a `/`, else
/// in the agent's `PATH`), when that is a symlink to `exe` or to another
/// file in `exe`'s directory (a launcher already pointing at a newer
/// version, e.g. `~/.local/bin/claude` -> `~/.local/share/claude/versions/<v>`).
/// The launcher's path stays the same across updates; `exe`'s does not.
fn launcher(exe: &Path, args: &[String], cwd: Option<&Path>, env: &[String]) -> Option<PathBuf> {
    let command = args.first()?;
    let candidates: Vec<PathBuf> = if command.contains('/') {
        let path = Path::new(command);
        vec![if path.is_relative() { cwd?.join(path) } else { path.to_path_buf() }]
    } else {
        env_value(env, "PATH")?.split(':').filter(|d| d.starts_with('/')).map(|d| Path::new(d).join(command)).collect()
    };
    // The first one that exists is what the shell ran (or would run now).
    let found = candidates.into_iter().find(|c| std::fs::symlink_metadata(c).is_ok())?;
    if !std::fs::symlink_metadata(&found).ok()?.file_type().is_symlink() {
        return None;
    }
    let target = std::fs::canonicalize(&found).ok()?;
    let same_dir = |a: &Path, b: &Path| a.parent().is_some() && a.parent() == b.parent();
    (target == exe || same_dir(&target, exe)).then_some(found)
}

/// The agent behind the D-Bus caller `caller_pid`, whose processes must be
/// user `uid`'s, looked up under `proc_root` (`/proc`). `None` when the
/// caller itself cannot be read or is another user's.
pub fn identify(proc_root: &Path, caller_pid: u32, uid: u32) -> Option<Agent> {
    let caller = read_process(proc_root, caller_pid)?;
    if caller.uid != Some(uid) {
        return None;
    }
    let mut found: Option<(u32, PathBuf)> = None;
    let (mut pid, mut process) = (caller_pid, caller);
    for _ in 0..MAX_STEPS {
        let name = file_name(&process.exe);
        if name == "systemd" {
            break;
        }
        if !SKIPPED.contains(&name) {
            found = Some((pid, process.exe));
            break;
        }
        let parent = process.ppid;
        if parent <= 1 {
            break;
        }
        match read_process(proc_root, parent) {
            Some(p) if p.uid == Some(uid) => (pid, process) = (parent, p),
            _ => break,
        }
    }
    let caller_env = environment(proc_root, caller_pid);
    let (agent_pid, exe, identified) = match found {
        Some((pid, exe)) => (pid, exe, true),
        None => {
            let exe = read_process(proc_root, caller_pid)?.exe;
            (caller_pid, exe, false)
        }
    };
    let agent_env = if agent_pid == caller_pid { caller_env.clone() } else { environment(proc_root, agent_pid) };
    let mut executable = exe.to_string_lossy().into_owned();
    let mut id = executable.clone();
    let mut identified = identified;
    if identified {
        let dir = proc_root.join(agent_pid.to_string());
        let args = std::fs::read(dir.join("cmdline")).map(|d| nul_split(&d)).unwrap_or_default();
        let cwd = std::fs::read_link(dir.join("cwd")).ok();
        if is_interpreter(file_name(&exe)) {
            match script_of(&args, cwd.as_deref()) {
                Some(script) => id = format!("{executable} {script}"),
                None => identified = false,
            }
        } else if let Some(launcher) = launcher(&exe, &args, cwd.as_deref(), &agent_env) {
            executable = launcher.to_string_lossy().into_owned();
            id = executable.clone();
        }
    }
    let name = env_value(&caller_env, "HELIX_AGENT_NAME").or_else(|| env_value(&agent_env, "HELIX_AGENT_NAME")).map(clean_name).unwrap_or_default();
    let ssh = [&caller_env, &agent_env].iter().any(|env| env_value(env, "SSH_CONNECTION").is_some() || env_value(env, "SSH_CLIENT").is_some());
    Some(Agent { id, executable, name, identified, pid: agent_pid, caller_pid, ssh })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake `/proc` with processes (pid, ppid, uid, exe, args, cwd, env).
    struct FakeProc(PathBuf);

    impl FakeProc {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("helix-agent-proc-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            FakeProc(dir)
        }

        fn add(&self, pid: u32, ppid: u32, uid: u32, exe: &str, args: &[&str], env: &[&str]) {
            let dir = self.0.join(pid.to_string());
            std::fs::create_dir_all(&dir).unwrap();
            std::os::unix::fs::symlink(exe, dir.join("exe")).unwrap();
            std::os::unix::fs::symlink("/home/me/work", dir.join("cwd")).unwrap();
            std::fs::write(dir.join("stat"), format!("{pid} (odd ) name) S {ppid} 1 1 0")).unwrap();
            std::fs::write(dir.join("status"), format!("Name:\tx\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n")).unwrap();
            let join = |parts: &[&str]| parts.iter().flat_map(|p| p.bytes().chain([0])).collect::<Vec<u8>>();
            std::fs::write(dir.join("cmdline"), join(args)).unwrap();
            std::fs::write(dir.join("environ"), join(env)).unwrap();
        }
    }

    impl Drop for FakeProc {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn walks_past_the_cli_and_shells() {
        let p = FakeProc::new("walk");
        p.add(1, 0, 0, "/usr/lib/systemd/systemd", &[], &[]);
        p.add(10, 1, 1000, "/usr/lib/systemd/systemd", &["systemd", "--user"], &[]);
        p.add(20, 10, 1000, "/opt/agent/bin/agent", &["agent"], &["HELIX_AGENT_NAME=Example\u{7} Agent"]);
        p.add(30, 20, 1000, "/usr/bin/bash", &["bash", "-c", "helix-agent windows"], &[]);
        p.add(40, 30, 1000, "/usr/bin/helix-agent", &["helix-agent", "windows"], &[]);
        let agent = identify(&p.0, 40, 1000).unwrap();
        assert_eq!(agent.id, "/opt/agent/bin/agent");
        assert_eq!(agent.executable, "/opt/agent/bin/agent");
        assert_eq!(agent.name, "Example Agent", "from the agent's environment, control characters removed");
        assert!(agent.identified);
        assert_eq!((agent.pid, agent.caller_pid), (20, 40));
        assert!(!agent.ssh);

        // The caller's own name wins; SSH in either environment counts.
        p.add(41, 30, 1000, "/usr/bin/helix-agent", &["helix-agent"], &["HELIX_AGENT_NAME=Mine", "SSH_CONNECTION=1.2.3.4 5 6.7.8.9 22"]);
        let agent = identify(&p.0, 41, 1000).unwrap();
        assert_eq!(agent.name, "Mine");
        assert!(agent.ssh);

        // A direct D-Bus caller is the agent itself.
        assert_eq!(identify(&p.0, 20, 1000).unwrap().id, "/opt/agent/bin/agent");
        // Another user's caller, or one that is gone: nobody.
        assert_eq!(identify(&p.0, 40, 1001), None);
        assert_eq!(identify(&p.0, 99, 1000), None);
    }

    #[test]
    fn unidentified_when_only_wrappers() {
        let p = FakeProc::new("orphan");
        p.add(10, 1, 1000, "/usr/lib/systemd/systemd", &["systemd", "--user"], &[]);
        // systemd-run --user helix-agent …: the parent is the service manager.
        p.add(40, 10, 1000, "/usr/bin/helix-agent (deleted)", &["helix-agent"], &[]);
        let agent = identify(&p.0, 40, 1000).unwrap();
        assert_eq!(agent.id, "/usr/bin/helix-agent", "the caller, without \" (deleted)\"");
        assert!(!agent.identified);
        // A parent of another user ends the walk too.
        p.add(50, 1, 0, "/usr/bin/sudo", &[], &[]);
        p.add(60, 50, 1000, "/usr/bin/bash", &[], &[]);
        p.add(70, 60, 1000, "/usr/bin/helix-agent", &[], &[]);
        assert!(!identify(&p.0, 70, 1000).unwrap().identified);
    }

    #[test]
    fn launchers_keep_the_id_across_updates() {
        let p = FakeProc::new("launcher");
        let root = std::fs::canonicalize(&p.0).unwrap().join("home");
        let (versions, bin, other) = (root.join("versions"), root.join("bin"), root.join("other"));
        for d in [&versions, &bin, &other] {
            std::fs::create_dir_all(d).unwrap();
        }
        for v in ["1.0", "1.1"] {
            std::fs::write(versions.join(v), "").unwrap();
        }
        std::fs::write(other.join("tool"), "").unwrap();
        std::os::unix::fs::symlink(versions.join("1.1"), bin.join("claude")).unwrap();
        std::os::unix::fs::symlink(other.join("tool"), bin.join("tool")).unwrap();
        std::fs::write(bin.join("plain"), "").unwrap();
        let path = format!("PATH=/nowhere:{}", bin.display());
        let (v10, v11) = (versions.join("1.0").display().to_string(), versions.join("1.1").display().to_string());

        // Started as `claude` from PATH: the launcher is the id, for the
        // current version and for one still running after an update.
        for (pid, exe) in [(20, &v11), (30, &v10)] {
            p.add(pid, 1, 1000, exe, &["claude"], &[&path]);
            p.add(pid + 1, pid, 1000, "/usr/bin/helix-agent", &[], &[]);
            let agent = identify(&p.0, pid + 1, 1000).unwrap();
            assert_eq!(agent.id, bin.join("claude").display().to_string());
            assert_eq!(agent.executable, agent.id);
            assert!(agent.identified);
        }
        // By its path, too.
        p.add(40, 1, 1000, &v11, &[&bin.join("claude").display().to_string()], &[]);
        assert_eq!(identify(&p.0, 40, 1000).unwrap().id, bin.join("claude").display().to_string());

        // Not a launcher: a symlink to something else in another directory
        // (a claimed command), a plain file, or no such command.
        for (pid, command) in [(50, "tool"), (51, "plain"), (52, "missing")] {
            p.add(pid, 1, 1000, &v11, &[command], &[&path]);
            assert_eq!(identify(&p.0, pid, 1000).unwrap().id, v11, "{command}");
        }
    }

    #[test]
    fn interpreters_are_told_apart_by_script() {
        let p = FakeProc::new("interp");
        p.add(20, 1, 1000, "/usr/bin/python3.12", &["python3", "-u", "agent.py", "--fast"], &[]);
        p.add(21, 20, 1000, "/usr/bin/helix-agent", &[], &[]);
        let agent = identify(&p.0, 21, 1000).unwrap();
        assert_eq!(agent.id, "/usr/bin/python3.12 /home/me/work/agent.py");
        assert_eq!(agent.executable, "/usr/bin/python3.12");
        assert!(agent.identified);

        p.add(30, 1, 1000, "/usr/bin/node", &["node", "-e", "run()"], &[]);
        p.add(31, 30, 1000, "/usr/bin/helix-agent", &[], &[]);
        let agent = identify(&p.0, 31, 1000).unwrap();
        assert_eq!(agent.id, "/usr/bin/node", "inline code has no script");
        assert!(!agent.identified);
    }

    #[test]
    fn scripts_and_names() {
        let s = |args: &[&str]| script_of(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>(), Some(Path::new("/w")));
        assert_eq!(s(&["python3", "/abs/a.py"]).as_deref(), Some("/abs/a.py"));
        assert_eq!(s(&["python3", "-B", "rel/a.py", "x"]).as_deref(), Some("/w/rel/a.py"));
        assert_eq!(s(&["python3", "-m", "agent.main"]).as_deref(), Some("-m agent.main"));
        assert_eq!(s(&["python3", "-c", "print()"]), None);
        assert_eq!(s(&["python3"]), None);
        assert_eq!(s(&["perl", "-"]), None);
        for name in ["python", "python3", "python3.12", "node", "nodejs", "bun", "deno", "perl", "ruby", "php", "pypy3"] {
            assert!(is_interpreter(name), "{name}");
        }
        for name in ["python3-config", "python3.", "pythonista", "agent", "bash"] {
            assert!(!is_interpreter(name), "{name}");
        }
        let long = "x".repeat(100);
        assert_eq!(clean_name(&long).chars().count(), MAX_NAME);
        assert_eq!(clean_name(" A\nB\u{1b}[31m "), "AB[31m");
    }
}
