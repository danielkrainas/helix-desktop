# Helix

**Helix** is a Rust desktop environment that begins with compatibility against GNOME’s public contracts and is designed to evolve beyond them.

This repository is the implementation. The living specification and contracts live in [helix-desktop-spec](https://github.com/danielkrainas/helix-desktop-spec).

## Status

Early implementation. `helix-session` serves `org.gnome.SessionManager` and launches the session, `helix-comp` is a Smithay compositor (nested, or on bare hardware via DRM/KMS) with `IdleMonitor` and `DisplayConfig`, `helix-shell` serves `org.gnome.Shell` and draws a top panel, `helix-portals` serves the Settings, Inhibit, Screenshot, ScreenCast, RemoteDesktop and Clipboard portal backends, and `helix-settings` reads the system GSettings.

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

## Building

Besides Rust, building needs the development packages listed in
`.github/workflows/ci.yml` (on Debian/Ubuntu: `libwayland-dev
libxkbcommon-dev libseat-dev libinput-dev libudev-dev libgbm-dev libdrm-dev
libegl-dev libglib2.0-dev libpipewire-0.3-dev libspa-0.2-dev clang`). The
PipeWire bindings are generated with libclang; without the `clang` package,
point it at GCC's headers instead, e.g.
`BINDGEN_EXTRA_CLANG_ARGS=-I/usr/lib/gcc/x86_64-linux-gnu/13/include`.

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
- A `Setenv` value goes into the D-Bus activation environment before the
  call returns, and to every component the launcher starts afterwards.
  Running processes keep their environment: apps the shell launches
  directly get the shell's environment, while D-Bus activatable apps get
  the activation environment.
- `Logout` accepts modes 0 (normal), 1 (no confirmation), 2 (force) and
  rejects anything else.
- Ending the session follows the spec's sequence (see "Logging out"
  below): confirm, query clients, end, stop, `SessionOver`. Without a shell
  to ask, `Logout(0|1)`/`Shutdown`/`Reboot` are refused with
  `org.gnome.SessionManager.Error.Inhibited` while an inhibitor holds the
  logout flag (1). `Logout(2)` ignores inhibitors and client refusals, and
  takes over a sequence still waiting for confirmation. Inhibitors without
  the logout flag (e.g. idle) never block ending the session.
- After `Shutdown`/`Reboot` end the session, systemd-logind powers off or
  restarts the machine; `Suspend` asks logind to suspend. While any
  inhibitor has the suspend flag (4), the session manager blocks system
  sleep through logind. `SessionIsActive` follows the logind session.
  With `[power] system = "none"` (or no config at all) logind is never
  called.
- Clients and inhibitors are tied to the caller's bus connection and are
  released when it disconnects. A one-shot `busctl call Inhibit ...` therefore
  holds its inhibitor only for the duration of that call.

Tests: `cargo test -p helix-session` runs unit tests for the session rules
and integration tests that start a private `dbus-daemon` (skipped if it is not
installed).

## Running the portal backends

`helix-portals` serves `org.freedesktop.impl.portal.Settings`,
`org.freedesktop.impl.portal.Inhibit`,
`org.freedesktop.impl.portal.Screenshot`,
`org.freedesktop.impl.portal.ScreenCast`,
`org.freedesktop.impl.portal.RemoteDesktop` and
`org.freedesktop.impl.portal.Clipboard` (for remote desktop sessions) as
`org.freedesktop.impl.portal.desktop.helix`. Settings come from the system's
GSettings (dconf + installed GNOME schemas); `org.freedesktop.appearance` is
derived from them (`color-scheme`, `accent-color`, `contrast`,
`reduced-motion`), and sandboxed apps also see the interface, window
manager, sound, privacy and mouse schemas as namespaces. Portal
inhibitions are forwarded to `org.gnome.SessionManager`. Screenshots and
color picking go through the shell's `org.gnome.Shell.Screenshot`: unless
the portal frontend says the app's permission was already checked, the user
picks what to capture in the shell's screenshot UI; an app may also ask
for a window the user clicks.

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

