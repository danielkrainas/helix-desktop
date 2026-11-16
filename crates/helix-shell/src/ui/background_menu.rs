//! The desktop background's menu (helix-desktop-spec
//! `contracts/wayland/README.md`, "Background menu"), as GNOME Shell's
//! `js/ui/backgroundMenu.js`: "Change Background…", "Display Settings"
//! and "Settings", on a right click or a long press on the background.

use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface;
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind};
use smithay_client_toolkit::shell::WaylandSurface;

use super::window_menu::{Choice, Item};
use super::Ui;
use helix_i18n::tr;

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

/// How long a press must be held (Clutter's `long-press-duration`).
pub(super) const LONG_PRESS: std::time::Duration = std::time::Duration::from_millis(500);
/// How far (logical pixels) it may move meanwhile (Clutter's drag
/// threshold).
pub(super) const LONG_PRESS_SLOP: f64 = 8.0;

/// A Settings panel an entry opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SettingsPanel {
    Background,
    Display,
    /// Settings itself, at no particular panel.
    Overview,
}

impl SettingsPanel {
    /// GNOME's desktop file for it (as `backgroundMenu.js` names them).
    pub(super) fn gnome_desktop_id(self) -> &'static str {
        match self {
            SettingsPanel::Background => "gnome-background-panel.desktop",
            SettingsPanel::Display => "gnome-display-panel.desktop",
            SettingsPanel::Overview => "org.gnome.Settings.desktop",
        }
    }

    /// The panel's command-line name (both settings apps take these).
    fn arg(self) -> Option<&'static str> {
        match self {
            SettingsPanel::Background => Some("background"),
            SettingsPanel::Display => Some("display"),
            SettingsPanel::Overview => None,
        }
    }
}

/// The menu's entries.
pub(super) fn items() -> Vec<Option<Item>> {
    let item = |label: &str, panel| Some(Item { label: label.to_owned(), choice: Choice::Settings(panel), checked: None, active: false, sub: Vec::new() });
    vec![
        item(tr("Change Background…"), SettingsPanel::Background),
        None,
        item(tr("Display Settings"), SettingsPanel::Display),
        item(tr("Settings"), SettingsPanel::Overview),
    ]
}

