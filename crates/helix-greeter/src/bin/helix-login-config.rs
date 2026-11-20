//! helix-login-config: change the login screen's configuration.
//!
//! Installed as `/usr/libexec/helix/helix-login-config` and run as root
//! through `pkexec` (polkit action `org.helixdesktop.login-screen-config`):
//! reads the keys to change on standard input, a TOML document of at most
//! 16 KiB (`user-list`, `banner`, `accent`, `clock-format`, and
//! `remove = [...]` naming keys to delete), checks it, and makes the
//! changes in `/etc/helix/greeter.toml` (see `helix_greeter::login_config`;
//! helixdesktop/spec `components/greeter.md`, "Changing the
//! configuration"). Takes no arguments, and the file it writes is fixed:
//! nothing in the environment or the arguments can change it.
//!
//! Exit status: 0 written (nothing printed), 1 invalid input, 2 the file
//! could not be read or written (the reason on standard error), 64
//! arguments given.

use std::io::Read;
use std::path::Path;
use std::process::ExitCode;

use helix_greeter::login_config::{install, CONFIG_PATH, MAX_INPUT};

fn main() -> ExitCode {
    if std::env::args_os().len() > 1 {
        eprintln!("usage: helix-login-config < changes\n\nMakes the changes on standard input in {CONFIG_PATH}; takes no arguments.");
        return ExitCode::from(64);
    }
    let mut input = Vec::new();
    // One byte more than allowed, so that too long is noticed.
    if let Err(err) = std::io::stdin().lock().take(MAX_INPUT as u64 + 1).read_to_end(&mut input) {
        eprintln!("helix-login-config: cannot read the changes: {err}");
        return ExitCode::from(1);
    }
    match install(&input, Path::new(CONFIG_PATH)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("helix-login-config: {err}");
            ExitCode::from(err.exit_code())
        }
    }
}
