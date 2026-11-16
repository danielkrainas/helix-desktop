//! The quick settings menu (helix-desktop-spec `contracts/wayland/README.md`,
//! "Quick settings"): volume, brightness, switches and the power actions,
//! under the panel's Power button.

use smithay_client_toolkit::reexports::client::protocol::wl_shm;
use smithay_client_toolkit::shell::wlr_layer::KeyboardInteractivity;
use smithay_client_toolkit::shell::WaylandSurface;

use super::{baseline, label, physical, power_menu_items, size, theme, Canvas, KeyEvent, Keysym, OverlayHit, Rect, Text, Ui, DIM_FG, FG, panel_height};
use crate::PowerAction;
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind};
use crate::status::Toggle;
use helix_draw::Image;
use crate::{QuickChange, UiRequest};
use helix_i18n::tr;
use helix_i18n::trf;

/// A power profile as the user knows it.
fn profile_label(name: &str) -> String {
    match name {
        "power-saver" => tr("Power Saver").into(),
        "balanced" => tr("Balanced").into(),
        "performance" => tr("Performance").into(),
        other => other.into(),
    }
}

const MENU_W: i32 = 400;
const ROW_H: i32 = 44;
const PAD: i32 = 14;
const LIST_ROW_H: i32 = 34;
/// A power action's row.
const ACTION_H: i32 = 38;

/// A row of the open Wi-Fi or Bluetooth list.
pub(super) struct ListRow {
    icon: Option<Image>,
    name: String,
    secured: bool,
    connected: bool,
    /// The chosen one of a choice (the power mode).
    checked: bool,
    /// What clicking it does (none: a heading or a note, drawn dim).
    hit: Option<OverlayHit>,
    dim: bool,
}

/// A quick settings switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Switch {
    Wifi,
    Bluetooth,
    Airplane,
    AutoRotate,
    NightLight,
    DarkStyle,
    DoNotDisturb,
    /// Power Mode (power-profiles-daemon): on away from Balanced; its "›"
    /// lists the modes.
    PowerMode,
    /// An extension's toggle (index into the extensions' toggles).
    Ext(usize),
}

impl Switch {
    /// Its id in `org.helix.shell` `quick-settings` (built-in ones).
    pub(super) fn id(self) -> &'static str {
        match self {
            Switch::Ext(_) => "",
            Switch::Wifi => "helix:wifi",
            Switch::Bluetooth => "helix:bluetooth",
            Switch::Airplane => "helix:airplane-mode",
            Switch::AutoRotate => "helix:auto-rotate",
            Switch::NightLight => "helix:night-light",
            Switch::DarkStyle => "helix:dark-style",
            Switch::DoNotDisturb => "helix:do-not-disturb",
            Switch::PowerMode => "helix:power-mode",
        }
    }
}

/// The switches `order` lists (by id), in its order, of those there are
/// (switch, label, id).
pub(super) fn ordered_switches(available: Vec<(Switch, String, String)>, order: &[String]) -> Vec<(Switch, String)> {
    let mut out: Vec<(Switch, String)> = Vec::new();
    for id in order {
        if let Some(found) = available.iter().find(|(_, _, i)| i == id) {
            if !out.iter().any(|(s, _)| *s == found.0) {
                out.push((found.0, found.1.clone()));
            }
        }
    }
    out
}

/// What the menu shows (read when it opens and after changes).
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub(super) struct QuickState {
    /// (percent, muted, max percent)
    pub volume: Option<(u32, bool, u32)>,
    pub brightness: Option<u32>,
}

const COLOR: &str = "org.gnome.settings-daemon.plugins.color";
const INTERFACE: &str = "org.gnome.desktop.interface";

fn settings(schema: &str, key: &str) -> Option<helix_settings::Settings> {
    let source = gio::SettingsSchemaSource::default()?;
    source.lookup(schema, true)?.has_key(key).then(|| helix_settings::Settings::new(schema))
}

/// The size of quick settings' symbolic icons (logical px): 16, growing
/// with the text scale as the controls holding them do (GNOME sizes them
/// in em).
fn icon_px() -> i32 {
    theme::grow(16)
}

impl Ui {
    /// A symbolic icon in the panel's text color at [`icon_px`], cached.
    fn symbolic_icon_px_grown(&mut self, name: &str, scale: f64) -> Option<Image> {
        self.symbolic_icon_px(name, f64::from(icon_px()), scale)
    }
}

/// Dark Style's icon: `dark-mode-symbolic`, else (an icon theme without
/// it) a night sky.
fn dark_style_icon() -> &'static str {
    static FOUND: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *FOUND.get_or_init(|| crate::icons::symbolic("dark-mode-symbolic", 16, [0, 0, 0]).is_some()) { "dark-mode-symbolic" } else { "weather-clear-night-symbolic" }
}

/// The volume's icon at `percent` (muted: the muted one).
fn volume_icon(percent: u32, muted: bool) -> &'static str {
    match percent {
        _ if muted => "audio-volume-muted-symbolic",
        0 => "audio-volume-muted-symbolic",
        1..=33 => "audio-volume-low-symbolic",
        34..=66 => "audio-volume-medium-symbolic",
        67..=100 => "audio-volume-high-symbolic",
        _ => "audio-volume-overamplified-symbolic",
    }
}

/// The settings app to open from quick settings: Helix's (helix-apps),
/// else GNOME's; `None` when neither is installed.
fn settings_app() -> Option<&'static str> {
    let on_path = |name: &str| std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()));
    ["helix-settings-app", "gnome-control-center"].into_iter().find(|name| on_path(name))
}

/// Open the settings app ([`settings_app`]).
fn open_settings_app() {
    let Some(name) = settings_app() else { return };
    let app = gio::AppInfo::create_from_commandline(name, Some(tr("Settings")), gio::AppInfoCreateFlags::NONE);
    if let Err(err) = app.and_then(|a| gio::prelude::AppInfoExt::launch(&a, &[], None::<&gio::AppLaunchContext>)) {
        tracing::warn!(%err, "cannot open Settings");
    }
}

/// The slider value (0..=max) for a click at `x` on `r`.
pub(super) fn slider_value(r: Rect, x: f64, max: u32) -> u32 {
    let f = ((x - f64::from(r.x)) / f64::from(r.w.max(1))).clamp(0.0, 1.0);
    (f * f64::from(max)).round() as u32
}

impl Ui {
    fn switch_on(&self, switch: Switch) -> bool {
        match switch {
            Switch::Wifi => self.status.wifi.as_ref().is_some_and(|w| w.0),
            Switch::Bluetooth => self.status.bluetooth == Some(true),
            Switch::Airplane => self.status.airplane,
            Switch::AutoRotate => !crate::extra_keys::orientation_locked(),
            Switch::NightLight => settings(COLOR, "night-light-enabled").is_some_and(|s| s.boolean("night-light-enabled")),
            Switch::DarkStyle => settings(INTERFACE, "color-scheme").is_some_and(|s| s.string("color-scheme") == "prefer-dark"),
            Switch::DoNotDisturb => self.notify.do_not_disturb,
            Switch::PowerMode => self.status.profile.as_deref().is_some_and(|p| p != "balanced"),
            Switch::Ext(i) => self.ext.toggles.get(i).is_some_and(|t| t.active),
        }
    }

    fn flip(&mut self, switch: Switch) {
        let on = !self.switch_on(switch);
        let toggle = |t: Toggle| UiRequest::QuickSettings(QuickChange::Toggle(t));
        match switch {
            Switch::Wifi => {
                let _ = self.requests.send(toggle(Toggle::Wifi(on)));
            }
            Switch::Bluetooth => {
                let _ = self.requests.send(toggle(Toggle::Bluetooth(on)));
            }
            Switch::Airplane => {
                let _ = self.requests.send(toggle(Toggle::Airplane(on)));
            }
            Switch::AutoRotate => crate::extra_keys::toggle_orientation_lock(),
            Switch::NightLight => {
                if let Some(s) = settings(COLOR, "night-light-enabled") {
                    let _ = s.set_boolean("night-light-enabled", on);
                }
            }
            Switch::DarkStyle => {
                if let Some(s) = settings(INTERFACE, "color-scheme") {
                    let _ = s.set_string("color-scheme", if on { "prefer-dark" } else { "default" });
                }
            }
            Switch::DoNotDisturb => {
                self.notify.do_not_disturb = on;
                let _ = self.requests.send(UiRequest::SetDoNotDisturb(on));
            }
            // On: Power Saver (else the first mode that is not Balanced);
            // off: Balanced.
            Switch::PowerMode => {
                let profiles = &self.status.profiles;
                let target = if on { profiles.iter().find(|p| *p == "power-saver").or_else(|| profiles.iter().find(|p| *p != "balanced")) } else { profiles.iter().find(|p| *p == "balanced") };
                if let Some(name) = target.cloned() {
                    let _ = self.requests.send(toggle(Toggle::Profile(name)));
                }
            }
            // Shown once the extension says so.
            Switch::Ext(i) => {
                if let Some(t) = self.ext.toggles.get(i).filter(|t| t.sensitive) {
                    let _ = self.requests.send(UiRequest::ExtAction(t.full_id.clone(), crate::ext_host::Action::Toggle));
                }
            }
        }
        gio::Settings::sync();
    }

    /// Open quick settings (focusing its first control when `opened` by
    /// the keyboard), or close it.
    pub(super) fn toggle_power_menu(&mut self, opened: Opened) {
        if self.power_menu.is_some() {
            self.close_quick_settings();
            return;
        }
        self.close_launcher(true);
        self.close_overview(true);
        let mut overlay = self.new_overlay("helix-power-menu", opened.interactivity(), QuickNav { opened, ..Default::default() });
        // The first control, once painted.
        overlay.state.focus = opened.keyboard().then_some(OverlayHit::Inside);
        self.power_menu = Some(overlay);
        self.quick_list = None;
        self.quick_power_open = false;
        let _ = self.requests.send(UiRequest::QuickSettings(QuickChange::Refresh));
        let _ = self.requests.send(UiRequest::QuickSettingsOpened);
    }

