//! `helix-extensions`: list, turn on and off, configure, install and
//! develop Helix extensions (helixdesktop/spec
//! `contracts/extensions/README.md`, "helix-extensions").

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use helix_shell::ext_host::store::{self, Kind};
use zbus::zvariant::OwnedValue;
use zbus::Connection;

const USAGE: &str = "usage: helix-extensions COMMAND
  list                    the extensions and their states
  info ID                 one extension
  enable ID               turn it on (the shell asks you to allow what it asks to do)
  disable ID              turn it off
  restart ID              start it again
  logs ID                 its recent log
  set ID KEY VALUE        change a setting
  command EXT/CMD         run a command
  bind EXT/CMD KEYS|none  set or clear a command's key, e.g. <Super><Shift>c
  install PATH            install a directory or a .tar(.gz/.xz) archive
  remove ID               remove a per-user extension
  run PATH                run a directory as a development extension";

const BUS: &str = "org.gnome.Shell";
const PATH: &str = "/org/helixdesktop/Extensions1";
const IFACE: &str = "org.helixdesktop.Extensions1";

type Entries = HashMap<String, HashMap<String, OwnedValue>>;

fn text(e: &HashMap<String, OwnedValue>, k: &str) -> String {
    e.get(k).and_then(|v| String::try_from(v.clone()).ok()).unwrap_or_default()
}

fn flag(e: &HashMap<String, OwnedValue>, k: &str) -> bool {
    e.get(k).and_then(|v| bool::try_from(v.clone()).ok()).unwrap_or(false)
}

fn list(e: &HashMap<String, OwnedValue>, k: &str) -> Vec<String> {
    e.get(k).and_then(|v| <Vec<String>>::try_from(v.clone()).ok()).unwrap_or_default()
}

async fn shell() -> Result<Connection, String> {
    let conn = Connection::session().await.map_err(|e| format!("no session bus: {e}"))?;
    Ok(conn)
}

async fn call<B, R>(conn: &Connection, method: &str, body: &B) -> Result<R, String>
where
    B: serde::ser::Serialize + zbus::zvariant::DynamicType,
    R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
{
    let reply = conn.call_method(Some(BUS), PATH, Some(IFACE), method, body).await.map_err(|e| match e {
        zbus::Error::MethodError(_, Some(msg), _) => msg,
        other => format!("the Helix shell did not answer ({other})"),
    })?;
    reply.body().deserialize().map_err(|e| e.to_string())
}

async fn entries() -> Result<Entries, String> {
    let conn = shell().await?;
    let (entries,): (Entries,) = call(&conn, "ListExtensions", &()).await?;
    Ok(entries)
}

/// Without a running shell: what the directories hold.
fn offline() -> Entries {
    let value = |s: String| OwnedValue::try_from(zbus::zvariant::Value::from(s)).expect("text");
    store::discover(&store::user_dir(), &store::system_dirs())
        .into_values()
        .map(|f| {
            let mut e: HashMap<String, OwnedValue> = HashMap::new();
            match &f.manifest {
                Ok(m) => {
                    e.insert("name".into(), value(m.name.clone()));
                    e.insert("version".into(), value(m.version.clone()));
                    e.insert("description".into(), value(m.description.clone()));
                    e.insert("state".into(), value("unknown (the Helix shell is not running)".into()));
                }
                Err(reason) => {
                    e.insert("state".into(), value("invalid".into()));
                    e.insert("error".into(), value(reason.clone()));
                }
            }
            e.insert("path".into(), value(f.dir.to_string_lossy().into_owned()));
            e.insert("type".into(), value(f.kind.name().into()));
            (f.id, e)
        })
        .collect()
}

