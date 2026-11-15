//! The quick settings menu (helix-desktop-spec `contracts/wayland/README.md`,
//! "Quick settings"): volume, brightness, switches and the power actions,
//! under the panel's Power button.

use smithay_client_toolkit::reexports::client::protocol::wl_shm;
use smithay_client_toolkit::shell::wlr_layer::KeyboardInteractivity;
use smithay_client_toolkit::shell::WaylandSurface;

use super::{baseline, label, physical, power_menu_items, theme, Canvas, KeyEvent, Keysym, OverlayHit, Rect, Text, Ui, DIM_FG, FG, panel_height};
use crate::PowerAction;
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind};
use crate::status::Toggle;
use helix_draw::Image;
use crate::{QuickChange, UiRequest};

/// A power profile as the user knows it.
fn profile_label(name: &str) -> String {
    match name {
        "power-saver" => "Power Saver".into(),
        "balanced" => "Balanced".into(),
        "performance" => "Performance".into(),
        other => other.into(),
    }
}

const MENU_W: i32 = 320;
const ROW_H: i32 = 40;
const PAD: i32 = 12;
const LIST_ROW_H: i32 = 32;

/// A row of the open Wi-Fi or Bluetooth list.
pub(super) struct ListRow {
    icon: Option<Image>,
    name: String,
    secured: bool,
    connected: bool,
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

fn settings(schema: &str, key: &str) -> Option<gio::Settings> {
    let source = gio::SettingsSchemaSource::default()?;
    source.lookup(schema, true)?.has_key(key).then(|| gio::Settings::new(schema))
}

/// The slider value (0..=max) for a click at `x` on `r`.
pub(super) fn slider_value(r: Rect, x: f64, max: u32) -> u32 {
    let f = ((x - f64::from(r.x)) / f64::from(r.w.max(1))).clamp(0.0, 1.0);
    (f * f64::from(max)).round() as u32
}

impl Ui {
    fn switch_on(&self, switch: Switch) -> bool {
        use gio::prelude::*;
        match switch {
            Switch::Wifi => self.status.wifi.as_ref().is_some_and(|w| w.0),
            Switch::Bluetooth => self.status.bluetooth == Some(true),
            Switch::Airplane => self.status.airplane,
            Switch::AutoRotate => !crate::extra_keys::orientation_locked(),
            Switch::NightLight => settings(COLOR, "night-light-enabled").is_some_and(|s| s.boolean("night-light-enabled")),
            Switch::DarkStyle => settings(INTERFACE, "color-scheme").is_some_and(|s| s.string("color-scheme") == "prefer-dark"),
            Switch::DoNotDisturb => self.notify.do_not_disturb,
            Switch::Ext(i) => self.ext.toggles.get(i).is_some_and(|t| t.active),
        }
    }

