//! Touchscreens mapped to their monitor, and drawing tablets: their tools
//! go to apps over the tablet protocol (`helix_comp::tablet_v2`) where the
//! app asks for it, and drive the pointer elsewhere; pads' buttons, rings
//! and strips go to the focused app or do what their settings say
//! (helix-desktop-spec `contracts/gsettings/input-settings.md`,
//! "Touchscreens and tablets").

use std::collections::HashMap;

use helix_comp::edid::Identity;
use helix_comp::tablet::{self, ButtonAction, PadAction, Stylus, Tablet};
use helix_comp::tablet_v2::{Axes, PadGroup, PadInfo, TabletInfo, ToolCaps, ToolInfo};
use smithay::backend::input::{
    ButtonState, Device, DeviceCapability, Event, InputBackend, ProximityState, TabletToolButtonEvent, TabletToolCapabilities, TabletToolDescriptor, TabletToolEvent,
    TabletToolProximityEvent, TabletToolTipEvent, TabletToolTipState, TabletToolType,
};
use smithay::input::pointer::{CursorImageAttributes, CursorImageStatus, CursorImageSurfaceData, MotionEvent};
use smithay::reexports::wayland_protocols::wp::tablet::zv2::server::zwp_tablet_tool_v2;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::SERIAL_COUNTER;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle};

use super::Helix;

const BTN_LEFT: u32 = 0x110;
/// Fractions of the tablet are read at this resolution.
const FRACTION: i32 = 1 << 20;

/// A pen in proximity: its tablet's settings, read when it came near.
struct Pen {
    device: String,
    tablet: Tablet,
    stylus: Stylus,
    /// The tool, as the tablet protocol knows it.
    tool: u64,
    eraser: bool,
    tip_down: bool,
    /// Over an app that takes tablet events (no pointer emulation there).
    on_client: bool,
}

/// A stable key for a tool: its type, serial and hardware id.
pub fn tool_key(desc: &TabletToolDescriptor) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (desc.tool_type as u32, desc.hardware_serial, desc.hardware_id_wacom).hash(&mut h);
    h.finish()
}

fn tool_info(desc: &TabletToolDescriptor) -> ToolInfo {
    use zwp_tablet_tool_v2::Type as T;
    let kind = match desc.tool_type {
        TabletToolType::Eraser => T::Eraser,
        TabletToolType::Brush => T::Brush,
        TabletToolType::Pencil => T::Pencil,
        TabletToolType::Airbrush => T::Airbrush,
        TabletToolType::Mouse => T::Mouse,
        TabletToolType::Lens => T::Lens,
        _ => T::Pen,
    };
    let c = desc.capabilities;
    ToolInfo {
        id: tool_key(desc),
        kind,
        serial: desc.hardware_serial,
        hardware_id: desc.hardware_id_wacom,
        caps: ToolCaps {
            tilt: c.contains(TabletToolCapabilities::TILT),
            pressure: c.contains(TabletToolCapabilities::PRESSURE),
            distance: c.contains(TabletToolCapabilities::DISTANCE),
            rotation: c.contains(TabletToolCapabilities::ROTATION),
            slider: c.contains(TabletToolCapabilities::SLIDER),
            wheel: c.contains(TabletToolCapabilities::WHEEL),
        },
    }
}

#[derive(Default)]
pub struct TabletInput {
    /// Tablets' sizes (mm), by device id, from libinput.
    pub sizes: HashMap<String, (f64, f64)>,
    /// Monitors stepped past with `switch-monitor`, by device id.
    switched: HashMap<String, usize>,
    pen: Option<Pen>,
    /// Touchscreens' `output` setting, by device id (read at each touch down).
    touch_outputs: HashMap<String, Option<Vec<String>>>,
    /// Pads' USB ids, by device id.
    pad_usb: HashMap<String, Option<(u32, u32)>>,
    /// Tools whose eraser button was set up, by serial: in button mode.
    eraser_configured: HashMap<u64, bool>,
}

