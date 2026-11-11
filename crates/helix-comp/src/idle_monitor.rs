//! `org.gnome.Mutter.IdleMonitor` D-Bus interface over [`IdleCore`].
//!
//! Contract: `helix-desktop-spec/contracts/dbus/org.gnome.Mutter.IdleMonitor.xml`
//! and `org.gnome.Mutter.IdleMonitor.md`.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use tokio::sync::Notify;
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::{fdo, interface};

use crate::idle::IdleCore;

pub const BUS_NAME: &str = "org.gnome.Mutter.IdleMonitor";
/// Where the object manager lives.
pub const MANAGER_PATH: &str = "/org/gnome/Mutter/IdleMonitor";
/// The monitor covering all input devices.
pub const CORE_PATH: &str = "/org/gnome/Mutter/IdleMonitor/Core";

/// Monotonic milliseconds since service start.
#[derive(Debug, Clone, Copy)]
pub struct Clock {
    base: Instant,
}

impl Clock {
    pub fn new() -> Self {
        Self {
            base: Instant::now(),
        }
    }

    pub fn now(&self) -> u64 {
        self.base.elapsed().as_millis() as u64
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared between the D-Bus object and the timer task.
#[derive(Debug, Clone)]
pub struct Shared {
    pub core: Arc<Mutex<IdleCore>>,
    pub clock: Clock,
    /// Wakes the timer task when watches change.
    pub rearm: Arc<Notify>,
}

impl Shared {
    pub fn new() -> Self {
        let clock = Clock::new();
        Self {
            core: Arc::new(Mutex::new(IdleCore::new(clock.now()))),
            clock,
            rearm: Arc::new(Notify::new()),
        }
    }

    pub fn core(&self) -> MutexGuard<'_, IdleCore> {
        self.core.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record activity and emit `WatchFired` for user-active watches.
    pub async fn activity(&self, emitter: &SignalEmitter<'_>) -> zbus::Result<()> {
        let fired = self.core().activity(self.clock.now());
        self.rearm.notify_one();
        for id in fired {
            IdleMonitor::watch_fired(emitter, id).await?;
        }
        Ok(())
    }

    /// Fire due idle watches.
    pub async fn poll(&self, emitter: &SignalEmitter<'_>) -> zbus::Result<()> {
        let fired = self.core().poll(self.clock.now());
        for id in fired {
            IdleMonitor::watch_fired(emitter, id).await?;
        }
        Ok(())
    }
}

impl Default for Shared {
    fn default() -> Self {
        Self::new()
    }
}

pub struct IdleMonitor {
    shared: Shared,
}

impl IdleMonitor {
    pub fn new(shared: Shared) -> Self {
        Self { shared }
    }
}

fn sender(header: &Header<'_>) -> Option<String> {
    header.sender().map(|s| s.to_string())
}

#[interface(name = "org.gnome.Mutter.IdleMonitor")]
impl IdleMonitor {
    async fn get_idletime(&self) -> u64 {
        self.shared.core().idletime(self.shared.clock.now())
    }

    async fn add_idle_watch(
        &self,
        interval: u64,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<u32> {
        let id = self
            .shared
            .core()
            .add_idle_watch(interval, sender(&header).as_deref())
            .map_err(|e| fdo::Error::InvalidArgs(e.to_string()))?;
        self.shared.rearm.notify_one();
        Ok(id)
    }

    async fn add_user_active_watch(&self, #[zbus(header)] header: Header<'_>) -> u32 {
        self.shared
            .core()
            .add_user_active_watch(sender(&header).as_deref())
    }

    async fn remove_watch(&self, id: u32) -> fdo::Result<()> {
        self.shared
            .core()
            .remove_watch(id)
            .map_err(|e| fdo::Error::InvalidArgs(e.to_string()))?;
        self.shared.rearm.notify_one();
        Ok(())
    }

    async fn reset_idletime(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        self.shared.activity(&emitter).await?;
        Ok(())
    }

    #[zbus(signal)]
    pub async fn watch_fired(emitter: &SignalEmitter<'_>, id: u32) -> zbus::Result<()>;
}
