//! Which monitor shows the login screen: the primary one, as the
//! compositor's `org.gnome.Mutter.DisplayConfig` (on the greeter's own
//! session bus) reports it, matched to a Wayland output by connector name.
//! Without an answer, the first monitor announced.

use std::collections::HashMap;

use zbus::zvariant::OwnedValue;

/// A logical monitor in `GetCurrentState`: x, y, scale, transform,
/// primary, its monitors' (connector, vendor, product, serial), properties.
pub type Logical = (i32, i32, f64, u32, bool, Vec<(String, String, String, String)>, HashMap<String, OwnedValue>);
/// A monitor in `GetCurrentState`: its (connector, …), modes, properties.
type Monitor = ((String, String, String, String), Vec<(String, i32, i32, f64, f64, Vec<f64>, HashMap<String, OwnedValue>)>, HashMap<String, OwnedValue>);

/// The primary logical monitor's first connector.
pub fn primary_of(logical: &[Logical]) -> Option<String> {
    logical.iter().find(|l| l.4).and_then(|l| l.5.first()).map(|m| m.0.clone())
}

/// The connector of the primary monitor, from DisplayConfig on `conn`.
pub async fn primary_connector(conn: &zbus::Connection) -> Option<String> {
    let reply = conn
        .call_method(Some("org.gnome.Mutter.DisplayConfig"), "/org/gnome/Mutter/DisplayConfig", Some("org.gnome.Mutter.DisplayConfig"), "GetCurrentState", &())
        .await
        .map_err(|err| tracing::debug!(%err, "no DisplayConfig; the login screen goes on the first monitor"))
        .ok()?;
    let (_, _, logical, _): (u32, Vec<Monitor>, Vec<Logical>, HashMap<String, OwnedValue>) = reply.body().deserialize().ok()?;
    primary_of(&logical)
}

/// Of the screens (their outputs' names, in the order announced), the one
/// that shows the login screen: the primary's, else the first.
pub fn primary_index(names: &[Option<String>], primary: Option<&str>) -> usize {
    primary.and_then(|p| names.iter().position(|n| n.as_deref() == Some(p))).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logical(primary: bool, connector: &str) -> Logical {
        (0, 0, 1.0, 0, primary, vec![(connector.into(), "Acme".into(), "Screen".into(), "1".into())], HashMap::new())
    }

    #[test]
    fn choosing() {
        assert_eq!(primary_of(&[logical(false, "eDP-1"), logical(true, "HDMI-A-1")]), Some("HDMI-A-1".into()));
        assert_eq!(primary_of(&[logical(false, "eDP-1")]), None);
        let names = [Some("eDP-1".to_owned()), None, Some("HDMI-A-1".to_owned())];
        assert_eq!(primary_index(&names, Some("HDMI-A-1")), 2);
        assert_eq!(primary_index(&names, Some("DP-9")), 0, "not there: the first");
        assert_eq!(primary_index(&names, None), 0);
    }

    struct FakeDisplayConfig;

    #[zbus::interface(name = "org.gnome.Mutter.DisplayConfig")]
    impl FakeDisplayConfig {
        fn get_current_state(&self) -> (u32, Vec<Monitor>, Vec<Logical>, HashMap<String, OwnedValue>) {
            let monitor = |c: &str| ((c.to_owned(), "Acme".to_owned(), "Screen".to_owned(), "1".to_owned()), Vec::new(), HashMap::new());
            (1, vec![monitor("eDP-1"), monitor("HDMI-A-1")], vec![logical(false, "eDP-1"), logical(true, "HDMI-A-1")], HashMap::new())
        }
    }

    /// Against a fake DisplayConfig on a private bus.
    #[test]
    fn asks_display_config() {
        let Some(bus) = helix_test_support::TestBus::start() else { return };
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let _service = bus.builder().name("org.gnome.Mutter.DisplayConfig").unwrap().serve_at("/org/gnome/Mutter/DisplayConfig", FakeDisplayConfig).unwrap().build().await.unwrap();
            let conn = bus.connect().await;
            assert_eq!(primary_connector(&conn).await.as_deref(), Some("HDMI-A-1"));
        });
        let Some(empty) = helix_test_support::TestBus::start() else { return };
        rt.block_on(async {
            let conn = empty.connect().await;
            assert_eq!(primary_connector(&conn).await, None, "no compositor service");
        });
    }
}
