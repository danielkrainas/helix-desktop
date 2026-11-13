//! Applying keyboard and pointing device settings
//! (`helix_comp::input_settings`).

use helix_comp::input_settings::{self, AccelCurve, InputSettings, Pointer, PointingStick, Touchpad};
use smithay::input::keyboard::{Layout, XkbConfig};
use smithay::reexports::input::{
    AccelProfile, AsRaw, ClickMethod, Device, DeviceCapability, Led, ScrollButtonLockState, ScrollMethod, SendEventsMode,
    TapButtonMap,
};

use super::Helix;

impl Helix {
    /// Read the settings and apply whatever changed.
    pub fn apply_input_settings(&mut self) {
        let new = input_settings::read();
        let old = self.input_settings.replace(new.clone());
        let keyboard = self.seat.get_keyboard().expect("seat has a keyboard");
        // A new keymap starts with no lock keys on; they are kept.
        let locks = keyboard.modifier_state();

        let same_map = |a: &InputSettings, b: &InputSettings| match (&a.keymap, &b.keymap) {
            (Some(a), Some(b)) => (&a.model, &a.layout, &a.variant, &a.options) == (&b.model, &b.layout, &b.variant, &b.options),
            (None, None) => true,
            _ => false,
        };
        // A remote desktop session's locked keymap holds off the input
        // sources (they are applied again when it ends).
        let held = self.keymap_locked();
        let mut keymap_applied = false;
        if !held && old.as_ref().is_none_or(|old| !same_map(old, &new)) {
            keymap_applied = true;
            let result = match &new.keymap {
                Some(map) => keyboard.set_xkb_config(
                    self,
                    XkbConfig {
                        model: &map.model,
                        layout: &map.layout,
                        variant: &map.variant,
                        options: Some(map.options.clone()),
                        ..XkbConfig::default()
                    },
                ),
                None if old.is_some() => keyboard.set_xkb_config(self, XkbConfig::default()),
                None => Ok(()),
            };
            match result {
                Ok(()) => tracing::info!(keymap = ?new.keymap, "keymap set"),
                Err(err) => tracing::warn!(?err, keymap = ?new.keymap, "cannot use the configured keymap"),
            }
        }
        if let Some(map) = new.keymap.as_ref().filter(|_| !held) {
            if old.as_ref().and_then(|o| o.keymap.as_ref()) != Some(map) {
                keymap_applied = true;
                keyboard.with_xkb_state(self, |mut context| context.set_layout(Layout(map.active)));
                tracing::info!(active = map.active, "keyboard layout active");
            }
        }
        if keymap_applied {
            self.set_lock_keys(Some(locks.num_lock), Some(locks.caps_lock));
            self.configured_keymap_applied();
        }
        if old.is_none() {
            if let Some(on) = new.numlock {
                tracing::info!(on, "num lock restored");
                self.set_lock_keys(Some(on), None);
            }
            self.saved_numlock = Some(self.seat.get_keyboard().is_some_and(|k| k.modifier_state().num_lock));
        }
        if old.as_ref().is_none_or(|old| old.repeat != new.repeat) {
            keyboard.change_repeat_info(new.repeat.0, new.repeat.1);
            tracing::info!(rate = new.repeat.0, delay = new.repeat.1, "key repeat");
        }
        let pointers = |s: &InputSettings| (s.mouse.clone(), s.touchpad.clone(), s.trackball.clone(), s.pointing_stick.clone());
        if old.as_ref().is_none_or(|old| pointers(old) != pointers(&new)) {
            for device in self.input_devices.clone() {
                configure_device(device, &new);
            }
        }
    }

    /// Turns Num Lock and Caps Lock on or off (`None` leaves one as it
    /// is), telling the focused app and the keyboards' lights.
    pub fn set_lock_keys(&mut self, num: Option<bool>, caps: Option<bool>) {
        let keyboard = self.seat.get_keyboard().expect("seat has a keyboard");
        let mut mods = keyboard.modifier_state();
        let before = (mods.num_lock, mods.caps_lock);
        mods.num_lock = num.unwrap_or(mods.num_lock);
        mods.caps_lock = caps.unwrap_or(mods.caps_lock);
        if (mods.num_lock, mods.caps_lock) == before {
            return;
        }
        let leds = keyboard.led_state();
        keyboard.set_modifier_state(mods);
        if let Some(focus) = keyboard.current_focus() {
            use smithay::input::keyboard::KeyboardTarget;
            let mods = keyboard.modifier_state();
            focus.modifiers(&self.seat.clone(), self, mods, smithay::utils::SERIAL_COUNTER.next_serial());
        }
        let now = keyboard.led_state();
        if now != leds {
            self.update_leds(now);
        }
    }

