# Helix

**Helix** is a Rust desktop environment that begins with compatibility against GNOME’s public contracts and is designed to evolve beyond them.

This repository is the implementation. The living specification and contracts live in [helix-desktop-spec](https://github.com/danielkrainas/helix-desktop-spec).

## Status

Early implementation. `helix-session` serves `org.gnome.SessionManager` and launches the session, `helix-comp` is a Smithay compositor (nested, or on bare hardware via DRM/KMS) with `IdleMonitor` and `DisplayConfig`, `helix-shell` serves `org.gnome.Shell` and draws a top panel, `helix-portals` serves the Settings and Inhibit portal backends, and `helix-settings` reads the system GSettings.

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
- Ending the session follows the spec's sequence (see "Logging out"
  below): confirm, query clients, end, stop, `SessionOver`. Without a shell
  to ask, `Logout(0|1)`/`Shutdown`/`Reboot` are refused with
  `org.gnome.SessionManager.Error.Inhibited` while an inhibitor holds the
  logout flag (1). `Logout(2)` ignores inhibitors and client refusals, and
  takes over a sequence still waiting for confirmation. Inhibitors without
  the logout flag (e.g. idle) never block ending the session.
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

`helix-comp` is built on [Smithay](https://github.com/Smithay/smithay) and
has two backends, chosen automatically or with `--backend`:

- `winit` (default inside a desktop): runs in a window of your current X11
  or Wayland session.
- `drm` (default from a text console): drives the displays directly with
  KMS, reads input through libinput, and gets device access from the seat
  (logind or seatd) via libseat, so it runs as a normal user and supports VT
  switching. One GPU (the seat's primary) drives all connected displays,
  side by side at their preferred modes. Clients can share GPU buffers
  (linux-dmabuf).

Building needs the libseat, libinput, libudev, gbm, drm and EGL development
packages (see `.github/workflows/ci.yml`).

```sh
# A throwaway bus keeps its D-Bus names away from your real GNOME session.
dbus-run-session -- bash -c '
  cargo run -p helix-comp -- --command gnome-calculator'
```

It serves `org.gnome.Mutter.IdleMonitor` and the read side of
`org.gnome.Mutter.DisplayConfig` (`ApplyMonitorsConfigAllowed` is false), and
announces its socket with `SessionManager.Setenv("WAYLAND_DISPLAY", ...)`,
which is what the session launcher waits for.

X11 apps run through Xwayland (the `xwayland` package must be installed):
the compositor starts it, manages its windows alongside Wayland ones, and
bridges the clipboard and primary selection. Its `DISPLAY` is announced to
the session manager before `WAYLAND_DISPLAY`, so the whole session and
D-Bus-activated services see it; when nested, the host's `DISPLAY` is never
passed on to Helix's children. Helix draws no title bars yet, so X11 apps
that rely on the window manager for one can be moved with Super+drag.

Window management: windows open in a cascade below any panels and get
focus, and stay out from under panels when the work area changes; click
to focus; client-side (GTK) decorations; dragging and resizing
from the header bar and edges; maximize, fullscreen and minimize requests;
menus and popovers (popup grabs). Panels and other `wlr-layer-shell`
surfaces are supported, including exclusive keyboard focus.

Protocols beyond the core: xdg-decoration, xdg-activation, viewporter,
fractional-scale (always 1 for now), primary selection, cursor-shape (named
cursors are shown on the host window when nested). Not yet: linux-dmabuf
(GTK falls back to shared-memory rendering), text-input / input methods,
compositing client-drawn cursor surfaces.

Shortcuts handled by the compositor:

| Keys | Action |
|------|--------|
| Super (alone) | Show applications (asks the shell over `org.gnome.Shell`) |
| Alt+Tab | Cycle windows (includes minimized ones) |
| Alt+F4 | Close the focused window |
| Super+Up | Toggle maximize |
| Super+Down | Restore a maximized window, else minimize |
| Super+drag | Move the window under the pointer |
| Super+L | Lock the screen |

When nested, the host desktop's window manager may grab some of these (e.g.
Alt+Tab under XFCE) before Helix sees them.

## Notifications

The shell serves both notification interfaces GNOME Shell does:
`org.freedesktop.Notifications` (libnotify, `notify-send`, most non-GNOME
apps) and `org.gtk.Notifications` (GLib's `GNotification`, used by GNOME
apps when that name is owned). They share one queue: a banner below the
panel shows one notification at a time with its action buttons; clicking
it runs the default action (for GLib apps, by activating the app over
`org.freedesktop.Application`), × dismisses it. Banners expire after the
app's timeout or 5 seconds; critical/urgent ones stay until handled.

`org.gnome.desktop.notifications show-banners` = false acts as do not
disturb (only critical notifications show). While the screen is locked
banners wait and appear after unlocking. Not yet: icons, body markup,
sounds, and a notification list to review missed ones.

## Screen locking

The shell serves `org.gnome.ScreenSaver` and draws the lock screen on the
standard `ext-session-lock-v1` protocol; the compositor then shows nothing
of the session and sends input only to the lock screen, and stays locked
(blank) if the shell crashes until the restarted shell takes the lock back.
`SessionManager`'s `SessionIsLocked` follows it.

It locks on Super+L, `org.gnome.ScreenSaver.Lock`, and after GNOME's idle
settings (`org.gnome.desktop.session idle-delay` +
`org.gnome.desktop.screensaver lock-delay`, if `lock-enabled`; default 5
minutes) unless an app holds an idle inhibitor.

Unlocking checks your password with PAM, through the `helix` service.
**Install it before relying on locking**:

```sh
sudo install -m 644 session/pam.d/helix /etc/pam.d/helix
```

Until that file exists Helix refuses to lock rather than showing a lock
screen nobody could unlock. (PAM is loaded at run time; building needs no
PAM development package.)

For development only, building helix-shell with the `insecure-test-auth`
feature makes the lock screen accept the password `helix-test` instead of
using PAM. Never use such a build for real.

## Logging out

The panel's **Power** button opens a menu: Lock Screen, Log Out…,
Restart…, Power Off…. The last three call `SessionManager`'s `Logout(0)`,
`Reboot` and `Shutdown`, like any other caller would.

The session manager then asks the shell's
`org.gnome.SessionManager.EndSessionDialog` to confirm. The dialog counts
down 60 seconds, lists the apps inhibiting logout (with their reasons) and
offers Cancel or Log Out/Restart/Power Off ("… Anyway" when something
inhibits); Escape cancels, Enter confirms.

Each registered client (`RegisterClient`) gets an object at
`/org/gnome/SessionManager/Client<n>` (`ClientPrivate`). Once confirmed,
clients get `QueryEndSession` and have one second to answer with
`EndSessionResponse`. If one says no, the dialog comes back listing that
client like an inhibitor. Then `EndSession` (ten seconds to clean up),
`Stop`, and `SessionOver`. Cancelling sends `CancelEndSession`. Inhibitors
are objects at `/org/gnome/SessionManager/Inhibitor<cookie>`.

helix-portals registers as a client, so the Inhibit portal's session
monitors see the query (state 2, "query end") and can answer it with
`QueryEndResponse`.

`Shutdown` and `Reboot` still only end the session; they do not power off
or restart the machine yet.

## Running on real hardware

Try this from a free text console, not from inside your desktop:

1. `cargo build`, then check KMS is available. On NVIDIA,
   `sudo cat /sys/module/nvidia_drm/parameters/modeset` must print `Y`
   (kernel parameter `nvidia-drm.modeset=1`).
2. Switch to a text console (e.g. Ctrl+Alt+F3) and log in.
3. Start the whole session, logging to a file:

   ```sh
   cd path/to/helix-desktop
   PATH=$PWD/target/debug:$PATH RUST_LOG=info \
     dbus-run-session -- helix-session --config session/helix-session.toml 2> ~/helix.log
   ```

   or just the compositor with one app:
   `target/debug/helix-comp --backend drm --command gnome-calculator`.
4. **Ctrl+Alt+Backspace quits Helix** at any time; Ctrl+Alt+F1..F12 switch
   to other consoles (and back).

If it can't get a seat session or finds no display it exits with an error
instead of taking over the screen.

## Running the shell

`helix-shell` serves `org.gnome.Shell` (overview state, OSD requests,
accelerator grabs; `Eval` is always refused) and, when `WAYLAND_DISPLAY`
points at the compositor, draws:

- a top panel with an **Apps** button, a clock and a **Power** menu (see
  "Logging out"); `ShowOSD` level requests show as a level bar in the panel
  for two seconds;
- an application launcher, opened by the Apps button, Super, or
  `ShowApplications`: type to search installed apps (from their `.desktop`
  files), arrows to select, Enter or click to launch, Escape or a click
  outside to close. Its open state is `OverviewActive`.

Text uses the system sans-serif font (via `fc-match`).

Global accelerators work end to end: `GrabAccelerator` asks the compositor
to bind the key through a private Wayland protocol
(`crates/helix-protocols/protocols/helix-keybindings-v1.xml`), and presses
come back as `AcceleratorActivated` / `AcceleratorDeactivated`, filtered by
the grab's action modes (normal or overview). The compositor's own
shortcuts can't be grabbed. Only the shell the compositor started itself
(`helix-comp --shell helix-shell`, on a pre-connected `WAYLAND_SOCKET`) can
see that protocol; a shell started any other way still serves D-Bus and
draws its UI, but its accelerators never fire.

The compositor also supervises the shell: if it exits, it is restarted on a
fresh privileged connection after a growing delay (0.5 s, 1 s, 2 s… up to
8 s). If it exits 5 times within a minute, the compositor stops restarting
it and logs an error. Clients' accelerator grabs do not survive a restart,
as in GNOME. When the compositor is stopped (SIGTERM/SIGINT or
Ctrl+Alt+Backspace) it stops the shell too, and a shell that loses its
compositor exits instead of lingering.

## A full nested session

With every binary built (`cargo build`), a session config pointing at
`target/debug/helix-comp`, `helix-portals` and `helix-shell` (see
`session/helix-session.toml`) can run nested:

```sh
dbus-run-session -- target/debug/helix-session --config my-session.toml
```

The nested session runs as you, with your real home directory, so apps you
start in it restore their usual sessions and open your real files. For
throwaway testing, point `HOME` (or the `XDG_*_HOME` variables) at a scratch
directory.

When testing nested inside a non-GNOME host desktop, leave `GNOME` out of
`XDG_CURRENT_DESKTOP` in the test config. Otherwise xdg-desktop-portal
follows the installed `gnome-portals.conf`, tries to start gnome-keyring on
the throwaway bus, and GTK apps wait ~25 s for that activation to time out.
