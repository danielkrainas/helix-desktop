//! GTK-style accelerator strings (`<Super>a`, `<Control><Alt>Delete`).

use std::fmt;

/// Modifier bits, in canonical order.
const MODIFIERS: &[(&str, &[&str])] = &[
    ("Shift", &["shift"]),
    ("Control", &["control", "ctrl", "ctl", "primary"]),
    ("Alt", &["alt", "mod1"]),
    ("Super", &["super"]),
    ("Hyper", &["hyper"]),
    ("Meta", &["meta"]),
];

/// A parsed accelerator, normalized so equivalent spellings compare equal.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Accelerator {
    /// Bitmask over [`MODIFIERS`].
    modifiers: u8,
    /// Key name; single letters are lowercased.
    key: String,
}

impl Accelerator {
    pub fn parse(text: &str) -> Option<Self> {
        let mut rest = text.trim();
        let mut modifiers = 0u8;
        while let Some(stripped) = rest.strip_prefix('<') {
            let end = stripped.find('>')?;
            let name = stripped[..end].to_ascii_lowercase();
            let bit = MODIFIERS
                .iter()
                .position(|(_, aliases)| aliases.contains(&name.as_str()))?;
            modifiers |= 1 << bit;
            rest = &stripped[end + 1..];
        }
        if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return None;
        }
        let key = if rest.chars().count() == 1 {
            rest.to_ascii_lowercase()
        } else {
            rest.to_owned()
        };
        Some(Self { modifiers, key })
    }
}

impl Accelerator {
    /// Modifier bits and key name for the `helix_keybindings_v1` protocol
    /// (shift 1, ctrl 2, alt 4, logo 8). `None` for Hyper/Meta, which the
    /// protocol does not carry.
    pub fn to_wire(&self) -> Option<(u32, String)> {
        const WIRE: [Option<u32>; 6] = [Some(1), Some(2), Some(4), Some(8), None, None];
        let mut bits = 0;
        for (bit, wire) in WIRE.iter().enumerate() {
            if self.modifiers & (1 << bit) != 0 {
                bits |= (*wire)?;
            }
        }
        Some((bits, self.key.clone()))
    }
}

impl fmt::Display for Accelerator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (bit, (name, _)) in MODIFIERS.iter().enumerate() {
            if self.modifiers & (1 << bit) != 0 {
                write!(f, "<{name}>")?;
            }
        }
        f.write_str(&self.key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_normalize() {
        let a = Accelerator::parse("<Super>a").unwrap();
        assert_eq!(a.to_string(), "<Super>a");
        assert_eq!(Accelerator::parse("<super>A"), Some(a));
        assert_eq!(
            Accelerator::parse("<Alt><Ctrl>Delete"),
            Accelerator::parse("<Control><Alt>Delete")
        );
        assert_eq!(
            Accelerator::parse("<Primary><Mod1>Delete").unwrap().to_string(),
            "<Control><Alt>Delete"
        );
        assert_eq!(Accelerator::parse("XF86AudioMute").unwrap().to_string(), "XF86AudioMute");
    }

    #[test]
    fn wire_form() {
        assert_eq!(Accelerator::parse("<Super>a").unwrap().to_wire(), Some((8, "a".into())));
        assert_eq!(
            Accelerator::parse("<Control><Shift>F9").unwrap().to_wire(),
            Some((1 | 2, "F9".into()))
        );
        assert_eq!(Accelerator::parse("XF86AudioMute").unwrap().to_wire(), Some((0, "XF86AudioMute".into())));
        assert_eq!(Accelerator::parse("<Hyper>a").unwrap().to_wire(), None);
    }

    #[test]
    fn rejects_invalid() {
        for bad in ["", "<Super>", "<Bogus>a", "<Super", "a b", "<Super>a-b"] {
            assert_eq!(Accelerator::parse(bad), None, "{bad:?}");
        }
    }
}