    /// Lights the keyboards' lock key lights (bare hardware).
    pub fn update_leds(&mut self, state: smithay::input::keyboard::LedState) {
        let leds = helix_comp::input_settings::leds(state.num, state.caps, state.scroll);
        for device in &mut self.input_devices {
            if device.has_capability(DeviceCapability::Keyboard) {
                device.led_update(Led::from_bits_truncate(leds));
            }
        }
    }

    /// Remembers Num Lock for the next session when it changed (after a
    /// key).
    pub fn note_numlock(&mut self) {
        let on = self.seat.get_keyboard().is_some_and(|k| k.modifier_state().num_lock);
        if self.saved_numlock.is_some_and(|saved| saved != on) {
            self.saved_numlock = Some(on);
            input_settings::save_numlock(on);
        }
    }

    /// A libinput device appeared (bare hardware).
    pub fn input_device_added(&mut self, device: &Device) {
        if device.has_capability(DeviceCapability::TabletTool) {
            if let Some(size) = device.size() {
                self.tablet.sizes.insert(device.sysname().to_owned(), size);
            }
        }
        let settings = self.input_settings.clone().unwrap_or_default();
        configure_device(device.clone(), &settings);
        let mut device = device.clone();
        if device.has_capability(DeviceCapability::Keyboard) {
            if let Some(keyboard) = self.seat.get_keyboard() {
                let state = keyboard.led_state();
                device.led_update(Led::from_bits_truncate(input_settings::leds(state.num, state.caps, state.scroll)));
            }
        }
        self.input_devices.push(device.clone());
        self.tablet_device_added(&device);
    }

    pub fn input_device_removed(&mut self, device: &Device) {
        self.input_devices.retain(|d| d != device);
        self.tablet_device_removed(device);
    }
}

/// What kind of pointing device this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Mouse,
    Touchpad,
    Trackball,
    PointingStick,
}

fn kind(device: &Device) -> Kind {
    if device.config_tap_finger_count() > 0 {
        return Kind::Touchpad;
    }
    let tagged = |property: &str| {
        smithay::reexports::udev::Device::from_subsystem_sysname("input".into(), device.sysname().into())
            .ok()
            .and_then(|d| d.property_value(property).map(|v| v == "1"))
            .unwrap_or(false)
    };
    if tagged("ID_INPUT_TRACKBALL") {
        Kind::Trackball
    } else if tagged("ID_INPUT_POINTINGSTICK") {
        Kind::PointingStick
    } else {
        Kind::Mouse
    }
}

fn configure_device(mut device: Device, settings: &InputSettings) {
    if !device.has_capability(DeviceCapability::Pointer) {
        return;
    }
    let kind = kind(&device);
    tracing::debug!(name = device.name(), ?kind, "configuring pointing device");
    match kind {
        Kind::Touchpad => configure_touchpad(&mut device, &settings.touchpad),
        Kind::Mouse => configure_pointer(&mut device, &settings.mouse),
        Kind::Trackball => configure_pointer(&mut device, &settings.trackball),
        Kind::PointingStick => configure_pointing_stick(&mut device, &settings.pointing_stick),
    }
}

fn configure_pointing_stick(device: &mut Device, p: &PointingStick) {
    configure_pointer(device, &p.pointer);
    let method = match p.scroll_method.as_str() {
        "on-button-down" => Some(ScrollMethod::OnButtonDown),
        "none" => Some(ScrollMethod::NoScroll),
        _ => device.config_scroll_default_method(),
    };
    if let Some(method) = method {
        let _ = device.config_scroll_set_method(method);
    }
    if p.scroll_method == "on-button-down" {
        let _ = device.config_scroll_set_button(0x112);
    }
}

