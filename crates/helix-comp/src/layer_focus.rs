//! Keyboard focus for layer-shell surfaces (`zwlr_layer_surface_v1`
//! `keyboard_interactivity`; helix-desktop-spec `contracts/wayland/README.md`,
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
}
