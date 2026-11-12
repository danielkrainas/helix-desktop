//! A stylus's eraser button (helix-desktop-spec
//! `contracts/gsettings/input-settings.md`, "Tablets"): libinput 1.29
//! added a setting that makes the firmware eraser button send a stylus
//! button instead of turning the pen into an eraser. Helix builds against
//! older libinput too, so the functions are looked up when first needed;
//! without them the setting has no effect.

use std::ffi::{c_void, CStr};
use std::sync::OnceLock;

/// libinput's eraser button modes (`LIBINPUT_CONFIG_ERASER_BUTTON_*`).
pub const MODE_DEFAULT: u32 = 0;
pub const MODE_BUTTON: u32 = 1;
/// `LIBINPUT_CONFIG_STATUS_SUCCESS`.
const SUCCESS: u32 = 0;

type GetModes = unsafe extern "C" fn(*mut c_void) -> u32;
type SetValue = unsafe extern "C" fn(*mut c_void, u32) -> u32;

struct Api {
    get_modes: GetModes,
    set_mode: SetValue,
    set_button: SetValue,
}

fn symbol(name: &CStr) -> Option<*mut c_void> {
    // SAFETY: dlsym with RTLD_DEFAULT searches the libraries already
    // loaded (libinput is linked); a missing name gives null.
    let p = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
    (!p.is_null()).then_some(p)
}

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| {
        let get_modes = symbol(c"libinput_tablet_tool_config_eraser_button_get_modes")?;
        let set_mode = symbol(c"libinput_tablet_tool_config_eraser_button_set_mode")?;
        let set_button = symbol(c"libinput_tablet_tool_config_eraser_button_set_button")?;
        // SAFETY: these are libinput's functions of these signatures
        // (libinput.h, since 1.29).
        unsafe {
            Some(Api {
                get_modes: std::mem::transmute::<*mut c_void, GetModes>(get_modes),
                set_mode: std::mem::transmute::<*mut c_void, SetValue>(set_mode),
                set_button: std::mem::transmute::<*mut c_void, SetValue>(set_button),
            })
        }
    })
    .as_ref()
}

/// Whether the running libinput can configure eraser buttons.
pub fn available() -> bool {
    api().is_some()
}

/// Why configuring failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// libinput is older than 1.29.
    Unavailable,
    /// The tool has no eraser button that can be a button.
    NoEraserButton,
    /// libinput refused the setting (its status code).
    Status(u32),
}

/// Configure `tool` (a `struct libinput_tablet_tool *`): `Some(button)`
/// makes its eraser button send `button` (an evdev code); `None` gives
/// it back its default behavior.
///
/// # Safety
/// `tool` must be a live libinput tablet tool.
pub unsafe fn configure(tool: *mut c_void, button: Option<u32>) -> Result<(), Refused> {
    let api = api().ok_or(Refused::Unavailable)?;
    // SAFETY: the caller's promise about `tool`.
    let modes = unsafe { (api.get_modes)(tool) };
    let Some(button) = button else {
        if modes & MODE_BUTTON == 0 {
            return Ok(());
        }
        // SAFETY: as above.
        let status = unsafe { (api.set_mode)(tool, MODE_DEFAULT) };
        return if status == SUCCESS { Ok(()) } else { Err(Refused::Status(status)) };
    };
    if modes & MODE_BUTTON == 0 {
        return Err(Refused::NoEraserButton);
    }
    // SAFETY: as above.
    let status = unsafe { (api.set_button)(tool, button) };
    if status != SUCCESS {
        return Err(Refused::Status(status));
    }
    // SAFETY: as above.
    let status = unsafe { (api.set_mode)(tool, MODE_BUTTON) };
    if status == SUCCESS { Ok(()) } else { Err(Refused::Status(status)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On this machine's libinput the functions may or may not exist;
    /// either way nothing is called without them.
    #[test]
    fn lookup() {
        let present = symbol(c"libinput_tablet_tool_config_eraser_button_set_mode").is_some();
        assert_eq!(available(), present);
        if !present {
            // SAFETY: never dereferenced: the API is missing.
            assert_eq!(unsafe { configure(std::ptr::null_mut(), Some(0x149)) }, Err(Refused::Unavailable));
        }
        // Something every process has, to show the lookup itself works.
        assert!(symbol(c"malloc").is_some());
    }
}
