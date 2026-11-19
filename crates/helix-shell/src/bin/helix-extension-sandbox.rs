//! Starts bubblewrap for an extension sandbox (helixdesktop/spec
//! `contracts/extensions/README.md`, "Sandboxes"): `helix-extension-sandbox
//! ARGS…` runs `/usr/bin/bwrap --unshare-user --disable-userns
//! --die-with-parent ARGS…`.
//!
//! It exists so that an AppArmor profile can let the sandbox, and only it,
//! use user namespaces where AppArmor restricts them (Ubuntu 24.04 and
//! later): the profile is attached to this program, and bwrap inherits it.
//! Anyone may run it, so it runs only the system's bubblewrap, never with
//! options that would give the sandboxed program capabilities or user
//! namespaces of its own (see `launcher_command`).

use std::os::unix::process::CommandExt;

use helix_shell::ext_host::sandbox::{launcher_command, SYSTEM_BWRAP};

fn main() {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let command = match launcher_command(&args) {
        Ok(command) => command,
        Err(err) => {
            eprintln!("helix-extension-sandbox: {err}");
            std::process::exit(2);
        }
    };
    let err = std::process::Command::new(SYSTEM_BWRAP).args(command).env_remove("LD_PRELOAD").env_remove("LD_LIBRARY_PATH").exec();
    eprintln!("helix-extension-sandbox: cannot run {SYSTEM_BWRAP}: {err}");
    std::process::exit(127);
}