impl Helix {
    /// The output named by a [vendor, product, serial] setting.
    fn output_for_spec(&self, spec: &[String]) -> Option<Output> {
        self.space.outputs().find(|o| o.user_data().get::<Identity>().is_some_and(|id| id.matches(spec))).cloned()
    }

    /// Where a touchscreen's touches land: its `output` setting's monitor,
    /// else the built-in panel, else the primary monitor.
    pub(super) fn touch_area_for(&mut self, device: &impl Device, refresh: bool) -> Option<Rectangle<i32, Logical>> {
        let id = device.id();
        if refresh || !self.tablet.touch_outputs.contains_key(&id) {
            let spec = tablet::touchscreen_output(device.usb_id());
            self.tablet.touch_outputs.insert(id.clone(), spec);
        }
        let named = self.tablet.touch_outputs.get(&id).cloned().flatten().and_then(|spec| self.output_for_spec(&spec));
        match named {
            Some(output) => self.space.output_geometry(&output),
            None => self.touch_area(),
        }
    }

    /// The monitor a tablet maps to: its `output` setting's, else the
    /// primary; `switch-monitor` presses step on through the monitors.
    fn tablet_output(&self, device: &str, tablet: &Tablet) -> Option<Rectangle<i32, Logical>> {
        let base = tablet.output.as_ref().and_then(|spec| self.output_for_spec(spec)).or_else(|| self.primary())?;
        let mut outputs: Vec<(Output, Rectangle<i32, Logical>)> = self.space.outputs().filter_map(|o| Some((o.clone(), self.space.output_geometry(o)?))).collect();
        outputs.sort_by_key(|(_, g)| (g.loc.x, g.loc.y));
        let start = outputs.iter().position(|(o, _)| *o == base).unwrap_or(0);
        let step = self.tablet.switched.get(device).copied().unwrap_or(0);
        outputs.get((start + step) % outputs.len().max(1)).map(|(_, g)| *g)
    }

    fn pen_position<B: InputBackend>(&self, pen: &Pen, event: &impl TabletToolEvent<B>) -> Option<Point<f64, Logical>> {
        let area = self.tablet_output(&pen.device, &pen.tablet)?;
        let frac = (event.x_transformed(FRACTION) / f64::from(FRACTION), event.y_transformed(FRACTION) / f64::from(FRACTION));
        let size = self.tablet.sizes.get(&pen.device).copied();
        let (x, y) = tablet::map_point(frac, &pen.tablet, size, (f64::from(area.size.w), f64::from(area.size.h)))?;
        Some(area.loc.to_f64() + Point::from((x, y)))
    }