fn print_entry(id: &str, e: &HashMap<String, OwnedValue>, full: bool) {
    let version = text(e, "version");
    println!("{id}  {}{}  [{}]", text(e, "name"), if version.is_empty() { String::new() } else { format!(" {version}") }, text(e, "state"));
    if full {
        let description = text(e, "description");
        if !description.is_empty() {
            println!("  {description}");
        }
        println!("  type: {}, path: {}", text(e, "type"), text(e, "path"));
        println!(
            "  {}",
            match (flag(e, "sandboxed"), flag(e, "leaves-sandbox")) {
                (true, false) => "sandboxed",
                (true, true) => "sandboxed, but its permissions let it out of its sandbox (full access)",
                (false, _) => "full access (not sandboxed)",
            }
        );
        let permissions = list(e, "permissions");
        if !permissions.is_empty() {
            let can: Vec<String> = permissions.iter().map(|p| helix_shell::ext_host::manifest::permission_text(p)).collect();
            println!("  can: {}{}", can.join(", "), if flag(e, "approved") { "" } else { " (not approved)" });
        }
        if let Some(commands) = e.get("commands").and_then(|v| <Vec<(String, String, String)>>::try_from(v.clone()).ok()) {
            for (id, title, keys) in commands {
                println!("  command {id}: {title} [{}]", if keys.is_empty() { "no key" } else { keys.as_str() });
            }
        }
        let error = text(e, "error");
        if !error.is_empty() {
            println!("  {}: {error}", if text(e, "state") == "running" { "last log line" } else { "reason" });
        }
    }
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target)?;
            std::fs::set_permissions(&target, entry.metadata()?.permissions())?;
        }
        // Symbolic links are not followed or copied.
    }
    Ok(())
}

/// A new directory only this user can enter, with a name nobody can
/// guess or create first (`mkdtemp`: mode 0700, fails rather than reuse).
fn private_dir() -> std::io::Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|d| d.is_dir()).unwrap_or_else(std::env::temp_dir);
    let mut template = base.join("helix-extensions-install-XXXXXX").into_os_string().into_vec();
    template.push(0);
    // SAFETY: a writable NUL-terminated template, as mkdtemp wants.
    if unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) }.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    template.pop();
    Ok(PathBuf::from(std::ffi::OsString::from_vec(template)))
}

/// Removes the unpacking directory however `install` ends.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn install(path: &Path) -> Result<String, String> {
    let mut _scratch = None;
    let source: PathBuf = if path.is_dir() {
        path.to_owned()
    } else {
        let scratch = private_dir().map_err(|e| format!("cannot make a directory to unpack into: {e}"))?;
        _scratch = Some(Scratch(scratch.clone()));
        let status = std::process::Command::new("tar").arg("-xf").arg(path).arg("-C").arg(&scratch).arg("--no-same-owner").status().map_err(|e| format!("cannot run tar: {e}"))?;
        if !status.success() {
            return Err(format!("cannot unpack {}", path.display()));
        }
        let dirs: Vec<PathBuf> = std::fs::read_dir(&scratch).map_err(|e| e.to_string())?.flatten().map(|e| e.path()).filter(|p| p.is_dir() && !p.is_symlink()).collect();
        match dirs.as_slice() {
            [one] => one.clone(),
            _ => return Err("the archive must hold exactly one directory".into()),
        }
    };
    let found = store::load(&source, Kind::Development);
    (|| {
        let m = found.manifest.map_err(|e| format!("not a valid extension: {e}"))?;
        let dest = store::user_dir().join(&m.id);
        let staging = store::user_dir().join(format!(".{}.new", m.id));
        let _ = std::fs::remove_dir_all(&staging);
        copy_dir(&source, &staging).map_err(|e| e.to_string())?;
        if dest.exists() {
            std::fs::remove_dir_all(&dest).map_err(|e| e.to_string())?;
        }
        std::fs::rename(&staging, &dest).map_err(|e| e.to_string())?;
        Ok(m.id)
    })()
}

