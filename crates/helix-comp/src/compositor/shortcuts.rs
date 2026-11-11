//! Compositor-level keyboard shortcuts, decided independently of Smithay
//! types so the rules can be tested.
//!
//! | Keys | Action |
//! |------|--------|
//! | Super (alone) | Show applications (asks the shell) |
//! | Alt+F4 | Close the focused window |
//! | Super+Up | Toggle maximize |
//! | Super+Down | Restore a maximized window, else minimize |
//! | Super+L | Lock the screen (asks the shell) |
//! | Ctrl+Alt+F1..F12 | Switch VT (bare hardware only) |
//! | Ctrl+Alt+Backspace | Quit Helix (always available escape hatch) |
//!
//! After these, key combinations bound by the shell (via the private
//! `helix_keybindings_v1` protocol) are matched, with exact modifiers.

use smithay::input::keyboard::Keysym;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    ToggleOverview,
    /// Escape during a window menu Move/Resize.
    CancelFollow,
    /// An arrow key during Move/Resize: move the pointer by this much.
    FollowStep(i32, i32),
    /// Return / Space during Move/Resize: end it, as a click does.
    EndFollow,
    /// The locate-pointer key was pressed and released alone.
    LocatePointer,
    SwitchVt(i32),
    Quit,
    Lock,
    /// A shell binding was pressed (`true`) or released (`false`).
    Binding(u32, bool),
    /// restore-shortcuts while an app has them.
    RestoreShortcuts,
}

/// Why a binding was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindError {
    /// Already bound, or used by a compositor shortcut.
    Taken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Binding {
    id: u32,
    mods: Mods,
    key: Key,
    /// Delivered while the session is locked too.
    when_locked: bool,
}

/// The key of a binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// Whatever key produces this symbol.
    Sym(Keysym),
    /// A physical key (XKB keycode), whatever symbol the layout gives it.
    Code(u32),
}

/// XKB keycode of the key above Tab (evdev KEY_GRAVE + 8).
pub const ABOVE_TAB: u32 = 49;

/// F1..F12 → 1..12.
fn function_key(sym: Keysym) -> Option<i32> {
    let raw = sym.raw();
    let f1 = Keysym::F1.raw();
    (f1..f1 + 12).contains(&raw).then(|| (raw - f1 + 1) as i32)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Mods {
    pub alt: bool,
    pub logo: bool,
    pub ctrl: bool,
    pub shift: bool,
}

/// What to do with one key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    /// Keep the event from the focused client.
    pub intercept: bool,
    pub action: Option<Action>,
}

const FORWARD: Decision = Decision {
    intercept: false,
    action: None,
};

#[derive(Debug)]
pub struct Shortcuts {
    /// The overlay key is down and nothing else was pressed since.
    super_alone: bool,
    /// Keys that show the launcher pressed alone (`overlay-key`).
    overlay: Vec<Keysym>,
    /// The `locate-pointer-key`, when locating is on.
    locate: Option<Keysym>,
    /// The locate key is down and nothing else was pressed since.
    locate_alone: bool,
    /// Keys whose press was intercepted (and the binding they triggered);
    /// their release is intercepted too.
    suppressed: Vec<(u32, Option<u32>)>,
    bindings: Vec<Binding>,
}

impl Default for Shortcuts {
    fn default() -> Self {
        Self {
            super_alone: false,
            overlay: vec![Keysym::Super_L, Keysym::Super_R],
            locate: None,
            locate_alone: false,
            suppressed: Vec::new(),
            bindings: Vec::new(),
        }
    }
}

/// The compositor's own action for a press, if any.
fn builtin(mods: Mods, sym: Keysym) -> Option<Action> {
    let ctrl_alt = mods.ctrl && mods.alt && !mods.logo;
    match sym {
        Keysym::BackSpace if ctrl_alt => Some(Action::Quit),
        _ if ctrl_alt && function_key(sym).is_some() => function_key(sym).map(Action::SwitchVt),
        Keysym::l if mods.logo && !mods.alt && !mods.ctrl => Some(Action::Lock),
        _ => None,
    }
}

impl Shortcuts {
    /// Bind `mods`+`key` for the shell under `id`.
    pub fn bind(&mut self, id: u32, mods: Mods, key: Key) -> Result<(), BindError> {
        let reserved = match key {
            Key::Sym(sym) => self.overlay.contains(&sym) || builtin(mods, sym).is_some(),
            Key::Code(_) => false,
        };
        let duplicate = self.bindings.iter().any(|b| b.mods == mods && b.key == key);
        if reserved || duplicate {
            return Err(BindError::Taken);
        }
        self.bindings.push(Binding { id, mods, key, when_locked: false });
        Ok(())
    }

