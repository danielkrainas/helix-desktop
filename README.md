# Helix

**Helix** is a Rust desktop environment that begins with compatibility against GNOME’s public contracts and is designed to evolve beyond them.

This repository is the implementation. The living specification and contracts live in [helix-desktop-spec](https://github.com/danielkrainas/helix-desktop-spec).

## Status

Early implementation. `helix-session` serves `org.gnome.SessionManager` and launches the session, `helix-comp` is a Smithay compositor (nested only, for now) with `IdleMonitor` and `DisplayConfig`, `helix-shell` serves `org.gnome.Shell` and draws a top panel, `helix-portals` serves the Settings and Inhibit portal backends, and `helix-settings` reads the system GSettings.

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

With no session config, `helix-session` only serves SessionManager (as
above). Given a config (`--config FILE`, or `helix/session.toml` under
`$XDG_CONFIG_HOME` / `$XDG_CONFIG_DIRS`), it is also the session launcher: it
starts each component in order, waits for it to be ready (a bus name, or a
variable passed to `Setenv`), hands accumulated environment to later
components and to D-Bus activation, then calls `Initialized` itself. If a
required component fails to start or exits, the session ends; when the
session ends, components are stopped in reverse order. See
`session/helix-session.toml` for the default session.

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

## Running the compositor

`helix-comp` is built on [Smithay](https://github.com/Smithay/smithay). For
now it only has the nested (winit) backend: it runs in a window of your
current X11 or Wayland session. Running on bare hardware (DRM/KMS) needs the
libseat, libinput, libudev and gbm development packages and comes later.

```sh
# A throwaway bus keeps its D-Bus names away from your real GNOME session.
dbus-run-session -- bash -c '
  cargo run -p helix-comp -- --command gnome-calculator'
```

It serves `org.gnome.Mutter.IdleMonitor` and the read side of
`org.gnome.Mutter.DisplayConfig` (`ApplyMonitorsConfigAllowed` is false), and
announces its socket with `SessionManager.Setenv("WAYLAND_DISPLAY", ...)`,
which is what the session launcher waits for. Windows open in a cascade;
click to focus. Moving and resizing windows are not implemented yet. Panels
and other `wlr-layer-shell` surfaces are supported, and windows open outside
the space they reserve.

## Running the shell

`helix-shell` serves `org.gnome.Shell` (overview state, OSD requests,
accelerator grabs; `Eval` is always refused) and, when `WAYLAND_DISPLAY`
points at the compositor, draws a top panel with a clock. `ShowOSD` level
requests show as a level bar in the panel for two seconds.

Accelerator grabs are recorded and validated, but `AcceleratorActivated` is
not emitted yet: the compositor and shell are separate processes and the
key-grab hand-off between them is still open (see the spec's `gaps.md`).

## A full nested session

With every binary built (`cargo build`), a session config pointing at
`target/debug/helix-comp`, `helix-portals` and `helix-shell` (see
`session/helix-session.toml`) can run nested:

```sh
dbus-run-session -- target/debug/helix-session --config my-session.toml
```

When testing nested inside a non-GNOME host desktop, leave `GNOME` out of
`XDG_CURRENT_DESKTOP` in the test config. Otherwise xdg-desktop-portal
follows the installed `gnome-portals.conf`, tries to start gnome-keyring on
the throwaway bus, and GTK apps wait ~25 s for that activation to time out.