async fn run(args: &[String]) -> Result<(), String> {
    let arg = |i: usize| args.get(i).cloned().ok_or_else(|| USAGE.to_owned());
    match args.first().map(String::as_str) {
        Some("list") => {
            let entries = entries().await.unwrap_or_else(|err| {
                eprintln!("{err}; listing the installed directories");
                offline()
            });
            let mut ids: Vec<&String> = entries.keys().collect();
            ids.sort();
            if ids.is_empty() {
                println!("No extensions are installed.");
            }
            for id in ids {
                print_entry(id, &entries[id], false);
            }
        }
        Some("info") => {
            let id = arg(1)?;
            let entries = entries().await.unwrap_or_else(|_| offline());
            let e = entries.get(&id).ok_or_else(|| format!("no extension {id}"))?;
            print_entry(&id, e, true);
        }
        Some("enable") => {
            // The shell asks the user before approving anything; `--yes`
            // is accepted for older scripts and ignored.
            let id = args.iter().skip(1).find(|a| !a.starts_with('-')).cloned().ok_or(USAGE)?;
            match call::<_, ()>(&shell().await?, "Enable", &(id.as_str(), true)).await {
                Ok(()) => println!("{id} enabled"),
                Err(err) if err.contains("AccessDenied") || err.contains("not approved") => return Err(format!("{id} was not turned on: not allowed")),
                Err(err) => return Err(err),
            }
        }
        Some("disable") => call::<_, ()>(&shell().await?, "Disable", &(arg(1)?,)).await?,
        Some("restart") => call::<_, ()>(&shell().await?, "Restart", &(arg(1)?,)).await?,
        Some("logs") => {
            let (lines,): (Vec<String>,) = call(&shell().await?, "GetLog", &(arg(1)?,)).await?;
            for line in lines {
                println!("{line}");
            }
        }
        Some("set") => {
            let (id, key, value) = (arg(1)?, arg(2)?, arg(3)?);
            let found = store::discover(&store::user_dir(), &store::system_dirs()).remove(&id).ok_or_else(|| format!("no extension {id}"))?;
            let m = found.manifest.map_err(|e| format!("{id} is invalid: {e}"))?;
            let decl = m.settings.iter().find(|d| d.key == key).ok_or_else(|| format!("{id} has no setting {key}"))?;
            let parsed = decl.from_text(&value)?;
            call::<_, ()>(&shell().await?, "SetSetting", &(id.as_str(), key.as_str(), zbus::zvariant::Value::from(parsed.to_value()))).await?;
        }
        Some("command") => call::<_, ()>(&shell().await?, "RunCommand", &(arg(1)?,)).await?,
        Some("bind") => {
            let (command, keys) = (arg(1)?, arg(2)?);
            let keys = if keys == "none" { String::new() } else { keys };
            call::<_, ()>(&shell().await?, "BindCommand", &(command.as_str(), keys.as_str())).await?;
        }
        Some("install") => {
            let id = install(Path::new(&arg(1)?))?;
            println!("installed {id}; turn it on with: helix-extensions enable {id}");
        }
        Some("remove") => {
            let id = arg(1)?;
            if !helix_shell::ext_host::manifest::valid_id(&id) {
                return Err(format!("{id:?} is not an extension id"));
            }
            let dir = store::user_dir().join(&id);
            if !dir.join("extension.toml").is_file() {
                return Err(format!("{id} is not a per-user extension"));
            }
            if let Ok(conn) = shell().await {
                let _ = call::<_, ()>(&conn, "Disable", &(id.as_str(),)).await;
            }
            std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
            println!("removed {id}");
        }
        Some("run") => {
            let path = std::fs::canonicalize(arg(1)?).map_err(|e| e.to_string())?;
            let conn = shell().await?;
            let (id,): (String,) = call(&conn, "RunDevelopment", &(path.to_string_lossy().as_ref(),)).await?;
            eprintln!("running {id} from {} (Ctrl+C stops it)", path.display());
            let mut shown: Vec<String> = Vec::new();
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        let _ = call::<_, ()>(&conn, "StopDevelopment", &(id.as_str(),)).await;
                        return Ok(());
                    }
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {
                        let Ok((lines,)) = call::<_, (Vec<String>,)>(&conn, "GetLog", &(id.as_str(),)).await else { continue };
                        // Print what is new since last time.
                        let start = (0..=lines.len()).rev().find(|&k| shown.len() >= k && lines[..k] == shown[shown.len() - k..]).unwrap_or(0);
                        for line in &lines[start..] {
                            println!("{line}");
                        }
                        shown = lines;
                    }
                }
            }
        }
        _ => return Err(USAGE.to_owned()),
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(err) = run(&args).await {
        eprintln!("{err}");
        std::process::exit(1);
    }
}
