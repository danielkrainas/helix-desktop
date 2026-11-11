//! The built-in panel's backlight, for DisplayConfig's `Backlight` and
//! `SetBacklight` (helix-desktop-spec `org.gnome.Mutter.DisplayConfig.md`,
//! "Backlight"): a kernel backlight device, written through logind.

use std::path::{Path, PathBuf};

use helix_session::logind::Logind;

/// The lowest value offered, so the panel never goes fully dark.
pub const MIN: i32 = 1;

/// A kernel backlight device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub name: String,
    pub max: i32,
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_owned())
}

/// The preferred device under `root` (`/sys/class/backlight`): firmware,
/// then platform, then raw.
pub fn find_device(root: &Path) -> Option<Device> {
    let rank = |kind: &str| match kind {
        "firmware" => 0,
        "platform" => 1,
        "raw" => 2,
        _ => 3,
    };
    let mut found: Vec<(i32, Device)> = std::fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let dir = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let max: i32 = read_trimmed(&dir.join("max_brightness"))?.parse().ok()?;
            let kind = read_trimmed(&dir.join("type")).unwrap_or_default();
            (max >= MIN).then(|| (rank(&kind), Device { name, max }))
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));
    found.into_iter().next().map(|(_, d)| d)
}

/// Where the brightness is read, and how it is written.
#[derive(Debug, Clone)]
pub struct Backlight {
    root: PathBuf,
    logind: Option<Logind>,
}

impl Backlight {
    pub fn new(root: PathBuf, logind: Option<Logind>) -> Self {
        Self { root, logind }
    }

    /// `/sys/class/backlight`, written through logind on the system bus.
    pub async fn system() -> Self {
        Self::new(PathBuf::from("/sys/class/backlight"), Logind::system().await)
    }

    pub fn device(&self) -> Option<Device> {
        find_device(&self.root)
    }

    /// Current value of `device`.
    pub fn value(&self, device: &Device) -> Option<i32> {
        read_trimmed(&self.root.join(&device.name).join("brightness"))?.parse().ok()
    }

    pub async fn set(&self, device: &Device, value: i32) -> Result<(), String> {
        let logind = self.logind.as_ref().ok_or("no logind to set the brightness")?;
        let value = u32::try_from(value).map_err(|_| "negative brightness")?;
        logind
            .set_brightness("backlight", &device.name, value)
            .await
            .map_err(|e| format!("logind refused to set the brightness: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(root: &Path, name: &str, kind: &str, max: i32) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("type"), format!("{kind}\n")).unwrap();
        std::fs::write(dir.join("max_brightness"), format!("{max}\n")).unwrap();
        std::fs::write(dir.join("brightness"), "7\n").unwrap();
    }

    #[test]
    fn prefers_firmware_then_platform_then_raw() {
        let root = std::env::temp_dir().join(format!("helix-backlight-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(find_device(&root), None, "no directory");
        device(&root, "intel_backlight", "raw", 1000);
        assert_eq!(find_device(&root).unwrap().name, "intel_backlight");
        device(&root, "dell_backlight", "platform", 15);
        assert_eq!(find_device(&root).unwrap().name, "dell_backlight");
        device(&root, "acpi_video0", "firmware", 100);
        device(&root, "broken", "firmware", 0);
        let found = find_device(&root).unwrap();
        assert_eq!(found, Device { name: "acpi_video0".into(), max: 100 });
        let backlight = Backlight::new(root.clone(), None);
        assert_eq!(backlight.value(&found), Some(7));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