    /// The pen moved: over an app that takes tablet events, the tool's
    /// events go to it (the pointer follows without telling it); elsewhere
    /// the pen moves the pointer.
    fn pen_moved<B: InputBackend>(&mut self, event: &impl TabletToolEvent<B>) {
        let Some(mut pen) = self.tablet.pen.take() else { return };
        let time = event.time_msec();
        let pos = if pen.tablet.relative {
            let d = event.delta();
            let current = self.seat.get_pointer().map(|p| p.current_location()).unwrap_or_default();
            Some(self.clamp_to_outputs(current + Point::from((d.x * tablet::RELATIVE_PX_PER_MM, d.y * tablet::RELATIVE_PX_PER_MM))))
        } else {
            self.pen_position(&pen, event)
        };
        let Some(pos) = pos else {
            self.tablet.pen = Some(pen);
            return;
        };
        let pointer = self.seat.get_pointer().unwrap();
        let serial: u32 = SERIAL_COUNTER.next_serial().into();
        // Grabs (window moves, menus), frames and the lock screen keep
        // the pen a pointer.
        let target = if pointer.is_grabbed() || self.frame_under(pos).is_some() { None } else { self.surface_under(pos) };
        let takes_tablet = target.as_ref().is_some_and(|(surface, _)| surface.client().is_some_and(|c| self.tablet_v2.has_seat(&c.id())));
        let over = if takes_tablet { target.as_ref().map(|(surface, origin)| (surface, (pos.x - origin.x, pos.y - origin.y))) } else { None };
        let aware = self.tablet_v2.tool_over(pen.tool, &pen.device, over, serial, time);
        if aware {
            if !pen.on_client {
                // The pointer leaves the app; the app sets the tool's cursor.
                self.cursor_status = CursorImageStatus::default_named();
            }
            pointer.motion(self, None, &MotionEvent { location: pos, serial: serial.into(), time });
            pointer.frame(self);
            let curve = if pen.eraser { pen.stylus.eraser_pressure } else { pen.stylus.pressure };
            let axes = Axes {
                pressure: event.pressure_has_changed().then(|| curve.apply(event.pressure())),
                distance: event.distance_has_changed().then(|| event.distance()),
                tilt: (event.tilt_x_has_changed() || event.tilt_y_has_changed()).then(|| event.tilt()),
                rotation: event.rotation_has_changed().then(|| event.rotation()),
                slider: event.slider_has_changed().then(|| event.slider_position()),
                wheel: event.wheel_has_changed().then(|| (event.wheel_delta(), event.wheel_delta_discrete())),
            };
            self.tablet_v2.tool_axes(pen.tool, &axes);
            self.tablet_v2.tool_frame(pen.tool, time);
        } else {
            if pen.on_client {
                self.cursor_status = CursorImageStatus::default_named();
            }
            if pen.tablet.relative {
                let current = pointer.current_location();
                let delta = pos - current;
                self.pointer_moved_by(delta, delta, Event::time(event), time);
            } else {
                self.pointer_moved_to(pos, Event::time(event), time);
            }
        }
        pen.on_client = aware;
        self.tablet.pen = Some(pen);
        self.device_moved(time);
    }

    pub(super) fn tablet_proximity<B: InputBackend>(&mut self, event: &B::TabletToolProximityEvent) {
        let device = event.device();
        match event.state() {
            ProximityState::In => {
                let usb = device.usb_id();
                let tool = event.tool();
                let info = tool_info(&tool);
                if !self.tablet_v2.has_tool(info.id) {
                    let dh = self.display_handle.clone();
                    self.tablet_v2.add_tool::<Helix>(&dh, info.clone());
                }
                if !self.tablet_v2.tablets().iter().any(|t| t.id == device.id()) {
                    let dh = self.display_handle.clone();
                    self.tablet_v2.add_tablet::<Helix>(&dh, tablet_info(&device.id(), &device.name(), usb));
                }
                self.tablet.pen = Some(Pen {
                    device: device.id(),
                    tablet: tablet::read_tablet(usb),
                    stylus: tablet::read_stylus_settings(tool.hardware_serial, usb),
                    tool: info.id,
                    eraser: tool.tool_type == TabletToolType::Eraser,
                    tip_down: false,
                    on_client: false,
                });
                self.pen_moved(event);
            }
            ProximityState::Out => {
                if let Some(pen) = self.tablet.pen.take() {
                    if pen.on_client {
                        if pen.tip_down {
                            self.tablet_v2.tool_tip(pen.tool, false, 0);
                        }
                        self.tablet_v2.tool_out(pen.tool, event.time_msec());
                        self.cursor_status = CursorImageStatus::default_named();
                    } else if pen.tip_down {
                        self.device_button(BTN_LEFT, ButtonState::Released, event.time_msec());
                    }
                }
            }
        }
    }

    pub(super) fn tablet_axis<B: InputBackend>(&mut self, event: &impl TabletToolEvent<B>) {
        self.pen_moved(event);
    }

