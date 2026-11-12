//! Keyboard focus and the text caret, for the magnifier
//! (helix-desktop-spec `contracts/gsettings/a11y-magnifier.md`, "Focus
//! and caret"): followed on the accessibility bus (AT-SPI) while the
//! magnifier wants them.

use std::collections::HashMap;
use std::time::Duration;

use futures_util::StreamExt;
use smithay::reexports::calloop::channel::Sender;
use tokio::sync::watch;
use zbus::zvariant::OwnedValue;
use zbus::Connection;

const REGISTRY: &str = "org.a11y.atspi.Registry";
const REGISTRY_PATH: &str = "/org/a11y/atspi/registry";
/// The AT-SPI events followed.
pub const EVENTS: [&str; 2] = ["object:state-changed:focused", "object:text-caret-moved"];
/// `ATSPI_COORD_TYPE_WINDOW`: relative to the app's window.
const WINDOW_COORDS: u32 = 1;
/// How long an app may take to say where something is.
const ASK_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Focus,
    Caret,
    /// The text the caret is in, when the app cannot say where the caret
    /// itself is.
    CaretArea,
}

/// Where the focused object or the caret is, relative to the focused
/// window: (x, y, width, height).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackPoint {
    pub kind: Kind,
    pub rect: (i32, i32, i32, i32),
}

impl TrackPoint {
    /// The point followed: a focused object's center; the caret's left
    /// edge, halfway down.
    pub fn point(&self) -> (f64, f64) {
        let (x, y, w, h) = self.rect;
        match self.kind {
            Kind::Focus | Kind::CaretArea => (f64::from(x) + f64::from(w) / 2.0, f64::from(y) + f64::from(h) / 2.0),
            Kind::Caret => (f64::from(x), f64::from(y) + f64::from(h) / 2.0),
        }
    }
}

/// The rectangle in a reply: one `(iiii)` struct or four `i`s.
fn rect_of(message: &zbus::Message) -> Option<(i32, i32, i32, i32)> {
    let body = message.body();
    body.deserialize::<((i32, i32, i32, i32),)>().map(|r| r.0).or_else(|_| body.deserialize::<(i32, i32, i32, i32)>()).ok()
}

/// An event's kind, first detail.
fn event_of(message: &zbus::Message) -> Option<(String, i32)> {
    let body = message.body();
    body.deserialize::<(String, i32, i32, OwnedValue, HashMap<String, OwnedValue>)>()
        .map(|(k, d, ..)| (k, d))
        .or_else(|_| body.deserialize::<(String, i32, i32, OwnedValue, (String, zbus::zvariant::OwnedObjectPath))>().map(|(k, d, ..)| (k, d)))
        .ok()
}

/// Follow focus and caret while `wanted` is true, sending where they are
/// to `out`.
pub async fn run(session: Connection, mut wanted: watch::Receiver<bool>, out: Sender<TrackPoint>) {
    loop {
        while !*wanted.borrow_and_update() {
            if wanted.changed().await.is_err() {
                return;
            }
        }
        if let Err(err) = follow(&session, &mut wanted, &out).await {
            tracing::info!(%err, "cannot follow focus on the accessibility bus");
            // Try again when asked again.
            if wanted.changed().await.is_err() {
                return;
            }
        }
    }
}

async fn follow(session: &Connection, wanted: &mut watch::Receiver<bool>, out: &Sender<TrackPoint>) -> zbus::Result<()> {
    let reply = session.call_method(Some("org.a11y.Bus"), "/org/a11y/bus", Some("org.a11y.Bus"), "GetAddress", &()).await?;
    let address: String = reply.body().deserialize()?;
    let bus = zbus::connection::Builder::address(address.as_str())?.build().await?;
    let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).interface("org.a11y.atspi.Event.Object")?.build();
    let mut signals = zbus::MessageStream::for_match_rule(rule, &bus, Some(64)).await?;
    for event in EVENTS {
        bus.call_method(Some(REGISTRY), REGISTRY_PATH, Some(REGISTRY), "RegisterEvent", &(event, Vec::<String>::new(), "")).await?;
    }
    tracing::info!("following focus and caret for the magnifier");
    loop {
        tokio::select! {
            changed = wanted.changed() => {
                if changed.is_err() || !*wanted.borrow_and_update() {
                    break;
                }
            }
            message = signals.next() => {
                let Some(Ok(message)) = message else { break };
                let header = message.header();
                let (Some(member), Some(sender), Some(path)) = (header.member(), header.sender(), header.path()) else { continue };
                let Some((kind, detail)) = event_of(&message) else {
                    tracing::debug!(member = %member, signature = %message.body().signature(), "accessibility event not understood");
                    continue;
                };
                tracing::debug!(member = %member, kind, detail, "accessibility event");
                let asked = match (member.as_str(), kind.as_str()) {
                    ("StateChanged", "focused") if detail == 1 => Some(Kind::Focus),
                    ("TextCaretMoved", _) => Some(Kind::Caret),
                    _ => None,
                };
                let Some(kind) = asked else { continue };
                let (sender, path) = (sender.to_owned(), path.to_owned());
                let call = async {
                    let extents = |sender, path| bus.call_method(Some(sender), path, Some("org.a11y.atspi.Component"), "GetExtents", &(WINDOW_COORDS,));
                    match kind {
                        Kind::Caret => match bus.call_method(Some(sender.clone()), path.clone(), Some("org.a11y.atspi.Text"), "GetCharacterExtents", &(detail, WINDOW_COORDS)).await {
                            // The text's own bounds instead.
                            Err(zbus::Error::MethodError(..)) => extents(sender, path).await.map(|r| (Kind::CaretArea, r)),
                            other => other.map(|r| (Kind::Caret, r)),
                        },
                        _ => extents(sender, path).await.map(|r| (kind, r)),
                    }
                };
                let (kind, reply) = match tokio::time::timeout(ASK_TIMEOUT, call).await {
                    Ok(Ok(reply)) => reply,
                    Ok(Err(err)) => {
                        tracing::debug!(%err, "cannot ask where it is");
                        continue;
                    }
                    Err(_) => continue,
                };
                tracing::debug!(rect = ?rect_of(&reply), "where it is");
                if let Some(rect) = rect_of(&reply).filter(|r| r.2 >= 0 && r.3 > 0 && (r.0, r.1) != (-1, -1)) {
                    if out.send(TrackPoint { kind, rect }).is_err() {
                        break;
                    }
                }
            }
        }
    }
    for event in EVENTS {
        let _ = bus.call_method(Some(REGISTRY), REGISTRY_PATH, Some(REGISTRY), "DeregisterEvent", &(event,)).await;
    }
    tracing::info!("no longer following focus and caret");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points() {
        assert_eq!(TrackPoint { kind: Kind::Focus, rect: (10, 20, 100, 40) }.point(), (60.0, 40.0));
        assert_eq!(TrackPoint { kind: Kind::Caret, rect: (10, 20, 1, 16) }.point(), (10.0, 28.0));
    }
}