    /// Stop the discovery "Pair New Device…" started once the Bluetooth
    /// list or the menu is closed.
    pub(super) fn quick_discovery_check(&mut self) {
        if self.bt_discovery && (self.power_menu.is_none() || self.quick_list != Some(Switch::Bluetooth)) {
            self.bt_discovery = false;
            let _ = self.requests.send(UiRequest::QuickSettings(QuickChange::Toggle(Toggle::BluetoothDiscovery(false))));
        }
    }

    pub(super) fn quick_settings_changed(&mut self, state: QuickState) {
        self.quick = state;
        self.draw_power_menu();
    }

    /// A click on the menu; false when it should close.
    pub(super) fn quick_click(&mut self, hit: OverlayHit, x: f64, rect: Rect) -> bool {
        crate::click_feedback();
        tracing::debug!(?hit, x, "quick settings click");
        match hit {
            OverlayHit::Power(action) => {
                tracing::info!(?action, "power menu");
                let _ = self.requests.send(UiRequest::Power(action));
                false
            }
            OverlayHit::VolumeSlider => {
                let max = self.quick.volume.map_or(100, |v| v.2);
                let _ = self.requests.send(UiRequest::QuickSettings(QuickChange::Volume(slider_value(rect, x, max))));
                true
            }
            OverlayHit::Mute => {
                let _ = self.requests.send(UiRequest::QuickSettings(QuickChange::Mute));
                true
            }
            OverlayHit::BrightnessSlider => {
                let _ = self.requests.send(UiRequest::QuickSettings(QuickChange::Brightness(slider_value(rect, x, 100))));
                true
            }
            OverlayHit::Switch(switch) => {
                self.flip(switch);
                self.draw_power_menu();
                true
            }
            OverlayHit::Profile(index) => {
                if let Some(name) = self.status.profiles.get(index).cloned() {
                    let _ = self.requests.send(UiRequest::QuickSettings(QuickChange::Toggle(Toggle::Profile(name))));
                }
                true
            }
            OverlayHit::ExtSlider(i) => {
                if let Some(s) = self.ext.sliders.get(i) {
                    let value = f64::from(slider_value(rect, x, 1000)) / 1000.0;
                    let _ = self.requests.send(UiRequest::ExtAction(s.full_id.clone(), crate::ext_host::Action::SetValue(value)));
                }
                true
            }
            OverlayHit::ExtDetails(i) => {
                // The details menu takes the place of quick settings.
                if let Some(t) = self.ext.toggles.get(i) {
                    let _ = self.requests.send(UiRequest::ExtAction(t.full_id.clone(), crate::ext_host::Action::Details { x: rect.x }));
                }
                true
            }
            OverlayHit::Expand(switch) => {
                self.quick_list = if self.quick_list == Some(switch) { None } else { Some(switch) };
                self.draw_power_menu();
                true
            }
            OverlayHit::Network(index) => {
                if let Some(network) = self.status.networks.get(index).filter(|n| !n.active) {
                    let _ = self.requests.send(UiRequest::QuickSettings(QuickChange::Toggle(Toggle::ConnectWifi(network.ssid.clone()))));
                }
                true
            }
            OverlayHit::BtDevice(index) => {
                if let Some(device) = self.status.devices.get(index) {
                    let toggle = Toggle::BluetoothDevice { path: device.path.clone(), connect: !device.connected };
                    let _ = self.requests.send(UiRequest::QuickSettings(QuickChange::Toggle(toggle)));
                }
                true
            }
            OverlayHit::BtSendFiles => {
                let _ = self.requests.send(UiRequest::QuickSettings(QuickChange::Toggle(Toggle::BluetoothSendFiles)));
                false
            }
            OverlayHit::PairNew => {
                self.bt_discovery = true;
                let _ = self.requests.send(UiRequest::QuickSettings(QuickChange::Toggle(Toggle::BluetoothDiscovery(true))));
                self.draw_power_menu();
                true
            }
            OverlayHit::BtNearby(index) => {
                if let Some(device) = self.status.nearby.get(index) {
                    let toggle = Toggle::BluetoothPair { path: device.path.clone(), name: device.name.clone() };
                    let _ = self.requests.send(UiRequest::QuickSettings(QuickChange::Toggle(toggle)));
                }
                true
            }
            OverlayHit::PowerMenu => {
                self.quick_power_open = !self.quick_power_open;
                self.draw_power_menu();
                true
            }
            OverlayHit::QuickScreenshot => {
                // Quick settings closes first: it is not in the picture.
                let _ = self.requests.send(UiRequest::ShellKey(crate::shell_keys::KeyAction::Screenshot(crate::screenshot::ScreenshotKey::Ui)));
                false
            }
            OverlayHit::QuickSettingsApp => {
                open_settings_app();
                false
            }
            OverlayHit::Inside => true,
            _ => false,
        }
    }

    pub(super) fn draw_power_menu(&mut self) {
        let Some(mut overlay) = self.power_menu.take() else { return };
        if !(overlay.configured && overlay.width > 0) {
            self.power_menu = Some(overlay);
            return;
        }
        let (w, h) = (overlay.width as usize, overlay.height as usize);
        let scale = self.scales.of(overlay.layer.wl_surface());
        let mut model = self.quick_model(scale);
        (model.hover, model.pressed) = (overlay.state.hover, overlay.state.pressed);
        let Ok((buffer, data)) = self.buffers.create_buffer(overlay.layer.wl_surface(), Ui::draw_power_menu, physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888) else {
            self.power_menu = Some(overlay);
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale).mirrored(super::mirror::on(overlay.layer.wl_surface()));
        // A focus that is not there (yet, or any more: a list closed)
        // moves to the first control.
        let stops = model.stops();
        if overlay.state.focus.is_some_and(|f| !stops.contains(&f)) {
            overlay.state.focus = stops.first().copied();
        }
        let mut scroll = overlay.state.scroll;
        let painted = paint_quick(&mut canvas, (self.text.as_ref(), self.bold.as_ref()), (w, h), &model, overlay.state.focus, &mut scroll);
        overlay.state.scroll = scroll;
        overlay.hits = painted.hits;
        overlay.state.stops = painted.stops;
        overlay.state.list_area = painted.list_area;
        overlay.state.body_area = painted.body_area;
        self.atspi_record(overlay.layer.wl_surface(), vec![painted.node]);
        let surface = overlay.layer.wl_surface();
        self.scales.prepare(&self.qh, surface, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            overlay.layer.commit();
        }
        self.power_menu = Some(overlay);
    }