    /// Let binding `id` work while the session is locked.
    pub fn allow_when_locked(&mut self, id: u32) {
        for b in self.bindings.iter_mut().filter(|b| b.id == id) {
            b.when_locked = true;
        }
    }

    pub fn works_when_locked(&self, id: u32) -> bool {
        self.bindings.iter().any(|b| b.id == id && b.when_locked)
    }

    pub fn unbind(&mut self, id: u32) {
        self.bindings.retain(|b| b.id != id);
    }

    /// The keys of `overlay-key` and `locate-pointer-key`.
    pub fn set_special_keys(&mut self, overlay: Vec<Keysym>, locate: Option<Keysym>) {
        self.overlay = overlay;
        self.locate = locate;
    }

    pub fn key(&mut self, keycode: u32, pressed: bool, mods: Mods, sym: Keysym) -> Decision {
        // The locate key works alone and is never kept from the client.
        let locate_alone = std::mem::take(&mut self.locate_alone);
        if self.locate == Some(sym) {
            if pressed {
                self.locate_alone = true;
            } else if locate_alone {
                return Decision { intercept: false, action: Some(Action::LocatePointer) };
            }
        }
        if !pressed {
            let suppressed = self.take_suppressed(keycode);
            if self.overlay.contains(&sym) {
                let alone = std::mem::take(&mut self.super_alone);
                return Decision {
                    intercept: suppressed.is_some(),
                    action: alone.then_some(Action::ToggleOverview),
                };
            }
            return Decision {
                intercept: suppressed.is_some(),
                action: suppressed.flatten().map(|id| Action::Binding(id, false)),
            };
        }

        if self.overlay.contains(&sym) {
            self.super_alone = true;
            self.suppressed.push((keycode, None));
            return Decision {
                intercept: true,
                action: None,
            };
        }
        self.super_alone = false;

        if let Some(action) = builtin(mods, sym) {
            self.suppressed.push((keycode, None));
            return Decision {
                intercept: true,
                action: Some(action),
            };
        }
        let matches = |b: &&Binding| {
            b.mods == mods
                && match b.key {
                    Key::Sym(s) => s == sym,
                    Key::Code(c) => c == keycode,
                }
        };
        let binding = self.bindings.iter().find(matches).map(|b| b.id);
        let action = binding.map(|id| Action::Binding(id, true));
        if let Some(id) = binding {
            self.suppressed.push((keycode, Some(id)));
            Decision {
                intercept: true,
                action,
            }
        } else {
            FORWARD
        }
    }

