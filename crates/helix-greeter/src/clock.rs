//! The clock: time and date in the locale's words (C library `strftime`).

use std::ffi::CString;

/// `now` (seconds since the epoch) as local time, formatted by `pattern`.
pub fn format(now: i64, pattern: &str) -> String {
    let Ok(pattern) = CString::new(pattern) else {
        return String::new();
    };
    let t = now as libc::time_t;
    // SAFETY: localtime_r fills `tm`; strftime writes at most `buf.len()`
    // bytes and returns how many.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return String::new();
        }
        let mut buf = [0u8; 128];
        let n = libc::strftime(buf.as_mut_ptr().cast(), buf.len(), pattern.as_ptr(), &tm);
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// "14:05".
pub fn time(now: i64) -> String {
    format(now, "%H:%M")
}

/// "Thursday, November 12".
pub fn date(now: i64) -> String {
    format(now, "%A, %B %-d")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        let t = time(0);
        assert_eq!(t.len(), 5, "{t}");
        assert_eq!(&t[2..3], ":");
        assert!(!date(0).is_empty());
        // The epoch in any time zone.
        assert!(["1969", "1970"].contains(&format(0, "%Y").as_str()));
        assert_eq!(format(0, "bad\0"), "");
    }
}
