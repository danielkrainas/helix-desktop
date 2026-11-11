# Session

Files for starting a Helix session.

| File | Install to | Purpose |
|------|-----------|---------|
| `helix.desktop` | `$datadir/wayland-sessions/` | Lets display managers offer the Helix session (runs `helix-session`) |
| `helix-session.toml` | `/etc/xdg/helix/session.toml` | Components the session launcher starts, in order |
| `helix.portal` | `$datadir/xdg-desktop-portal/portals/` | Registers `helix-portals` with xdg-desktop-portal |
| `helix-portals.conf` | `$datadir/xdg-desktop-portal/` | Prefers Helix's portal backends when `XDG_CURRENT_DESKTOP` contains `Helix` |

`helix-session` reads `$XDG_CONFIG_HOME/helix/session.toml` first, then
`$XDG_CONFIG_DIRS/helix/session.toml`, or a file given with `--config`. See
`crates/helix-session/src/launcher.rs` for the config format.