/// Apply a custom acceleration profile (libinput 1.23). False if the
/// device or libinput cannot.
fn apply_custom_accel(device: &mut Device, pointer: Option<&AccelCurve>, scroll: Option<&AccelCurve>) -> bool {
    use smithay::reexports::input::ffi;
    // SAFETY: the config is created, filled and destroyed here; the device
    // pointer is valid while `device` lives.
    unsafe {
        let config = ffi::libinput_config_accel_create(ffi::libinput_config_accel_profile_LIBINPUT_CONFIG_ACCEL_PROFILE_CUSTOM);
        if config.is_null() {
            return false;
        }
        let mut ok = true;
        for (curve, accel_type) in [
            (pointer, ffi::libinput_config_accel_type_LIBINPUT_ACCEL_TYPE_MOTION),
            (scroll, ffi::libinput_config_accel_type_LIBINPUT_ACCEL_TYPE_SCROLL),
        ] {
            if let Some(curve) = curve {
                let mut points = curve.speeds.clone();
                let status = ffi::libinput_config_accel_set_points(config, accel_type, curve.step, points.len(), points.as_mut_ptr());
                ok &= status == ffi::libinput_config_status_LIBINPUT_CONFIG_STATUS_SUCCESS;
            }
        }
        if ok {
            let status = ffi::libinput_device_config_accel_apply(device.as_raw_mut(), config);
            ok = status == ffi::libinput_config_status_LIBINPUT_CONFIG_STATUS_SUCCESS;
        }
        ffi::libinput_config_accel_destroy(config);
        ok
    }
}

// Unsupported options report an error per device; that is expected (a mouse
// has no tapping), so the results are ignored.
fn configure_pointer(device: &mut Device, p: &Pointer) {
    let _ = device.config_accel_set_speed(p.speed.clamp(-1.0, 1.0));
    let custom = p.accel_profile == "custom"
        && apply_custom_accel(device, p.custom_accel.pointer.as_ref(), p.custom_accel.scroll.as_ref());
    if !custom {
        let profile = match p.accel_profile.as_str() {
            "flat" => Some(AccelProfile::Flat),
            "adaptive" => Some(AccelProfile::Adaptive),
            _ => device.config_accel_default_profile(),
        };
        if let Some(profile) = profile {
            let _ = device.config_accel_set_profile(profile);
        }
    }
    match input_settings::x_button_to_evdev(p.scroll_button) {
        Some(button) => {
            let _ = device.config_scroll_set_method(ScrollMethod::OnButtonDown);
            let _ = device.config_scroll_set_button(button);
            let lock = if p.scroll_button_lock { ScrollButtonLockState::Enabled } else { ScrollButtonLockState::Disabled };
            let _ = device.config_scroll_set_button_lock(lock);
        }
        None if device.config_tap_finger_count() == 0 => {
            if let Some(method) = device.config_scroll_default_method() {
                let _ = device.config_scroll_set_method(method);
            }
        }
        None => {}
    }
    let _ = device.config_scroll_set_natural_scroll_enabled(p.natural_scroll);
    let _ = device.config_left_handed_set(p.left_handed);
    let _ = device.config_middle_emulation_set_enabled(p.middle_click_emulation);
}

fn configure_touchpad(device: &mut Device, t: &Touchpad) {
    configure_pointer(device, &t.pointer);
    let _ = device.config_tap_set_enabled(t.tap_to_click);
    let _ = device.config_tap_set_drag_enabled(t.tap_and_drag);
    let _ = device.config_tap_set_drag_lock_enabled(t.tap_and_drag_lock);
    let map = match t.tap_button_map.as_str() {
        "lrm" => Some(TapButtonMap::LeftRightMiddle),
        "lmr" => Some(TapButtonMap::LeftMiddleRight),
        _ => device.config_tap_default_button_map(),
    };
    if let Some(map) = map {
        let _ = device.config_tap_set_button_map(map);
    }
    let scroll = if t.two_finger_scrolling {
        ScrollMethod::TwoFinger
    } else if t.edge_scrolling {
        ScrollMethod::Edge
    } else {
        ScrollMethod::NoScroll
    };
    let _ = device.config_scroll_set_method(scroll);
    let _ = device.config_dwt_set_enabled(t.disable_while_typing);
    let click = match t.click_method.as_str() {
        "areas" => Some(ClickMethod::ButtonAreas),
        "fingers" => Some(ClickMethod::Clickfinger),
        "none" => None,
        _ => device.config_click_default_method(),
    };
    if let Some(click) = click {
        let _ = device.config_click_set_method(click);
    }
    let mode = match t.send_events.as_str() {
        "disabled" => SendEventsMode::DISABLED,
        "disabled-on-external-mouse" => SendEventsMode::DISABLED_ON_EXTERNAL_MOUSE,
        _ => SendEventsMode::ENABLED,
    };
    let _ = device.config_send_events_set_mode(mode);
}
