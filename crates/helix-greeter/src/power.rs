//! Restart and Power Off through logind (helix-desktop-spec
//! `components/greeter.md`, "Power").

const LOGIND: &str = "org.freedesktop.login1";
const LOGIND_PATH: &str = "/org/freedesktop/login1";
const LOGIND_MANAGER: &str = "org.freedesktop.login1.Manager";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    Restart,
    PowerOff,
}

impl PowerAction {
    fn methods(self) -> (&'static str, &'static str) {
        match self {
            Self::Restart => ("CanReboot", "Reboot"),
            Self::PowerOff => ("CanPowerOff", "PowerOff"),
        }
    }
}

/// Which actions logind offers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Offered {
    pub restart: bool,
    pub power_off: bool,
}

/// A `Can…` answer that allows the action (perhaps after asking polkit).
pub fn allowed(answer: &str) -> bool {
    matches!(answer, "yes" | "challenge")
}

async fn can(system: &zbus::Connection, action: PowerAction) -> bool {
    let reply = system.call_method(Some(LOGIND), LOGIND_PATH, Some(LOGIND_MANAGER), action.methods().0, &()).await;
    match reply.and_then(|r| r.body().deserialize::<String>()) {
        Ok(answer) => allowed(&answer),
        Err(err) => {
            tracing::info!(%err, ?action, "logind does not offer it");
            false
        }
    }
}

pub async fn offered(system: &zbus::Connection) -> Offered {
    Offered { restart: can(system, PowerAction::Restart).await, power_off: can(system, PowerAction::PowerOff).await }
}

/// Ask logind to restart or power off (interactive: polkit may ask).
pub async fn act(system: &zbus::Connection, action: PowerAction) -> zbus::Result<()> {
    system.call_method(Some(LOGIND), LOGIND_PATH, Some(LOGIND_MANAGER), action.methods().1, &(true,)).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers() {
        assert!(allowed("yes"));
        assert!(allowed("challenge"));
        assert!(!allowed("no"));
        assert!(!allowed("na"));
    }

    /// Read-only, against the host's logind; skipped without a system bus.
    #[test]
    fn host_logind() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let Ok(system) = zbus::Connection::system().await else {
                eprintln!("no system bus; skipping");
                return;
            };
            let _ = offered(&system).await;
        });
    }
}
