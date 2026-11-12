//! A monitor's identity from its EDID base block (helix-desktop-spec
//! `contracts/dbus/org.gnome.Mutter.DisplayConfig.md`, "Monitor identity").

/// What tells monitors apart: vendor, product, serial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub vendor: String,
    pub product: String,
    pub serial: String,
}

impl Identity {
    /// The identity when there is no EDID.
    pub fn unknown() -> Self {
        Self { vendor: "Unknown".into(), product: "Unknown".into(), serial: "0".into() }
    }

    /// As settings name a monitor: [vendor, product, serial].
    pub fn matches(&self, spec: &[String]) -> bool {
        spec.len() >= 3 && spec[0] == self.vendor && spec[1] == self.product && spec[2] == self.serial
    }
}

const HEADER: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];

/// A display descriptor's text: up to 13 bytes, ended by a newline, padded
/// with spaces.
fn descriptor_text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|b| *b == 0x0a).unwrap_or(bytes.len());
    bytes[..end].iter().map(|b| if b.is_ascii_graphic() || *b == b' ' { *b as char } else { '?' }).collect::<String>().trim().to_owned()
}

/// The identity in `edid`: the three-letter PNP vendor id; the monitor
/// name descriptor, else the product code as `0x` and four hex digits; the
/// serial number descriptor, else the serial number as `0x` and eight hex
/// digits.
pub fn parse(edid: &[u8]) -> Option<Identity> {
    if edid.len() < 128 || edid[..8] != HEADER {
        return None;
    }
    let id = u16::from_be_bytes([edid[8], edid[9]]);
    let letter = |shift: u16| {
        let v = ((id >> shift) & 0x1f) as u8;
        (1..=26).contains(&v).then(|| (b'A' + v - 1) as char)
    };
    let vendor: String = [letter(10)?, letter(5)?, letter(0)?].iter().collect();
    let code = u16::from_le_bytes([edid[10], edid[11]]);
    let number = u32::from_le_bytes([edid[12], edid[13], edid[14], edid[15]]);
    let (mut name, mut serial) = (None, None);
    for d in edid[54..126].chunks(18) {
        if d[0] == 0 && d[1] == 0 {
            match d[3] {
                0xfc => name = Some(descriptor_text(&d[5..18])),
                0xff => serial = Some(descriptor_text(&d[5..18])),
                _ => {}
            }
        }
    }
    Some(Identity {
        vendor,
        product: name.filter(|n| !n.is_empty()).unwrap_or_else(|| format!("0x{code:04x}")),
        serial: serial.filter(|s| !s.is_empty()).unwrap_or_else(|| format!("0x{number:08x}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edid(name: Option<&str>, serial: Option<&str>) -> Vec<u8> {
        let mut e = vec![0u8; 128];
        e[..8].copy_from_slice(&HEADER);
        // "DEL": D=4, E=5, L=12.
        let id: u16 = (4 << 10) | (5 << 5) | 12;
        e[8..10].copy_from_slice(&id.to_be_bytes());
        e[10..12].copy_from_slice(&0xa0c4u16.to_le_bytes());
        e[12..16].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        let mut put = |at: usize, tag: u8, text: &str| {
            e[at + 3] = tag;
            let mut t = text.as_bytes().to_vec();
            t.push(0x0a);
            t.resize(13, b' ');
            e[at + 5..at + 18].copy_from_slice(&t);
        };
        if let Some(n) = name {
            put(72, 0xfc, n);
        }
        if let Some(s) = serial {
            put(90, 0xff, s);
        }
        e
    }

    #[test]
    fn host_monitors() {
        // Whatever monitors this machine has: each EDID gives an identity.
        let Ok(dir) = std::fs::read_dir("/sys/class/drm") else { return };
        for entry in dir.flatten() {
            let Ok(bytes) = std::fs::read(entry.path().join("edid")) else { continue };
            if bytes.len() >= 128 {
                let id = parse(&bytes);
                println!("{}: {id:?}", entry.file_name().to_string_lossy());
                assert!(id.is_some_and(|i| i.vendor.len() == 3));
            }
        }
    }

    #[test]
    fn identities() {
        let full = parse(&edid(Some("DELL U2720Q"), Some("ABC123"))).unwrap();
        assert_eq!(full, Identity { vendor: "DEL".into(), product: "DELL U2720Q".into(), serial: "ABC123".into() });
        let bare = parse(&edid(None, None)).unwrap();
        assert_eq!((bare.product.as_str(), bare.serial.as_str()), ("0xa0c4", "0x12345678"));
        assert!(parse(&[0u8; 128]).is_none());
        assert!(full.matches(&["DEL".into(), "DELL U2720Q".into(), "ABC123".into()]));
        assert!(!full.matches(&["".into(), "".into(), "".into()]));
    }
}