    pub(super) fn tablet_tip<B: InputBackend>(&mut self, event: &B::TabletToolTipEvent) {
        let down = event.tip_state() == TabletToolTipState::Down;
        let Some(pen) = self.tablet.pen.as_mut() else { return };
        if pen.tip_down == down {
            return;
        }
        pen.tip_down = down;
        let (on_client, tool) = (pen.on_client, pen.tool);
        if on_client {
            // As a click would, the tip focuses the window it touches.
            if down && !self.lock.locked {
                let pos = self.seat.get_pointer().unwrap().current_location();
                if let Some(window) = self.window_under(pos).map(|(w, _)| w.clone()) {
                    self.focus_window_with(&window, self.wm_settings.raise_on_click);
                }
            }
            let serial = SERIAL_COUNTER.next_serial().into();
            self.tablet_v2.tool_tip(tool, down, serial);
            self.tablet_v2.tool_frame(tool, event.time_msec());
            return;
        }
        let state = if down { ButtonState::Pressed } else { ButtonState::Released };
        self.device_button(BTN_LEFT, state, event.time_msec());
    }

    pub(super) fn tablet_button<B: InputBackend>(&mut self, event: &B::TabletToolButtonEvent) {
        let Some(pen) = self.tablet.pen.as_ref() else { return };
        let action = pen.stylus.buttons.iter().find(|(code, _)| *code == event.button()).map(|(_, a)| a.clone());
        let (on_client, tool) = (pen.on_client, pen.tool);
        match action {
            Some(ButtonAction::Button(code)) if on_client => {
                let serial = SERIAL_COUNTER.next_serial().into();
                self.tablet_v2.tool_button(tool, code, event.button_state() == ButtonState::Pressed, serial);
                self.tablet_v2.tool_frame(tool, event.time_msec());
            }
            Some(ButtonAction::Button(code)) => self.device_button(code, event.button_state(), event.time_msec()),
            Some(ButtonAction::SwitchMonitor) if event.button_state() == ButtonState::Pressed => {
                let device = pen.device.clone();
                self.switch_tablet_monitor(&device);
            }
            Some(ButtonAction::Keybinding(keys)) if event.button_state() == ButtonState::Pressed => self.send_accelerator(&keys),
            _ => {}
        }
    }

    /// A tool came near (libinput only): set up its eraser button as its
    /// settings say (libinput 1.29 and later; see `eraser_button`).
    pub(super) fn configure_eraser_button(&mut self, event: &smithay::reexports::input::event::tablet_tool::TabletToolProximityEvent) {
        use smithay::reexports::input::event::tablet_tool::TabletToolEventTrait;
        use smithay::reexports::input::AsRaw;
        use smithay::backend::input::Device as _;
        use smithay::reexports::input::event::EventTrait;
        let tool = TabletToolEventTrait::tool(event);
        let usb = EventTrait::device(event).usb_id();
        let wanted = tablet::read_stylus_settings(tool.serial(), usb).eraser_button.is_some();
        let key = tool.serial();
        if self.tablet.eraser_configured.get(&key) == Some(&wanted) {
            return;
        }
        // SAFETY: the tool belongs to this live event.
        let result = unsafe { helix_comp::eraser_button::configure(tool.as_raw_mut().cast(), wanted.then_some(tablet::ERASER_BUTTON)) };
        match result {
            Ok(()) => {
                tracing::info!(serial = key, button_mode = wanted, "stylus eraser button set up");
                self.tablet.eraser_configured.insert(key, wanted);
            }
            Err(helix_comp::eraser_button::Refused::Unavailable) if wanted => {
                tracing::info!("eraser-button-mode needs libinput 1.29 or later; ignored");
                self.tablet.eraser_configured.insert(key, wanted);
            }
            Err(helix_comp::eraser_button::Refused::Unavailable) => {}
            Err(err) => {
                tracing::info!(?err, serial = key, "the stylus eraser button cannot be set up");
                self.tablet.eraser_configured.insert(key, wanted);
            }
        }
    }

    fn switch_tablet_monitor(&mut self, device: &str) {
        *self.tablet.switched.entry(device.to_owned()).or_default() += 1;
        tracing::info!(device, "tablet switched to the next monitor");
    }

