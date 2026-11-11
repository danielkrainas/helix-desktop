//! C library locale access for `GetLocale`.

use std::ffi::CStr;

/// Adopt the locale from the environment (`LANG`, `LC_*`), as a C program
/// calling `setlocale(LC_ALL, "")` would. Call once at startup, before other
/// threads touch the locale.
pub fn init_from_env() {
    // SAFETY: the argument is a valid NUL-terminated string. Called at
    // startup; nothing else in this process changes the locale.
    unsafe {
        libc::setlocale(libc::LC_ALL, c"".as_ptr());
    }
}

/// The current locale for an `LC_*` category, or `None` if the category is
/// unknown to the C library.
pub fn current(category: i32) -> Option<String> {
    // SAFETY: a null locale argument only queries. The returned string is
    // copied before any further setlocale call could invalidate it, and this
    // process only sets the locale once, at startup.
    unsafe {
        let ptr = libc::setlocale(category, std::ptr::null());
        if ptr.is_null() {
            None
        } else {
            Some(CStr::from_ptr(ptr).to_string_lossy().into_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_and_unknown_categories() {
        assert!(current(libc::LC_MESSAGES).is_some());
        assert!(current(libc::LC_ALL).is_some());
        assert_eq!(current(-12345), None);
    }
}
