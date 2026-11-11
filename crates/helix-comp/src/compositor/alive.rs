//! Unresponsive apps (helix-desktop-spec `contracts/wayland/README.md`,
//! "Unresponsive apps"): ping an app (Wayland, or X11 with `_NET_WM_PING`)
//! when its window gets focus,
//! and have the shell ask whether to wait or force it to quit when it does
//! not answer within `check-alive-timeout`.

use std::time::{Duration, Instant};

use smithay::desktop::Window;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{IsAlive, SERIAL_COUNTER};
use smithay::wayland::shell::xdg::ShellClient;

use super::{ClientState, Helix};

/// Who was pinged.
#[derive(PartialEq)]
enum Target {
    Wayland(ShellClient),
    /// An X11 window (its id).
    X11(u32),
}

struct Ping {
    target: Target,
    window: Window,
    /// When the ping went out (or the user chose to wait).
    since: Instant,
    /// The shell was told the app is not responding.
    told: bool,
}

#[derive(Default)]
pub struct AliveState {
    pings: Vec<Ping>,
}

impl Helix {
    /// `window` got focus: check its app answers.
    pub fn check_alive(&mut self, window: &Window) {
        if self.wm_settings.check_alive_ms == 0 {
            return;
        }
        let target = match (window.toplevel(), window.x11_surface()) {
            (Some(toplevel), _) => Target::Wayland(toplevel.client()),
            (None, Some(x11)) => Target::X11(x11.window_id()),
            _ => return,
        };
        if self.alive.pings.iter().any(|p| p.target == target) {
            return;
        }
        let sent = match &target {
            Target::Wayland(client) => client.send_ping(SERIAL_COUNTER.next_serial()).is_ok(),
            Target::X11(id) => self.x11ping.as_ref().is_some_and(|p| p.ping(*id)),
        };
        if sent {
            self.alive.pings.push(Ping { target, window: window.clone(), since: Instant::now(), told: false });
        }
    }

    /// An X11 window answered `_NET_WM_PING`.
    pub fn x11_pong(&mut self, id: u32) {
        self.answered(|t| *t == Target::X11(id));
    }

    /// An app answered.
    pub fn pong(&mut self, client: &ShellClient) {
        self.answered(|t| matches!(t, Target::Wayland(c) if c == client));
    }

    fn answered(&mut self, matches: impl Fn(&Target) -> bool) {
        let answered: Vec<Ping> = {
            let (answered, rest) = std::mem::take(&mut self.alive.pings).into_iter().partition(|p| matches(&p.target));
            self.alive.pings = rest;
            answered
        };
        for ping in answered.into_iter().filter(|p| p.told) {
            self.tell_not_responding(&ping.window, true);
        }
    }

    /// Every tick: tell the shell about apps that did not answer in time.
    pub fn alive_tick(&mut self, now: Instant) {
        let timeout = Duration::from_millis(u64::from(self.wm_settings.check_alive_ms));
        self.alive.pings.retain(|p| match &p.target {
            Target::Wayland(client) => client.alive(),
            Target::X11(_) => p.window.alive(),
        });
        let late: Vec<Window> = self
            .alive
            .pings
            .iter_mut()
            .filter(|p| !p.told && !timeout.is_zero() && now.duration_since(p.since) >= timeout)
            .map(|p| {
                p.told = true;
                p.window.clone()
            })
            .collect();
        for window in late {
            tracing::info!("an app is not responding");
            self.tell_not_responding(&window, false);
        }
    }

    fn tell_not_responding(&mut self, window: &Window, responding: bool) {
        self.sync_window_list();
        let Some(id) = self.window_list.ids.iter().find(|(w, _)| w == window).map(|(_, id)| *id) else { return };
        for listener in self.window_list.listeners.iter().filter(|l| l.version() >= 5) {
            listener.not_responding(id, u32::from(responding));
        }
    }

    /// "Wait": ask again after another timeout if it still does not answer.
    pub fn wait_for(&mut self, window: &Window) {
        for ping in self.alive.pings.iter_mut().filter(|p| p.window == *window) {
            ping.since = Instant::now();
            ping.told = false;
        }
    }

    /// "Force Quit": kill the app's process.
    pub fn force_quit(&mut self, window: &Window) {
        if let Some(x11) = window.x11_surface() {
            if let Some(ping) = &self.x11ping {
                tracing::info!("force quitting an unresponsive X11 app");
                ping.kill(x11.window_id());
            }
            self.alive.pings.retain(|p| p.window != *window);
            return;
        }
        let Some(toplevel) = window.toplevel() else { return };
        let Some(client) = toplevel.wl_surface().client() else { return };
        // Never the shell (or anything Helix started as privileged).
        if client.get_data::<ClientState>().is_some_and(|c| c.privileged) {
            return;
        }
        match client.get_credentials(&self.display_handle) {
            Ok(credentials) if credentials.pid > 0 && credentials.pid as u32 != std::process::id() => {
                tracing::info!(pid = credentials.pid, "force quitting an unresponsive app");
                // SAFETY: plain kill(2) on a process id.
                unsafe {
                    libc::kill(credentials.pid, libc::SIGKILL);
                }
            }
            _ => {
                // No process to kill: drop its connection instead.
                let _ = toplevel.client().unresponsive();
            }
        }
        self.alive.pings.retain(|p| p.window != *window);
    }
}