    /// What quick settings shows now.
    fn quick_model(&mut self, scale: f64) -> QuickModel {
        // The switches there are, with their labels and states.
        let mut switches: Vec<(Switch, String)> = Vec::new();
        if self.status.wifi.is_some() {
            switches.push((Switch::Wifi, tr("Wi-Fi").into()));
        }
        if self.status.bluetooth.is_some() {
            switches.push((Switch::Bluetooth, tr("Bluetooth").into()));
        }
        if self.status.wifi.is_some() || self.status.bluetooth.is_some() {
            switches.push((Switch::Airplane, tr("Airplane Mode").into()));
        }
        if self.status.accelerometer {
            switches.push((Switch::AutoRotate, tr("Auto Rotate").into()));
        }
        switches.extend([(Switch::NightLight, tr("Night Light").into()), (Switch::DarkStyle, tr("Dark Style").into()), (Switch::DoNotDisturb, tr("Do Not Disturb").into())]);
        let mut available: Vec<(Switch, String, String)> = switches.into_iter().map(|(s, name)| (s, name, s.id().to_owned())).collect();
        // Nothing from extensions while locked.
        let locked = self.lock.session.is_some();
        if !locked {
            available.extend(self.ext.toggles.iter().enumerate().map(|(i, t)| (Switch::Ext(i), t.title.clone(), t.full_id.clone())));
        }
        // In the order of `org.helix.shell` `quick-settings`; Power Mode
        // after them.
        let mut switches = ordered_switches(available, &self.panel_keys.quick);
        if !self.status.profiles.is_empty() {
            switches.push((Switch::PowerMode, tr("Power Mode").into()));
        }
        // Extensions' sliders, in that order too.
        let ext_sliders: Vec<usize> = if locked {
            Vec::new()
        } else {
            let mut order: Vec<usize> = Vec::new();
            for id in &self.panel_keys.quick {
                if let Some(i) = self.ext.sliders.iter().position(|s| s.full_id == *id || (s.development && !self.panel_keys.quick.contains(&s.full_id))) {
                    if !order.contains(&i) {
                        order.push(i);
                    }
                }
            }
            let extra: Vec<usize> = (0..self.ext.sliders.len()).filter(|i| self.ext.sliders[*i].development && !order.contains(i)).collect();
            order.extend(extra);
            order
        };
        let mut sliders = Vec::new();
        if let Some((percent, muted, max)) = self.quick.volume {
            let icon = self.symbolic_icon_px_grown(volume_icon(percent, muted), scale);
            sliders.push(Slider { name: tr("Volume").into(), id: "volume".into(), icon, value: percent, max, dim: muted, hit: Some(OverlayHit::VolumeSlider), mute: Some(muted) });
        }
        if let Some(percent) = self.quick.brightness {
            let icon = self.symbolic_icon_px_grown("display-brightness-symbolic", scale);
            sliders.push(Slider { name: tr("Brightness").into(), id: "brightness".into(), icon, value: percent, max: 100, dim: false, hit: Some(OverlayHit::BrightnessSlider), mute: None });
        }
        for &i in &ext_sliders {
            let Some(s) = self.ext.sliders.get(i).cloned() else { continue };
            let value = (s.value * 100.0).round() as u32;
            let icon = if s.icon.is_empty() { None } else { self.symbolic_icon_px_grown(&s.icon, scale) };
            sliders.push(Slider { name: s.title.clone(), id: format!("slider:{}", s.full_id), icon, value, max: 100, dim: !s.sensitive, hit: s.sensitive.then_some(OverlayHit::ExtSlider(i)), mute: None });
        }
        let profile_label_now = self.status.profile.as_deref().map(profile_label);
        let mut toggles: Vec<SwitchTile> = Vec::new();
        for (switch, name) in &switches {
            let ext = if let Switch::Ext(i) = switch { self.ext.toggles.get(*i).cloned() } else { None };
            let key = if switch.id().is_empty() { ext.as_ref().map_or_else(String::new, |t| t.full_id.clone()) } else { switch.id().to_owned() };
            let more = match switch {
                Switch::Wifi => Some((OverlayHit::Expand(*switch), tr("Wi-Fi Networks").to_owned())),
                Switch::Bluetooth => Some((OverlayHit::Expand(*switch), trf!("{name} Devices", name = name))),
                Switch::PowerMode => Some((OverlayHit::Expand(*switch), tr("Power Modes").to_owned())),
                Switch::Ext(i) if ext.as_ref().is_some_and(|t| t.has_details && t.sensitive) => Some((OverlayHit::ExtDetails(*i), trf!("{name} Settings", name = name))),
                _ => None,
            };
            let on = self.switch_on(*switch);
            // What it is on, under its title.
            let subtitle = match switch {
                Switch::Wifi => self.status.wifi.as_ref().and_then(|(on, ssid)| ssid.clone().filter(|_| *on)).unwrap_or_default(),
                Switch::Bluetooth => {
                    let connected: Vec<&str> = self.status.devices.iter().filter(|d| d.connected).map(|d| d.name.as_str()).collect();
                    match connected.as_slice() {
                        [] => String::new(),
                        [one] => (*one).to_owned(),
                        more => helix_i18n::trnf!("{n} device", "{n} devices", more.len()),
                    }
                }
                Switch::PowerMode => profile_label_now.clone().unwrap_or_default(),
                _ => ext.as_ref().map_or_else(String::new, |t| t.subtitle.clone()),
            };
            let icon_name = match switch {
                Switch::Wifi if on => crate::status::wifi_icon(self.status.networks.iter().find(|n| n.active).map_or(100, |n| n.strength)).to_owned(),
                Switch::Wifi => "network-wireless-disabled-symbolic".into(),
                Switch::Bluetooth if on => "bluetooth-active-symbolic".into(),
                Switch::Bluetooth => "bluetooth-disabled-symbolic".into(),
                Switch::Airplane => "airplane-mode-symbolic".into(),
                Switch::AutoRotate if on => "rotation-allowed-symbolic".into(),
                Switch::AutoRotate => "rotation-locked-symbolic".into(),
                Switch::NightLight => "night-light-symbolic".into(),
                Switch::DarkStyle => dark_style_icon().into(),
                Switch::DoNotDisturb => "notifications-disabled-symbolic".into(),
                Switch::PowerMode => format!("power-profile-{}-symbolic", self.status.profile.as_deref().unwrap_or("balanced")),
                Switch::Ext(_) => ext.as_ref().map_or_else(String::new, |t| t.icon.clone()),
            };
            // The icon in its words' color: dark on a light accent.
            let color = theme::control_text(theme::Look { on, ..Default::default() });
            let icon = if icon_name.is_empty() { None } else { self.symbolic_icon_color(&icon_name, f64::from(icon_px()), scale, color) };
            toggles.push(SwitchTile { switch: *switch, name: name.clone(), on, dim: ext.as_ref().is_some_and(|t| !t.sensitive), subtitle, icon, more, key });
        }
        let profiles: Vec<(String, bool)> = self.status.profiles.iter().map(|p| (profile_label(p), self.status.profile.as_deref() == Some(p.as_str()))).collect();
        // The open list.
        let list_open = self.quick_list.filter(|s| toggles.iter().any(|t| t.switch == *s));
        let row = |name: String, hit: Option<OverlayHit>| ListRow { icon: None, name, secured: false, connected: false, checked: false, hit, dim: hit.is_none() };
        let rows: Vec<ListRow> = match list_open {
            Some(Switch::Wifi) => {
                let networks = self.status.networks.clone();
                networks
                    .iter()
                    .enumerate()
                    .map(|(i, n)| ListRow { icon: self.symbolic_icon_px_grown(crate::status::wifi_icon(n.strength), scale), name: n.ssid.clone(), secured: n.secured, connected: n.active, checked: false, hit: Some(OverlayHit::Network(i)), dim: false })
                    .collect()
            }
            Some(Switch::Bluetooth) => {
                let mut rows: Vec<ListRow> = self.status.devices.iter().enumerate().map(|(i, d)| ListRow { connected: d.connected, ..row(d.name.clone(), Some(OverlayHit::BtDevice(i))) }).collect();
                if !self.status.devices.is_empty() {
                    rows.push(row(tr("Send Files…").into(), Some(OverlayHit::BtSendFiles)));
                }
                if self.bt_discovery || self.status.discovering {
                    rows.push(row(tr("Nearby Devices").into(), None));
                    if self.status.nearby.is_empty() {
                        rows.push(row(tr("Searching…").into(), None));
                    }
                    rows.extend(self.status.nearby.iter().enumerate().map(|(i, d)| row(d.name.clone(), Some(OverlayHit::BtNearby(i)))));
                } else {
                    rows.push(row(tr("Pair New Device…").into(), Some(OverlayHit::PairNew)));
                }
                rows
            }
            Some(Switch::PowerMode) => {
                let names = self.status.profiles.clone();
                names
                    .iter()
                    .zip(&profiles)
                    .enumerate()
                    .map(|(i, (p, (label, active)))| ListRow { icon: self.symbolic_icon_px_grown(&format!("power-profile-{p}-symbolic"), scale), checked: *active, ..row(label.clone(), Some(OverlayHit::Profile(i))) })
                    .collect()
            }
            _ => Vec::new(),
        };
        let lock = if rows.iter().any(|r| r.secured) { self.symbolic_icon_px_grown("channel-secure-symbolic", scale) } else { None };
        // The top row: the battery, then Screenshot, Settings, Lock and
        // Power (its list holds the other power actions).
        let battery = self.status.battery.clone().map(|(icon, percent)| (self.symbolic_icon_px_grown(&icon, scale), helix_i18n::format_percent(percent.round())));
        let items = power_menu_items(self.power, !crate::locked_down("disable-lock-screen"), !crate::locked_down("disable-log-out"), self.user_switch && crate::user_switch::allowed());
        let mut buttons = Vec::new();
        if !locked {
            buttons.push(TopButton { hit: OverlayHit::QuickScreenshot, icon: self.symbolic_icon_px_grown("camera-photo-symbolic", scale), name: tr("Take Screenshot").into() });
            if settings_app().is_some() {
                buttons.push(TopButton { hit: OverlayHit::QuickSettingsApp, icon: self.symbolic_icon_px_grown("emblem-system-symbolic", scale), name: tr("Settings").into() });
            }
        }
        if items.iter().any(|(_, a)| *a == PowerAction::Lock) {
            buttons.push(TopButton { hit: OverlayHit::Power(PowerAction::Lock), icon: self.symbolic_icon_px_grown("system-lock-screen-symbolic", scale), name: tr("Lock Screen").into() });
        }
        let actions: Vec<(&'static str, PowerAction)> = items.into_iter().filter(|(_, a)| *a != PowerAction::Lock).collect();
        if !actions.is_empty() {
            buttons.push(TopButton { hit: OverlayHit::PowerMenu, icon: self.symbolic_icon_px_grown("system-shutdown-symbolic", scale), name: tr("Power Off / Log Out").into() });
        }
        let power_open = self.quick_power_open && !actions.is_empty();
        QuickModel { battery, buttons, power_open, actions, sliders, toggles, list_open, rows, lock, profiles, status_shapes: helix_draw::status_shapes(), hover: None, pressed: None }
    }

    /// Pointer input on quick settings other than clicks (the wheel);
    /// true if it was for it.
    pub(super) fn quick_pointer(&mut self, event: &PointerEvent) -> bool {
        if !self.power_menu.as_ref().is_some_and(|m| m.layer.wl_surface() == &event.surface) {
            return false;
        }
        match event.kind {
            PointerEventKind::Axis { vertical, .. } if vertical.absolute != 0.0 || vertical.discrete != 0 => {
                let (x, y) = event.position;
                self.quick_scroll(x, y, vertical.absolute > 0.0 || vertical.discrete > 0);
                true
            }
            _ => false,
        }
    }

    /// The wheel over quick settings at (x, y): the list scrolls when over
    /// it, else the body (when it does not fit).
    pub(super) fn quick_scroll(&mut self, x: f64, y: f64, down: bool) {
        let Some(menu) = self.power_menu.as_mut() else { return };
        let step = |v: i32| if down { v } else { -v };
        if menu.state.list_area.is_some_and(|r| r.contains(x, y)) {
            menu.state.scroll.list = (menu.state.scroll.list as i32 + step(1)).max(0) as usize;
        } else {
            menu.state.scroll.body = (menu.state.scroll.body + step(helix_draw::theme::grow(48))).max(0);
        }
        self.draw_power_menu();
    }

    // ----- Keyboard -------------------------------------------------------

    /// Move the focus to `hit` and show it.
    fn quick_focus(&mut self, hit: Option<OverlayHit>) {
        let Some(menu) = self.power_menu.as_mut() else { return };
        if menu.state.focus != hit {
            menu.state.focus = hit;
            self.draw_power_menu();
        }
    }

    /// A slider's value moved by the keyboard to `value`.
    fn quick_slide(&mut self, hit: OverlayHit, value: u32) {
        match hit {
            OverlayHit::VolumeSlider => {
                if let Some((_, muted, max)) = self.quick.volume {
                    let value = value.min(max);
                    self.quick.volume = Some((value, muted, max));
                    let _ = self.requests.send(UiRequest::QuickSettings(QuickChange::Volume(value)));
                }
            }
            OverlayHit::BrightnessSlider => {
                let value = value.min(100);
                self.quick.brightness = Some(value);
                let _ = self.requests.send(UiRequest::QuickSettings(QuickChange::Brightness(value)));
            }
            OverlayHit::ExtSlider(i) => {
                if let Some(s) = self.ext.sliders.get_mut(i) {
                    s.value = f64::from(value.min(100)) / 100.0;
                    let _ = self.requests.send(UiRequest::ExtAction(s.full_id.clone(), crate::ext_host::Action::SetValue(s.value)));
                }
            }
            _ => return,
        }
        self.draw_power_menu();
    }

    /// A slider's value and range.
    fn quick_slider_value(&self, hit: OverlayHit) -> Option<(u32, u32)> {
        match hit {
            OverlayHit::VolumeSlider => self.quick.volume.map(|(v, _, max)| (v, max)),
            OverlayHit::BrightnessSlider => self.quick.brightness.map(|v| (v, 100)),
            OverlayHit::ExtSlider(i) => self.ext.sliders.get(i).map(|s| ((s.value * 100.0).round() as u32, 100)),
            _ => None,
        }
    }

    /// Close quick settings; the panel item it was opened from gets the
    /// focus back.
    pub(super) fn close_quick_settings(&mut self) {
        let Some(menu) = self.power_menu.take() else { return };
        if let Opened::Panel(i) = menu.state.opened {
            self.refocus_panel(i);
        }
        drop(menu);
        self.quick_discovery_check();
        self.draw_panel();
    }

    pub(super) fn quick_key(&mut self, event: &KeyEvent) {
        let Some(menu) = self.power_menu.as_ref() else { return };
        let stops = menu.state.stops.clone();
        let at = menu.state.focus.and_then(|f| stops.iter().position(|(_, h)| *h == f));
        let focused = at.map(|i| stops[i]);
        let slider = focused.and_then(|(_, hit)| self.quick_slider_value(hit).map(|v| (hit, v)));
        match quick_key_action(event.keysym, &stops, at, slider) {
            QuickKey::Close => {
                // A list closes first, its "›" taking the focus.
                if let Some(open) = self.quick_list.take() {
                    self.quick_discovery_check();
                    if let Some(menu) = self.power_menu.as_mut() {
                        menu.state.focus = Some(OverlayHit::Expand(open));
                    }
                    self.draw_power_menu();
                } else {
                    self.close_quick_settings();
                }
            }
            QuickKey::Focus(i) => self.quick_focus(stops.get(i).map(|(_, h)| *h)),
            QuickKey::Slide(hit, value) => self.quick_slide(hit, value),
            QuickKey::Activate => {
                let Some((rect, hit)) = focused else { return };
                if let OverlayHit::Expand(switch) = hit {
                    // Opening a list focuses its first row.
                    let opening = self.quick_list != Some(switch);
                    self.quick_click(hit, f64::from(rect.x), rect);
                    if opening {
                        let first = self.power_menu.as_ref().and_then(|m| m.state.stops.iter().map(|(_, h)| *h).find(|h| is_list_row(*h)));
                        self.quick_focus(first.or(Some(hit)));
                    }
                    return;
                }
                let x = f64::from(rect.x) + f64::from(rect.w) / 2.0;
                if !self.quick_click(hit, x, rect) {
                    self.close_quick_settings();
                }
            }
            QuickKey::None => {}
        }
    }
}

/// How quick settings was opened, and so where the focus goes when it
/// closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Opened {
    /// By a click: it takes the keyboard only when clicked.
    #[default]
    Pointer,
    /// By a key (Super+S): it takes the keyboard, focusing its first
    /// control.
    Keyboard,
    /// By Enter on the panel item with this index (`switch-panels`): as
    /// `Keyboard`, and the panel item gets the focus back.
    Panel(usize),
}

