//! Every exported D-Bus interface must match its contract XML in
//! helix-desktop-spec. Members not implemented yet are listed explicitly;
//! the check fails if a listed member gets implemented, so update the list.

use helix_test_support::conformance::{assert_conformant, check, introspect, load_dbus_contract};
use helix_test_support::TestBus;

#[tokio::test(flavor = "multi_thread")]
async fn org_gnome_session_manager() {
    const IFACE: &str = "org.gnome.SessionManager";
    // Tracked in helix-desktop-spec/gaps.md.
    const KNOWN_UNIMPLEMENTED: &[&str] = &[
        "method Suspend",
        "method CanShutdown",
        "method CanReboot",
        "method CanSuspend",
        "method SetRebootToFirmwareSetup",
        "method RegisterRestore",
        "method DeletedInstanceIds",
        "method UnregisterRestore",
    ];

    let Some(bus) = TestBus::start() else { return };
    let service = helix_session::SessionService::start(bus.builder())
        .await
        .unwrap();
    let actual = introspect(service.connection(), IFACE, "/org/gnome/SessionManager").await;
    let problems = check(
        IFACE,
        &load_dbus_contract("org.gnome.SessionManager.xml"),
        &actual,
        KNOWN_UNIMPLEMENTED,
    );
    assert_conformant(IFACE, &problems);
}
