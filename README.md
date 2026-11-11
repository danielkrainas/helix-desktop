# Helix

**Helix** is a Rust desktop environment that begins with compatibility against GNOME’s public contracts and is designed to evolve beyond them.

This repository is the implementation. The living specification and contracts live in [helix-desktop-spec](https://github.com/danielkrainas/helix-desktop-spec).

## Status

Early implementation. `helix-session` serves `org.gnome.SessionManager`, `helix-portals` serves the Settings and Inhibit portal backends, and `helix-settings` reads the system GSettings. The compositor and shell are still scaffolding.

## Target

- **GNOME version**: 51
- **Language**: Rust
- **License**: CC0-1.0

## Compatibility goals (v0)

- Major session and service D-Bus interfaces
- GSettings schemas used by the desktop and common tools
- Sufficient Wayland protocol usage for normal desktop operation
- Ability to host existing GTK / libadwaita applications

Full GNOME Shell extension API compatibility is out of scope.

## Structure

This is a Cargo workspace:

```
crates/
  helix-comp/       # compositor / window management surface
  helix-shell/      # shell UI
  helix-session/    # session manager and D-Bus services
  helix-settings/   # GSettings / settings integration
  helix-portals/    # portal backends
  helix-test-support/ # test helpers: private D-Bus, contract checks
session/            # startup units and scripts
tests/contract/     # conformance tests against helix-desktop-spec
```

Contracts are defined in the companion [helix-desktop-spec](https://github.com/danielkrainas/helix-desktop-spec) repository.

## Running the session skeleton

`helix-session` owns `org.gnome.SessionManager` on the session bus. Run it on a
throwaway bus so it never conflicts with a real desktop session:

```sh
dbus-run-session -- bash -c '
  cargo run -p helix-session &
  sleep 1
  SM="busctl --user call org.gnome.SessionManager /org/gnome/SessionManager org.gnome.SessionManager"
  $SM Initialized           # initializing -> running, emits SessionRunning
  $SM IsSessionRunning      # b true
  $SM Logout u 0            # emits SessionOver; the service exits
  wait'
```

Set `RUST_LOG=debug` for more detail. SIGTERM/SIGINT also end the session
cleanly (emitting `SessionOver`).

Implemented: everything in the contract except `Suspend`, the `Can*`
availability methods except `CanRebootToFirmwareSetup`,
`SetRebootToFirmwareSetup` and the session-restore methods (see the spec's
`gaps.md`). `RestoreSupported` is false.

Current behavior worth knowing:

- `Setenv` and `InitializationError` are only accepted before `Initialized`.
  A fatal `InitializationError` ends the session.
- `Logout` accepts modes 0 (normal), 1 (no confirmation), 2 (force) and
  rejects anything else.
- `Logout(0|1)`/`Shutdown`/`Reboot` are refused with
  `org.gnome.SessionManager.Error.Inhibited` while an inhibitor holds the
  logout flag (1). `Logout(2)` ignores inhibitors. Inhibitors without the
  logout flag (e.g. idle) never block ending the session.
- `Shutdown`/`Reboot` only end the session; they never power off the machine.
- Clients and inhibitors are tied to the caller's bus connection and are
  released when it disconnects. A one-shot `busctl call Inhibit ...` therefore
  holds its inhibitor only for the duration of that call.

Tests: `cargo test -p helix-session` runs unit tests for the session rules
and integration tests that start a private `dbus-daemon` (skipped if it is not
installed).

## Running the portal backends

`helix-portals` serves `org.freedesktop.impl.portal.Settings` and
`org.freedesktop.impl.portal.Inhibit` as
`org.freedesktop.impl.portal.desktop.helix`. Settings come from the system's
GSettings (dconf + installed GNOME schemas); `org.freedesktop.appearance` is
derived from them (`color-scheme`, `contrast`, `reduced-motion`). Portal
inhibitions are forwarded to `org.gnome.SessionManager`.

To try it without touching your real settings:

```sh
dbus-run-session -- env GSETTINGS_BACKEND=memory bash -c '
  cargo run -p helix-portals &
  sleep 1
  busctl --user call org.freedesktop.impl.portal.desktop.helix \
    /org/freedesktop/portal/desktop org.freedesktop.impl.portal.Settings \
    Read ss org.freedesktop.appearance color-scheme
  kill %1; wait'
```

For xdg-desktop-portal to use it, install `session/helix.portal` into
`$datadir/xdg-desktop-portal/portals/` and `session/helix-portals.conf` into
`$datadir/xdg-desktop-portal/`, and include `Helix` in `XDG_CURRENT_DESKTOP`.