It serves `org.gnome.Mutter.IdleMonitor` and `org.gnome.Mutter.DisplayConfig`,
and announces its socket with `SessionManager.Setenv("WAYLAND_DISPLAY", ...)`,
which is what the session launcher waits for.

Monitor settings (e.g. from GNOME Settings' Displays panel) go through
`ApplyMonitorsConfig`: verify, temporary or persistent. On bare hardware it
changes modes, position, scale and rotation, mirrors
monitors, and turns monitors off; nested, only the scale can change (the
window decides the size, and it can't rotate). A persistent configuration
is saved in `~/.config/helix/monitors.toml` for that set of monitors and
comes back when they are connected again or the compositor restarts.
Scales go from 1 to 4 in quarter steps (125%, 150%, …) where the
monitor's resolution divides evenly; apps that support fractional scaling
(GTK 4, Qt 6) draw sharply at the exact scale, others are drawn at the
next whole scale and scaled down. Helix's own panel, launcher, dialogs
and lock screen draw sharply at every scale too.
A persistent change asks "Keep the new display settings?" with a 20-second
countdown (Escape or Revert goes back, Enter or Keep keeps it); only kept
changes are remembered, and if nobody answers the old settings return.
While GNOME Settings' Displays panel is open, each monitor shows its number
in the top-left corner (`ShowMonitorLabels`). Mode changes and turning
monitors off have only been tried in tests, not on real hardware.

Keyboard and pointer settings follow GNOME's GSettings live: keyboard
layouts from `org.gnome.desktop.input-sources` (switch with Super+Space or
whatever `switch-input-source` says: a switcher lists the layouts by name,
most recently used first, and a quick press goes back to the previous
one), key repeat, and, on bare hardware, mouse, touchpad, trackball and
pointing stick settings (speed, acceleration including custom curves,
natural scrolling, tap-to-click, touchpad scroll speed, scrolling with a
held button, left-handed and more; custom curves need libinput 1.23). With `per-window` set, each window keeps its own layout. IBus input
sources (Chinese, Japanese, Korean and other input methods) work as
under GNOME, without environment variables: apps use the Wayland
text-input protocol and the shell is the input method, starting its own
`ibus-daemon` when needed and showing IBus's candidates next to the text
cursor; the keyboard layout follows the engine's (from IBus's component
files). Touchscreens work on bare hardware: touches go to the app under
the finger (and focus its window), the panel and launcher take a tap as a
click, and the touchscreen covers the built-in panel, or else the primary
monitor. No touch gestures yet.

X11 apps run through Xwayland (the `xwayland` package must be installed):
the compositor starts it, manages its windows alongside Wayland ones, and
bridges the clipboard and primary selection. Its `DISPLAY` is announced to
the session manager before `WAYLAND_DISPLAY`, so the whole session and
D-Bus-activated services see it; when nested, the host's `DISPLAY` is never
passed on to Helix's children. As under GNOME, X11 windows that don't
draw their own get a title bar: their title, the buttons of
`button-layout`, light or dark with `color-scheme`, and the title bar
click actions from GNOME's window preferences (double-click maximizes by
default); dragging it moves the window, and an invisible border around it
resizes. Wayland apps draw their own decorations.

