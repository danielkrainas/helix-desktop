//! A dock item that keeps the files dropped on it (API 1.1): its badge
//! counts them, its menu lists the last ones, a click forgets them.

use helix_extension::{DockItemState, Event, Extension, MenuEntry};

#[tokio::main(flavor = "current_thread")]
async fn main() -> zbus::Result<()> {
    let mut ext = Extension::connect().await?;
    let menu = ext.menu("files", vec![MenuEntry { enabled: false, ..MenuEntry::item(1, "Drop files here") }]).await?;
    let item = ext.dock_item("shelf", DockItemState { icon: "folder-symbolic".into(), menu: Some("files".into()), ..Default::default() }).await?;
    ext.start().await?;
    let mut files: Vec<String> = Vec::new();
    while let Some(event) = ext.next_event().await {
        match event {
            Event::Drop { uris, .. } => {
                ext.log(2, &format!("dropped: {}", uris.join(" "))).await?;
                files.extend(uris);
            }
            Event::DockActivate { .. } => files.clear(),
            _ => continue,
        }
        let count = files.len() as i64;
        item.update(|s| {
            s.count = count;
            s.running = count > 0;
        })
        .await?;
        let mut entries: Vec<MenuEntry> = files.iter().rev().take(10).enumerate().map(|(i, f)| MenuEntry { enabled: false, ..MenuEntry::item(i as i32 + 1, f.rsplit('/').next().unwrap_or(f)) }).collect();
        if entries.is_empty() {
            entries.push(MenuEntry { enabled: false, ..MenuEntry::item(1, "Drop files here") });
        }
        menu.set(entries).await?;
    }
    Ok(())
}
