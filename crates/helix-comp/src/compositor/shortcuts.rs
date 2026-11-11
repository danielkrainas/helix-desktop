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

use smithay::input::keyboard::Keysym;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    ShowApplications,
    CycleWindows,
    CloseWindow,
    ToggleMaximize,
    RestoreOrMinimize,
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
    /// Keys whose press was intercepted; their release is too.
    suppressed: Vec<u32>,
}

fn is_super(sym: Keysym) -> bool {
    sym == Keysym::Super_L || sym == Keysym::Super_R
}

impl Shortcuts {
    pub fn key(&mut self, keycode: u32, pressed: bool, mods: Mods, sym: Keysym) -> Decision {
        if !pressed {
            let was_suppressed = self.take_suppressed(keycode);
            if is_super(sym) {
                let alone = std::mem::take(&mut self.super_alone);
                return Decision {
                    intercept: was_suppressed,
                    action: alone.then_some(Action::ShowApplications),
                };
            }
            return Decision {
                intercept: was_suppressed,
                action: None,
            };
        }

        if is_super(sym) {
            self.super_alone = true;
            self.suppressed.push(keycode);
            return Decision {
                intercept: true,
                action: None,
            };
        }
        self.super_alone = false;

        let action = match sym {
            Keysym::Tab if mods.alt && !mods.logo && !mods.ctrl => Some(Action::CycleWindows),
            Keysym::F4 if mods.alt && !mods.logo && !mods.ctrl => Some(Action::CloseWindow),
            Keysym::Up if mods.logo && !mods.alt && !mods.ctrl => Some(Action::ToggleMaximize),
            Keysym::Down if mods.logo && !mods.alt && !mods.ctrl => Some(Action::RestoreOrMinimize),
            _ => None,
        };
        if action.is_some() {
            self.suppressed.push(keycode);
            Decision {
                intercept: true,
                action,
            }
        } else {
            FORWARD
        }
    }

    fn take_suppressed(&mut self, keycode: u32) -> bool {
        match self.suppressed.iter().position(|k| *k == keycode) {
            Some(i) => {
                self.suppressed.swap_remove(i);
                true
            }
            None => false,
        }
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
    fn super_arrows() {
        let mut s = Shortcuts::default();
        assert_eq!(s.key(111, true, logo(), Keysym::Up).action, Some(Action::ToggleMaximize));
        assert_eq!(s.key(116, true, logo(), Keysym::Down).action, Some(Action::RestoreOrMinimize));
        assert_eq!(s.key(111, true, Mods::default(), Keysym::Up), FORWARD);
    }
}