Screen sharing works through the ScreenCast portal: when an app asks, the
shell lets you pick a monitor ("Share your screen?"), the compositor
records it into a PipeWire video stream (`org.gnome.Mutter.ScreenCast`,
BGRx, up to 30 frames a second, only when something changed; frames are
read back from the GPU in BGRA without conversion where the GPU allows,
one frame behind so the compositor never waits on the GPU), and the panel shows a red "Sharing" label
until it ends. Single windows can be shared too (the stream follows the
window's size; nothing is sent while it is hidden). When an app asks to
remember the choice, a shared monitor (or window, found again by app and
title) is shared again later without asking, as long as it is there;
remote control is remembered the same way, for the devices allowed. An app asking only for
a virtual monitor gets a new 1920×1080 one (right of the others; windows
can be moved there) without being asked. `org.gnome.Mutter.ScreenCast`
also records areas, and can send the pointer as PipeWire cursor metadata
instead of drawing it into the frames.

Remote control works through the RemoteDesktop portal: after the user
allows it (a dialog naming the app and the devices, or the share dialog
when the screen is shared too), the app can send keyboard, pointer and
touch input, either over D-Bus or through an EIS socket (libei, via
`ConnectToEIS`), as gnome-remote-desktop, Deskflow and similar tools do.
`org.gnome.Mutter.RemoteDesktop` is served for Helix's portal backend.
An allowed session that asked for it (the Clipboard portal) shares the
clipboard both ways, with Wayland and X11 apps, and a session may set its
own keyboard keymap (`SetKeymap`, optionally locked against input source
changes until it ends).

Night Light follows GNOME's settings (enabled, temperature, manual
schedule, or sunset to sunrise at the last known location): the shell
fades every monitor's gamma through DisplayConfig's `SetCrtcGamma`, which
the compositor applies to the DRM CRTCs (nested, the ramps are accepted
but have no visible effect). Its state is published as gnome-settings-daemon's
`org.gnome.SettingsDaemon.Color` (active, temperature, sunrise and sunset),
which also lets GNOME Settings preview it and disable it until tomorrow.
`SetOutputCTM` sets a monitor's color transformation matrix (the CRTC's
`CTM` property on bare hardware) the same way.

Workspaces follow GNOME's settings: a fixed number (`num-workspaces`) or
dynamic ones (always one empty workspace at the end), optionally only on
the primary monitor. Super+Page Up/Down (and the other
`switch-to-workspace-*` keys) switch, Super+Shift+Page Up/Down move the
focused window along, with a popup showing the workspaces and the
active one's name (`workspace-names`); the panel shows a dot per
workspace. The window
switcher lists only the current workspace's windows by default.

With several monitors, new windows open on the one under the pointer,
maximize and fullscreen use the monitor a window is mostly on, the panel
sits on the primary monitor, and windows (and the panel) of a monitor that
is unplugged or turned off move to the remaining ones.

Window management: windows open in a cascade below any panels and get
focus, and stay out from under panels when the work area changes; click
to focus; client-side (GTK) decorations; dragging and resizing
from the header bar and edges; maximize, fullscreen and minimize requests;
menus and popovers (popup grabs). Panels and other `wlr-layer-shell`
surfaces are supported, including exclusive keyboard focus.

Protocols beyond the core: xdg-decoration, xdg-activation, viewporter,
fractional-scale, primary selection, cursor-shape, text-input and
(for the shell) input-method and virtual-keyboard (named
cursors are shown on the host window when nested), and linux-dmabuf with
feedback naming the GPU, so GTK 4 and other GL/Vulkan apps hand over GPU
buffers instead of copying pixels (nested too). Also: idle-inhibit,
pointer-constraints and relative-pointer (for games), pointer-gestures
(3- and 4-finger sideways swipes switch workspaces instead),
keyboard-shortcuts-inhibit (the shell asks once per app and remembers
the answer in the permission store; Super+Escape takes the shortcuts
back), xdg-foreign, presentation-time, fifo, commit-timing,
single-pixel-buffer, linux-drm-syncobj (explicit sync, when the GPU
supports syncobj eventfd waits; GPU buffers otherwise wait for their
implicit fences) and, for Xwayland, xwayland-keyboard-grab (off
unless `org.gnome.mutter.wayland xwayland-allow-grabs` and the access
rules allow it), and color-management (sRGB only: Helix does not convert
colors and has no HDR output; `output-luminance` dims a monitor through its
gamma ramps on bare hardware), xdg-toplevel-icon (window icons, shown in the
switchers; X11 windows' `_NET_WM_ICON` too; otherwise the app's icon). Not yet: HDR.

