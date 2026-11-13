//! `helix-extensions`: list, turn on and off, configure, install and
//! develop Helix extensions (helix-desktop-spec
//! `contracts/extensions/README.md`, "helix-extensions").

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use helix_shell::ext_host::store::{self, Kind};
use zbus::zvariant::OwnedValue;
use zbus::Connection;

const USAGE: &str = "usage: helix-extensions COMMAND
  list                    the extensions and their states
  info ID                 one extension
  enable [--yes] ID       turn it on, approving what it asks to do
  disable ID              turn it off
  restart ID              start it again
  logs ID                 its recent log
  set ID KEY VALUE        change a setting
  install PATH            install a directory or a .tar(.gz/.xz) archive
  remove ID               remove a per-user extension
  run PATH                run a directory as a development extension";

const BUS: &str = "org.gnome.Shell";
const PATH: &str = "/org/helix/Extensions1";
const IFACE: &str = "org.helix.Extensions1";

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
        let permissions = list(e, "permissions");
        if !permissions.is_empty() {
            let can: Vec<&str> = permissions.iter().map(|p| helix_shell::ext_host::manifest::permission_text(p)).collect();
            println!("  can: {}{}", can.join(", "), if flag(e, "approved") { "" } else { " (not approved)" });
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

fn install(path: &Path) -> Result<String, String> {
    let scratch = std::env::temp_dir().join(format!("helix-extensions-install-{}", std::process::id()));
    let source: PathBuf = if path.is_dir() {
        path.to_owned()
    } else {
        std::fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
        let status = std::process::Command::new("tar").arg("-xf").arg(path).arg("-C").arg(&scratch).arg("--no-same-owner").status().map_err(|e| format!("cannot run tar: {e}"))?;
        if !status.success() {
            return Err(format!("cannot unpack {}", path.display()));
        }
        let dirs: Vec<PathBuf> = std::fs::read_dir(&scratch).map_err(|e| e.to_string())?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        match dirs.as_slice() {
            [one] => one.clone(),
            _ => return Err("the archive must hold exactly one directory".into()),
        }
    };
    let found = store::load(&source, Kind::Development);
    let result = (|| {
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
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    result
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
            let yes = args.iter().any(|a| a == "--yes" || a == "-y");
            let id = args.iter().skip(1).find(|a| !a.starts_with('-')).cloned().ok_or(USAGE)?;
            let entries = entries().await?;
            let e = entries.get(&id).ok_or_else(|| format!("no extension {id}"))?;
            let permissions = list(e, "permissions");
            if !permissions.is_empty() && !yes {
                let can: Vec<&str> = permissions.iter().map(|p| helix_shell::ext_host::manifest::permission_text(p)).collect();
                print!("{} asks to: {}. Allow? [y/N] ", text(e, "name"), can.join(", "));
                std::io::stdout().flush().ok();
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer).map_err(|e| e.to_string())?;
                if !matches!(answer.trim(), "y" | "Y" | "yes") {
                    return Err("not enabled".into());
                }
            }
            call::<_, ()>(&shell().await?, "Enable", &(id.as_str(), true)).await?;
            println!("{id} enabled");
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