    /// A tablet or pad was plugged in (bare hardware).
    pub(super) fn tablet_device_added(&mut self, device: &smithay::reexports::input::Device) {
        let dh = self.display_handle.clone();
        let usb = Device::usb_id(device);
        let id = Device::id(device);
        if device.has_capability(DeviceCapability::TabletTool.into()) {
            self.tablet_v2.add_tablet::<Helix>(&dh, tablet_info(&id, &Device::name(device), usb));
        }
        if device.has_capability(DeviceCapability::TabletPad.into()) {
            let tablet = self
                .tablet_v2
                .tablets()
                .iter()
                .find(|t| t.usb.is_some() && t.usb == usb)
                .or_else(|| self.tablet_v2.tablets().first())
                .map(|t| t.id.clone())
                .unwrap_or_default();
            let groups = (0..device.tablet_pad_number_of_mode_groups().max(0) as u32)
                .filter_map(|g| device.tablet_pad_mode_group(g))
                .map(|g| {
                    let count = |n: i32| 0..n.max(0) as u32;
                    PadGroup {
                        buttons: count(device.tablet_pad_number_of_buttons()).filter(|b| g.has_button(*b)).collect(),
                        rings: count(device.tablet_pad_number_of_rings()).filter(|r| g.has_ring(*r)).collect(),
                        strips: count(device.tablet_pad_number_of_strips()).filter(|s| g.has_strip(*s)).collect(),
                        modes: g.number_of_modes(),
                        mode: g.mode(),
                    }
                })
                .collect();
            let info = PadInfo { id: id.clone(), tablet, path: format!("/dev/input/{id}"), buttons: device.tablet_pad_number_of_buttons().max(0) as u32, groups };
            tracing::info!(?info, "tablet pad");
            self.tablet.pad_usb.insert(id, usb);
            self.tablet_v2.add_pad::<Helix>(&dh, info);
        }
    }

    pub(super) fn tablet_device_removed(&mut self, device: &smithay::reexports::input::Device) {
        let id = Device::id(device);
        self.tablet_v2.remove_tablet(&id);
        self.tablet_v2.remove_pad(&id);
        self.tablet.pad_usb.remove(&id);
    }

    /// The keyboard focus, if it is an app's surface.
    fn keyboard_surface(&self) -> Option<WlSurface> {
        let keyboard = self.seat.get_keyboard()?;
        keyboard.current_focus().map(|f| f.into())
    }

    /// A pad button, ring or strip (bare hardware).
    pub(super) fn pad_event(&mut self, event: smithay::reexports::input::event::TabletPadEvent) {
        use smithay::reexports::input::event::tablet_pad::{ButtonState as PadButton, RingAxisSource, StripAxisSource, TabletPadEventTrait};
        use smithay::reexports::input::event::{EventTrait, TabletPadEvent as E};
        let device = event.device();
        let pad = Device::id(&device);
        let time = TabletPadEventTrait::time(&event);
        let usb = self.tablet.pad_usb.get(&pad).copied().flatten();
        // Mode changes come with any event of the group.
        let group = TabletPadEventTrait::mode_group(&event).index() as usize;
        let mode = TabletPadEventTrait::mode(&event);
        let serial: u32 = SERIAL_COUNTER.next_serial().into();
        let focus = self.keyboard_surface();
        self.tablet_v2.pad_focus(&pad, focus.as_ref(), serial);
        self.tablet_v2.pad_mode(&pad, group, mode, time, serial);
        match event {
            E::Button(b) => {
                let button = b.button_number();
                let pressed = b.button_state() == PadButton::Pressed;
                match tablet::read_pad_button(usb, button) {
                    PadAction::App => self.tablet_v2.pad_button(&pad, time, button, pressed),
                    PadAction::Help if pressed => {
                        let count = device.tablet_pad_number_of_buttons().max(0) as u32;
                        let actions: Vec<(u32, PadAction)> = (0..count).map(|b| (b, tablet::read_pad_button(usb, b))).collect();
                        let said = self.tablet_v2.feedback(&pad);
                        self.pad_help(tablet::pad_help(&actions, &said));
                    }
                    PadAction::SwitchMonitor if pressed => {
                        let tablet = self.tablet_v2.pads().iter().find(|p| p.id == pad).map(|p| p.tablet.clone()).unwrap_or_default();
                        self.switch_tablet_monitor(&tablet);
                    }
                    PadAction::Keybinding(keys) if pressed => self.send_accelerator(&keys),
                    _ => {}
                }
            }
            E::Ring(r) => {
                let position = r.position();
                let finger = r.source() == RingAxisSource::Finger;
                self.tablet_v2.pad_ring(&pad, r.number(), (position >= 0.0).then_some(position), finger, time);
            }
            E::Strip(s) => {
                let position = s.position();
                let finger = s.source() == StripAxisSource::Finger;
                self.tablet_v2.pad_strip(&pad, s.number(), (position >= 0.0).then_some(position), finger, time);
            }
            _ => {}
        }
    }