The pointer: apps' own cursor images are drawn by Helix (nested too, with
the host's pointer hidden over the window) and get frame callbacks; named
cursors come from the XCursor theme (`cursor-theme`, `cursor-size`,
animated ones animate) on bare hardware, and from the host when nested.
Screenshots and screen casts (embedded or as metadata) show the same
image.

Shortcuts handled by the compositor:

| Keys | Action |
|------|--------|
| Super (alone) | Show or hide the overview (asks the shell over `org.gnome.Shell`) |
| Super+L | Lock the screen |

Alt+Tab (the application switcher), Alt+\` (windows of the current
application), Super+Space (keyboard layouts), Print (screenshots), the
volume and brightness keys, and GNOME's window keys (Alt+F4 close,
Super+Up/Down maximize and restore, Super+H minimize, Alt+Space window
menu, Alt+F7/F8 move and resize, Super+Shift+arrows to another monitor,
and the rest of `org.gnome.desktop.wm.keybindings`) belong to the shell,
which binds them from GSettings.

GNOME's window preferences apply: focus follows the pointer with
`focus-mode` sloppy or mouse (with `auto-raise` and
`focus-change-on-pointer-rest`), `raise-on-click`, modifier clicks
(`mouse-button-modifier`, Super by default: drag to move, middle drag to
resize, right click for the window menu; `resize-with-right-button` swaps
them), new windows centered (`center-new-windows`), maximized when they
fill the screen (`auto-maximize`) or kept behind the focused window
(`focus-new-windows` strict), dialogs over their parent (modal ones
attached with `attach-modal-dialogs`), the `overlay-key`, audible and
visual bells for apps using `xdg_system_bell_v1`, and locate pointer
(`locate-pointer`, Ctrl).

When an app stops answering (no reply to a ping, or for X11 apps to
`_NET_WM_PING`, within
`check-alive-timeout` after its window gets focus), the shell asks
whether to wait or force it to quit.

Super+Left / Super+Right (`org.gnome.mutter.keybindings`) tile the
focused window to half the screen; with `org.gnome.mutter edge-tiling`,
dragging a window against a monitor's left or right edge tiles it (top
edge: maximize), with a preview of where it will go.

The window menu (Alt+Space, a right click on a GTK header bar or on an X11
title bar) offers minimize, maximize, move, resize, Always on Top, Always
on Visible Workspace, moving to another workspace or monitor, and close.
Move and Resize follow the pointer or the arrow keys (10 pixels, 1 with
Control); a click, Return or Space ends them and Escape undoes them. Volume and brightness keys keep working while the screen is
locked.

On laptops, DisplayConfig's `Backlight` and `SetBacklight` control the
built-in panel through the kernel backlight device (written via logind's
`SetBrightness`, so no extra permissions are needed); brightness changed
elsewhere shows up within two seconds. The shell asks the compositor for
screenshots over a private protocol
(`crates/helix-protocols/protocols/helix-screenshot-v1.xml`); the image
has the resolution of the highest-scale monitor it covers. When nested, the host
desktop's window manager may grab some of these keys (e.g. Alt+Tab and
Super+Space under XFCE) before Helix sees them.

## Notifications

The shell serves both notification interfaces GNOME Shell does:
`org.freedesktop.Notifications` (libnotify, `notify-send`, most non-GNOME
apps) and `org.gtk.Notifications` (GLib's `GNotification`, used by GNOME
apps when that name is owned). They share one queue: a banner below the
panel shows one notification at a time with its action buttons; clicking
it runs the default action (for GLib apps, by activating the app over
`org.freedesktop.Application`), × dismisses it. Banners show the app's
icon (from the icon theme, PNG or SVG, or image data), body markup (bold,
underline, links that open in the default app) and play the sound the app
asks for. They go after the app's timeout or 5 seconds (critical/urgent
ones stay until handled), but the notification stays in the list:
clicking the clock opens it (newest first, × to dismiss, Clear, and a
Do Not Disturb switch), and a dot next to the clock means there is
something new in it.

`org.gnome.desktop.notifications show-banners` = false acts as do not
disturb (only critical notifications show a banner; the rest go straight
to the list). While the screen is locked banners wait and appear after
unlocking.

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

## Session restore and power queries

`SessionManager` answers `CanShutdown`, `CanReboot` and `CanSuspend` from
logind (with suspend shown as blocked while something inhibits it), and
supports session restore (`RegisterRestore` and friends, used by
xdg-desktop-portal): apps registered when you log out (or that crashed)
are started again at the next login through their desktop file, with
`gio launch`. Records live in `~/.local/state/helix/restore.toml`.

## Logging out

The panel's **Power** button opens a menu: Lock Screen, Suspend, Log Out…,
Restart…, Power Off…. Suspend, Restart and Power Off appear only when
logind allows them without a password (`CanSuspend` etc. answer `yes`).
The entries call `SessionManager`'s `Suspend`, `Logout(0)`, `Reboot` and
`Shutdown`, like any other caller would.

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

Once the session is over, the session manager asks logind to power off
(`Shutdown`) or restart (`Reboot`).

Before the system sleeps, the shell locks the screen if
`org.gnome.desktop.screensaver lock-enabled` is on (the default). It holds
a logind sleep delay inhibitor and lets the system sleep only once the lock
screen is shown.

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
- an overview, opened by Super (or `toggle-overview`): workspace
  thumbnails (click to switch, drop a window there to move it) above
  pictures of the active workspace's windows (click to activate, × to
  close, arrows and Enter); typing searches in the launcher; Super+Alt+Up
  / Down step between the desktop, the overview and the launcher;
- an application launcher, opened by the Apps button, Super+A, or
  `ShowApplications`: open windows first, then installed apps (from their
  `.desktop` files); type to search, arrows to select, Enter or click to
  switch to the window or launch the app, Escape or a click outside to
  close. `OverviewActive` is true while it or the overview is open;
- switchers (Tab again, Shift for backward, to move; let go of the
  modifier to switch; Escape cancels): applications on Alt+Tab and
  Super+Tab (`switch-applications`, one entry per app with its window
  count, most recently used first), windows on `switch-windows`, the
  current application's windows on Alt+\` (`switch-group`), and keyboard
  layouts on Super+Space;
- in the application switcher, Down (or the key above Tab) shows the
  selected application's windows, Up goes back; Alt+Escape and Alt+F6
  (`cycle-windows`, `cycle-group`) switch windows directly without a
  switcher; Ctrl+Alt+Tab (`switch-panels`) puts keyboard focus on the
  panel (arrows, Enter, Escape); `toggle-on-all-workspaces` keeps a window
  on every workspace;
- screenshots: Print opens the screenshot UI (drag an area, or Enter for
  the whole screen), Shift+Print captures the screen and Alt+Print the
  focused window at once. Images go to `~/Pictures/Screenshots`, are
  copied to the clipboard (paste works in X11 apps too), and a
  "Screenshot captured" notification opens the image, or shows it in the
  file manager with its button.
  `org.gnome.Shell.Screenshot` serves the Screenshot portal and the GNOME
  Screenshot app; other callers are refused;
- volume keys (through `pactl`, which PipeWire and PulseAudio both serve)
  and brightness keys, with the new level shown in the panel; playback
  keys (play/pause, stop, next, previous) control the playing MPRIS media
  player, or else a paused one.

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

**Set `system = "none"` under `[power]` in a nested test config.** The
default, `logind`, really powers off or restarts the machine when you
confirm Power Off or Restart in the nested session.

The nested session runs as you, with your real home directory, so apps you
start in it restore their usual sessions and open your real files. For
throwaway testing, point `HOME` (or the `XDG_*_HOME` variables) at a scratch
directory.

When testing nested inside a non-GNOME host desktop, leave `GNOME` out of
`XDG_CURRENT_DESKTOP` in the test config. Otherwise xdg-desktop-portal
follows the installed `gnome-portals.conf`, tries to start gnome-keyring on
the throwaway bus, and GTK apps wait ~25 s for that activation to time out.
