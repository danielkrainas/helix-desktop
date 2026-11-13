//! Starts bubblewrap for an extension sandbox (helix-desktop-spec
//! `contracts/extensions/README.md`, "Sandboxes"): `helix-extension-sandbox
//! ARGS…` runs `bwrap ARGS…`.
//!
//! It exists so that an AppArmor profile can let the sandbox, and only it,
//! use user namespaces where AppArmor restricts them (Ubuntu 24.04 and
//! later): the profile is attached to this program, and bwrap inherits it.

use std::os::unix::process::CommandExt;

fn main() {
    let bwrap = std::env::var_os("PATH")
        .and_then(|p| std::env::split_paths(&p).map(|d| d.join("bwrap")).find(|b| b.is_file()))
        .unwrap_or_else(|| "/usr/bin/bwrap".into());
    let err = std::process::Command::new(&bwrap).args(std::env::args_os().skip(1)).exec();
    eprintln!("helix-extension-sandbox: cannot run {}: {err}", bwrap.display());
    std::process::exit(127);
}