impl Opened {
    pub(super) fn keyboard(self) -> bool {
        self != Opened::Pointer
    }

    /// The layer surface's keyboard interactivity.
    pub(super) fn interactivity(self) -> KeyboardInteractivity {
        if self.keyboard() { KeyboardInteractivity::Exclusive } else { KeyboardInteractivity::OnDemand }
    }
}

/// Quick settings' keyboard state.
#[derive(Debug, Default)]
pub(super) struct QuickNav {
    pub opened: Opened,
    /// The control with the keyboard focus.
    pub focus: Option<OverlayHit>,
    /// The controls the keyboard reaches, in order, where the last paint
    /// put them.
    pub stops: Vec<(Rect, OverlayHit)>,
    /// How far the open list and the menu's body are scrolled.
    pub scroll: QuickScroll,
    /// Where the last paint put the list and the scrolled body.
    pub list_area: Option<Rect>,
    pub body_area: Rect,
    /// The control under the pointer, and the one pressed.
    pub hover: Option<OverlayHit>,
    pub pressed: Option<OverlayHit>,
}

/// The list shows at most this many rows; more scroll inside it.
pub(super) const LIST_MAX: usize = 6;

/// Scroll positions in quick settings.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct QuickScroll {
    /// List rows scrolled past.
    pub list: usize,
    /// Logical px of the body (sliders to Power Mode) scrolled past, when
    /// the menu is taller than the screen; the power actions stay.
    pub body: i32,
}

/// A slider's bar (it acts from the press on, following the pointer).
pub(super) fn is_slider(hit: OverlayHit) -> bool {
    matches!(hit, OverlayHit::VolumeSlider | OverlayHit::BrightnessSlider | OverlayHit::ExtSlider(_))
}

/// A row of the open Wi-Fi or Bluetooth list.
fn is_list_row(hit: OverlayHit) -> bool {
    matches!(hit, OverlayHit::Network(_) | OverlayHit::BtDevice(_) | OverlayHit::BtSendFiles | OverlayHit::PairNew | OverlayHit::BtNearby(_))
}

/// What a key does in quick settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum QuickKey {
    None,
    Close,
    /// Focus the stop at this index.
    Focus(usize),
    /// Set a slider to a value.
    Slide(OverlayHit, u32),
    Activate,
}

/// A direction for arrow keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Dir {
    Up,
    Down,
    Left,
    Right,
}

/// The stop nearest `from` in direction `dir` (by centers; a stop
/// straight that way beats one further aside), if any.
pub(super) fn spatial(stops: &[(Rect, OverlayHit)], from: usize, dir: Dir) -> Option<usize> {
    let center = |r: &Rect| (f64::from(r.x) + f64::from(r.w) / 2.0, f64::from(r.y) + f64::from(r.h) / 2.0);
    let (fx, fy) = center(&stops.get(from)?.0);
    stops
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != from)
        .filter_map(|(i, (r, _))| {
            let (x, y) = center(r);
            let (along, aside) = match dir {
                Dir::Up => (fy - y, (x - fx).abs()),
                Dir::Down => (y - fy, (x - fx).abs()),
                Dir::Left => (fx - x, (y - fy).abs()),
                Dir::Right => (x - fx, (y - fy).abs()),
            };
            // Left and right stay on the row.
            let sideways = matches!(dir, Dir::Left | Dir::Right);
            (along > 0.5 && !(sideways && aside > f64::from(r.h) / 2.0)).then_some((i, along + 2.0 * aside))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i)
}

/// The keyboard step of a slider (5 % of its range, at least 1).
fn slider_step(max: u32) -> u32 {
    (max / 20).max(1)
}

/// What `keysym` does with `stops` focused at `at`; `slider` is the
/// focused slider with its (value, max).
pub(super) fn quick_key_action(keysym: Keysym, stops: &[(Rect, OverlayHit)], at: Option<usize>, slider: Option<(OverlayHit, (u32, u32))>) -> QuickKey {
    let n = stops.len();
    if keysym == Keysym::Escape {
        return QuickKey::Close;
    }
    if n == 0 {
        return QuickKey::None;
    }
    // Nothing focused yet (opened by a click): the first key that moves
    // focuses the first (or last) control.
    let Some(at) = at else {
        return match keysym {
            Keysym::Tab | Keysym::Down | Keysym::Right | Keysym::Home => QuickKey::Focus(0),
            Keysym::ISO_Left_Tab | Keysym::Up | Keysym::Left | Keysym::End => QuickKey::Focus(n - 1),
            _ => QuickKey::None,
        };
    };
    if let Some((hit, (value, max))) = slider {
        let step = slider_step(max);
        let slide = |v: u32| QuickKey::Slide(hit, v.min(max));
        match keysym {
            Keysym::Left | Keysym::KP_Left => return slide(value.saturating_sub(step)),
            Keysym::Right | Keysym::KP_Right => return slide(value + step),
            Keysym::Page_Down => return slide(value.saturating_sub(2 * step)),
            Keysym::Page_Up => return slide(value + 2 * step),
            Keysym::Home => return slide(0),
            Keysym::End => return slide(max),
            _ => {}
        }
    }
    let arrow = |dir: Dir| QuickKey::Focus(spatial(stops, at, dir).unwrap_or(at));
    match keysym {
        Keysym::Tab => QuickKey::Focus((at + 1) % n),
        Keysym::ISO_Left_Tab => QuickKey::Focus((at + n - 1) % n),
        Keysym::Down | Keysym::KP_Down => arrow(Dir::Down),
        Keysym::Up | Keysym::KP_Up => arrow(Dir::Up),
        Keysym::Right | Keysym::KP_Right => arrow(Dir::Right),
        Keysym::Left | Keysym::KP_Left => arrow(Dir::Left),
        Keysym::Home => QuickKey::Focus(0),
        Keysym::End => QuickKey::Focus(n - 1),
        Keysym::Return | Keysym::KP_Enter | Keysym::space if slider.is_none() => QuickKey::Activate,
        _ => QuickKey::None,
    }
}

/// A slider row of quick settings.
pub(super) struct Slider {
    /// Its accessible name ("Volume").
    pub name: String,
    pub id: String,
    /// The icon at its start (the volume's level or muted, brightness).
    pub icon: Option<Image>,
    pub value: u32,
    pub max: u32,
    pub dim: bool,
    /// Its bar's hit (none: it cannot be changed).
    pub hit: Option<OverlayHit>,
    /// The volume's mute state (its icon is the mute button).
    pub mute: Option<bool>,
}

/// A switch of quick settings: a pill with an icon, a title and a
/// subtitle.
pub(super) struct SwitchTile {
    pub switch: Switch,
    pub name: String,
    pub on: bool,
    /// An extension's toggle that cannot be changed now.
    pub dim: bool,
    /// What it is on ("Home" for Wi-Fi, the power mode); empty: none.
    pub subtitle: String,
    /// Its icon, in the color of its words.
    pub icon: Option<Image>,
    /// Its "›" (a list or an extension's menu) and that part's name.
    pub more: Option<(OverlayHit, String)>,
    /// Its id on the accessibility bus.
    pub key: String,
}

