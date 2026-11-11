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

#[tokio::test(flavor = "multi_thread")]
async fn mutter_idle_monitor_and_display_config() {
    use helix_comp::{display_config, idle_monitor, CompositorServices};

    let Some(bus) = TestBus::start() else { return };
    let services = CompositorServices::start(bus.builder(), vec![]).await.unwrap();
    let conn = services.connection();
    assert_matches_contract(
        conn,
        idle_monitor::BUS_NAME,
        idle_monitor::CORE_PATH,
        "org.gnome.Mutter.IdleMonitor",
        "org.gnome.Mutter.IdleMonitor.xml",
        &[],
    )
    .await;
    // Write side is not implemented yet (see helix-desktop-spec/gaps.md).
    assert_matches_contract(
        conn,
        display_config::BUS_NAME,
        display_config::OBJECT_PATH,
        "org.gnome.Mutter.DisplayConfig",
        "org.gnome.Mutter.DisplayConfig.xml",
        &[
            "method ApplyConfiguration",
            "method ApplyMonitorsConfig",
            "method SetBacklight",
            "method ChangeBacklight",
            "method GetCrtcGamma",
            "method SetCrtcGamma",
            "method SetOutputCTM",
        ],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn org_gnome_shell() {
    let Some(bus) = TestBus::start() else { return };
    let (service, _ui) = helix_shell::ShellService::start(bus.builder()).await.unwrap();
    assert_matches_contract(
        service.connection(),
        helix_shell::BUS_NAME,
        helix_shell::OBJECT_PATH,
        "org.gnome.Shell",
        "org.gnome.Shell.xml",
        &[],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn org_gnome_screensaver() {
    let Some(bus) = TestBus::start() else { return };
    let (service, _ui) = helix_shell::ShellService::start(bus.builder()).await.unwrap();
    assert_matches_contract(
        service.connection(),
        helix_shell::screensaver::BUS_NAME,
        helix_shell::screensaver::OBJECT_PATH,
        "org.gnome.ScreenSaver",
        "org.gnome.ScreenSaver.xml",
        &[],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn notifications() {
    let Some(bus) = TestBus::start() else { return };
    let (service, _ui) = helix_shell::ShellService::start(bus.builder()).await.unwrap();
    use helix_shell::notifications::{FDO_BUS_NAME, FDO_PATH, GTK_BUS_NAME, GTK_PATH};
    assert_matches_contract(
        service.connection(),
        FDO_BUS_NAME,
        FDO_PATH,
        "org.freedesktop.Notifications",
        "org.freedesktop.Notifications.xml",
        &[],
    )
    .await;
    assert_matches_contract(service.connection(), GTK_BUS_NAME, GTK_PATH, "org.gtk.Notifications", "org.gtk.Notifications.xml", &[]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn end_session_dialog_and_session_objects() {
    let Some(bus) = TestBus::start() else { return };
    let (shell, _ui) = helix_shell::ShellService::start(bus.builder()).await.unwrap();
    assert_matches_contract(
        shell.connection(),
        helix_shell::BUS_NAME,
        helix_shell::end_dialog::OBJECT_PATH,
        "org.gnome.SessionManager.EndSessionDialog",
        "org.gnome.SessionManager.EndSessionDialog.xml",
        &[],
    )
    .await;

    // A client and an inhibitor object exported by the session manager.
    let session = helix_session::SessionService::start(bus.builder()).await.unwrap();
    let client = bus.connect().await;
    let reply = client
        .call_method(Some("org.gnome.SessionManager"), "/org/gnome/SessionManager", Some("org.gnome.SessionManager"), "RegisterClient", &("org.example.App", ""))
        .await
        .unwrap();
    let client_path: zbus::zvariant::OwnedObjectPath = reply.body().deserialize().unwrap();
    let reply = client
        .call_method(Some("org.gnome.SessionManager"), "/org/gnome/SessionManager", Some("org.gnome.SessionManager"), "Inhibit", &("org.example.App", 0u32, "busy", 1u32))
        .await
        .unwrap();
    let cookie: u32 = reply.body().deserialize().unwrap();
    let conn = session.connection();
    assert_matches_contract(conn, "org.gnome.SessionManager", client_path.as_str(), "org.gnome.SessionManager.ClientPrivate", "org.gnome.SessionManager.ClientPrivate.xml", &[]).await;
    let inhibitor = format!("/org/gnome/SessionManager/Inhibitor{cookie}");
    assert_matches_contract(conn, "org.gnome.SessionManager", &inhibitor, "org.gnome.SessionManager.Inhibitor", "org.gnome.SessionManager.Inhibitor.xml", &[]).await;
}
