//! Hover and press for a surface's clickable parts (helix-desktop-spec
//! `components/README.md`, "Shell look"): the part under the pointer is
//! lit, the part pressed looks pressed, and a click happens on release
//! over the part that was pressed, so moving away while pressed cancels
//! it. Nothing stays lit once the pointer leaves the surface.

use smithay_client_toolkit::seat::pointer::PointerEventKind;

use super::BTN_LEFT;

/// What a pointer event changed.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Outcome<H> {
    /// The look changed: draw again.
    pub redraw: bool,
    /// Released over the part that was pressed: do what it does.
    pub click: Option<H>,
    /// Pressed on a part (for parts that act at once, such as a slider).
    pub down: Option<H>,
}

impl<H> Default for Outcome<H> {
    fn default() -> Self {
        Self { redraw: false, click: None, down: None }
    }
}

/// Follow `kind` (at a place where `hit` is under the pointer) on a
/// surface whose part under the pointer is `hover` and pressed part is
/// `pressed`.
pub(super) fn track<H: Clone + PartialEq>(hover: &mut Option<H>, pressed: &mut Option<H>, kind: &PointerEventKind, hit: Option<H>) -> Outcome<H> {
    let (before_hover, before_pressed) = (hover.clone(), pressed.clone());
    let mut out = Outcome::default();
    match kind {
        PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => *hover = hit,
        PointerEventKind::Leave { .. } => {
            *hover = None;
            *pressed = None;
        }
        PointerEventKind::Press { button: BTN_LEFT, .. } => {
            *hover = hit.clone();
            *pressed = hit.clone();
            out.down = hit;
        }
        PointerEventKind::Release { button: BTN_LEFT, .. } => {
            *hover = hit.clone();
            if let Some(was) = pressed.take() {
                if hit.as_ref() == Some(&was) {
                    out.click = Some(was);
                }
            }
        }
        _ => {}
    }
    out.redraw = *hover != before_hover || *pressed != before_pressed;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn motion() -> PointerEventKind {
        PointerEventKind::Motion { time: 0 }
    }
    fn press() -> PointerEventKind {
        PointerEventKind::Press { time: 0, button: BTN_LEFT, serial: 1 }
    }
    fn release() -> PointerEventKind {
        PointerEventKind::Release { time: 0, button: BTN_LEFT, serial: 2 }
    }

    #[test]
    fn click_on_release() {
        let (mut hover, mut pressed) = (None, None);
        let out = track(&mut hover, &mut pressed, &motion(), Some(1));
        assert!(out.redraw && out.click.is_none());
        assert_eq!(hover, Some(1));
        let out = track(&mut hover, &mut pressed, &press(), Some(1));
        assert_eq!((out.redraw, out.down, out.click), (true, Some(1), None), "pressed, nothing done yet");
        let out = track(&mut hover, &mut pressed, &release(), Some(1));
        assert_eq!((out.redraw, out.click), (true, Some(1)), "released over it: clicked");
        assert_eq!((hover, pressed), (Some(1), None));
    }

    #[test]
    fn moving_away_cancels() {
        let (mut hover, mut pressed) = (None, None);
        track(&mut hover, &mut pressed, &press(), Some(1));
        let out = track(&mut hover, &mut pressed, &motion(), Some(2));
        assert!(out.redraw);
        assert_eq!((hover, pressed), (Some(2), Some(1)), "still pressed, the other lit");
        let out = track(&mut hover, &mut pressed, &release(), Some(2));
        assert_eq!(out.click, None, "released elsewhere: nothing");
        assert_eq!(pressed, None);
    }

    #[test]
    fn leaving_clears() {
        let (mut hover, mut pressed) = (None, None);
        track(&mut hover, &mut pressed, &press(), Some(1));
        let out = track(&mut hover, &mut pressed, &PointerEventKind::Leave { serial: 3 }, None);
        assert!(out.redraw);
        assert_eq!((hover, pressed), (None::<i32>, None));
        let out = track(&mut hover, &mut pressed, &motion(), None);
        assert!(!out.redraw, "nothing changed");
    }
}