/// A round button of quick settings' top row.
pub(super) struct TopButton {
    pub hit: OverlayHit,
    pub icon: Option<Image>,
    /// Its name (on the accessibility bus).
    pub name: String,
}

/// Everything quick settings shows.
pub(super) struct QuickModel {
    /// The battery: its icon and its charge in words ("80 %").
    pub battery: Option<(Option<Image>, String)>,
    /// The top row's buttons: Screenshot, Settings, Lock, Power.
    pub buttons: Vec<TopButton>,
    /// The power actions, shown under the top row when Power is open.
    pub power_open: bool,
    pub actions: Vec<(&'static str, PowerAction)>,
    pub sliders: Vec<Slider>,
    pub toggles: Vec<SwitchTile>,
    pub list_open: Option<Switch>,
    pub rows: Vec<ListRow>,
    /// The secured-network sign, and a chosen row's check mark.
    pub lock: Option<Image>,
    /// Power profiles (label, active).
    pub profiles: Vec<(String, bool)>,
    pub status_shapes: bool,
    /// The control under the pointer, and the one pressed.
    pub hover: Option<OverlayHit>,
    pub pressed: Option<OverlayHit>,
}

impl QuickModel {
    /// The controls the keyboard reaches, in order: the top row's
    /// buttons, the power actions (when open), sliders (the mute first),
    /// switches (each then its "›"), the open list's rows.
    pub(super) fn stops(&self) -> Vec<OverlayHit> {
        let mut out: Vec<OverlayHit> = self.buttons.iter().map(|b| b.hit).collect();
        if self.power_open {
            out.extend(self.actions.iter().map(|(_, a)| OverlayHit::Power(*a)));
        }
        for s in &self.sliders {
            if s.mute.is_some() {
                out.push(OverlayHit::Mute);
            }
            out.extend(s.hit);
        }
        for t in &self.toggles {
            out.push(OverlayHit::Switch(t.switch));
            out.extend(t.more.as_ref().map(|m| m.0));
        }
        if self.list_open.is_some() {
            out.extend(self.rows.iter().filter_map(|r| r.hit));
        }
        out
    }

