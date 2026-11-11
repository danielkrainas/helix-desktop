//! Server side of the private `helix_keybindings_v1` protocol: the shell
//! binds global key combinations; presses are delivered to it instead of
//! the focused client.

use std::collections::HashMap;

use helix_protocols::server::helix_keybinding_v1::{self, HelixKeybindingV1};
use helix_protocols::server::helix_keybindings_manager_v1::{
    self, HelixKeybindingsManagerV1, Modifier,
};
use smithay::input::keyboard::{xkb, Keysym};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

use super::shortcuts::{BindError, Mods};
use super::{ClientState, Helix};

/// Live bindings by id.
#[derive(Debug, Default)]
pub struct Keybindings {
    objects: HashMap<u32, HelixKeybindingV1>,
    next_id: u32,
}

impl Keybindings {
    pub fn object(&self, id: u32) -> Option<&HelixKeybindingV1> {
        self.objects.get(&id)
    }
}

/// The binding id stored on each helix_keybinding_v1 object (0 = inert).
pub struct BindingId(pub u32);

/// Resolve an XKB keysym name; single letters are matched unshifted.
pub fn keysym_from_name(name: &str) -> Option<Keysym> {
    let name = if name.chars().count() == 1 { name.to_lowercase() } else { name.to_owned() };
    let sym = xkb::keysym_from_name(&name, xkb::KEYSYM_NO_FLAGS);
    (sym.raw() != 0).then_some(sym)
}

pub fn mods_from_wire(bits: u32) -> Mods {
    let m = Modifier::from_bits_truncate(bits);
    Mods {
        shift: m.contains(Modifier::Shift),
        ctrl: m.contains(Modifier::Ctrl),
        alt: m.contains(Modifier::Alt),
        logo: m.contains(Modifier::Logo),
    }
}

impl GlobalDispatch<HelixKeybindingsManagerV1, ()> for Helix {
    fn bind(
        _state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<HelixKeybindingsManagerV1>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }

    fn can_view(client: Client, _data: &()) -> bool {
        // Only the shell the compositor launched itself may bind keys.
        client.get_data::<ClientState>().is_some_and(|c| c.privileged)
    }
}

impl Dispatch<HelixKeybindingsManagerV1, ()> for Helix {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &HelixKeybindingsManagerV1,
        request: helix_keybindings_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            helix_keybindings_manager_v1::Request::Bind { id, modifiers, keysym } => {
                let mods = match modifiers {
                    smithay::reexports::wayland_server::WEnum::Value(m) => mods_from_wire(m.bits()),
                    smithay::reexports::wayland_server::WEnum::Unknown(bits) => mods_from_wire(bits),
                };
                let Some(sym) = keysym_from_name(&keysym) else {
                    let obj = data_init.init(id, BindingId(0));
                    obj.failed(helix_keybinding_v1::ErrorReason::UnknownKey);
                    tracing::info!(keysym, "keybinding refused: unknown key");
                    return;
                };
                state.keybindings.next_id = state.keybindings.next_id.wrapping_add(1).max(1);
                let binding = state.keybindings.next_id;
                let obj = data_init.init(id, BindingId(binding));
                match state.shortcuts.bind(binding, mods, sym) {
                    Ok(()) => {
                        tracing::info!(keysym, ?mods, binding, "keybinding bound");
                        state.keybindings.objects.insert(binding, obj.clone());
                        obj.bound();
                    }
                    Err(BindError::Taken) => {
                        tracing::info!(keysym, ?mods, "keybinding refused: taken");
                        obj.failed(helix_keybinding_v1::ErrorReason::Taken);
                    }
                }
            }
            helix_keybindings_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<HelixKeybindingV1, BindingId> for Helix {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &HelixKeybindingV1,
        _request: helix_keybinding_v1::Request,
        _data: &BindingId,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // Only `destroy`; cleanup happens in `destroyed`.
    }

    fn destroyed(
        state: &mut Self,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        resource: &HelixKeybindingV1,
        data: &BindingId,
    ) {
        if data.0 != 0 && state.keybindings.objects.remove(&data.0).is_some() {
            state.shortcuts.unbind(data.0);
            tracing::debug!(binding = data.0, id = ?resource.id(), "keybinding removed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keysym_names() {
        assert_eq!(keysym_from_name("a"), Some(Keysym::a));
        assert_eq!(keysym_from_name("A"), Some(Keysym::a), "letters are matched unshifted");
        assert_eq!(keysym_from_name("F9"), Some(Keysym::F9));
        assert_eq!(keysym_from_name("XF86AudioMute"), Some(Keysym::XF86_AudioMute));
        assert_eq!(keysym_from_name("NotAKey"), None);
    }

    #[test]
    fn wire_modifiers() {
        assert_eq!(mods_from_wire(0), Mods::default());
        let m = mods_from_wire(2 | 8);
        assert!(m.ctrl && m.logo && !m.alt && !m.shift);
    }
}
