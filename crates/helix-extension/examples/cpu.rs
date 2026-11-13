//! A panel item with a graph of the processor's use, from /proc/stat,
//! once a second.

use std::time::Duration;

use helix_extension::{Extension, PanelItemState};

/// (busy, total) jiffies.
fn read() -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let fields: Vec<u64> = stat.lines().next()?.split_whitespace().skip(1).filter_map(|v| v.parse().ok()).collect();
    let total: u64 = fields.iter().sum();
    let idle = fields.get(3)? + fields.get(4).copied().unwrap_or(0);
    Some((total - idle, total))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> zbus::Result<()> {
    let mut ext = Extension::connect().await?;
    let item = ext.panel_item("graph", PanelItemState { icon: "utilities-system-monitor-symbolic".into(), accessible_name: "Processor use".into(), ..Default::default() }).await?;
    ext.start().await?;
    let mut last = read();
    let mut graph: Vec<f64> = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            e = ext.next_event() => if e.is_none() { break },
            _ = tick.tick() => {
                let now = read();
                if let (Some((b0, t0)), Some((b1, t1))) = (last, now) {
                    let used = if t1 > t0 { (b1 - b0) as f64 / (t1 - t0) as f64 } else { 0.0 };
                    graph.push(used);
                    if graph.len() > 32 {
                        graph.remove(0);
                    }
                    let tooltip = format!("Processor\n{:.0} % in use", used * 100.0);
                    let values = graph.clone();
                    item.update(|s| {
                        s.graph = values;
                        s.tooltip = tooltip;
                    })
                    .await?;
                }
                last = now;
            }
        }
    }
    Ok(())
}
