//! A quick settings toggle that keeps the screen on (permission
//! `inhibit-idle`), for the `minutes` setting's minutes (0: until turned
//! off).

use std::time::Duration;

use helix_extension::{Event, Extension, ToggleState};

#[tokio::main(flavor = "current_thread")]
async fn main() -> zbus::Result<()> {
    let mut ext = Extension::connect().await?;
    let toggle = ext.toggle("toggle", ToggleState { title: "Caffeine".into(), icon: "weather-clear-night-symbolic".into(), ..Default::default() }).await?;
    ext.start().await?;
    let mut handle: Option<u32> = None;
    let mut until: Option<tokio::time::Instant> = None;
    loop {
        let wait = async {
            match until {
                Some(t) => tokio::time::sleep_until(t).await,
                None => std::future::pending().await,
            }
        };
        let event = tokio::select! {
            e = ext.next_event() => match e { Some(e) => e, None => break },
            _ = wait => Event::SetActive { toggle: "toggle".into(), active: false },
        };
        let Event::SetActive { active, .. } = event else { continue };
        if active && handle.is_none() {
            handle = Some(ext.inhibit_idle("Caffeine is on").await?);
            let minutes = i64::try_from(ext.setting("minutes").await?).unwrap_or(0);
            until = (minutes > 0).then(|| tokio::time::Instant::now() + Duration::from_secs(minutes as u64 * 60));
            let subtitle = if minutes > 0 { format!("{minutes} min") } else { String::new() };
            toggle.update(|s| {
                s.active = true;
                s.subtitle = subtitle;
            })
            .await?;
        } else if !active {
            if let Some(h) = handle.take() {
                ext.uninhibit(h).await?;
            }
            until = None;
            toggle.update(|s| {
                s.active = false;
                s.subtitle.clear();
            })
            .await?;
        }
    }
    Ok(())
}