/// How to start Settings at a panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Launch {
    /// A desktop file, by id.
    Desktop(&'static str),
    /// A command line (program and arguments), for the app `desktop_id`.
    Command { line: String, desktop_id: &'static str },
}

/// How to open `panel` with `app` (the settings program found, as quick
/// settings finds it) when GNOME's desktop file for it is `gnome_desktop`
/// installed; `None` with no settings app at all.
pub(super) fn launch_for(panel: SettingsPanel, app: Option<&str>, gnome_desktop: bool) -> Option<Launch> {
    let with_arg = |program: &str| match panel.arg() {
        Some(arg) => format!("{program} {arg}"),
        None => program.to_owned(),
    };
    match app {
        Some("helix-settings-app") => Some(Launch::Command { line: with_arg("helix-settings-app"), desktop_id: "org.helix.Settings.desktop" }),
        _ if gnome_desktop => Some(Launch::Desktop(panel.gnome_desktop_id())),
        Some(program) => Some(Launch::Command { line: with_arg(program), desktop_id: "org.gnome.Settings.desktop" }),
        None => None,
    }
}

/// Start Settings at `panel`, as the shell starts apps (each in its own
/// scope).
pub(super) fn open_settings(panel: SettingsPanel) {
    use gio::prelude::*;
    let id = panel.gnome_desktop_id();
    let gnome = gio::AppInfo::all().into_iter().find(|a| a.id().is_some_and(|i| i == id));
    let launch = launch_for(panel, super::quick::settings_app(), gnome.is_some());
    tracing::info!(?panel, ?launch, "background menu: opening Settings");
    let result = match launch {
        Some(Launch::Desktop(id)) => match gnome {
            Some(app) => app.launch(&[], Some(&crate::app_scope::context(Some(id)))).map_err(|e| e.to_string()),
            None => Err(format!("no {id}")),
        },
        Some(Launch::Command { line, desktop_id }) => gio::AppInfo::create_from_commandline(&line, Some(tr("Settings")), gio::AppInfoCreateFlags::NONE)
            .and_then(|a| a.launch(&[], Some(&crate::app_scope::context(Some(desktop_id)))))
            .map_err(|e| e.to_string()),
        // As GNOME Shell: the entry is there, choosing it only logs.
        None => Err(format!("Settings panel for desktop file {} could not be loaded", panel.gnome_desktop_id())),
    };
    if let Err(err) = result {
        tracing::warn!(%err, "cannot open Settings");
    }
}

/// A primary-button or touch press on the background that may become a
/// long press.
pub(super) struct Hold {
    serial: u64,
    surface: WlSurface,
    at: (f64, f64),
}

/// Whether a press held at `from` has moved too far at `to` to be a long
/// press.
pub(super) fn moved_too_far(from: (f64, f64), to: (f64, f64)) -> bool {
    (to.0 - from.0).hypot(to.1 - from.1) > LONG_PRESS_SLOP
}

impl Ui {
    /// The monitor origin of a background surface (`None`: not one).
    fn background_origin(&self, surface: &WlSurface) -> Option<(i32, i32)> {
        let view = self.backgrounds.iter().find(|b| b.layer.wl_surface() == surface)?;
        Some(self.output_state.info(&view.output).and_then(|i| i.logical_position).unwrap_or((0, 0)))
    }

    /// Pointer input on a background surface; true if it was for one.
    /// A right press opens the menu; a left press (or touch) held still
    /// for [`LONG_PRESS`] opens it too.
    pub(super) fn background_pointer(&mut self, event: &PointerEvent) -> bool {
        let Some(origin) = self.background_origin(&event.surface) else { return false };
        let (x, y) = event.position;
        match event.kind {
            PointerEventKind::Press { button, .. } => {
                self.bg_hold = None;
                // A press on the background takes the desktop icons'
                // selection away (as a press beside them did).
                self.desktop_icons_unselect();
                match button {
                    BTN_RIGHT => self.open_background_menu(origin, (x, y)),
                    BTN_LEFT => self.hold_background(event.surface.clone(), (x, y)),
                    _ => {}
                }
            }
            PointerEventKind::Motion { .. } => {
                if self.bg_hold.as_ref().is_some_and(|h| moved_too_far(h.at, (x, y))) {
                    self.bg_hold = None;
                }
            }
            PointerEventKind::Release { .. } | PointerEventKind::Leave { .. } => self.bg_hold = None,
            _ => {}
        }
        true
    }

    /// Start timing a press for a long press.
    fn hold_background(&mut self, surface: WlSurface, at: (f64, f64)) {
        self.bg_hold_serial += 1;
        let serial = self.bg_hold_serial;
        self.bg_hold = Some(Hold { serial, surface, at });
        let Some(handle) = self.loop_handle.as_ref() else { return };
        let _ = handle.insert_source(Timer::from_duration(LONG_PRESS), move |_, _, ui: &mut Ui| {
            if let Some(hold) = ui.bg_hold.take_if(|h| h.serial == serial) {
                if let Some(origin) = ui.background_origin(&hold.surface) {
                    tracing::debug!(at = ?hold.at, "background long press");
                    ui.open_background_menu(origin, hold.at);
                }
            }
            TimeoutAction::Drop
        });
    }

    /// Open the menu at surface point `at` of the monitor at `origin`.
    fn open_background_menu(&mut self, origin: (i32, i32), at: (f64, f64)) {
        if self.lock.session.is_some() {
            return;
        }
        let (x, y) = (origin.0 + at.0 as i32, origin.1 + at.1 as i32);
        // Laid out left to right, drawn mirrored right to left.
        let x = if helix_draw::rtl() { self.mirrored_screen_x(x, y) } else { x };
        tracing::debug!(x, y, "background menu");
        self.open_menu(0, x, y, items(), Vec::new());
    }

    /// An entry of the menu was chosen.
    pub(super) fn background_menu_choice(&mut self, panel: SettingsPanel) {
        // As GNOME: the overview (and app grid) closes first.
        self.close_launcher(true);
        open_settings(panel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_are_gnomes() {
        let labels: Vec<Option<(String, Choice)>> = items().into_iter().map(|i| i.map(|i| (i.label, i.choice))).collect();
        assert_eq!(
            labels,
            [
                Some(("Change Background…".to_owned(), Choice::Settings(SettingsPanel::Background))),
                None,
                Some(("Display Settings".to_owned(), Choice::Settings(SettingsPanel::Display))),
                Some(("Settings".to_owned(), Choice::Settings(SettingsPanel::Overview))),
            ]
        );
    }

    #[test]
    fn settings_app_choice() {
        use SettingsPanel::*;
        // Helix's settings app first, at the panel.
        assert_eq!(launch_for(Background, Some("helix-settings-app"), true), Some(Launch::Command { line: "helix-settings-app background".into(), desktop_id: "org.helix.Settings.desktop" }));
        assert_eq!(launch_for(Overview, Some("helix-settings-app"), false), Some(Launch::Command { line: "helix-settings-app".into(), desktop_id: "org.helix.Settings.desktop" }));
        // GNOME's desktop files, as GNOME Shell starts them.
        assert_eq!(launch_for(Display, Some("gnome-control-center"), true), Some(Launch::Desktop("gnome-display-panel.desktop")));
        assert_eq!(launch_for(Overview, None, true), Some(Launch::Desktop("org.gnome.Settings.desktop")));
        // No desktop file: the command line.
        assert_eq!(launch_for(Display, Some("gnome-control-center"), false), Some(Launch::Command { line: "gnome-control-center display".into(), desktop_id: "org.gnome.Settings.desktop" }));
        assert_eq!(launch_for(Background, None, false), None);
    }

    #[test]
    fn long_press_slop() {
        assert!(!moved_too_far((100.0, 100.0), (105.0, 106.0)));
        assert!(moved_too_far((100.0, 100.0), (109.0, 100.0)));
    }

    /// The keyboard works in it as in the shell's other menus.
    #[test]
    fn keyboard() {
        use super::super::window_menu::{menu_key, MenuKey};
        use smithay_client_toolkit::seat::keyboard::Keysym;
        let items = items();
        assert_eq!(menu_key(&items, None, Keysym::Down, None, false), MenuKey::Select(0));
        // The separator is skipped.
        assert_eq!(menu_key(&items, Some(0), Keysym::Down, None, false), MenuKey::Select(2));
        assert_eq!(menu_key(&items, Some(3), Keysym::Down, None, false), MenuKey::Select(0));
        assert_eq!(menu_key(&items, Some(2), Keysym::Return, None, false), MenuKey::Choose(2));
        assert_eq!(menu_key(&items, Some(0), Keysym::Escape, None, false), MenuKey::Close);
        assert_eq!(menu_key(&items, Some(0), Keysym::s, Some('s'), false), MenuKey::Select(3));
    }
}