    fn flip(&mut self, switch: Switch) {
        use gio::prelude::*;
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
        let model = self.quick_model(scale);
        let Ok((buffer, data)) = self.pool.create_buffer(physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888) else {
            self.power_menu = Some(overlay);
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale);
        // A focus that is not there (yet, or any more: a list closed)
        // moves to the first control.
        let stops = model.stops();
        if overlay.state.focus.is_some_and(|f| !stops.contains(&f)) {
            overlay.state.focus = stops.first().copied();
        }
        let mut scroll = overlay.state.scroll;
        let painted = paint_quick(&mut canvas, self.text.as_ref(), (w, h), &model, overlay.state.focus, &mut scroll);
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
        if let Some((on, ssid)) = &self.status.wifi {
            let name = ssid.as_ref().filter(|_| *on).cloned().unwrap_or_else(|| "Wi-Fi".into());
            switches.push((Switch::Wifi, name));
        }
        if self.status.bluetooth.is_some() {
            switches.push((Switch::Bluetooth, "Bluetooth".into()));
        }
        if self.status.wifi.is_some() || self.status.bluetooth.is_some() {
            switches.push((Switch::Airplane, "Airplane Mode".into()));
        }
        if self.status.accelerometer {
            switches.push((Switch::AutoRotate, "Auto Rotate".into()));
        }
        switches.extend([(Switch::NightLight, "Night Light".into()), (Switch::DarkStyle, "Dark Style".into()), (Switch::DoNotDisturb, "Do Not Disturb".into())]);
        let mut available: Vec<(Switch, String, String)> = switches.into_iter().map(|(s, name)| (s, name, s.id().to_owned())).collect();
        // Nothing from extensions while locked.
        let locked = self.lock.session.is_some();
        if !locked {
            available.extend(self.ext.toggles.iter().enumerate().map(|(i, t)| (Switch::Ext(i), t.title.clone(), t.full_id.clone())));
        }
        // In the order of `org.helix.shell` `quick-settings`.
        let switches = ordered_switches(available, &self.panel_keys.quick);
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
            sliders.push(Slider { label: if muted { "Muted" } else { "Volume" }.into(), name: "Volume".into(), id: "volume".into(), value: percent, max, dim: muted, hit: Some(OverlayHit::VolumeSlider), mute: Some(muted) });
        }
        if let Some(percent) = self.quick.brightness {
            sliders.push(Slider { label: "Brightness".into(), name: "Brightness".into(), id: "brightness".into(), value: percent, max: 100, dim: false, hit: Some(OverlayHit::BrightnessSlider), mute: None });
        }
        for &i in &ext_sliders {
            let Some(s) = self.ext.sliders.get(i) else { continue };
            let value = (s.value * 100.0).round() as u32;
            sliders.push(Slider { label: s.title.clone(), name: s.title.clone(), id: format!("slider:{}", s.full_id), value, max: 100, dim: !s.sensitive, hit: s.sensitive.then_some(OverlayHit::ExtSlider(i)), mute: None });
        }
        let toggles: Vec<SwitchTile> = switches
            .iter()
            .map(|(switch, name)| {
                let ext = if let Switch::Ext(i) = switch { self.ext.toggles.get(*i) } else { None };
                let key = if switch.id().is_empty() { ext.map_or_else(String::new, |t| t.full_id.clone()) } else { switch.id().to_owned() };
                let more = match switch {
                    Switch::Wifi => Some((OverlayHit::Expand(*switch), "Wi-Fi Networks".to_owned())),
                    Switch::Bluetooth => Some((OverlayHit::Expand(*switch), format!("{name} Devices"))),
                    Switch::Ext(i) if ext.is_some_and(|t| t.has_details && t.sensitive) => Some((OverlayHit::ExtDetails(*i), format!("{name} Settings"))),
                    _ => None,
                };
                SwitchTile { switch: *switch, name: name.clone(), on: self.switch_on(*switch), dim: ext.is_some_and(|t| !t.sensitive), subtitle: ext.map_or_else(String::new, |t| t.subtitle.clone()), more, key }
            })
            .collect();
        let profiles: Vec<(String, bool)> = self.status.profiles.iter().map(|p| (profile_label(p), self.status.profile.as_deref() == Some(p.as_str()))).collect();
        // The open list.
        let list_open = self.quick_list.filter(|s| toggles.iter().any(|t| t.switch == *s));
        let row = |name: String, hit: Option<OverlayHit>| ListRow { icon: None, name, secured: false, connected: false, hit, dim: hit.is_none() };
        let rows: Vec<ListRow> = match list_open {
            Some(Switch::Wifi) => {
                let networks = self.status.networks.clone();
                networks
                    .iter()
                    .enumerate()
                    .map(|(i, n)| ListRow { icon: self.symbolic_icon(crate::status::wifi_icon(n.strength), scale), name: n.ssid.clone(), secured: n.secured, connected: n.active, hit: Some(OverlayHit::Network(i)), dim: false })
                    .collect()
            }
            Some(Switch::Bluetooth) => {
                let mut rows: Vec<ListRow> = self.status.devices.iter().enumerate().map(|(i, d)| ListRow { connected: d.connected, ..row(d.name.clone(), Some(OverlayHit::BtDevice(i))) }).collect();
                if !self.status.devices.is_empty() {
                    rows.push(row("Send Files…".into(), Some(OverlayHit::BtSendFiles)));
                }
                if self.bt_discovery || self.status.discovering {
                    rows.push(row("Nearby Devices".into(), None));
                    if self.status.nearby.is_empty() {
                        rows.push(row("Searching…".into(), None));
                    }
                    rows.extend(self.status.nearby.iter().enumerate().map(|(i, d)| row(d.name.clone(), Some(OverlayHit::BtNearby(i)))));
                } else {
                    rows.push(row("Pair New Device…".into(), Some(OverlayHit::PairNew)));
                }
                rows
            }
            _ => Vec::new(),
        };
        let lock = if rows.iter().any(|r| r.secured) { self.symbolic_icon("channel-secure-symbolic", scale) } else { None };
        let items = power_menu_items(self.power, !crate::locked_down("disable-lock-screen"), !crate::locked_down("disable-log-out"), self.user_switch && crate::user_switch::allowed());
        QuickModel { sliders, toggles, list_open, rows, lock, profiles, items, status_shapes: helix_draw::status_shapes() }
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
    /// What the row says ("Volume", "Muted") and its accessible name.
    pub label: String,
    pub name: String,
    pub id: String,
    pub value: u32,
    pub max: u32,
    pub dim: bool,
    /// Its bar's hit (none: it cannot be changed).
    pub hit: Option<OverlayHit>,
    /// The volume's mute state (its label mutes).
    pub mute: Option<bool>,
}

/// A switch of quick settings.
pub(super) struct SwitchTile {
    pub switch: Switch,
    pub name: String,
    pub on: bool,
    /// An extension's toggle that cannot be changed now.
    pub dim: bool,
    pub subtitle: String,
    /// Its "›" (a list or an extension's menu) and that part's name.
    pub more: Option<(OverlayHit, String)>,
    /// Its id on the accessibility bus.
    pub key: String,
}

/// Everything quick settings shows.
pub(super) struct QuickModel {
    pub sliders: Vec<Slider>,
    pub toggles: Vec<SwitchTile>,
    pub list_open: Option<Switch>,
    pub rows: Vec<ListRow>,
    /// The secured-network sign.
    pub lock: Option<Image>,
    /// Power profiles (label, active).
    pub profiles: Vec<(String, bool)>,
    pub items: Vec<(&'static str, PowerAction)>,
    pub status_shapes: bool,
}

impl QuickModel {
    /// The controls the keyboard reaches, in order: sliders (and the
    /// mute), switches (each then its "›"), the open list's rows, power
    /// modes, power actions.
    pub(super) fn stops(&self) -> Vec<OverlayHit> {
        let mut out = Vec::new();
        for s in &self.sliders {
            out.extend(s.hit);
            if s.mute.is_some() {
                out.push(OverlayHit::Mute);
            }
        }
        for t in &self.toggles {
            out.push(OverlayHit::Switch(t.switch));
            out.extend(t.more.as_ref().map(|m| m.0));
        }
        if self.list_open.is_some() {
            out.extend(self.rows.iter().filter_map(|r| r.hit));
        }
        out.extend((0..self.profiles.len()).map(OverlayHit::Profile));
        out.extend(self.items.iter().map(|(_, a)| OverlayHit::Power(*a)));
        out
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
pub(super) fn paint_quick(canvas: &mut Canvas, text: Option<&Text>, (w, h): (usize, usize), m: &QuickModel, focus: Option<OverlayHit>, scroll: &mut QuickScroll) -> QuickPainted {
    // A focused list row inside the list's window.
    if let Some(row) = focus.and_then(|f| m.rows.iter().position(|r| r.hit == Some(f))) {
        if row < scroll.list {
            scroll.list = row;
        } else if row >= scroll.list + LIST_MAX {
            scroll.list = row + 1 - LIST_MAX;
        }
    }
    let painted = paint_quick_at(canvas, text, (w, h), m, focus, scroll);
    // A focused control in the scrolled body outside its window: scroll
    // to it.
    let body = painted.body_area;
    let shown = |r: &Rect| r.y >= body.y && r.y + r.h <= body.y + body.h;
    let Some(r) = focus.and_then(|f| painted.drawn.iter().find(|(_, h)| *h == f)).map(|(r, _)| *r).filter(|r| !shown(r) && !matches!(focus, Some(OverlayHit::Power(_)))) else { return painted };
    scroll.body += if r.y < body.y { r.y - body.y - 4 } else { r.y + r.h - (body.y + body.h) + 4 };
    paint_quick_at(canvas, text, (w, h), m, focus, scroll)
}

fn paint_quick_at(canvas: &mut Canvas, text: Option<&Text>, (w, h): (usize, usize), m: &QuickModel, focus: Option<OverlayHit>, scroll: &mut QuickScroll) -> QuickPainted {
    canvas.fill(0x00000000);
    // Lengths that hold text grow with the text scale.
    let grow = helix_draw::theme::grow;
    let (menu_w, row_h, list_row_h, tile_h, item_h, name_w, value_w) = (grow(MENU_W), grow(ROW_H), grow(LIST_ROW_H), grow(40), grow(36), grow(92), grow(48));
    let tile_step = tile_h + 8;
    let switch_rows = m.toggles.len().div_ceil(3) as i32;
    // At most LIST_MAX rows of the list show; it scrolls.
    let list_shown = m.rows.len().clamp(1, LIST_MAX);
    scroll.list = scroll.list.min(m.rows.len().saturating_sub(LIST_MAX));
    let list_h = if m.list_open.is_some() { 8 + list_shown as i32 * list_row_h + 8 } else { 0 };
    let profile_h = if m.profiles.is_empty() { 0 } else { grow(26) + item_h + 8 };
    // The body (sliders to Power Mode) scrolls when the whole does not
    // fit under the panel; the power actions stay below it.
    let body_h = PAD + m.sliders.len() as i32 * row_h + switch_rows * tile_step + list_h + profile_h + PAD;
    let actions_h = 1 + m.items.len() as i32 * item_h + 8;
    let mx = w as i32 - menu_w - 6;
    let my = panel_height() as i32 + 4;
    let room = (h as i32 - my - 8).max(actions_h + row_h);
    let body_view = body_h.min(room - actions_h);
    scroll.body = scroll.body.clamp(0, body_h - body_view);
    let mh = body_view + actions_h;
    theme::popover(canvas, mx, my, menu_w, mh);
    let body_area = Rect { x: mx, y: my, w: menu_w, h: body_view };
    canvas.clip(mx, my, menu_w, body_view);
    let mut hits: Vec<(Rect, OverlayHit)> = Vec::new();
    let mut list_view = None;
    // Where each focusable control is drawn (its ring goes around that).
    let mut drawn: Vec<(Rect, OverlayHit)> = Vec::new();
    let mut y = my + PAD - scroll.body;
    // Sliders: a label, a bar, the value.
    for s in &m.sliders {
        let name_r = Rect { x: mx + PAD, y, w: name_w, h: row_h };
        label(text, canvas, &s.label, name_r.x as f32, baseline(text, y, row_h, 14.0), 14.0, if s.dim { DIM_FG } else { FG }, name_w as f32);
        if s.mute.is_some() {
            hits.push((name_r, OverlayHit::Mute));
            drawn.push((Rect { x: name_r.x - 4, y: y + 6, w: name_w - 4, h: row_h - 12 }, OverlayHit::Mute));
        }
        let bar = Rect { x: mx + PAD + name_w + 4, y: y + row_h / 2 - 9, w: menu_w - 2 * PAD - name_w - 4 - value_w, h: 18 };
        theme::slider(canvas, bar.x, bar.y, bar.w, bar.h, f64::from(s.value.min(s.max)) / f64::from(s.max.max(1)), s.dim);
        let pct = format!("{}%", s.value);
        label(text, canvas, &pct, (bar.x + bar.w + 10) as f32, baseline(text, y, row_h, 13.0), 13.0, DIM_FG, (value_w - 8) as f32);
        if let Some(hit) = s.hit {
            // The whole row height is clickable on the bar.
            hits.push((Rect { x: bar.x, y, w: bar.w, h: row_h }, hit));
            drawn.push((Rect { x: bar.x - 2, y: bar.y - 4, w: bar.w + 4, h: bar.h + 8 }, hit));
        }
        y += row_h;
    }
    // Switches, three to a row.
    let sw = (menu_w - 2 * PAD - 2 * 8) / 3;
    for (i, t) in m.toggles.iter().enumerate() {
        let (row, col) = (i as i32 / 3, i as i32 % 3);
        let r = Rect { x: mx + PAD + col * (sw + 8), y: y + 2 + row * tile_step, w: sw, h: tile_h };
        let look = theme::Look { on: t.on, ..Default::default() };
        theme::control(canvas, r.x, r.y, r.w, r.h, look);
        // Text on it: dark on a light accent while on.
        let on_fg = theme::control_text(look);
        // Wi-Fi and Bluetooth: a "›" for their list; an extension's
        // toggle for its details menu.
        let arrow_w = if t.more.is_some() { grow(20) } else { 0 };
        // `show-status-shapes`: a bar for on, a ring for off.
        let shape_w = if m.status_shapes {
            canvas.status_shape(f64::from(r.x) + 11.0, f64::from(r.y) + f64::from(r.h) / 2.0, 10.0, t.on, on_fg);
            16
        } else {
            0
        };
        let room = r.w - 8 - arrow_w - shape_w;
        let tw = text.map_or(0.0, |x| x.width(&t.name, 12.0)).min(room as f32);
        let fg = if t.dim && !t.on { DIM_FG } else { on_fg };
        let mid = (r.x + shape_w) as f32 + (r.w - arrow_w - shape_w) as f32 / 2.0;
        if t.subtitle.is_empty() {
            label(text, canvas, &t.name, mid - tw / 2.0, baseline(text, r.y, r.h, 12.0), 12.0, fg, room as f32);
        } else {
            // The subtitle under the title.
            label(text, canvas, &t.name, mid - tw / 2.0, (r.y + r.h * 17 / 40) as f32, 12.0, fg, room as f32);
            let sw2 = text.map_or(0.0, |x| x.width(&t.subtitle, 10.0)).min(room as f32);
            label(text, canvas, &t.subtitle, mid - sw2 / 2.0, (r.y + r.h * 31 / 40) as f32, 10.0, on_fg, room as f32);
        }
        if let Some((hit, _)) = &t.more {
            let a = Rect { x: r.x + r.w - arrow_w, y: r.y, w: arrow_w, h: r.h };
            canvas.rect(a.x, a.y + 8, 1, a.h - 16, if t.on { on_fg } else { DIM_FG });
            let glyph = if m.list_open == Some(t.switch) { "‹" } else { "›" };
            label(text, canvas, glyph, (a.x + 6) as f32, baseline(text, a.y, a.h, 16.0), 16.0, on_fg, (arrow_w - 6) as f32);
            hits.push((a, *hit));
            drawn.push((a, *hit));
            drawn.push((Rect { w: r.w - arrow_w, ..r }, OverlayHit::Switch(t.switch)));
        } else {
            drawn.push((r, OverlayHit::Switch(t.switch)));
        }
        hits.push((r, OverlayHit::Switch(t.switch)));
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
            let empty = if open == Switch::Wifi { "No Networks" } else { "No Devices" };
            label(text, canvas, empty, (lx + 12) as f32, baseline(text, ry, list_row_h, 13.0), 13.0, DIM_FG, (lw - 24) as f32);
        }
        let list_area = Rect { x: lx, y: y + 4, w: lw, h: list_h - 8 };
        list_view = Some(list_area);
        canvas.clip(lx, (y + 4).max(my), lw, (list_h - 8).min(my + body_view - (y + 4).max(my)).max(0));
        for ListRow { icon, name, secured, connected, hit, dim } in &m.rows {
            let r = Rect { x: lx, y: ry, w: lw, h: list_row_h };
            let mut tx = r.x + 12;
            if let Some(icon) = icon {
                canvas.image(f64::from(tx), f64::from(r.y + (r.h - 16) / 2), 16.0, 16.0, icon);
                tx += 24;
            }
            let state_w = if *connected { grow(76) } else { 0 };
            let lock_w = if *secured { 22 } else { 0 };
            label(text, canvas, name, tx as f32, baseline(text, r.y, r.h, 13.0), 13.0, if *dim { DIM_FG } else { FG }, (r.x + r.w - tx - state_w - lock_w - 8) as f32);
            if *secured {
                if let Some(lock) = &m.lock {
                    canvas.image(f64::from(r.x + r.w - state_w - lock_w - 4), f64::from(r.y + (r.h - 14) / 2), 14.0, 14.0, lock);
                }
            }
            if *connected {
                label(text, canvas, "Connected", (r.x + r.w - state_w) as f32, baseline(text, r.y, r.h, 12.0), 12.0, theme::text_accent(), (state_w - 8) as f32);
            }
            if let Some(hit) = hit {
                hits.push((r, *hit));
                drawn.push((Rect { x: r.x + 2, y: r.y + 2, w: r.w - 4, h: r.h - 4 }, *hit));
            }
            ry += list_row_h;
        }
        // Where it is in the list, when it scrolls.
        if m.rows.len() > LIST_MAX {
            let track = list_area.h - 8;
            let thumb = (track * LIST_MAX as i32 / m.rows.len() as i32).max(12);
            let at = (track - thumb) * scroll.list as i32 / (m.rows.len() - LIST_MAX) as i32;
            canvas.rect(lx + lw - 5, list_area.y + 4 + at, 3, thumb, theme::TEXT_DIM);
        }
        canvas.clip(mx, my, menu_w, body_view);
        y += list_h;
    }
    // Power mode: one button per profile.
    if !m.profiles.is_empty() {
        label(text, canvas, "Power Mode", (mx + PAD) as f32, (y + grow(18)) as f32, 13.0, DIM_FG, (menu_w - 2 * PAD) as f32);
        let n = m.profiles.len() as i32;
        let bw = (menu_w - 2 * PAD - (n - 1) * 8) / n;
        for (i, (name, active)) in m.profiles.iter().enumerate() {
            let r = Rect { x: mx + PAD + i as i32 * (bw + 8), y: y + grow(26), w: bw, h: item_h };
            let look = theme::Look { on: *active, ..Default::default() };
            theme::control(canvas, r.x, r.y, r.w, r.h, look);
            let tw = text.map_or(0.0, |t| t.width(name, 12.0)).min((r.w - 8) as f32);
            label(text, canvas, name, r.x as f32 + (r.w as f32 - tw) / 2.0, baseline(text, r.y, r.h, 12.0), 12.0, theme::control_text(look), (r.w - 8) as f32);
            hits.push((r, OverlayHit::Profile(i)));
            drawn.push((r, OverlayHit::Profile(i)));
        }
    }
    canvas.unclip();
    // Where the body is, when it scrolls.
    if body_h > body_view {
        let thumb = (body_view * body_view / body_h).max(16);
        let at = (body_view - thumb) * scroll.body / (body_h - body_view).max(1);
        canvas.rect(mx + menu_w - 4, my + at, 3, thumb, theme::TEXT_DIM);
    }
    // Clicks only where the body shows (the list's rows only in its
    // window).
    let list_window = list_view.map(|r: Rect| Rect { y: r.y.max(my), h: (r.y + r.h).min(my + body_view) - r.y.max(my), ..r });
    hits.retain(|(r, hit)| {
        let inside = |area: &Rect| r.y + r.h > area.y && r.y < area.y + area.h;
        inside(&body_area) && (!is_list_row(*hit) || list_window.is_some_and(|l| inside(&l)))
    });
    let mut y = my + body_view;
    theme::separator(canvas, mx + PAD, y, menu_w - 2 * PAD);
    y += 1;
    // Power actions.
    for (i, (name, action)) in m.items.iter().enumerate() {
        let r = Rect { x: mx + 4, y: y + 4 + i as i32 * item_h, w: menu_w - 8, h: item_h };
        label(text, canvas, name, (r.x + 12) as f32, baseline(text, r.y, r.h, 15.0), 15.0, FG, (r.w - 24) as f32);
        hits.push((r, OverlayHit::Power(*action)));
        drawn.push((Rect { x: r.x + 2, y: r.y + 2, w: r.w - 4, h: r.h - 4 }, OverlayHit::Power(*action)));
    }
    // The focus ring.
    if let Some((r, _)) = focus.and_then(|f| drawn.iter().find(|(_, h)| *h == f)) {
        helix_draw::theme::focus_ring(canvas, r.x, r.y, r.w, r.h);
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
    let mut menu = Node::new(Role::Menu, "System").id("menu").at(bounds.into());
    for s in &m.sliders {
        let slider = Node::new(Role::Slider, &s.name).id(s.id.clone()).range(f64::from(s.value), 0.0, f64::from(s.max), f64::from(slider_step(s.max)));
        menu = menu.child(match s.hit {
            Some(hit) => slider.at(at(hit)).click().focusable().focused(focused(hit)),
            None => slider.disabled(true),
        });
        if let Some(muted) = s.mute {
            menu = menu.child(Node::button("Mute", at(OverlayHit::Mute)).id("mute").toggled(muted).focusable().focused(focused(OverlayHit::Mute)));
        }
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
    if m.list_open.is_some() {
        let entries = m.rows.iter().enumerate().map(|(i, row)| {
            let state = [(row.connected, "Connected"), (row.secured, "Secured")].iter().filter(|(on, _)| *on).map(|(_, s)| *s).collect::<Vec<_>>().join(", ");
            match row.hit.and_then(|hit| hits.iter().find(|(_, h)| *h == hit)) {
                Some((r, hit)) => Node::new(Role::MenuItem, &row.name).id(format!("row:{i}")).at((*r).into()).click().describe(state).focusable().focused(focused(*hit)),
                None => Node::label(&row.name),
            }
        });
        menu = menu.child(Node::new(Role::Group, "").id("list").children(entries));
    }
    if !m.profiles.is_empty() {
        let modes = m.profiles.iter().enumerate().map(|(i, (name, active))| Node::new(Role::RadioButton, name).at(at(OverlayHit::Profile(i))).click().toggled(*active).focusable().focused(focused(OverlayHit::Profile(i))));
        menu = menu.child(Node::new(Role::Group, "Power Mode").id("profiles").children(modes));
    }
    for (name, action) in &m.items {
        let hit = OverlayHit::Power(*action);
        menu = menu.child(Node::new(Role::MenuItem, *name).at(at(hit)).click().focusable().focused(focused(hit)));
    }
    menu
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// Quick settings as on a laptop: volume and brightness, seven
    /// switches, three power modes; the Wi-Fi list open with `networks`.
    pub(in crate::ui) fn sample(networks: Option<usize>) -> QuickModel {
        let slider = |label: &str, id: &str, value, hit, mute| Slider { label: label.into(), name: label.into(), id: id.into(), value, max: 100, dim: false, hit: Some(hit), mute };
        let tile = |switch: Switch, name: &str, on: bool| SwitchTile {
            switch,
            name: name.into(),
            on,
            dim: false,
            subtitle: String::new(),
            more: matches!(switch, Switch::Wifi | Switch::Bluetooth).then(|| (OverlayHit::Expand(switch), format!("{name} more"))),
            key: switch.id().into(),
        };
        let names = ["Home", "Office 5G", "Café", "Library", "Neighbour", "Printer-Direct", "Guest", "Lab", "Hotspot", "Upstairs", "Garage", "Attic"];
        let rows = (0..networks.unwrap_or(0)).map(|i| ListRow { icon: None, name: names[i % names.len()].into(), secured: i % 3 != 2, connected: i == 0, hit: Some(OverlayHit::Network(i)), dim: false }).collect();
        QuickModel {
            sliders: vec![slider("Volume", "volume", 45, OverlayHit::VolumeSlider, Some(false)), slider("Brightness", "brightness", 80, OverlayHit::BrightnessSlider, None)],
            toggles: vec![
                tile(Switch::Wifi, "Home", true),
                tile(Switch::Bluetooth, "Bluetooth", false),
                tile(Switch::Airplane, "Airplane Mode", false),
                tile(Switch::NightLight, "Night Light", true),
                tile(Switch::DarkStyle, "Dark Style", true),
                tile(Switch::DoNotDisturb, "Do Not Disturb", false),
            ],
            list_open: networks.map(|_| Switch::Wifi),
            rows,
            lock: None,
            profiles: vec![("Power Saver".into(), false), ("Balanced".into(), true), ("Performance".into(), false)],
            items: vec![("Lock Screen", PowerAction::Lock), ("Suspend", PowerAction::Suspend), ("Log Out…", PowerAction::LogOut), ("Restart…", PowerAction::Restart), ("Power Off…", PowerAction::PowerOff)],
            status_shapes: false,
        }
    }

    fn paint(m: &QuickModel, focus: Option<OverlayHit>) -> QuickPainted {
        let mut data = vec![0u8; 1280 * 800 * 4];
        let mut canvas = Canvas::scaled(&mut data, 1280, 800, 1.0);
        paint_quick(&mut canvas, None, (1280, 800), m, focus, &mut QuickScroll::default())
    }

    #[test]
    fn keyboard_order() {
        let m = sample(Some(2));
        use OverlayHit as H;
        let stops = m.stops();
        assert_eq!(
            stops[..8],
            [H::VolumeSlider, H::Mute, H::BrightnessSlider, H::Switch(Switch::Wifi), H::Expand(Switch::Wifi), H::Switch(Switch::Bluetooth), H::Expand(Switch::Bluetooth), H::Switch(Switch::Airplane)]
        );
        let rest: Vec<_> = stops[11..].to_vec();
        assert_eq!(rest[..2], [H::Network(0), H::Network(1)], "the open list after the switches");
        assert_eq!(rest[2..5], [H::Profile(0), H::Profile(1), H::Profile(2)]);
        assert_eq!(*rest.last().unwrap(), H::Power(PowerAction::PowerOff));
        // Every stop is painted where it can be clicked.
        let painted = paint(&m, None);
        assert_eq!(painted.stops.iter().map(|(_, h)| *h).collect::<Vec<_>>(), stops);
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
        assert_eq!(key(Keysym::ISO_Left_Tab, H::VolumeSlider), QuickKey::Focus(stops.len() - 1));
        // Arrows move in the grid of switches.
        assert_eq!(key(Keysym::Right, H::Switch(Switch::Airplane)), QuickKey::Focus(index(H::Switch(Switch::Airplane))), "the row's end");
        assert_eq!(key(Keysym::Left, H::Switch(Switch::Airplane)), QuickKey::Focus(index(H::Expand(Switch::Bluetooth))));
        assert_eq!(key(Keysym::Down, H::Switch(Switch::Bluetooth)), QuickKey::Focus(index(H::Switch(Switch::DarkStyle))));
        assert_eq!(key(Keysym::Up, H::Switch(Switch::DarkStyle)), QuickKey::Focus(index(H::Switch(Switch::Bluetooth))));
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
        let painted = paint_quick(&mut canvas, None, (w, h), &m, None, &mut scroll);
        let menu = painted.hits.iter().find(|(_, hit)| *hit == H::Inside).unwrap().0;
        assert!(menu.y + menu.h <= h as i32 - 8, "{menu:?} fits");
        for (r, hit) in &painted.hits {
            assert!(*hit == H::Outside || r.y + r.h <= menu.y + menu.h, "{hit:?} at {r:?} is drawn on screen");
        }
        // The power actions stay; the list shows LIST_MAX rows.
        assert!(painted.hits.iter().any(|(_, hit)| *hit == H::Power(PowerAction::PowerOff)));
        let list = painted.list_area.expect("a list");
        let rows = painted.hits.iter().filter(|(r, hit)| is_list_row(*hit) && r.y >= list.y && r.y + r.h <= list.y + list.h).count();
        assert!(rows <= LIST_MAX);
        // Every stop, focused, ends up where it can be clicked.
        for (_, stop) in painted.stops.clone() {
            let mut canvas = Canvas::scaled(&mut data, w, h, 1.0);
            let p = paint_quick(&mut canvas, None, (w, h), &m, Some(stop), &mut scroll);
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
        let switch = painted.node.find(&|n| n.name == "Home" && n.role == crate::a11y::Role::Switch).unwrap();
        assert!(switch.focusable && !switch.focused);
        // The ring is drawn around the focused row (accent pixels just
        // outside its left edge).
        let (r, _) = painted.stops.iter().find(|(_, h)| *h == H::Network(1)).copied().unwrap();
        let mut data = vec![0u8; 1280 * 800 * 4];
        let mut canvas = Canvas::scaled(&mut data, 1280, 800, 1.0);
        paint_quick(&mut canvas, None, (1280, 800), &m, Some(H::Network(1)), &mut QuickScroll::default());
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
