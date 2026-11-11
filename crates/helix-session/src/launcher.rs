//! The simple session launcher.
//!
//! Reads a TOML config naming the session's components, starts them in
//! order (waiting for each to become ready), propagates environment to
//! children and to D-Bus activation, then marks the session initialized.
//!
//! ```toml
//! [session]
//! name = "helix"
//!
//! [environment]
//! XDG_CURRENT_DESKTOP = "Helix:GNOME"
//!
//! [[component]]
//! name = "compositor"
//! command = ["helix-comp"]
//! required = true
//! wait_for_env = "WAYLAND_DISPLAY"   # ready once it calls Setenv for this
//!
//! [[component]]
//! name = "portals"
//! command = ["helix-portals"]
//! wait_for_name = "org.freedesktop.impl.portal.desktop.helix"
//! ```

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::process::{Child, Command};
use zbus::fdo::DBusProxy;

use crate::SessionService;

/// Parsed session config.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionConfig {
    #[serde(default)]
    pub session: SessionSection,
    /// Set for every component and for D-Bus activated services.
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    /// Started in order.
    #[serde(default, rename = "component")]
    pub components: Vec<Component>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionSection {
    /// `SessionName`.
    #[serde(default = "default_name")]
    pub name: String,
}

impl Default for SessionSection {
    fn default() -> Self {
        Self {
            name: default_name(),
        }
    }
}

fn default_name() -> String {
    "helix".to_owned()
}

fn default_timeout() -> u64 {
    10
}

/// One process the session starts.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Component {
    pub name: String,
    /// Program and arguments.
    pub command: Vec<String>,
    /// If it fails to start or exits, the session ends.
    #[serde(default)]
    pub required: bool,
    /// Ready once this bus name has an owner.
    pub wait_for_name: Option<String>,
    /// Ready once this variable was set through `Setenv`.
    pub wait_for_env: Option<String>,
    /// How long to wait for readiness.
    #[serde(default = "default_timeout")]
    pub ready_timeout_secs: u64,
}

#[derive(Debug)]
pub enum LaunchError {
    Config(PathBuf, String),
    Spawn(String, std::io::Error),
    NotReady(String),
    ExitedEarly(String, std::process::ExitStatus),
    Bus(zbus::Error),
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(p, e) => write!(f, "invalid session config {}: {e}", p.display()),
            Self::Spawn(n, e) => write!(f, "cannot start component {n}: {e}"),
            Self::NotReady(n) => write!(f, "component {n} did not become ready in time"),
            Self::ExitedEarly(n, s) => write!(f, "component {n} exited during startup ({s})"),
            Self::Bus(e) => write!(f, "D-Bus error during launch: {e}"),
        }
    }
}

impl std::error::Error for LaunchError {}

impl From<zbus::Error> for LaunchError {
    fn from(e: zbus::Error) -> Self {
        Self::Bus(e)
    }
}

impl From<zbus::fdo::Error> for LaunchError {
    fn from(e: zbus::fdo::Error) -> Self {
        Self::Bus(e.into())
    }
}

impl SessionConfig {
    pub fn parse(text: &str) -> Result<Self, String> {
        let config: Self = toml::from_str(text).map_err(|e| e.to_string())?;
        for c in &config.components {
            if c.command.is_empty() {
                return Err(format!("component {} has an empty command", c.name));
            }
        }
        Ok(config)
    }

    pub fn load(path: &Path) -> Result<Self, LaunchError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| LaunchError::Config(path.to_owned(), e.to_string()))?;
        Self::parse(&text).map_err(|e| LaunchError::Config(path.to_owned(), e))
    }

    /// `$XDG_CONFIG_HOME/helix/session.toml`, then each
    /// `$XDG_CONFIG_DIRS/helix/session.toml` (default `/etc/xdg`).
    pub fn find() -> Option<PathBuf> {
        let home = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
        let dirs = std::env::var("XDG_CONFIG_DIRS")
            .ok()
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| "/etc/xdg".to_owned());
        home.into_iter()
            .chain(dirs.split(':').map(PathBuf::from))
            .map(|d| d.join("helix/session.toml"))
            .find(|p| p.is_file())
    }
}

/// Running components, in start order.
pub struct Children {
    procs: Vec<(Component, Child)>,
}