    /// How the control `hit` looks under the pointer.
    fn look(&self, hit: OverlayHit, on: bool) -> theme::Look {
        theme::Look { on, hovered: self.hover == Some(hit), pressed: self.pressed == Some(hit) }
    }
}

/// What painting quick settings gives back.
pub(super) struct QuickPainted {
    pub hits: Vec<(Rect, OverlayHit)>,
    pub stops: Vec<(Rect, OverlayHit)>,
    pub node: crate::a11y::Node,
    /// The list's window and the body's (they scroll).
    pub list_area: Option<Rect>,
    pub body_area: Rect,
    /// Where each control was drawn (scrolled; perhaps out of sight).
    pub drawn: Vec<(Rect, OverlayHit)>,
}

/// Paint quick settings (`m`) on a `w`×`h` overlay with the keyboard
/// focus on `focus`, scrolled by `scroll` (kept in range, and moved so the
/// focused control shows).
pub(super) fn paint_quick(canvas: &mut Canvas, fonts: (Option<&Text>, Option<&Text>), (w, h): (usize, usize), m: &QuickModel, focus: Option<OverlayHit>, scroll: &mut QuickScroll) -> QuickPainted {
    // A focused list row inside the list's window.
    if let Some(row) = focus.and_then(|f| m.rows.iter().position(|r| r.hit == Some(f))) {
        if row < scroll.list {
            scroll.list = row;
        } else if row >= scroll.list + LIST_MAX {
            scroll.list = row + 1 - LIST_MAX;
        }
    }
    let painted = paint_quick_at(canvas, fonts, (w, h), m, focus, scroll);
    // A focused control in the scrolled body outside its window: scroll
    // to it.
    let body = painted.body_area;
    let shown = |r: &Rect| r.y >= body.y && r.y + r.h <= body.y + body.h;
    let fixed = |f: Option<OverlayHit>| f.is_some_and(|f| m.buttons.iter().any(|b| b.hit == f));
    let Some(r) = focus.and_then(|f| painted.drawn.iter().find(|(_, h)| *h == f)).map(|(r, _)| *r).filter(|r| !shown(r) && !fixed(focus)) else { return painted };
    scroll.body += if r.y < body.y { r.y - body.y - 4 } else { r.y + r.h - (body.y + body.h) + 4 };
    paint_quick_at(canvas, fonts, (w, h), m, focus, scroll)
}

/// The size of quick settings' round buttons and of a toggle's height
/// (logical px, before the text scale).
const ROUND: i32 = 36;
const TOGGLE_H: i32 = 56;
/// A toggle's "›" part.
const MORE_W: i32 = 32;

fn paint_quick_at(canvas: &mut Canvas, (text, bold): (Option<&Text>, Option<&Text>), (w, h): (usize, usize), m: &QuickModel, focus: Option<OverlayHit>, scroll: &mut QuickScroll) -> QuickPainted {
    canvas.fill(0x00000000);
    // Lengths that hold text grow with the text scale.
    let grow = helix_draw::theme::grow;
    let (menu_w, row_h, list_row_h, tile_h, item_h) = (grow(MENU_W), grow(ROW_H), grow(LIST_ROW_H), grow(TOGGLE_H), grow(ACTION_H));
    let top_h = grow(ROUND) + 8;
    let tile_step = tile_h + 8;
    let switch_rows = m.toggles.len().div_ceil(2) as i32;
    // At most LIST_MAX rows of the list show; it scrolls.
    let list_shown = m.rows.len().clamp(1, LIST_MAX);
    scroll.list = scroll.list.min(m.rows.len().saturating_sub(LIST_MAX));
    let list_h = if m.list_open.is_some() { 8 + list_shown as i32 * list_row_h + 8 } else { 0 };
    let actions_h = if m.power_open { 8 + m.actions.len() as i32 * item_h + 8 + 8 } else { 0 };
    // The top row stays; the body under it (the power actions to the open
    // list) scrolls when the whole does not fit under the panel.
    let body_h = actions_h + m.sliders.len() as i32 * row_h + 4 + switch_rows * tile_step + list_h + PAD;
    let mx = w as i32 - menu_w - 6;
    let my = panel_height() as i32 + 4;
    let room = (h as i32 - my - 8).max(PAD + top_h + row_h);
    let body_view = body_h.min(room - PAD - top_h);
    scroll.body = scroll.body.clamp(0, body_h - body_view);
    let mh = PAD + top_h + body_view;
    theme::popover(canvas, mx, my, menu_w, mh);
    let mut hits: Vec<(Rect, OverlayHit)> = Vec::new();
    // Where each focusable control is drawn (its ring goes around that),
    // and the ring's radius.
    let mut drawn: Vec<(Rect, OverlayHit)> = Vec::new();
    let mut radii: Vec<(OverlayHit, f64)> = Vec::new();
    let fg_for = |look: theme::Look, dim: bool| if dim && !look.on { DIM_FG } else { theme::control_text(look) };

    // The top row: the battery, then round buttons at the end.
    let d = grow(ROUND);
    let ty = my + PAD;
    if let Some((icon, charge)) = &m.battery {
        let mut x = mx + PAD + 4;
        if let Some(icon) = icon {
            let ip = icon_px();
            canvas.image(f64::from(x), f64::from(ty + (d - ip) / 2), f64::from(ip), f64::from(ip), icon);
            x += ip + 6;
        }
        label(text, canvas, charge, x as f32, baseline(text, ty, d, size::LABEL), size::LABEL, FG, (menu_w / 2 - x + mx) as f32);
    }
    let mut bx = mx + menu_w - PAD - d;
    for b in m.buttons.iter().rev() {
        let r = Rect { x: bx, y: ty, w: d, h: d };
        let (cx, cy) = (f64::from(r.x) + f64::from(d) / 2.0, f64::from(r.y) + f64::from(d) / 2.0);
        let look = m.look(b.hit, b.hit == OverlayHit::PowerMenu && m.power_open);
        theme::round_control(canvas, cx, cy, f64::from(d), look, false);
        if let Some(icon) = &b.icon {
            let ip = f64::from(icon_px());
            canvas.image(cx - ip / 2.0, cy - ip / 2.0, ip, ip, icon);
        }
        hits.push((r, b.hit));
        drawn.push((r, b.hit));
        radii.push((b.hit, f64::from(d) / 2.0));
        bx -= d + 8;
    }

    let body_top = my + PAD + top_h;
    let body_area = Rect { x: mx, y: body_top, w: menu_w, h: body_view };
    canvas.clip(mx, body_top, menu_w, body_view);
    let mut list_view = None;
    let mut y = body_top - scroll.body;

    // The power actions, in a well, while Power is open.
    if m.power_open {
        let (lx, lw) = (mx + PAD, menu_w - 2 * PAD);
        theme::well(canvas, lx, y + 4, lw, actions_h - 16);
        for (i, (name, action)) in m.actions.iter().enumerate() {
            let hit = OverlayHit::Power(*action);
            let r = Rect { x: lx + 4, y: y + 8 + i as i32 * item_h, w: lw - 8, h: item_h };
            theme::row(canvas, r.x, r.y, r.w, r.h, m.look(hit, false));
            label(text, canvas, name, (r.x + 12) as f32, baseline(text, r.y, r.h, size::BODY), size::BODY, FG, (r.w - 24) as f32);
            hits.push((r, hit));
            drawn.push((r, hit));
            radii.push((hit, theme::RADIUS_ROW));
        }
        y += actions_h;
    }

    // Sliders: an icon (the volume's mutes), the bar, the value.
    let value_w = grow(44);
    for s in &m.sliders {
        let icon_r = Rect { x: mx + PAD, y: y + (row_h - d) / 2, w: d, h: d };
        let (cx, cy) = (f64::from(icon_r.x) + f64::from(d) / 2.0, f64::from(icon_r.y) + f64::from(d) / 2.0);
        if s.mute.is_some() {
            theme::round_flat(canvas, cx, cy, f64::from(d), m.look(OverlayHit::Mute, false), false);
            hits.push((icon_r, OverlayHit::Mute));
            drawn.push((icon_r, OverlayHit::Mute));
            radii.push((OverlayHit::Mute, f64::from(d) / 2.0));
        }
        if let Some(icon) = &s.icon {
            let ip = f64::from(icon_px());
            canvas.image_alpha(cx - ip / 2.0, cy - ip / 2.0, ip, ip, icon, if s.dim { 0.6 } else { 1.0 });
        }
        let bar = Rect { x: icon_r.x + d + 6, y: y + row_h / 2 - 9, w: menu_w - 2 * PAD - d - 6 - value_w, h: 18 };
        let look = s.hit.map_or_else(theme::Look::default, |hit| m.look(hit, false));
        theme::slider_look(canvas, (bar.x, bar.y, bar.w, bar.h), f64::from(s.value.min(s.max)) / f64::from(s.max.max(1)), s.dim, look);
        let pct = helix_i18n::format_percent(f64::from(s.value));
        let pw = text.map_or(0.0, |t| t.width(&pct, size::CAPTION)).min((value_w - 4) as f32);
        label(text, canvas, &pct, (mx + menu_w - PAD) as f32 - pw, baseline(text, y, row_h, size::CAPTION), size::CAPTION, DIM_FG, (value_w - 4) as f32);
        if let Some(hit) = s.hit {
            // The whole row height is clickable on the bar.
            hits.push((Rect { x: bar.x, y, w: bar.w, h: row_h }, hit));
            drawn.push((Rect { x: bar.x - 2, y: bar.y - 4, w: bar.w + 4, h: bar.h + 8 }, hit));
            radii.push((hit, 13.0));
        }
        y += row_h;
    }
    y += 4;

    // Switches: pills, two to a row: an icon, a bold title and a
    // subtitle, and a "›" part for a list or an extension's menu.
    let tw = (menu_w - 2 * PAD - 8) / 2;
    for (i, t) in m.toggles.iter().enumerate() {
        let (row, col) = (i as i32 / 2, i as i32 % 2);
        let r = Rect { x: mx + PAD + col * (tw + 8), y: y + row * tile_step, w: tw, h: tile_h };
        let hit = OverlayHit::Switch(t.switch);
        let more_w = if t.more.is_some() { grow(MORE_W) } else { 0 };
        let main = Rect { w: r.w - more_w, ..r };
        let look = m.look(hit, t.on);
        let more_look = t.more.as_ref().map(|(more, _)| m.look(*more, t.on));
        theme::toggle_pill(canvas, (f64::from(r.x), f64::from(r.y), f64::from(r.w), f64::from(r.h)), f64::from(more_w), look, more_look);
        let fg = fg_for(look, t.dim);
        let mut x = main.x + 14;
        // `show-status-shapes`: a bar for on, a ring for off.
        if m.status_shapes {
            canvas.status_shape(f64::from(x) + 4.0, f64::from(r.y) + f64::from(r.h) / 2.0, 10.0, t.on, fg);
            x += 14;
        }
        if let Some(icon) = &t.icon {
            let ip = icon_px();
            canvas.image_alpha(f64::from(x), f64::from(r.y + (r.h - ip) / 2), f64::from(ip), f64::from(ip), icon, if t.dim && !t.on { 0.6 } else { 1.0 });
        }
        x += icon_px() + 8;
        let room = (main.x + main.w - x - 6) as f32;
        let title_face = bold.or(text);
        if t.subtitle.is_empty() {
            label(title_face, canvas, &t.name, x as f32, baseline(title_face, r.y, r.h, size::HEADING), size::HEADING, fg, room);
        } else {
            // The subtitle under the title.
            let mid = r.y + r.h / 2;
            label(title_face, canvas, &t.name, x as f32, (mid - 2) as f32, size::HEADING, fg, room);
            let subtitle_fg = if t.on { theme::control_text(look) } else { DIM_FG };
            label(text, canvas, &t.subtitle, x as f32, (mid + grow(15)) as f32, size::CAPTION, subtitle_fg, room);
        }
        if let Some((more, _)) = &t.more {
            let a = Rect { x: r.x + r.w - more_w, y: r.y, w: more_w, h: r.h };
            let towards = if m.list_open == Some(t.switch) { theme::Towards::Down } else { theme::Towards::Right };
            theme::chevron(canvas, f64::from(a.x) + f64::from(a.w) / 2.0 - 1.0, f64::from(a.y) + f64::from(a.h) / 2.0, 10.0, towards, fg_for(look, false));
            hits.push((a, *more));
            drawn.push((a, *more));
            radii.push((*more, f64::from(r.h) / 2.0));
        }
        hits.push((main, hit));
        drawn.push((main, hit));
        radii.push((hit, f64::from(r.h) / 2.0));
    }
    y += switch_rows * tile_step;

    // The open list.
    if let Some(open) = m.list_open {
        let lx = mx + PAD;
        let lw = menu_w - 2 * PAD;
        theme::well(canvas, lx, y + 4, lw, list_h - 8);
        // Every row is laid out (the keyboard reaches them all); the clip
        // shows the window.
        let mut ry = y + 8 - scroll.list as i32 * list_row_h;
        if m.rows.is_empty() {
            let empty = if open == Switch::Wifi { tr("No Networks") } else { tr("No Devices") };
            label(text, canvas, empty, (lx + 12) as f32, baseline(text, ry, list_row_h, size::CAPTION), size::CAPTION, DIM_FG, (lw - 24) as f32);
        }
        let list_area = Rect { x: lx, y: y + 4, w: lw, h: list_h - 8 };
        list_view = Some(list_area);
        canvas.clip(lx, (y + 4).max(body_top), lw, (list_h - 8).min(body_top + body_view - (y + 4).max(body_top)).max(0));
        for ListRow { icon, name, secured, connected, checked, hit, dim } in &m.rows {
            let r = Rect { x: lx + 4, y: ry, w: lw - 8, h: list_row_h };
            if let Some(hit) = hit {
                theme::row(canvas, r.x, r.y, r.w, r.h, m.look(*hit, false));
            }
            let mut tx = r.x + 8;
            if let Some(icon) = icon {
                let ip = icon_px();
                canvas.image(f64::from(tx), f64::from(r.y + (r.h - ip) / 2), f64::from(ip), f64::from(ip), icon);
                tx += ip + 8;
            }
            let state_w = if *connected { grow(76) } else { 0 };
            let lock_w = if *secured { 22 } else { 0 };
            let check_w = if *checked { 22 } else { 0 };
            label(text, canvas, name, tx as f32, baseline(text, r.y, r.h, size::LABEL), size::LABEL, if *dim { DIM_FG } else { FG }, (r.x + r.w - tx - state_w - lock_w - check_w - 8) as f32);
            if *secured {
                if let Some(lock) = &m.lock {
                    canvas.image(f64::from(r.x + r.w - state_w - lock_w - 4), f64::from(r.y + (r.h - 14) / 2), 14.0, 14.0, lock);
                }
            }
            if *connected {
                label(text, canvas, tr("Connected"), (r.x + r.w - state_w) as f32, baseline(text, r.y, r.h, size::CAPTION), size::CAPTION, theme::text_accent(), (state_w - 8) as f32);
            }
            if *checked {
                // The chosen one: a check mark at the end.
                let (cx, cy) = (f64::from(r.x + r.w - 14), f64::from(r.y + r.h / 2));
                canvas.line((cx - 5.0, cy), (cx - 2.0, cy + 3.5), 1.8, theme::text_accent());
                canvas.line((cx - 2.0, cy + 3.5), (cx + 5.0, cy - 4.0), 1.8, theme::text_accent());
            }
            if let Some(hit) = hit {
                hits.push((r, *hit));
                drawn.push((r, *hit));
                radii.push((*hit, theme::RADIUS_ROW));
            }
            ry += list_row_h;
        }
        // Where it is in the list, when it scrolls.
        if m.rows.len() > LIST_MAX {
            let track = list_area.h - 8;
            let thumb = (track * LIST_MAX as i32 / m.rows.len() as i32).max(12);
            let at = (track - thumb) * scroll.list as i32 / (m.rows.len() - LIST_MAX) as i32;
            canvas.round_rect(f64::from(lx + lw - 5), f64::from(list_area.y + 4 + at), 3.0, f64::from(thumb), 1.5, theme::TEXT_DIM);
        }
        canvas.clip(mx, body_top, menu_w, body_view);
    }
    canvas.unclip();
    // Where the body is, when it scrolls.
    if body_h > body_view {
        let thumb = (body_view * body_view / body_h).max(16);
        let at = (body_view - thumb) * scroll.body / (body_h - body_view).max(1);
        canvas.round_rect(f64::from(mx + menu_w - 5), f64::from(body_top + at), 3.0, f64::from(thumb), 1.5, theme::TEXT_DIM);
    }
    // Clicks only where the body shows (the list's rows only in its
    // window); the top row always shows.
    let list_window = list_view.map(|r: Rect| Rect { y: r.y.max(body_top), h: (r.y + r.h).min(body_top + body_view) - r.y.max(body_top), ..r });
    let top: Vec<OverlayHit> = m.buttons.iter().map(|b| b.hit).collect();
    // (Cut to what shows: a row half out of sight is clicked on its
    // visible half only.)
    let cut = |r: Rect, area: Rect| {
        let (y0, y1) = (r.y.max(area.y), (r.y + r.h).min(area.y + area.h));
        (y1 > y0).then_some(Rect { y: y0, h: y1 - y0, ..r })
    };
    let mut hits: Vec<(Rect, OverlayHit)> = hits
        .into_iter()
        .filter_map(|(r, hit)| {
            if top.contains(&hit) {
                return Some((r, hit));
            }
            let r = cut(r, body_area)?;
            let r = if is_list_row(hit) { cut(r, list_window?)? } else { r };
            Some((r, hit))
        })
        .collect();
    // The focus ring, following the control's corners.
    if let Some((r, hit)) = focus.and_then(|f| drawn.iter().find(|(_, h)| *h == f)) {
        let radius = radii.iter().find(|(h, _)| h == hit).map_or(theme::RADIUS_BUTTON, |(_, r)| *r);
        theme::focus_ring_round(canvas, f64::from(r.x), f64::from(r.y), f64::from(r.w), f64::from(r.h), radius);
    }
    // The keyboard's stops, in order, where their hits are.
    // (Controls scrolled out of sight stay stops: focusing one scrolls
    // to it.)
    let stops: Vec<(Rect, OverlayHit)> = m.stops().into_iter().filter_map(|stop| hits.iter().chain(&drawn).find(|(_, h)| *h == stop).copied()).collect();
    let bounds = Rect { x: mx, y: my, w: menu_w, h: mh };
    let node = quick_node(m, &hits, bounds, focus);
    hits.push((bounds, OverlayHit::Inside));
    hits.push((Rect { x: 0, y: 0, w: w as i32, h: h as i32 }, OverlayHit::Outside));
    QuickPainted { hits, stops, node, list_area: list_window, body_area, drawn }
}

/// Quick settings on the accessibility bus.
fn quick_node(m: &QuickModel, hits: &[(Rect, OverlayHit)], bounds: Rect, focus: Option<OverlayHit>) -> crate::a11y::Node {
    use crate::a11y::{Node, Role};
    let at = |hit: OverlayHit| hits.iter().find(|(_, h)| *h == hit).map(|(r, _)| crate::a11y::Rect::from(*r)).unwrap_or_default();
    let focused = |hit: OverlayHit| focus == Some(hit);
    let mut menu = Node::new(Role::Menu, tr("System")).id("menu").at(bounds.into());
    if let Some((_, charge)) = &m.battery {
        menu = menu.child(Node::label(trf!("Battery {charge}", charge = charge)).id("battery"));
    }
    for b in &m.buttons {
        let button = Node::button(b.name.as_str(), at(b.hit)).id(format!("{:?}", b.hit)).focusable().focused(focused(b.hit));
        menu = menu.child(if b.hit == OverlayHit::PowerMenu { button.expanded(m.power_open) } else { button });
    }
    if m.power_open {
        let actions = m.actions.iter().map(|(name, action)| {
            let hit = OverlayHit::Power(*action);
            Node::new(Role::MenuItem, *name).at(at(hit)).click().focusable().focused(focused(hit))
        });
        menu = menu.child(Node::new(Role::Group, tr("Power Off / Log Out")).id("power").children(actions));
    }
    for s in &m.sliders {
        if let Some(muted) = s.mute {
            menu = menu.child(Node::button(tr("Mute"), at(OverlayHit::Mute)).id("mute").toggled(muted).focusable().focused(focused(OverlayHit::Mute)));
        }
        let slider = Node::new(Role::Slider, &s.name).id(s.id.clone()).range(f64::from(s.value), 0.0, f64::from(s.max), f64::from(slider_step(s.max)));
        menu = menu.child(match s.hit {
            Some(hit) => slider.at(at(hit)).click().focusable().focused(focused(hit)),
            None => slider.disabled(true),
        });
    }
    for t in &m.toggles {
        let hit = OverlayHit::Switch(t.switch);
        let toggle = Node::new(Role::Switch, &t.name).id(t.key.clone()).at(at(hit)).click().toggled(t.on).focusable().focused(focused(hit));
        menu = menu.child(toggle.describe(t.subtitle.as_str()).disabled(t.dim));
        if let Some((more, label)) = &t.more {
            let button = Node::button(label.as_str(), at(*more)).id(format!("{}:more", t.key)).focusable().focused(focused(*more));
            menu = menu.child(if matches!(t.switch, Switch::Ext(_)) { button } else { button.expanded(m.list_open == Some(t.switch)) });
        }
    }
    if m.list_open == Some(Switch::PowerMode) {
        let modes = m.profiles.iter().enumerate().map(|(i, (name, active))| Node::new(Role::RadioButton, name).at(at(OverlayHit::Profile(i))).click().toggled(*active).focusable().focused(focused(OverlayHit::Profile(i))));
        menu = menu.child(Node::new(Role::Group, tr("Power Mode")).id("profiles").children(modes));
    } else if m.list_open.is_some() {
        let entries = m.rows.iter().enumerate().map(|(i, row)| {
            let state = [(row.connected, tr("Connected")), (row.secured, tr("Secured"))].iter().filter(|(on, _)| *on).map(|(_, s)| *s).collect::<Vec<_>>().join(", ");
            match row.hit.and_then(|hit| hits.iter().find(|(_, h)| *h == hit)) {
                Some((r, hit)) => Node::new(Role::MenuItem, &row.name).id(format!("row:{i}")).at((*r).into()).click().describe(state).focusable().focused(focused(*hit)),
                None => Node::label(&row.name),
            }
        });
        menu = menu.child(Node::new(Role::Group, "").id("list").children(entries));
    }
    menu
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A symbolic icon for the samples, `color`ed (none without the
    /// icon theme).
    fn icon(name: &str, color: u32) -> Option<Image> {
        crate::icons::symbolic(name, icon_px() as u32, [(color >> 16) as u8, (color >> 8) as u8, color as u8])
    }

    /// Quick settings as on a laptop: the battery and the top row,
    /// volume and brightness, seven switches with Power Mode; the Wi-Fi
    /// list open with `networks`.
    pub(in crate::ui) fn sample(networks: Option<usize>) -> QuickModel {
        let slider = |name: &str, id: &str, icon_name: &str, value, hit, mute| Slider { name: name.into(), id: id.into(), icon: icon(icon_name, FG), value, max: 100, dim: false, hit: Some(hit), mute };
        let tile = |switch: Switch, name: &str, subtitle: &str, icon_name: &str, on: bool| SwitchTile {
            switch,
            name: name.into(),
            on,
            dim: false,
            subtitle: subtitle.into(),
            icon: icon(icon_name, theme::control_text(theme::Look { on, ..Default::default() })),
            more: matches!(switch, Switch::Wifi | Switch::Bluetooth | Switch::PowerMode).then(|| (OverlayHit::Expand(switch), format!("{name} more"))),
            key: switch.id().into(),
        };
        let names = ["Home Network 5G", "Office 5G", "Café", "Library", "Neighbour", "Printer-Direct", "Guest", "Lab", "Hotspot", "Upstairs", "Garage", "Attic"];
        let rows = (0..networks.unwrap_or(0))
            .map(|i| ListRow { icon: icon("network-wireless-signal-good-symbolic", FG), name: names[i % names.len()].into(), secured: i % 3 != 2, connected: i == 0, checked: false, hit: Some(OverlayHit::Network(i)), dim: false })
            .collect();
        let button = |hit, icon_name: &str, name: &str| TopButton { hit, icon: icon(icon_name, FG), name: name.into() };
        QuickModel {
            battery: Some((icon("battery-level-80-symbolic", FG), "80%".into())),
            buttons: vec![
                button(OverlayHit::QuickScreenshot, "camera-photo-symbolic", tr("Take Screenshot")),
                button(OverlayHit::QuickSettingsApp, "emblem-system-symbolic", tr("Settings")),
                button(OverlayHit::Power(PowerAction::Lock), "system-lock-screen-symbolic", tr("Lock Screen")),
                button(OverlayHit::PowerMenu, "system-shutdown-symbolic", tr("Power Off / Log Out")),
            ],
            power_open: false,
            actions: vec![(tr("Suspend"), PowerAction::Suspend), (tr("Log Out…"), PowerAction::LogOut), (tr("Restart…"), PowerAction::Restart), (tr("Power Off…"), PowerAction::PowerOff)],
            sliders: vec![
                slider(tr("Volume"), "volume", "audio-volume-medium-symbolic", 45, OverlayHit::VolumeSlider, Some(false)),
                slider(tr("Brightness"), "brightness", "display-brightness-symbolic", 80, OverlayHit::BrightnessSlider, None),
            ],
            toggles: vec![
                tile(Switch::Wifi, tr("Wi-Fi"), "Home Network 5G", "network-wireless-signal-good-symbolic", true),
                tile(Switch::Bluetooth, tr("Bluetooth"), "", "bluetooth-disabled-symbolic", false),
                tile(Switch::Airplane, tr("Airplane Mode"), "", "airplane-mode-symbolic", false),
                tile(Switch::NightLight, tr("Night Light"), "", "night-light-symbolic", true),
                tile(Switch::DarkStyle, tr("Dark Style"), "", dark_style_icon(), true),
                tile(Switch::DoNotDisturb, tr("Do Not Disturb"), "", "notifications-disabled-symbolic", false),
                tile(Switch::PowerMode, tr("Power Mode"), tr("Balanced"), "power-profile-balanced-symbolic", false),
            ],
            list_open: networks.map(|_| Switch::Wifi),
            rows,
            lock: None,
            profiles: vec![(tr("Power Saver").into(), false), (tr("Balanced").into(), true), (tr("Performance").into(), false)],
            status_shapes: false,
            hover: None,
            pressed: None,
        }
    }

    /// [`sample`] with the power actions open.
    pub(in crate::ui) fn sample_power() -> QuickModel {
        QuickModel { power_open: true, ..sample(None) }
    }

    /// [`sample`] with the Power Mode list open.
    pub(in crate::ui) fn sample_modes() -> QuickModel {
        let mut m = sample(None);
        m.list_open = Some(Switch::PowerMode);
        m.rows = ["power-saver", "balanced", "performance"]
            .iter()
            .zip(m.profiles.iter())
            .enumerate()
            .map(|(i, (p, (name, active)))| ListRow { icon: icon(&format!("power-profile-{p}-symbolic"), FG), name: name.clone(), secured: false, connected: false, checked: *active, hit: Some(OverlayHit::Profile(i)), dim: false })
            .collect();
        m
    }

    fn paint(m: &QuickModel, focus: Option<OverlayHit>) -> QuickPainted {
        let mut data = vec![0u8; 1280 * 800 * 4];
        let mut canvas = Canvas::scaled(&mut data, 1280, 800, 1.0);
        paint_quick(&mut canvas, (None, None), (1280, 800), m, focus, &mut QuickScroll::default())
    }

    #[test]
    fn keyboard_order() {
        let m = sample(Some(2));
        use OverlayHit as H;
        let stops = m.stops();
        assert_eq!(stops[..4], [H::QuickScreenshot, H::QuickSettingsApp, H::Power(PowerAction::Lock), H::PowerMenu], "the top row first");
        assert_eq!(
            stops[4..12],
            [H::Mute, H::VolumeSlider, H::BrightnessSlider, H::Switch(Switch::Wifi), H::Expand(Switch::Wifi), H::Switch(Switch::Bluetooth), H::Expand(Switch::Bluetooth), H::Switch(Switch::Airplane)]
        );
        assert_eq!(stops[stops.len() - 4..], [H::Switch(Switch::PowerMode), H::Expand(Switch::PowerMode), H::Network(0), H::Network(1)], "the open list after the switches");
        assert!(!stops.contains(&H::Power(PowerAction::PowerOff)), "power actions only with Power open");
        // Every stop is painted where it can be clicked.
        let painted = paint(&m, None);
        assert_eq!(painted.stops.iter().map(|(_, h)| *h).collect::<Vec<_>>(), stops);
        // Power open: its actions right after the top row.
        let m = sample_power();
        let stops = m.stops();
        assert_eq!(stops[4..8], [H::Power(PowerAction::Suspend), H::Power(PowerAction::LogOut), H::Power(PowerAction::Restart), H::Power(PowerAction::PowerOff)]);
        assert_eq!(paint(&m, None).stops.iter().map(|(_, h)| *h).collect::<Vec<_>>(), stops);
        // The modes list: radio buttons, the active one checked.
        let m = sample_modes();
        let painted = paint(&m, Some(H::Profile(1)));
        let modes = painted.node.find(&|n| n.name == "Power Mode" && n.role == crate::a11y::Role::Group).expect("the modes");
        assert_eq!(modes.children.len(), 3);
        assert!(modes.children[1].focused);
    }

    #[test]
    fn keys() {
        use OverlayHit as H;
        let m = sample(None);
        let stops = paint(&m, None).stops;
        let index = |hit: H| stops.iter().position(|(_, h)| *h == hit).unwrap();
        let key = |k: Keysym, at: H| {
            let slider = matches!(at, H::VolumeSlider | H::BrightnessSlider).then_some((at, (45, 100)));
            quick_key_action(k, &stops, Some(index(at)), slider)
        };
        // Opened by a click: the first move focuses the first control.
        assert_eq!(quick_key_action(Keysym::Tab, &stops, None, None), QuickKey::Focus(0));
        assert_eq!(quick_key_action(Keysym::Escape, &stops, None, None), QuickKey::Close);
        // Sliders move by 5 %.
        assert_eq!(key(Keysym::Right, H::VolumeSlider), QuickKey::Slide(H::VolumeSlider, 50));
        assert_eq!(key(Keysym::Left, H::VolumeSlider), QuickKey::Slide(H::VolumeSlider, 40));
        assert_eq!(key(Keysym::End, H::BrightnessSlider), QuickKey::Slide(H::BrightnessSlider, 100));
        assert_eq!(key(Keysym::Page_Down, H::BrightnessSlider), QuickKey::Slide(H::BrightnessSlider, 35));
        assert_eq!(key(Keysym::Down, H::VolumeSlider), QuickKey::Focus(index(H::BrightnessSlider)));
        // Tab goes in order, wrapping.
        assert_eq!(key(Keysym::Tab, H::Switch(Switch::Wifi)), QuickKey::Focus(index(H::Expand(Switch::Wifi))));
        assert_eq!(key(Keysym::ISO_Left_Tab, H::QuickScreenshot), QuickKey::Focus(stops.len() - 1));
        // Arrows move in the grid of switches (two to a row).
        assert_eq!(key(Keysym::Right, H::Switch(Switch::Wifi)), QuickKey::Focus(index(H::Expand(Switch::Wifi))));
        assert_eq!(key(Keysym::Right, H::Expand(Switch::Wifi)), QuickKey::Focus(index(H::Switch(Switch::Bluetooth))));
        assert_eq!(key(Keysym::Right, H::Expand(Switch::Bluetooth)), QuickKey::Focus(index(H::Expand(Switch::Bluetooth))), "the row's end");
        assert_eq!(key(Keysym::Left, H::Switch(Switch::Bluetooth)), QuickKey::Focus(index(H::Expand(Switch::Wifi))));
        assert_eq!(key(Keysym::Down, H::Switch(Switch::Airplane)), QuickKey::Focus(index(H::Switch(Switch::DarkStyle))));
        assert_eq!(key(Keysym::Up, H::Switch(Switch::DarkStyle)), QuickKey::Focus(index(H::Switch(Switch::Airplane))));
        // The top row: left and right along it.
        assert_eq!(key(Keysym::Right, H::QuickScreenshot), QuickKey::Focus(index(H::QuickSettingsApp)));
        assert_eq!(key(Keysym::Left, H::PowerMenu), QuickKey::Focus(index(H::Power(PowerAction::Lock))));
        // Enter and Space work switches and buttons.
        assert_eq!(key(Keysym::Return, H::Switch(Switch::NightLight)), QuickKey::Activate);
        assert_eq!(key(Keysym::space, H::Power(PowerAction::Lock)), QuickKey::Activate);
        assert_eq!(key(Keysym::Escape, H::Power(PowerAction::Lock)), QuickKey::Close);
    }

    /// On a short screen with a long list, nothing is drawn below the
    /// screen's edge and every control can be reached by scrolling.
    #[test]
    fn short_screens() {
        use OverlayHit as H;
        let (w, h) = (853, 480);
        let m = sample(Some(10));
        let mut data = vec![0u8; w * h * 4];
        let mut scroll = QuickScroll::default();
        let mut canvas = Canvas::scaled(&mut data, w, h, 1.0);
        let painted = paint_quick(&mut canvas, (None, None), (w, h), &m, None, &mut scroll);
        let menu = painted.hits.iter().find(|(_, hit)| *hit == H::Inside).unwrap().0;
        assert!(menu.y + menu.h <= h as i32 - 8, "{menu:?} fits");
        for (r, hit) in &painted.hits {
            assert!(*hit == H::Outside || r.y + r.h <= menu.y + menu.h, "{hit:?} at {r:?} is drawn on screen");
        }
        // The top row stays; the list shows LIST_MAX rows.
        assert!(painted.hits.iter().any(|(_, hit)| *hit == H::PowerMenu));
        let list = painted.list_area.expect("a list");
        let rows = painted.hits.iter().filter(|(r, hit)| is_list_row(*hit) && r.y >= list.y && r.y + r.h <= list.y + list.h).count();
        assert!(rows <= LIST_MAX);
        // Every stop, focused, ends up where it can be clicked.
        for (_, stop) in painted.stops.clone() {
            let mut canvas = Canvas::scaled(&mut data, w, h, 1.0);
            let p = paint_quick(&mut canvas, (None, None), (w, h), &m, Some(stop), &mut scroll);
            assert!(p.hits.iter().any(|(_, hit)| *hit == stop), "{stop:?} reachable (scroll {scroll:?})");
        }
        assert!(scroll.list > 0, "the list scrolled to its last rows");
    }

    #[test]
    fn focus_shown_and_published() {
        use OverlayHit as H;
        let m = sample(Some(3));
        let painted = paint(&m, Some(H::Network(1)));
        let focused: Vec<&str> = {
            let mut out = Vec::new();
            fn walk<'a>(n: &'a crate::a11y::Node, out: &mut Vec<&'a str>) {
                if n.focused {
                    out.push(n.name.as_str());
                }
                n.children.iter().for_each(|c| walk(c, out));
            }
            walk(&painted.node, &mut out);
            out
        };
        assert_eq!(focused, ["Office 5G"], "only the focused row");
        let switch = painted.node.find(&|n| n.name == "Wi-Fi" && n.role == crate::a11y::Role::Switch).unwrap();
        assert!(switch.focusable && !switch.focused);
        // The ring is drawn around the focused row (accent pixels just
        // outside its left edge).
        let (r, _) = painted.stops.iter().find(|(_, h)| *h == H::Network(1)).copied().unwrap();
        let mut data = vec![0u8; 1280 * 800 * 4];
        let mut canvas = Canvas::scaled(&mut data, 1280, 800, 1.0);
        paint_quick(&mut canvas, (None, None), (1280, 800), &m, Some(H::Network(1)), &mut QuickScroll::default());
        let px = |x: i32, y: i32| &data[((y * 1280 + x) * 4) as usize..][..3];
        assert!((r.x - 6..r.x + 2).any(|x| px(x, r.y + r.h / 2) == [0xE4, 0x84, 0x35]), "ring left of the row");
    }

    #[test]
    fn switch_order() {
        let sw = |s: Switch, name: &str| (s, name.to_owned(), s.id().to_owned());
        let mut available = vec![sw(Switch::Wifi, "Wi-Fi"), sw(Switch::NightLight, "Night Light"), sw(Switch::DarkStyle, "Dark Style")];
        available.push((Switch::Ext(0), "Caffeine".into(), "org.x.C/toggle".into()));
        let order: Vec<String> = ["helix:dark-style", "org.x.C/toggle", "helix:bluetooth", "helix:wifi", "helix:dark-style", "x:y"].iter().map(|s| s.to_string()).collect();
        let got: Vec<Switch> = ordered_switches(available, &order).into_iter().map(|(s, _)| s).collect();
        assert_eq!(got, [Switch::DarkStyle, Switch::Ext(0), Switch::Wifi], "listed, available, once");
        let all: Vec<String> = crate::panel_layout::QUICK_SWITCHES.iter().map(|s| s.to_string()).collect();
        for s in [Switch::Wifi, Switch::Bluetooth, Switch::Airplane, Switch::AutoRotate, Switch::NightLight, Switch::DarkStyle, Switch::DoNotDisturb] {
            assert!(all.contains(&s.id().to_owned()));
        }
    }

    #[test]
    fn slider_clicks() {
        let r = Rect { x: 100, y: 0, w: 200, h: 8 };
        assert_eq!(slider_value(r, 100.0, 100), 0);
        assert_eq!(slider_value(r, 200.0, 100), 50);
        assert_eq!(slider_value(r, 400.0, 150), 150, "clamped");
        assert_eq!(slider_value(r, 150.0, 150), 38);
    }
}
