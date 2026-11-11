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

/// Introspect `path` on `conn` and check `iface` against `contract_file`.
async fn assert_matches_contract(
    conn: &zbus::Connection,
    dest: &str,
    path: &str,
    iface: &str,
    contract_file: &str,
    known_unimplemented: &[&str],
) {
    let actual = introspect(conn, dest, path).await;
    let problems = check(iface, &load_dbus_contract(contract_file), &actual, known_unimplemented);
    assert_conformant(iface, &problems);
}

#[tokio::test(flavor = "multi_thread")]
async fn portal_backends() {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use helix_portals::{PortalService, BUS_NAME, OBJECT_PATH};
    use zbus::zvariant::{ObjectPath, Value};

    let Some(bus) = TestBus::start() else { return };
    let session = helix_session::SessionService::start(bus.builder()).await.unwrap();
    let _session = tokio::spawn(session.run(std::future::pending()));
    let portals = PortalService::start(bus.builder(), Arc::new(Mutex::new(Default::default())))
        .await
        .unwrap();
    let conn = portals.connection();

    for iface in ["Settings", "Inhibit"] {
        let name = format!("org.freedesktop.impl.portal.{iface}");
        assert_matches_contract(conn, BUS_NAME, OBJECT_PATH, &name, &format!("{name}.xml"), &[]).await;
    }

    // Request and Session objects only exist once created; make one of each.
    let client = bus.connect().await;
    let request = ObjectPath::try_from(format!("{OBJECT_PATH}/request/1_1/c1")).unwrap();
    let session = ObjectPath::try_from(format!("{OBJECT_PATH}/session/1_1/c1")).unwrap();
    client
        .call_method(
            Some(BUS_NAME),
            OBJECT_PATH,
            Some("org.freedesktop.impl.portal.Inhibit"),
            "Inhibit",
            &(&request, "org.example.App", "", 8u32, HashMap::<&str, Value<'_>>::new()),
        )
        .await
        .unwrap();
    client
        .call_method(
            Some(BUS_NAME),
            OBJECT_PATH,
            Some("org.freedesktop.impl.portal.Inhibit"),
            "CreateMonitor",
            &(&request, &session, "org.example.App", ""),
        )
        .await
        .unwrap();
    for (iface, path) in [("Request", &request), ("Session", &session)] {
        let name = format!("org.freedesktop.impl.portal.{iface}");
        assert_matches_contract(conn, BUS_NAME, path.as_str(), &name, &format!("{name}.xml"), &[]).await;
    }
}
