//! Passwords in memory: wiped when dropped or cleared, never copied into a
//! reallocation that is freed without wiping, and never printed.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use zeroize::{Zeroize, Zeroizing};

/// Room for this many bytes from the start, so typing a password does not
/// reallocate (and leave copies behind) in all but extreme cases.
const CAPACITY: usize = 256;

/// The text typed into a field that may hold a password.
pub struct SecretText(Zeroizing<String>);

impl Default for SecretText {
    fn default() -> Self {
        Self(Zeroizing::new(String::with_capacity(CAPACITY)))
    }
}

impl SecretText {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn push_str(&mut self, s: &str) {
        let needed = self.0.len() + s.len();
        if needed > self.0.capacity() {
            // Grow by hand: the old buffer is wiped as it is dropped.
            let mut bigger = Zeroizing::new(String::with_capacity(needed.next_power_of_two()));
            bigger.push_str(&self.0);
            self.0 = bigger;
        }
        self.0.push_str(s);
    }

    /// Remove the last character, wiping its bytes.
    pub fn pop(&mut self) {
        let Some((start, _)) = self.0.char_indices().next_back() else { return };
        // SAFETY: zero bytes are valid UTF-8, and they are cut off at once.
        unsafe { self.0.as_mut_vec()[start..].zeroize() };
        self.0.truncate(start);
    }

    /// Wipe and empty (the capacity stays).
    pub fn clear(&mut self) {
        let capacity = self.0.capacity();
        self.0.zeroize();
        if self.0.capacity() < capacity {
            self.0 = Zeroizing::new(String::with_capacity(capacity));
        }
    }

    /// One "•" per character: what is shown of a password.
    pub fn mask(&self) -> String {
        "•".repeat(self.0.chars().count())
    }
}

/// A password on its way to greetd. Its `Debug` hides it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(Zeroizing<String>);

impl Secret {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for Secret {
    fn from(s: &str) -> Self {
        // `to_owned` allocates exactly once: no copies are left behind.
        Self(Zeroizing::new(s.to_owned()))
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl Serialize for Secret {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self(Zeroizing::new(String::deserialize(d)?)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_keeps_one_buffer() {
        let mut t = SecretText::default();
        let start = t.as_str().as_ptr();
        for _ in 0..CAPACITY / 2 {
            t.push_str("é");
        }
        assert_eq!(t.as_str().as_ptr(), start, "no reallocation within the reserved room");
        t.pop();
        assert_eq!(t.as_str().chars().count(), CAPACITY / 2 - 1);
        // The popped character's bytes are wiped.
        // SAFETY: reading initialized bytes within the capacity.
        let tail = unsafe { std::slice::from_raw_parts(start.add(t.as_str().len()), 2) };
        assert_eq!(tail, [0, 0]);
        assert_eq!(t.mask().chars().count(), CAPACITY / 2 - 1);
        t.clear();
        assert!(t.as_str().is_empty());
        assert_eq!(t.as_str().as_ptr(), start);
    }

    #[test]
    fn growing_past_the_room_keeps_the_text() {
        let mut t = SecretText::default();
        let long = "x".repeat(CAPACITY + 10);
        t.push_str("ab");
        t.push_str(&long);
        assert_eq!(t.as_str(), format!("ab{long}"));
    }

    #[test]
    fn secret_is_redacted_but_serialized() {
        let s = Secret::from("hunter2");
        assert_eq!(format!("{s:?}"), "<redacted>");
        assert_eq!(serde_json::to_string(&s).unwrap(), r#""hunter2""#);
        assert_eq!(serde_json::from_str::<Secret>(r#""pw""#).unwrap(), Secret::from("pw"));
    }
}