impl Children {
    /// Resolves with the name of the first required component that exits.
    /// Never resolves if there are none.
    pub async fn required_exited(&mut self) -> String {
        loop {
            for (component, child) in &mut self.procs {
                if component.required {
                    if let Ok(Some(status)) = child.try_wait() {
                        tracing::error!(component = %component.name, %status, "required component exited");
                        return component.name.clone();
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Process ids of components still running, by name.
    pub fn pids(&self) -> Vec<(String, u32)> {
        self.procs
            .iter()
            .filter_map(|(c, child)| child.id().map(|pid| (c.name.clone(), pid)))
            .collect()
    }

    /// SIGTERM every component (last started first), then SIGKILL whatever
    /// is still running after `grace`.
    pub async fn terminate(mut self, grace: Duration) {
        for (component, child) in self.procs.iter().rev() {
            if let Some(pid) = child.id() {
                tracing::info!(component = %component.name, pid, "stopping component");
                // SAFETY: plain kill(2) on a child we spawned and have not reaped.
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGTERM);
                }
            }
        }
        let deadline = Instant::now() + grace;
        for (component, child) in self.procs.iter_mut().rev() {
            let left = deadline.saturating_duration_since(Instant::now());
            if tokio::time::timeout(left, child.wait()).await.is_err() {
                tracing::warn!(component = %component.name, "did not stop in time; killing");
                let _ = child.kill().await;
            }
        }
    }
}

/// Start every component of `config` and mark the session initialized.
///
/// `extra_env` is applied to children on top of the inherited environment
/// (tests use it to point children at a private bus).
pub async fn launch(
    service: &SessionService,
    config: &SessionConfig,
    extra_env: &BTreeMap<String, String>,
) -> Result<Children, LaunchError> {
    let dbus = DBusProxy::new(service.connection()).await?;
    update_activation_env(&dbus, &config.environment).await;

    let mut children = Children { procs: Vec::new() };
    for component in &config.components {
        match start_component(service, &dbus, config, extra_env, component).await {
            Ok(child) => children.procs.push((component.clone(), child)),
            Err(err) if component.required => {
                children.terminate(Duration::from_secs(5)).await;
                return Err(err);
            }
            Err(err) => tracing::warn!(%err, "optional component failed to start"),
        }
        update_activation_env(&dbus, &service.environment()).await;
    }

    service.mark_initialized().await?;
    Ok(children)
}

async fn start_component(
    service: &SessionService,
    dbus: &DBusProxy<'_>,
    config: &SessionConfig,
    extra_env: &BTreeMap<String, String>,
    component: &Component,
) -> Result<Child, LaunchError> {
    let mut cmd = Command::new(&component.command[0]);
    cmd.args(&component.command[1..])
        .envs(extra_env)
        .envs(&config.environment)
        .envs(service.environment())
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|e| LaunchError::Spawn(component.name.clone(), e))?;
    tracing::info!(component = %component.name, pid = child.id(), "started component");

    let deadline = Instant::now() + Duration::from_secs(component.ready_timeout_secs);
    loop {
        if is_ready(service, dbus, component).await? {
            tracing::info!(component = %component.name, "component ready");
            return Ok(child);
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(LaunchError::ExitedEarly(component.name.clone(), status));
        }
        if Instant::now() >= deadline {
            let _ = child.kill().await;
            return Err(LaunchError::NotReady(component.name.clone()));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn is_ready(
    service: &SessionService,
    dbus: &DBusProxy<'_>,
    component: &Component,
) -> Result<bool, LaunchError> {
    if let Some(var) = &component.wait_for_env {
        if !service.environment().contains_key(var) {
            return Ok(false);
        }
    }
    if let Some(name) = &component.wait_for_name {
        let name = name
            .as_str()
            .try_into()
            .map_err(|e: zbus::names::Error| LaunchError::Bus(e.into()))?;
        if !dbus.name_has_owner(name).await? {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn update_activation_env(dbus: &DBusProxy<'_>, env: &BTreeMap<String, String>) {
    if env.is_empty() {
        return;
    }
    let env: HashMap<&str, &str> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    if let Err(err) = dbus.update_activation_environment(env).await {
        tracing::warn!(%err, "could not update the D-Bus activation environment");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_config() {
        let config = SessionConfig::parse(
            r#"
            [session]
            name = "test"
            [environment]
            XDG_CURRENT_DESKTOP = "Helix:GNOME"
            [[component]]
            name = "comp"
            command = ["helix-comp", "--flag"]
            required = true
            wait_for_env = "WAYLAND_DISPLAY"
            [[component]]
            name = "portals"
            command = ["helix-portals"]
            wait_for_name = "org.freedesktop.impl.portal.desktop.helix"
            ready_timeout_secs = 3
            "#,
        )
        .unwrap();
        assert_eq!(config.session.name, "test");
        assert_eq!(config.environment["XDG_CURRENT_DESKTOP"], "Helix:GNOME");
        assert_eq!(config.components.len(), 2);
        assert!(config.components[0].required);
        assert_eq!(config.components[0].ready_timeout_secs, 10);
        assert!(!config.components[1].required);
        assert_eq!(config.components[1].ready_timeout_secs, 3);
    }

    #[test]
    fn rejects_bad_configs() {
        assert!(SessionConfig::parse("[[component]]\nname = \"x\"\ncommand = []").is_err());
        assert!(SessionConfig::parse("[[component]]\nname = \"x\"\ncommand = [\"a\"]\ntypo = 1").is_err());
        assert_eq!(SessionConfig::parse("").unwrap(), SessionConfig::default());
    }
}