    /// `Some(binding)` if this key's press was intercepted.
    fn take_suppressed(&mut self, keycode: u32) -> Option<Option<u32>> {
        let i = self.suppressed.iter().position(|(k, _)| *k == keycode)?;
        Some(self.suppressed.swap_remove(i).1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUPER: u32 = 133;
    const TAB: u32 = 23;
    const A: u32 = 38;
    const F4: u32 = 70;

    fn alt() -> Mods {
        Mods { alt: true, ..Mods::default() }
    }
    fn logo() -> Mods {
        Mods { logo: true, ..Mods::default() }
    }

    #[test]
    fn super_alone_shows_applications() {
        let mut s = Shortcuts::default();
        assert_eq!(s.key(SUPER, true, Mods::default(), Keysym::Super_L), Decision { intercept: true, action: None });
        let release = s.key(SUPER, false, logo(), Keysym::Super_L);
        assert_eq!(release, Decision { intercept: true, action: Some(Action::ToggleOverview) });
    }

    #[test]
    fn super_with_another_key_does_not() {
        let mut s = Shortcuts::default();
        s.key(SUPER, true, Mods::default(), Keysym::Super_L);
        // Super+A is forwarded to the client and cancels "Super alone".
        assert_eq!(s.key(A, true, logo(), Keysym::a), FORWARD);
        assert_eq!(s.key(A, false, logo(), Keysym::a), FORWARD);
        assert_eq!(s.key(SUPER, false, logo(), Keysym::Super_L).action, None);
    }

    #[test]
    fn alt_tab_is_left_to_the_shell_and_alt_f4_is_intercepted() {
        let mut s = Shortcuts::default();
        // Unbound, Alt+Tab goes to the client; the shell may bind it (window
        // switcher), and then presses and releases go to the binding.
        assert_eq!(s.key(TAB, true, alt(), Keysym::Tab), FORWARD);
        assert_eq!(s.key(TAB, false, alt(), Keysym::Tab), FORWARD);
        assert_eq!(s.bind(7, alt(), Key::Sym(Keysym::Tab)), Ok(()));
        assert_eq!(s.key(TAB, true, alt(), Keysym::Tab), Decision { intercept: true, action: Some(Action::Binding(7, true)) });
        assert_eq!(s.key(TAB, false, alt(), Keysym::Tab), Decision { intercept: true, action: Some(Action::Binding(7, false)) });
        // A plain Tab goes to the client.
        assert_eq!(s.key(TAB, true, Mods::default(), Keysym::Tab), FORWARD);
        assert_eq!(s.key(TAB, false, Mods::default(), Keysym::Tab), FORWARD);
        assert_eq!(s.key(F4, true, alt(), Keysym::F4), FORWARD, "Alt+F4 is the shell's (close)");
    }

    #[test]
    fn vt_switch_and_quit() {
        let mut s = Shortcuts::default();
        let ctrl_alt = Mods { ctrl: true, alt: true, ..Mods::default() };
        assert_eq!(s.key(67, true, ctrl_alt, Keysym::F1).action, Some(Action::SwitchVt(1)));
        assert_eq!(s.key(96, true, ctrl_alt, Keysym::F12).action, Some(Action::SwitchVt(12)));
        assert_eq!(s.key(22, true, ctrl_alt, Keysym::BackSpace).action, Some(Action::Quit));
        // Without both modifiers these go to the client.
        assert_eq!(s.key(67, true, alt(), Keysym::F1), FORWARD);
        assert_eq!(s.key(22, true, Mods { ctrl: true, ..Mods::default() }, Keysym::BackSpace), FORWARD);
    }

    #[test]
    fn shell_bindings() {
        let mut s = Shortcuts::default();
        let ctrl_shift = Mods { ctrl: true, shift: true, ..Mods::default() };
        assert_eq!(s.bind(7, ctrl_shift, Key::Sym(Keysym::F9)), Ok(()));
        assert_eq!(s.bind(8, ctrl_shift, Key::Sym(Keysym::F9)), Err(BindError::Taken), "duplicate");
        assert_eq!(s.bind(9, ctrl_alt_f1().0, Key::Sym(Keysym::F1)), Err(BindError::Taken), "compositor shortcut");
        assert_eq!(s.bind(10, Mods::default(), Key::Sym(Keysym::Super_L)), Err(BindError::Taken), "Super");

        let d = s.key(75, true, ctrl_shift, Keysym::F9);
        assert_eq!(d, Decision { intercept: true, action: Some(Action::Binding(7, true)) });
        // Release is matched by keycode even if modifiers changed meanwhile.
        let d = s.key(75, false, Mods::default(), Keysym::F9);
        assert_eq!(d, Decision { intercept: true, action: Some(Action::Binding(7, false)) });
        // Different modifiers: not the binding.
        assert_eq!(s.key(75, true, Mods { ctrl: true, ..Mods::default() }, Keysym::F9), FORWARD);
        s.key(75, false, Mods::default(), Keysym::F9);

        s.unbind(7);
        assert_eq!(s.key(75, true, ctrl_shift, Keysym::F9), FORWARD);
    }

    #[test]
    fn physical_keys_and_locked() {
        let mut s = Shortcuts::default();
        assert_eq!(s.bind(3, alt(), Key::Code(ABOVE_TAB)), Ok(()));
        assert_eq!(s.bind(4, alt(), Key::Code(ABOVE_TAB)), Err(BindError::Taken));
        // Whatever the layout makes of the key.
        let d = s.key(ABOVE_TAB, true, alt(), Keysym::dead_circumflex);
        assert_eq!(d.action, Some(Action::Binding(3, true)));
        assert_eq!(s.key(ABOVE_TAB, false, alt(), Keysym::dead_circumflex).action, Some(Action::Binding(3, false)));
        assert!(!s.works_when_locked(3));
        s.allow_when_locked(3);
        assert!(s.works_when_locked(3));
        assert!(!s.works_when_locked(99));
    }

    fn ctrl_alt_f1() -> (Mods, Keysym) {
        (Mods { ctrl: true, alt: true, ..Mods::default() }, Keysym::F1)
    }
}