    /// Type a key combination (`<Control>z`) to the focused app.
    fn send_accelerator(&mut self, accelerator: &str) {
        use smithay::backend::input::KeyState;
        use smithay::input::keyboard::{xkb, Keycode};
        let Some((mods, key)) = helix_comp::wm_settings::parse_accelerator(accelerator) else {
            tracing::warn!(accelerator, "pad button: not a key combination");
            return;
        };
        let keysym = xkb::keysym_from_name(&key, xkb::KEYSYM_CASE_INSENSITIVE);
        let Some((keycode, shift)) = self.key_for_keysym(keysym.raw()) else {
            tracing::warn!(accelerator, "pad button: no key for it in the layout");
            return;
        };
        // evdev codes of the left modifiers.
        let held: Vec<u32> = [(mods.ctrl, 29), (mods.alt, 56), (mods.shift || shift, 42), (mods.logo, 125)].into_iter().filter(|(on, _)| *on).map(|(_, c)| c).collect();
        let time = self.now_ms();
        for code in &held {
            self.key_input(Keycode::new(code + 8), KeyState::Pressed, time);
        }
        self.key_input(keycode, KeyState::Pressed, time);
        self.key_input(keycode, KeyState::Released, time);
        for code in held.iter().rev() {
            self.key_input(Keycode::new(code + 8), KeyState::Released, time);
        }
    }

    /// Show what the pad's buttons do (an on-screen display).
    fn pad_help(&mut self, text: String) {
        tracing::info!(text, "pad help");
        let _ = self.events.send(helix_comp::Event::Osd { icon: "input-tablet-symbolic".into(), label: text });
    }

    /// An app set the cursor for `tool`.
    /// A named cursor (`wp_cursor_shape_device_v1`) for `tool`, taken while
    /// it is over an app that takes tablet events.
    pub(super) fn set_tool_shape(&mut self, tool: u64, icon: smithay::input::pointer::CursorIcon) {
        if self.tablet.pen.as_ref().is_some_and(|p| p.tool == tool && p.on_client) {
            self.cursor_status = CursorImageStatus::Named(icon);
        }
    }

    pub(super) fn set_tool_cursor(&mut self, tool: u64, cursor: Option<(WlSurface, (i32, i32))>) {
        if !self.tablet.pen.as_ref().is_some_and(|p| p.tool == tool && p.on_client) {
            return;
        }
        self.cursor_status = match cursor {
            Some((surface, hotspot)) => {
                smithay::wayland::compositor::with_states(&surface, |states| {
                    states.data_map.insert_if_missing_threadsafe(|| CursorImageSurfaceData::new(CursorImageAttributes { hotspot: (0, 0).into() }));
                    if let Some(data) = states.data_map.get::<CursorImageSurfaceData>() {
                        data.lock().unwrap().hotspot = hotspot.into();
                    }
                });
                CursorImageStatus::Surface(surface)
            }
            None => CursorImageStatus::Hidden,
        };
    }
}

fn tablet_info(id: &str, name: &str, usb: Option<(u32, u32)>) -> TabletInfo {
    TabletInfo { id: id.to_owned(), name: name.to_owned(), usb, path: if id.starts_with("event") { format!("/dev/input/{id}") } else { String::new() } }
}
