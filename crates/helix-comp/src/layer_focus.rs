//! Keyboard focus for layer-shell surfaces (`zwlr_layer_surface_v1`
//! `keyboard_interactivity`; helixdesktop/spec `contracts/wayland/README.md`,
//! "Shell surfaces and the keyboard").

/// A layer surface's `keyboard_interactivity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interactivity {
    None,
    Exclusive,
    OnDemand,
}

/// Which layer it is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Background,
    Bottom,
    Top,
    Overlay,
}

/// What happens to the keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Nothing changes.
    Keep,
    /// The surface takes the focus.
    Take,
    /// The surface had the focus and gives it back (to another surface
    /// asking for it exclusively, else the top window).
    GiveBack,
}

/// After the surface committed (it is mapped): a top or overlay surface
/// asking for exclusive focus takes it; one that had the focus and now
/// asks for none gives it back (the panel after `switch-panels`).
pub fn on_commit(interactivity: Interactivity, layer: Layer, focused: bool) -> Change {
    match interactivity {
        Interactivity::Exclusive if matches!(layer, Layer::Top | Layer::Overlay) && !focused => Change::Take,
        Interactivity::None if focused => Change::GiveBack,
        _ => Change::Keep,
    }
}

/// A button was pressed on the surface: one that may have the focus
/// (on demand, or exclusive on a lower layer) takes it.
pub fn on_click(interactivity: Interactivity, focused: bool) -> Change {
    match interactivity {
        Interactivity::OnDemand | Interactivity::Exclusive if !focused => Change::Take,
        _ => Change::Keep,
    }
}

/// Layer surfaces whose `exclusive` keyboard grab a click elsewhere
/// ended: the user pressed a button on a window or the desktop while one
/// had the keyboard. Such a surface does not take the keyboard back on
/// its next commit (the panel redrawing would otherwise steal it from
/// the window just clicked); it takes it again when clicked, or after it
/// stops asking for `exclusive` and asks anew.
#[derive(Debug, Clone)]
pub struct Released<S> {
    surfaces: Vec<S>,
}

impl<S> Default for Released<S> {
    fn default() -> Self {
        Self { surfaces: Vec::new() }
    }
}

impl<S: PartialEq + Clone> Released<S> {
    /// A button press outside the shell's surfaces moved the keyboard
    /// away from `surface`, which had it asking for `interactivity`.
    pub fn clicked_away(&mut self, surface: &S, interactivity: Interactivity) {
        if interactivity == Interactivity::Exclusive && !self.contains(surface) {
            self.surfaces.push(surface.clone());
        }
    }

    pub fn contains(&self, surface: &S) -> bool {
        self.surfaces.contains(surface)
    }

    /// The surface went away.
    pub fn forget(&mut self, surface: &S) {
        self.surfaces.retain(|s| s != surface);
    }

    /// [`on_commit`], for a surface that may have been clicked away from.
    pub fn on_commit(&mut self, surface: &S, interactivity: Interactivity, layer: Layer, focused: bool) -> Change {
        if interactivity != Interactivity::Exclusive {
            self.forget(surface);
        } else if self.contains(surface) {
            return Change::Keep;
        }
        on_commit(interactivity, layer, focused)
    }

    /// [`on_click`]: a click on the surface itself ends its release.
    pub fn on_click(&mut self, surface: &S, interactivity: Interactivity, focused: bool) -> Change {
        self.forget(surface);
        on_click(interactivity, focused)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commits() {
        assert_eq!(on_commit(Interactivity::Exclusive, Layer::Overlay, false), Change::Take);
        assert_eq!(on_commit(Interactivity::Exclusive, Layer::Top, true), Change::Keep);
        assert_eq!(on_commit(Interactivity::Exclusive, Layer::Bottom, false), Change::Keep, "only top and overlay grab");
        // An on-demand menu appearing does not take the focus by itself.
        assert_eq!(on_commit(Interactivity::OnDemand, Layer::Overlay, false), Change::Keep);
        assert_eq!(on_commit(Interactivity::OnDemand, Layer::Overlay, true), Change::Keep);
        // The panel giving up the keyboard after switch-panels.
        assert_eq!(on_commit(Interactivity::None, Layer::Top, true), Change::GiveBack);
        assert_eq!(on_commit(Interactivity::None, Layer::Top, false), Change::Keep);
    }

    #[test]
    fn clicks() {
        assert_eq!(on_click(Interactivity::OnDemand, false), Change::Take);
        assert_eq!(on_click(Interactivity::OnDemand, true), Change::Keep);
        assert_eq!(on_click(Interactivity::None, false), Change::Keep, "the panel never takes it by a click");
    }

    #[test]
    fn clicked_away() {
        let mut r = Released::default();
        // The panel has the keyboard (switch-panels); a click on a window
        // takes it. The panel's redraws don't take it back.
        r.clicked_away(&"panel", Interactivity::Exclusive);
        assert_eq!(r.on_commit(&"panel", Interactivity::Exclusive, Layer::Top, false), Change::Keep);
        assert_eq!(r.on_commit(&"panel", Interactivity::Exclusive, Layer::Top, false), Change::Keep);
        // It hands the keyboard back (none) and later asks again: it takes it.
        assert_eq!(r.on_commit(&"panel", Interactivity::None, Layer::Top, false), Change::Keep);
        assert_eq!(r.on_commit(&"panel", Interactivity::Exclusive, Layer::Top, false), Change::Take);
        // Clicked away, then clicked: it takes it at once.
        r.clicked_away(&"dock", Interactivity::Exclusive);
        assert_eq!(r.on_click(&"dock", Interactivity::Exclusive, false), Change::Take);
        assert_eq!(r.on_commit(&"dock", Interactivity::Exclusive, Layer::Top, true), Change::Keep);
        assert!(!r.contains(&"dock"));
        // On-demand surfaces simply lose it; nothing to remember.
        r.clicked_away(&"menu", Interactivity::OnDemand);
        assert!(!r.contains(&"menu"));
        // Other surfaces are unaffected.
        assert_eq!(r.on_commit(&"launcher", Interactivity::Exclusive, Layer::Overlay, false), Change::Take);
    }
}
