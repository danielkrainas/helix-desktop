//! libinput's events for the event loop: everything Smithay's own source
//! gives, plus tablet pad events (buttons, rings, strips), which it drops.

use std::io;
use std::os::fd::{AsFd, BorrowedFd};

use smithay::backend::input::InputEvent;
use smithay::backend::libinput::{LibinputInputBackend, PointerScrollAxis};
use smithay::reexports::calloop::{EventSource, Interest, Mode, Poll, PostAction, Readiness, Token, TokenFactory};
use smithay::reexports::input::{self as libinput, event};

/// One event from libinput.
pub enum LibinputEvent {
    Input(InputEvent<LibinputInputBackend>),
    Pad(event::TabletPadEvent),
}

pub struct LibinputSource {
    context: libinput::Libinput,
    token: Option<Token>,
}

impl LibinputSource {
    pub fn new(context: libinput::Libinput) -> Self {
        Self { context, token: None }
    }
}

impl AsFd for LibinputSource {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.context.as_fd()
    }
}

/// The event loop's form of a libinput event (`None`: not used).
fn convert(event: libinput::Event) -> Option<LibinputEvent> {
    use event::{DeviceEvent as D, GestureEvent as G, KeyboardEvent as K, PointerEvent as P, SwitchEvent as S, TabletToolEvent as T, TouchEvent as Tc};
    use event::gesture::{GestureHoldEvent as Hold, GesturePinchEvent as Pinch, GestureSwipeEvent as Swipe};
    let input = match event {
        libinput::Event::Device(D::Added(e)) => InputEvent::DeviceAdded { device: event::EventTrait::device(&e) },
        libinput::Event::Device(D::Removed(e)) => InputEvent::DeviceRemoved { device: event::EventTrait::device(&e) },
        libinput::Event::Keyboard(K::Key(event)) => InputEvent::Keyboard { event },
        libinput::Event::Pointer(P::Motion(event)) => InputEvent::PointerMotion { event },
        libinput::Event::Pointer(P::MotionAbsolute(event)) => InputEvent::PointerMotionAbsolute { event },
        libinput::Event::Pointer(P::Button(event)) => InputEvent::PointerButton { event },
        libinput::Event::Pointer(P::ScrollWheel(e)) => InputEvent::PointerAxis { event: PointerScrollAxis::Wheel(e) },
        libinput::Event::Pointer(P::ScrollFinger(e)) => InputEvent::PointerAxis { event: PointerScrollAxis::Finger(e) },
        libinput::Event::Pointer(P::ScrollContinuous(e)) => InputEvent::PointerAxis { event: PointerScrollAxis::Continuous(e) },
        libinput::Event::Touch(Tc::Down(event)) => InputEvent::TouchDown { event },
        libinput::Event::Touch(Tc::Motion(event)) => InputEvent::TouchMotion { event },
        libinput::Event::Touch(Tc::Up(event)) => InputEvent::TouchUp { event },
        libinput::Event::Touch(Tc::Cancel(event)) => InputEvent::TouchCancel { event },
        libinput::Event::Touch(Tc::Frame(event)) => InputEvent::TouchFrame { event },
        libinput::Event::Gesture(G::Swipe(Swipe::Begin(event))) => InputEvent::GestureSwipeBegin { event },
        libinput::Event::Gesture(G::Swipe(Swipe::Update(event))) => InputEvent::GestureSwipeUpdate { event },
        libinput::Event::Gesture(G::Swipe(Swipe::End(event))) => InputEvent::GestureSwipeEnd { event },
        libinput::Event::Gesture(G::Pinch(Pinch::Begin(event))) => InputEvent::GesturePinchBegin { event },
        libinput::Event::Gesture(G::Pinch(Pinch::Update(event))) => InputEvent::GesturePinchUpdate { event },
        libinput::Event::Gesture(G::Pinch(Pinch::End(event))) => InputEvent::GesturePinchEnd { event },
        libinput::Event::Gesture(G::Hold(Hold::Begin(event))) => InputEvent::GestureHoldBegin { event },
        libinput::Event::Gesture(G::Hold(Hold::End(event))) => InputEvent::GestureHoldEnd { event },
        libinput::Event::Tablet(T::Axis(event)) => InputEvent::TabletToolAxis { event },
        libinput::Event::Tablet(T::Proximity(event)) => InputEvent::TabletToolProximity { event },
        libinput::Event::Tablet(T::Tip(event)) => InputEvent::TabletToolTip { event },
        libinput::Event::Tablet(T::Button(event)) => InputEvent::TabletToolButton { event },
        libinput::Event::Switch(S::Toggle(event)) => InputEvent::SwitchToggle { event },
        libinput::Event::TabletPad(pad) => return Some(LibinputEvent::Pad(pad)),
        _ => return None,
    };
    Some(LibinputEvent::Input(input))
}

impl EventSource for LibinputSource {
    type Event = LibinputEvent;
    type Metadata = ();
    type Ret = ();
    type Error = io::Error;

    fn process_events<F>(&mut self, _: Readiness, token: Token, mut callback: F) -> io::Result<PostAction>
    where
        F: FnMut(Self::Event, &mut ()) -> Self::Ret,
    {
        if Some(token) == self.token {
            self.context.dispatch()?;
            for event in &mut self.context {
                if let Some(event) = convert(event) {
                    callback(event, &mut ());
                }
            }
        }
        Ok(PostAction::Continue)
    }

    fn register(&mut self, poll: &mut Poll, factory: &mut TokenFactory) -> smithay::reexports::calloop::Result<()> {
        self.token = Some(factory.token());
        // SAFETY: the context's descriptor lives as long as this source,
        // which unregisters it before going away.
        unsafe { poll.register(self.context.as_fd(), Interest::READ, Mode::Level, self.token.expect("just set")) }
    }

    fn reregister(&mut self, poll: &mut Poll, factory: &mut TokenFactory) -> smithay::reexports::calloop::Result<()> {
        self.token = Some(factory.token());
        poll.reregister(self.context.as_fd(), Interest::READ, Mode::Level, self.token.expect("just set"))
    }

    fn unregister(&mut self, poll: &mut Poll) -> smithay::reexports::calloop::Result<()> {
        self.token = None;
        poll.unregister(self.context.as_fd())
    }
}
