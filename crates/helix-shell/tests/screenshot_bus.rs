//! The shell's `org.gnome.Shell.Screenshot`, with a stand-in UI that
//! "captures" and "picks" as told.

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use helix_shell::screenshot::{CaptureError, Job, Picked, Rect, Target};
use helix_shell::{ShellService, UiEvent};
use helix_test_support::TestBus;
use zbus::zvariant::OwnedValue;
use zbus::Connection;

const PATH: &str = "/org/gnome/Shell/Screenshot";
const IFACE: &str = "org.gnome.Shell.Screenshot";

/// How the stand-in UI answers.
#[derive(Default)]
struct Script {
    picks: Vec<Picked>,
    fail_capture: bool,
    /// Jobs seen: "capture Screen", "pick Area", "flash 0,0 10x10".
    seen: Vec<String>,
}

fn png(rgb: [u8; 3]) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[rgb[0], rgb[1], rgb[2], 255]).unwrap();
    }
    bytes
}

fn fake_ui(mut ui: tokio::sync::mpsc::UnboundedReceiver<UiEvent>, script: Arc<Mutex<Script>>) {
    tokio::spawn(async move {
        while let Some(event) = ui.recv().await {
            let UiEvent::Screenshot(slot) = event else { continue };
            let Some(job) = slot.take() else { continue };
            let mut script = script.lock().unwrap();
            match job {
                Job::Capture { target, mut file, reply, .. } => {
                    script.seen.push(format!("capture {target:?}"));
                    if script.fail_capture {
                        let _ = reply.send(Err(CaptureError::NoWindow));
                    } else {
                        file.write_all(&png([255, 0, 51])).unwrap();
                        let rect = match target {
                            Target::Area(r) => r,
                            _ => Rect::new(0, 0, 10, 10),
                        };
                        let _ = reply.send(Ok(rect));
                    }
                }
                Job::Pick { mode, reply } => {
                    script.seen.push(format!("pick {mode:?}"));
                    let answer = if script.picks.is_empty() { Picked::Cancelled } else { script.picks.remove(0) };
                    let _ = reply.send(answer);
                }
                Job::Flash(r) => script.seen.push(format!("flash {},{} {}x{}", r.x, r.y, r.width, r.height)),
            }
        }
    });
}

async fn call<B, R>(conn: &Connection, method: &str, body: &B) -> zbus::Result<R>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
    R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
{
    let reply = conn.call_method(Some("org.gnome.Shell"), PATH, Some(IFACE), method, body).await?;
    reply.body().deserialize::<R>()
}

fn error_name(err: zbus::Error) -> String {
    match err {
        zbus::Error::MethodError(name, _, _) => name.to_string(),
        other => format!("{other}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn screenshots_through_the_ui() {
    let Some(bus) = TestBus::start() else { return };
    let (_shell, ui) = ShellService::start(bus.builder()).await.unwrap();
    let script = Arc::new(Mutex::new(Script::default()));
    fake_ui(ui, script.clone());
    let dir = std::env::temp_dir().join(format!("helix-screenshot-bus-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("shot").to_string_lossy().into_owned();

    // Not an allowed caller.
    let stranger = bus.connect().await;
    let denied = call::<_, (bool, String)>(&stranger, "Screenshot", &(false, false, target.as_str())).await;
    assert_eq!(error_name(denied.unwrap_err()), "org.freedesktop.DBus.Error.AccessDenied");

    // The GNOME Screenshot app may.
    let app = bus.connect().await;
    app.request_name("org.gnome.Screenshot").await.unwrap();
    let (ok, used): (bool, String) = call(&app, "Screenshot", &(false, true, target.as_str())).await.unwrap();
    assert!(ok);
    assert_eq!(used, format!("{target}.png"));
    assert!(std::fs::read(&used).unwrap().starts_with(b"\x89PNG"));
    // Taken names are not overwritten.
    let (_, again): (bool, String) = call(&app, "Screenshot", &(false, false, target.as_str())).await.unwrap();
    assert_eq!(again, format!("{target} (2).png"));

    let (ok, used): (bool, String) =
        call(&app, "ScreenshotArea", &(5, 6, 7, 8, false, dir.join("area.png").to_str().unwrap())).await.unwrap();
    assert!(ok && used.ends_with("area.png"));
    let invalid = call::<_, (bool, String)>(&app, "ScreenshotArea", &(0, 0, 0, 8, false, "x")).await;
    assert_eq!(error_name(invalid.unwrap_err()), "org.freedesktop.DBus.Error.InvalidArgs");

    // A failed capture leaves no file behind.
    script.lock().unwrap().fail_capture = true;
    let (ok, used): (bool, String) =
        call(&app, "ScreenshotWindow", &(true, false, false, dir.join("win").to_str().unwrap())).await.unwrap();
    assert_eq!((ok, used.as_str()), (false, ""));
    assert!(!dir.join("win.png").exists());
    script.lock().unwrap().fail_capture = false;

    // Selecting an area, and cancelling.
    script.lock().unwrap().picks = vec![Picked::Area(Rect::new(1, 2, 30, 40)), Picked::Cancelled];
    let area: (i32, i32, i32, i32) = call(&app, "SelectArea", &()).await.unwrap();
    assert_eq!(area, (1, 2, 30, 40));
    let cancelled = call::<_, (i32, i32, i32, i32)>(&app, "SelectArea", &()).await;
    assert_eq!(error_name(cancelled.unwrap_err()), "org.freedesktop.DBus.Error.Failed");

    // Picking a color: a point, then a 1x1 capture there.
    script.lock().unwrap().picks = vec![Picked::Point(3, 4)];
    let result: std::collections::HashMap<String, OwnedValue> = call(&app, "PickColor", &()).await.unwrap();
    let (r, g, b) = <(f64, f64, f64)>::try_from(result["color"].clone()).unwrap();
    assert_eq!((r, g, b), (1.0, 0.0, 0.2));

    call::<_, ()>(&app, "FlashArea", &(1, 2, 3, 4)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let seen = script.lock().unwrap().seen.clone();
    assert_eq!(
        seen,
        [
            "capture Screen",
            "flash 0,0 10x10",
            "capture Screen",
            "capture Area(Rect { x: 5, y: 6, width: 7, height: 8 })",
            "capture Window",
            "pick Area",
            "pick Area",
            "pick Point",
            "capture Area(Rect { x: 3, y: 4, width: 1, height: 1 })",
            "flash 1,2 3x4",
        ]
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
