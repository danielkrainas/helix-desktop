//! Switching users through fake display managers and AccountsService on a
//! test bus (helix-desktop-spec `org.gnome.desktop.lockdown.md`, "User
//! switching").

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use helix_shell::user_switch::{available, manager, switch, Manager};
use helix_test_support::TestBus;
use zbus::zvariant::OwnedObjectPath;

struct Gdm(Arc<AtomicUsize>);

#[zbus::interface(name = "org.gnome.DisplayManager.LocalDisplayFactory")]
impl Gdm {
    fn create_transient_display(&self) -> OwnedObjectPath {
        self.0.fetch_add(1, Ordering::SeqCst);
        OwnedObjectPath::try_from("/org/gnome/DisplayManager/Display2").unwrap()
    }
}

struct Seat(Arc<AtomicUsize>);

#[zbus::interface(name = "org.freedesktop.DisplayManager.Seat")]
impl Seat {
    fn switch_to_greeter(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct Accounts(usize);

#[zbus::interface(name = "org.freedesktop.Accounts")]
impl Accounts {
    fn list_cached_users(&self) -> Vec<OwnedObjectPath> {
        (0..self.0).map(|i| OwnedObjectPath::try_from(format!("/org/freedesktop/Accounts/User{}", 1000 + i)).unwrap()).collect()
    }
}

#[tokio::test]
async fn switches_through_the_display_manager() {
    let Some(bus) = TestBus::start() else { return };
    let conn = bus.connect().await;
    assert_eq!(available(&conn).await, None, "no display manager");

    // LightDM, found through the session's seat.
    // SAFETY: this test binary runs this one test.
    unsafe { std::env::set_var("XDG_SEAT_PATH", "/org/freedesktop/DisplayManager/Seat0") };
    let greeter = Arc::new(AtomicUsize::new(0));
    let _lightdm = bus.builder().serve_at("/org/freedesktop/DisplayManager/Seat0", Seat(greeter.clone())).unwrap().name("org.freedesktop.DisplayManager").unwrap().build().await.unwrap();
    let seat = Manager::LightDm(OwnedObjectPath::try_from("/org/freedesktop/DisplayManager/Seat0").unwrap());
    assert_eq!(manager(&conn).await, Some(seat.clone()));
    // Without AccountsService anyone might log in.
    assert_eq!(available(&conn).await, Some(seat.clone()));
    switch(&conn, &seat).await.unwrap();
    assert_eq!(greeter.load(Ordering::SeqCst), 1);

    // One user only: nothing to switch to.
    let accounts = bus.builder().serve_at("/org/freedesktop/Accounts", Accounts(1)).unwrap().name("org.freedesktop.Accounts").unwrap().build().await.unwrap();
    assert_eq!(available(&conn).await, None);
    drop(accounts);
    let _accounts = bus.builder().serve_at("/org/freedesktop/Accounts", Accounts(2)).unwrap().name("org.freedesktop.Accounts").unwrap().build().await.unwrap();
    assert_eq!(available(&conn).await, Some(seat));

    // GDM comes first.
    let displays = Arc::new(AtomicUsize::new(0));
    let _gdm = bus.builder().serve_at("/org/gnome/DisplayManager/LocalDisplayFactory", Gdm(displays.clone())).unwrap().name("org.gnome.DisplayManager").unwrap().build().await.unwrap();
    assert_eq!(available(&conn).await, Some(Manager::Gdm));
    switch(&conn, &Manager::Gdm).await.unwrap();
    assert_eq!(displays.load(Ordering::SeqCst), 1);
}
