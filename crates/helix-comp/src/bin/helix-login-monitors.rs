//! helix-login-monitors: make a monitor layout the login screen's.
//!
//! Installed as `/usr/libexec/helix/helix-login-monitors` and run as root
//! through `pkexec` (polkit action `org.helix.login-screen-monitors`):
//! reads one monitor configuration on standard input (the format of
//! `~/.config/helix/monitors.toml`, one `[[configuration]]` for the
//! monitors connected now), checks it, and writes it into
//! `/etc/xdg/helix/monitors.toml` (see `helix_comp::login_monitors`).
//! Takes no arguments.
//!
//! Exit status: 0 written, 1 invalid input, 2 the file could not be
//! written, 64 usage.

use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

use helix_comp::login_monitors::{install, Error, MAX_INPUT, SYSTEM_FILE};

fn target() -> PathBuf {
    // Tests only: another root directory, honoured when not running as
    // root (so never under pkexec, which also clears the environment).
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        if let Some(root) = std::env::var_os("HELIX_LOGIN_MONITORS_TEST_ROOT").filter(|r| !r.is_empty()) {
            return PathBuf::from(root).join(SYSTEM_FILE.trim_start_matches('/'));
        }
    }
    PathBuf::from(SYSTEM_FILE)
}

fn main() -> ExitCode {
    if std::env::args_os().len() > 1 {
        eprintln!("usage: helix-login-monitors < configuration\n\nPuts the monitor configuration on standard input into {SYSTEM_FILE}, for the login screen.");
        return ExitCode::from(64);
    }
    let mut input = Vec::new();
    // One byte more than allowed, so that too long is noticed.
    if let Err(err) = std::io::stdin().lock().take(MAX_INPUT as u64 + 1).read_to_end(&mut input) {
        eprintln!("helix-login-monitors: cannot read the configuration: {err}");
        return ExitCode::from(1);
    }
    let target = target();
    match install(&input, &target) {
        Ok(()) => {
            eprintln!("helix-login-monitors: {} updated", target.display());
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("helix-login-monitors: {err}");
            ExitCode::from(match err {
                Error::Invalid(_) => 1,
                Error::Io(_) => 2,
            })
        }
    }
}
