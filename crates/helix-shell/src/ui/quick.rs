//! The quick settings menu (helix-desktop-spec `contracts/wayland/README.md`,
//! "Quick settings"): volume, brightness, switches and the power actions,
//! under the panel's Power button.

use smithay_client_toolkit::reexports::client::protocol::wl_shm;
use smithay_client_toolkit::shell::wlr_layer::KeyboardInteractivity;
use smithay_client_toolkit::shell::WaylandSurface;

use super::{baseline, label, physical, power_menu_items, Canvas, OverlayHit, Rect, Ui, ACCENT, BANNER_BG, BANNER_BORDER, DIM_FG, FG, FIELD_BG, PANEL_HEIGHT};
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
struct ListRow {
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

    pub(super) fn toggle_power_menu(&mut self) {
        if self.power_menu.take().is_some() {
            return;
        }
        self.close_launcher(true);
        self.close_overview(true);
        let overlay = self.new_overlay("helix-power-menu", KeyboardInteractivity::OnDemand, ());
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
        let states: Vec<bool> = switches.iter().map(|(s, _)| self.switch_on(*s)).collect();
        let profiles: Vec<(String, bool)> = self.status.profiles.iter().map(|p| (profile_label(p), self.status.profile.as_deref() == Some(p.as_str()))).collect();
        let switch_rows = switches.len().div_ceil(3) as i32;
        // The open list.
        let list_open = self.quick_list.filter(|s| switches.iter().any(|(x, _)| x == s));
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
        let list_h = if list_open.is_some() { 8 + rows.len().max(1) as i32 * LIST_ROW_H + 8 } else { 0 };
        let profile_h = if profiles.is_empty() { 0 } else { 26 + 36 + 8 };
        let Ok((buffer, data)) = self.pool.create_buffer(physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888) else {
            self.power_menu = Some(overlay);
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale);
        canvas.fill(0x00000000);
        let text = self.text.as_ref();
        let items = power_menu_items(self.power, !crate::locked_down("disable-lock-screen"), !crate::locked_down("disable-log-out"), self.user_switch && crate::user_switch::allowed());
        let sliders = usize::from(self.quick.volume.is_some()) + usize::from(self.quick.brightness.is_some()) + ext_sliders.len();
        let mh = PAD + sliders as i32 * ROW_H + switch_rows * 48 + list_h + profile_h + PAD + 1 + items.len() as i32 * 36 + 8;
        let mx = w as i32 - MENU_W - 6;
        let my = PANEL_HEIGHT as i32 + 4;
        canvas.rect(mx - 1, my - 1, MENU_W + 2, mh + 2, BANNER_BORDER);
        canvas.rect(mx, my, MENU_W, mh, BANNER_BG);
        overlay.hits.clear();
        let mut y = my + PAD;
        // Sliders: a label, a bar, the value.
        let slider = |canvas: &mut Canvas, hits: &mut Vec<(Rect, OverlayHit)>, y: i32, name: &str, value: u32, max: u32, dim: bool, bar_hit: OverlayHit, name_hit: Option<OverlayHit>| {
            let name_r = Rect { x: mx + PAD, y, w: 92, h: ROW_H };
            label(text, canvas, name, name_r.x as f32, baseline(text, y, ROW_H, 14.0), 14.0, if dim { DIM_FG } else { FG }, 92.0);
            if let Some(hit) = name_hit {
                hits.push((name_r, hit));
            }
            let bar = Rect { x: mx + PAD + 96, y: y + ROW_H / 2 - 4, w: MENU_W - 2 * PAD - 96 - 48, h: 8 };
            canvas.rect(bar.x, bar.y, bar.w, bar.h, FIELD_BG);
            let filled = bar.w * value.min(max) as i32 / max.max(1) as i32;
            canvas.rect(bar.x, bar.y, filled, bar.h, if dim { DIM_FG } else { ACCENT });
            let pct = format!("{value}%");
            label(text, canvas, &pct, (bar.x + bar.w + 10) as f32, baseline(text, y, ROW_H, 13.0), 13.0, DIM_FG, 40.0);
            // The whole row height is clickable on the bar.
            hits.push((Rect { x: bar.x, y, w: bar.w, h: ROW_H }, bar_hit));
        };
        if let Some((percent, muted, max)) = self.quick.volume {
            slider(&mut canvas, &mut overlay.hits, y, if muted { "Muted" } else { "Volume" }, percent, max, muted, OverlayHit::VolumeSlider, Some(OverlayHit::Mute));
            y += ROW_H;
        }
        if let Some(percent) = self.quick.brightness {
            slider(&mut canvas, &mut overlay.hits, y, "Brightness", percent, 100, false, OverlayHit::BrightnessSlider, None);
            y += ROW_H;
        }
        for &i in &ext_sliders {
            let Some(s) = self.ext.sliders.get(i) else { continue };
            let percent = (s.value * 100.0).round() as u32;
            slider(&mut canvas, &mut overlay.hits, y, &s.title, percent, 100, !s.sensitive, OverlayHit::ExtSlider(i), None);
            if !s.sensitive {
                overlay.hits.retain(|(_, h)| *h != OverlayHit::ExtSlider(i));
            }
            y += ROW_H;
        }
        // Switches, three to a row.
        let sw = (MENU_W - 2 * PAD - 2 * 8) / 3;
        for (i, (switch, name)) in switches.iter().enumerate() {
            let (row, col) = (i as i32 / 3, i as i32 % 3);
            let r = Rect { x: mx + PAD + col * (sw + 8), y: y + 2 + row * 48, w: sw, h: 40 };
            let on = states[i];
            canvas.rect(r.x, r.y, r.w, r.h, if on { ACCENT } else { FIELD_BG });
            // Wi-Fi and Bluetooth: a "›" for their list; an extension's
            // toggle for its details menu.
            let ext_toggle = if let Switch::Ext(i) = switch { self.ext.toggles.get(*i) } else { None };
            let expandable = matches!(switch, Switch::Wifi | Switch::Bluetooth) || ext_toggle.is_some_and(|t| t.has_details && t.sensitive);
            let arrow_w = if expandable { 20 } else { 0 };
            // `show-status-shapes`: a bar for on, a ring for off.
            let shape_w = if helix_draw::status_shapes() {
                canvas.status_shape(f64::from(r.x) + 11.0, f64::from(r.y) + f64::from(r.h) / 2.0, 10.0, on, FG);
                16
            } else {
                0
            };
            let room = r.w - 8 - arrow_w - shape_w;
            let tw = text.map_or(0.0, |t| t.width(name, 12.0)).min(room as f32);
            let fg = if ext_toggle.is_some_and(|t| !t.sensitive) { DIM_FG } else { FG };
            let mid = (r.x + shape_w) as f32 + (r.w - arrow_w - shape_w) as f32 / 2.0;
            match ext_toggle.filter(|t| !t.subtitle.is_empty()) {
                // The subtitle under the title.
                Some(t) => {
                    label(text, &mut canvas, name, mid - tw / 2.0, (r.y + 17) as f32, 12.0, fg, room as f32);
                    let sw2 = text.map_or(0.0, |x| x.width(&t.subtitle, 10.0)).min(room as f32);
                    label(text, &mut canvas, &t.subtitle, mid - sw2 / 2.0, (r.y + 31) as f32, 10.0, if on { FG } else { DIM_FG }, room as f32);
                }
                None => label(text, &mut canvas, name, mid - tw / 2.0, baseline(text, r.y, r.h, 12.0), 12.0, fg, room as f32),
            }
            if expandable {
                let a = Rect { x: r.x + r.w - arrow_w, y: r.y, w: arrow_w, h: r.h };
                canvas.rect(a.x, a.y + 8, 1, a.h - 16, if on { FG } else { DIM_FG });
                let glyph = if list_open == Some(*switch) { "‹" } else { "›" };
                label(text, &mut canvas, glyph, (a.x + 6) as f32, baseline(text, a.y, a.h, 16.0), 16.0, FG, 12.0);
                let hit = match switch {
                    Switch::Ext(i) => OverlayHit::ExtDetails(*i),
                    _ => OverlayHit::Expand(*switch),
                };
                overlay.hits.push((a, hit));
            }
            overlay.hits.push((r, OverlayHit::Switch(*switch)));
        }
        y += switch_rows * 48;
        // The open list.
        if let Some(open) = list_open {
            let lx = mx + PAD;
            let lw = MENU_W - 2 * PAD;
            canvas.rect(lx, y + 4, lw, list_h - 8, FIELD_BG);
            let mut ry = y + 8;
            if rows.is_empty() {
                let empty = if open == Switch::Wifi { "No Networks" } else { "No Devices" };
                label(text, &mut canvas, empty, (lx + 12) as f32, baseline(text, ry, LIST_ROW_H, 13.0), 13.0, DIM_FG, (lw - 24) as f32);
            }
            for ListRow { icon, name, secured, connected, hit, dim } in &rows {
                let r = Rect { x: lx, y: ry, w: lw, h: LIST_ROW_H };
                let mut tx = r.x + 12;
                if let Some(icon) = icon {
                    canvas.image(f64::from(tx), f64::from(r.y + (r.h - 16) / 2), 16.0, 16.0, icon);
                    tx += 24;
                }
                let state_w = if *connected { 76 } else { 0 };
                let lock_w = if *secured { 22 } else { 0 };
                label(text, &mut canvas, name, tx as f32, baseline(text, r.y, r.h, 13.0), 13.0, if *dim { DIM_FG } else { FG }, (r.x + r.w - tx - state_w - lock_w - 8) as f32);
                if *secured {
                    if let Some(lock) = &lock {
                        canvas.image(f64::from(r.x + r.w - state_w - lock_w - 4), f64::from(r.y + (r.h - 14) / 2), 14.0, 14.0, lock);
                    }
                }
                if *connected {
                    label(text, &mut canvas, "Connected", (r.x + r.w - state_w) as f32, baseline(text, r.y, r.h, 12.0), 12.0, ACCENT, (state_w - 8) as f32);
                }
                if let Some(hit) = hit {
                    overlay.hits.push((r, *hit));
                }
                ry += LIST_ROW_H;
            }
            y += list_h;
        }
        // Power mode: one button per profile.
        if !profiles.is_empty() {
            label(text, &mut canvas, "Power Mode", (mx + PAD) as f32, (y + 18) as f32, 13.0, DIM_FG, (MENU_W - 2 * PAD) as f32);
            let n = profiles.len() as i32;
            let bw = (MENU_W - 2 * PAD - (n - 1) * 8) / n;
            for (i, (name, active)) in profiles.iter().enumerate() {
                let r = Rect { x: mx + PAD + i as i32 * (bw + 8), y: y + 26, w: bw, h: 36 };
                canvas.rect(r.x, r.y, r.w, r.h, if *active { ACCENT } else { FIELD_BG });
                let tw = text.map_or(0.0, |t| t.width(name, 12.0)).min((r.w - 8) as f32);
                label(text, &mut canvas, name, r.x as f32 + (r.w as f32 - tw) / 2.0, baseline(text, r.y, r.h, 12.0), 12.0, FG, (r.w - 8) as f32);
                overlay.hits.push((r, OverlayHit::Profile(i)));
            }
            y += profile_h;
        }
        y += PAD;
        canvas.rect(mx + PAD, y, MENU_W - 2 * PAD, 1, BANNER_BORDER);
        y += 1;
        // Power actions.
        for (i, (name, action)) in items.iter().enumerate() {
            let r = Rect { x: mx + 4, y: y + 4 + i as i32 * 36, w: MENU_W - 8, h: 36 };
            label(text, &mut canvas, name, (r.x + 12) as f32, baseline(text, r.y, r.h, 15.0), 15.0, FG, (r.w - 24) as f32);
            overlay.hits.push((r, OverlayHit::Power(*action)));
        }
        overlay.hits.push((Rect { x: mx, y: my, w: MENU_W, h: mh }, OverlayHit::Inside));
        overlay.hits.push((Rect { x: 0, y: 0, w: w as i32, h: h as i32 }, OverlayHit::Outside));
        let surface = overlay.layer.wl_surface();
        self.scales.prepare(&self.qh, surface, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            overlay.layer.commit();
        }
        self.power_menu = Some(overlay);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
