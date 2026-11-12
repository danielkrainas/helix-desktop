//! The polkit agent over a private bus, with a fake helper (helix-desktop-spec
//! `org.freedesktop.PolicyKit1.AuthenticationAgent.md`).

use std::collections::HashMap;
use std::time::Duration;

use helix_shell::auth::Secret;
use helix_shell::polkit::{Agent, AGENT_PATH};
use helix_shell::prompt::{Answer, Request};
use helix_shell::UiEvent;
use helix_test_support::TestBus;
use tokio::sync::mpsc::UnboundedReceiver;
use zbus::zvariant::Value;

const HELPER: &str = r#"#!/bin/sh
read cookie
echo "PAM_TEXT_INFO hello"
echo "PAM_PROMPT_ECHO_OFF Password: "
read pw
if [ "$pw" = "right" ] && [ "$cookie" = "c1" ]; then echo SUCCESS; else echo FAILURE; fi
"#;

async fn next(ui: &mut UnboundedReceiver<UiEvent>) -> UiEvent {
    tokio::time::timeout(Duration::from_secs(5), ui.recv()).await.expect("ui event").expect("open")
}

async fn prompt(ui: &mut UnboundedReceiver<UiEvent>) -> Request {
    loop {
        if let UiEvent::Prompt(slot) = next(ui).await {
            return slot.take().expect("request");
        }
    }
}

fn answer(request: Request, password: &str) {
    let _ = request.reply.send(Answer::Continue { entry: None, password: Some(Secret::new(password.into())), confirm: None, choice: false, selected: 0, picked: Vec::new(), place: None });
}

async fn begin(conn: &zbus::Connection, cookie: &str) -> zbus::Result<zbus::Message> {
    begin_with(conn, cookie, false).await
}

/// With `root`, root is offered as well as the test's user.
async fn begin_with(conn: &zbus::Connection, cookie: &str, root: bool) -> zbus::Result<zbus::Message> {
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let mut identities = vec![("unix-user", HashMap::from([("uid", Value::from(uid))]))];
    if root && uid != 0 {
        identities.push(("unix-user", HashMap::from([("uid", Value::from(0u32))])));
    }
    conn.call_method(
        Some("org.helix.TestAgent"),
        AGENT_PATH,
        Some("org.freedesktop.PolicyKit1.AuthenticationAgent"),
        "BeginAuthentication",
        &("org.example.do", "Authentication is needed to do it", "", HashMap::<&str, &str>::new(), cookie, identities),
    )
    .await
}

fn error_name(result: zbus::Result<zbus::Message>) -> String {
    match result {
        Err(zbus::Error::MethodError(name, _, _)) => name.to_string(),
        other => format!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn authenticates_through_the_helper() {
    let Some(bus) = TestBus::start() else { return };
    let dir = std::env::temp_dir().join(format!("helix-polkit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let helper = dir.join("helper");
    std::fs::write(&helper, HELPER).unwrap();
    std::fs::set_permissions(&helper, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    // SAFETY: this test binary runs this one test.
    unsafe { std::env::set_var("HELIX_POLKIT_HELPER", &helper) };

    let (ui_tx, mut ui) = tokio::sync::mpsc::unbounded_channel();
    let _agent = bus.builder().serve_at(AGENT_PATH, Agent::new(ui_tx)).unwrap().name("org.helix.TestAgent").unwrap().build().await.unwrap();
    let polkit = bus.builder().name("org.freedesktop.PolicyKit1").unwrap().build().await.unwrap();
    let stranger = bus.connect().await;

    // Not polkit: refused.
    assert_eq!(error_name(begin(&stranger, "c1").await), "org.freedesktop.PolicyKit1.Error.NotAuthorized");

    // A wrong password, then the right one.
    let call = tokio::spawn({
        let polkit = polkit.clone();
        async move { begin(&polkit, "c1").await }
    });
    let first = prompt(&mut ui).await;
    assert_eq!(first.prompt.password.as_deref(), Some("Password"));
    assert_eq!(first.prompt.description, "hello");
    assert_eq!(first.prompt.message, "Authentication is needed to do it");
    assert!(first.prompt.warning.is_empty());
    answer(first, "wrong");
    let second = prompt(&mut ui).await;
    assert_eq!(second.prompt.warning, "Sorry, that didn't work. Please try again.");
    answer(second, "right");
    loop {
        if matches!(next(&mut ui).await, UiEvent::PromptDone) {
            break;
        }
    }
    assert!(call.await.unwrap().is_ok());

    // The user cancels.
    let call = tokio::spawn({
        let polkit = polkit.clone();
        async move { begin(&polkit, "c2").await }
    });
    let _ = prompt(&mut ui).await.reply.send(Answer::Cancel);
    assert_eq!(error_name(call.await.unwrap()), "org.freedesktop.PolicyKit1.Error.Cancelled");

    // polkit cancels while the prompt is up.
    let call = tokio::spawn({
        let polkit = polkit.clone();
        async move { begin(&polkit, "c3").await }
    });
    let shown = prompt(&mut ui).await;
    polkit
        .call_method(Some("org.helix.TestAgent"), AGENT_PATH, Some("org.freedesktop.PolicyKit1.AuthenticationAgent"), "CancelAuthentication", &("c3",))
        .await
        .unwrap();
    assert_eq!(error_name(call.await.unwrap()), "org.freedesktop.PolicyKit1.Error.Cancelled");
    drop(shown);

    // Two users: the user picks first (ours listed first), then the
    // password; canceling the choice cancels.
    // SAFETY: getuid cannot fail.
    if unsafe { libc::getuid() } != 0 {
        let call = tokio::spawn({
            let polkit = polkit.clone();
            async move { begin_with(&polkit, "c1", true).await }
        });
        let choose = prompt(&mut ui).await;
        assert_eq!(choose.prompt.list.len(), 2);
        assert!(choose.prompt.list[1].contains("root"));
        assert!(choose.prompt.password.is_none());
        let _ = choose.reply.send(Answer::Continue { entry: None, password: None, confirm: None, choice: false, selected: 0, picked: Vec::new(), place: None });
        answer(prompt(&mut ui).await, "right");
        assert!(call.await.unwrap().is_ok());
        let call = tokio::spawn({
            let polkit = polkit.clone();
            async move { begin_with(&polkit, "c4", true).await }
        });
        let _ = prompt(&mut ui).await.reply.send(Answer::Cancel);
        assert_eq!(error_name(call.await.unwrap()), "org.freedesktop.PolicyKit1.Error.Cancelled");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}
