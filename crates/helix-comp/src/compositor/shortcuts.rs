//! Compositor-level keyboard shortcuts, decided independently of Smithay
//! types so the rules can be tested.
//!
//! | Keys | Action |
//! |------|--------|
//! | Super (alone) | Show applications (asks the shell) |
//! | Alt+Tab | Cycle windows |
//! | Alt+F4 | Close the focused window |
//! | Super+Up | Toggle maximize |
//! | Super+Down | Restore a maximized window, else minimize |
//! | Ctrl+Alt+F1..F12 | Switch VT (bare hardware only) |
//! | Ctrl+Alt+Backspace | Quit Helix (always available escape hatch) |
//!
//! After these, key combinations bound by the shell (via the private
//! `helix_keybindings_v1` protocol) are matched, with exact modifiers.

use smithay::input::keyboard::Keysym;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    ShowApplications,
    CycleWindows,
    CloseWindow,
    ToggleMaximize,
    RestoreOrMinimize,
    SwitchVt(i32),
    Quit,
    /// A shell binding was pressed (`true`) or released (`false`).
    Binding(u32, bool),
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
    sym: Keysym,
}

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

#[derive(Debug, Default)]
pub struct Shortcuts {
    /// Super is down and nothing else was pressed since.
    super_alone: bool,
    /// Keys whose press was intercepted (and the binding they triggered);
    /// their release is intercepted too.
    suppressed: Vec<(u32, Option<u32>)>,
    bindings: Vec<Binding>,
}

fn is_super(sym: Keysym) -> bool {
    sym == Keysym::Super_L || sym == Keysym::Super_R
}

/// The compositor's own action for a press, if any.
fn builtin(mods: Mods, sym: Keysym) -> Option<Action> {
    let ctrl_alt = mods.ctrl && mods.alt && !mods.logo;
    match sym {
        Keysym::BackSpace if ctrl_alt => Some(Action::Quit),
        _ if ctrl_alt && function_key(sym).is_some() => function_key(sym).map(Action::SwitchVt),
        Keysym::Tab if mods.alt && !mods.logo && !mods.ctrl => Some(Action::CycleWindows),
        Keysym::F4 if mods.alt && !mods.logo && !mods.ctrl => Some(Action::CloseWindow),
        Keysym::Up if mods.logo && !mods.alt && !mods.ctrl => Some(Action::ToggleMaximize),
        Keysym::Down if mods.logo && !mods.alt && !mods.ctrl => Some(Action::RestoreOrMinimize),
        _ => None,
    }
}

impl Shortcuts {
    /// Bind `mods`+`sym` for the shell under `id`.
    pub fn bind(&mut self, id: u32, mods: Mods, sym: Keysym) -> Result<(), BindError> {
        let reserved = is_super(sym) || builtin(mods, sym).is_some();
        let duplicate = self.bindings.iter().any(|b| b.mods == mods && b.sym == sym);
        if reserved || duplicate {
            return Err(BindError::Taken);
        }
        self.bindings.push(Binding { id, mods, sym });
        Ok(())
    }

    pub fn unbind(&mut self, id: u32) {
        self.bindings.retain(|b| b.id != id);
    }

    pub fn key(&mut self, keycode: u32, pressed: bool, mods: Mods, sym: Keysym) -> Decision {
        if !pressed {
            let suppressed = self.take_suppressed(keycode);
            if is_super(sym) {
                let alone = std::mem::take(&mut self.super_alone);
                return Decision {
                    intercept: suppressed.is_some(),
                    action: alone.then_some(Action::ShowApplications),
                };
            }
            return Decision {
                intercept: suppressed.is_some(),
                action: suppressed.flatten().map(|id| Action::Binding(id, false)),
            };
        }

        if is_super(sym) {
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
        let binding = self.bindings.iter().find(|b| b.mods == mods && b.sym == sym).map(|b| b.id);
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
        assert_eq!(release, Decision { intercept: true, action: Some(Action::ShowApplications) });
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
    fn alt_tab_and_alt_f4_are_intercepted_including_release() {
        let mut s = Shortcuts::default();
        let d = s.key(TAB, true, alt(), Keysym::Tab);
        assert_eq!(d, Decision { intercept: true, action: Some(Action::CycleWindows) });
        assert_eq!(s.key(TAB, false, alt(), Keysym::Tab), Decision { intercept: true, action: None });
        // A plain Tab goes to the client.
        assert_eq!(s.key(TAB, true, Mods::default(), Keysym::Tab), FORWARD);
        assert_eq!(s.key(TAB, false, Mods::default(), Keysym::Tab), FORWARD);
        assert_eq!(s.key(F4, true, alt(), Keysym::F4).action, Some(Action::CloseWindow));
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
        assert_eq!(s.bind(7, ctrl_shift, Keysym::F9), Ok(()));
        assert_eq!(s.bind(8, ctrl_shift, Keysym::F9), Err(BindError::Taken), "duplicate");
        assert_eq!(s.bind(9, alt(), Keysym::Tab), Err(BindError::Taken), "compositor shortcut");
        assert_eq!(s.bind(10, Mods::default(), Keysym::Super_L), Err(BindError::Taken), "Super");

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
    fn super_arrows() {
        let mut s = Shortcuts::default();
        assert_eq!(s.key(111, true, logo(), Keysym::Up).action, Some(Action::ToggleMaximize));
        assert_eq!(s.key(116, true, logo(), Keysym::Down).action, Some(Action::RestoreOrMinimize));
        assert_eq!(s.key(111, true, Mods::default(), Keysym::Up), FORWARD);
    }
}
