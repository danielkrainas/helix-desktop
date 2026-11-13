//! A panel item that counts clicks: click to count, scroll to change the
//! count, its menu resets it.

use helix_extension::{Event, Extension, MenuEntry, PanelItemState};

#[tokio::main(flavor = "current_thread")]
async fn main() -> zbus::Result<()> {
    let mut ext = Extension::connect().await?;
    let menu = ext.menu("main", vec![MenuEntry::item(1, "Reset"), MenuEntry::separator(2), MenuEntry::check(3, "Count Down", false)]).await?;
    let state = PanelItemState { icon: "starred-symbolic".into(), label: "0".into(), tooltip: "Clicks\nScroll to change".into(), menu: Some("main".into()), ..Default::default() };
    let item = ext.panel_item("count", state).await?;
    ext.start().await?;
    ext.log(2, "counter started").await?;
    let (mut count, mut down) = (0i64, false);
    while let Some(event) = ext.next_event().await {
        match event {
            Event::Activate { .. } => count += if down { -1 } else { 1 },
            Event::Scroll { delta, .. } => count -= i64::from(delta / 120),
            Event::MenuActivated { id: 1, .. } => count = 0,
            Event::MenuActivated { id: 3, .. } => {
                down = !down;
                menu.set(vec![MenuEntry::item(1, "Reset"), MenuEntry::separator(2), MenuEntry::check(3, "Count Down", down)]).await?;
            }
            _ => continue,
        }
        item.update(|s| s.label = count.to_string()).await?;
    }
    Ok(())
}
