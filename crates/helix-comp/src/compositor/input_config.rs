//! Applying keyboard, mouse and touchpad settings
//! (`helix_comp::input_settings`).

use helix_comp::input_settings::{self, InputSettings, Pointer, Touchpad};
use smithay::input::keyboard::{Layout, XkbConfig};
use smithay::reexports::input::{
    AccelProfile, ClickMethod, Device, DeviceCapability, ScrollMethod, SendEventsMode, TapButtonMap,
};

use super::Helix;

impl Helix {
    /// Read the settings and apply whatever changed.
    pub fn apply_input_settings(&mut self) {
        let new = input_settings::read();
        let old = self.input_settings.replace(new.clone());
        let keyboard = self.seat.get_keyboard().expect("seat has a keyboard");

        let same_map = |a: &InputSettings, b: &InputSettings| match (&a.keymap, &b.keymap) {
            (Some(a), Some(b)) => (&a.model, &a.layout, &a.variant, &a.options) == (&b.model, &b.layout, &b.variant, &b.options),
            (None, None) => true,
            _ => false,
        };
        if old.as_ref().is_none_or(|old| !same_map(old, &new)) {
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
        if let Some(map) = &new.keymap {
            if old.as_ref().and_then(|o| o.keymap.as_ref()) != Some(map) {
                keyboard.with_xkb_state(self, |mut context| context.set_layout(Layout(map.active)));
                tracing::info!(active = map.active, "keyboard layout active");
            }
        }
        if old.as_ref().is_none_or(|old| old.repeat != new.repeat) {
            keyboard.change_repeat_info(new.repeat.0, new.repeat.1);
            tracing::info!(rate = new.repeat.0, delay = new.repeat.1, "key repeat");
        }
        if old.as_ref().is_none_or(|old| old.mouse != new.mouse || old.touchpad != new.touchpad) {
            for device in self.input_devices.clone() {
                configure_device(device, &new);
            }
        }
    }

    /// A libinput device appeared (bare hardware).
    pub fn input_device_added(&mut self, device: &Device) {
        let settings = self.input_settings.clone().unwrap_or_default();
        configure_device(device.clone(), &settings);
        self.input_devices.push(device.clone());
    }

    pub fn input_device_removed(&mut self, device: &Device) {
        self.input_devices.retain(|d| d != device);
    }
}

fn configure_device(mut device: Device, settings: &InputSettings) {
    if !device.has_capability(DeviceCapability::Pointer) {
        return;
    }
    if device.config_tap_finger_count() > 0 {
        configure_touchpad(&mut device, &settings.touchpad);
    } else {
        configure_pointer(&mut device, &settings.mouse);
    }
}

// Unsupported options report an error per device; that is expected (a mouse
// has no tapping), so the results are ignored.
fn configure_pointer(device: &mut Device, p: &Pointer) {
    let _ = device.config_accel_set_speed(p.speed.clamp(-1.0, 1.0));
    let profile = match p.accel_profile.as_str() {
        "flat" => Some(AccelProfile::Flat),
        "adaptive" => Some(AccelProfile::Adaptive),
        _ => device.config_accel_default_profile(),
    };
    if let Some(profile) = profile {
        let _ = device.config_accel_set_profile(profile);
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
