//! Password checks for unlocking, through PAM.
//!
//! PAM (`libpam.so.0`) is loaded at run time, so building Helix needs no PAM
//! development package. Authentication uses the `helix` PAM service, which
//! must be installed as `/etc/pam.d/helix` (see `session/pam.d/helix`).
//! Without it (or without libpam) locking is refused rather than leaving a
//! lock screen nobody could unlock.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::Path;

use zeroize::{Zeroize, Zeroizing};

pub const SERVICE: &str = "helix";
const SERVICE_FILES: [&str; 3] = ["/etc/pam.d/helix", "/usr/lib/pam.d/helix", "/usr/share/pam.d/helix"];

const PAM_SUCCESS: c_int = 0;
const PAM_BUF_ERR: c_int = 5;
const PAM_CONV_ERR: c_int = 19;
const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_PROMPT_ECHO_ON: c_int = 2;
const PAM_ERROR_MSG: c_int = 3;
const PAM_TEXT_INFO: c_int = 4;
const PAM_REFRESH_CRED: c_int = 0x0010;

#[repr(C)]
struct PamMessage {
    msg_style: c_int,
    msg: *const c_char,
}

#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    resp_retcode: c_int,
}

type ConvFn = unsafe extern "C" fn(c_int, *mut *const PamMessage, *mut *mut PamResponse, *mut c_void) -> c_int;

#[repr(C)]
struct PamConv {
    conv: ConvFn,
    appdata_ptr: *mut c_void,
}

type PamStart = unsafe extern "C" fn(*const c_char, *const c_char, *const PamConv, *mut *mut c_void) -> c_int;
type PamHandleFn = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// Authentication cannot run at all (no PAM, no service file).
    Unavailable(String),
    /// Wrong password, locked account, and the like.
    Denied,
}

/// A password that is wiped from memory when dropped.
pub struct Secret(Vec<u8>);

impl Secret {
    pub fn new(s: String) -> Self {
        Self(s.into_bytes())
    }
}

impl Secret {
    /// The password's bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        for b in self.0.iter_mut() {
            // SAFETY: plain byte write; volatile so it is not optimized away.
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}

/// Room for this many bytes from the start, so typing a password does not
/// reallocate (and leave copies behind) in all but extreme cases.
const TYPED_CAPACITY: usize = 256;

/// The password being typed into the lock screen: one buffer, wiped when
/// characters are erased, when cleared and when dropped, and grown only by
/// copying into a new buffer and wiping the old one.
pub struct SecretText(Zeroizing<String>);

impl Default for SecretText {
    fn default() -> Self {
        Self(Zeroizing::new(String::with_capacity(TYPED_CAPACITY)))
    }
}

impl SecretText {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The number of characters typed (one dot each).
    pub fn chars(&self) -> usize {
        self.0.chars().count()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn push(&mut self, c: char) {
        self.push_str(c.encode_utf8(&mut [0; 4]));
    }

    pub fn push_str(&mut self, s: &str) {
        let needed = self.0.len() + s.len();
        if needed > self.0.capacity() {
            let mut bigger = Zeroizing::new(String::with_capacity(needed.next_power_of_two()));
            bigger.push_str(&self.0);
            // The old buffer is wiped as it is dropped.
            self.0 = bigger;
        }
        self.0.push_str(s);
    }

    /// Replace the text (an assistive technology or a paste set it).
    pub fn set(&mut self, s: &str) {
        self.clear();
        self.push_str(s);
    }

    /// Erase the last character, wiping its bytes.
    pub fn pop(&mut self) {
        let Some((start, _)) = self.0.char_indices().next_back() else { return };
        // SAFETY: zero bytes are valid UTF-8, and they are cut off at once.
        unsafe { self.0.as_mut_vec()[start..].zeroize() };
        self.0.truncate(start);
    }

    /// Wipe and empty (keeping the buffer).
    pub fn clear(&mut self) {
        self.0.zeroize();
    }

    /// Hand the password over for checking, leaving this empty.
    pub fn take(&mut self) -> Secret {
        // `to_vec` allocates exactly once: no copies are left behind.
        let secret = Secret(self.0.as_bytes().to_vec());
        self.clear();
        secret
    }
}

/// Checks passwords.
pub trait Authenticator: Send + Sync {
    /// Whether unlocking can work at all; locking is refused otherwise.
    fn available(&self) -> Result<(), String>;
    fn authenticate(&self, user: &str, password: &Secret) -> Result<(), AuthError>;
}

/// The real thing: the `helix` PAM service.
pub struct Pam;

fn load_libpam() -> Result<libloading::Library, String> {
    // SAFETY: loading the system PAM library runs its initializers, which is
    // what linking against it would do too.
    unsafe { libloading::Library::new("libpam.so.0") }.map_err(|e| format!("cannot load libpam: {e}"))
}

impl Authenticator for Pam {
    fn available(&self) -> Result<(), String> {
        load_libpam()?;
        if SERVICE_FILES.iter().any(|f| Path::new(f).exists()) {
            Ok(())
        } else {
            Err("PAM service file /etc/pam.d/helix is not installed (see session/pam.d/helix)".into())
        }
    }

    fn authenticate(&self, user: &str, password: &Secret) -> Result<(), AuthError> {
        self.available().map_err(AuthError::Unavailable)?;
        let lib = load_libpam().map_err(AuthError::Unavailable)?;
        let symbol_err = |e: libloading::Error| AuthError::Unavailable(e.to_string());
        // SAFETY: the symbols have the documented Linux-PAM signatures.
        let (start, authenticate, acct_mgmt, setcred, end) = unsafe {
            (
                *lib.get::<PamStart>(b"pam_start\0").map_err(symbol_err)?,
                *lib.get::<PamHandleFn>(b"pam_authenticate\0").map_err(symbol_err)?,
                *lib.get::<PamHandleFn>(b"pam_acct_mgmt\0").map_err(symbol_err)?,
                *lib.get::<PamHandleFn>(b"pam_setcred\0").map_err(symbol_err)?,
                *lib.get::<PamHandleFn>(b"pam_end\0").map_err(symbol_err)?,
            )
        };
        let mut data = ConvData::new(user, password).ok_or(AuthError::Denied)?;
        let conv = PamConv {
            conv: conversation,
            appdata_ptr: &mut data as *mut ConvData as *mut c_void,
        };
        let service = CString::new(SERVICE).expect("no NUL in service name");
        let mut handle: *mut c_void = std::ptr::null_mut();
        // SAFETY: valid C strings and conversation; `data` outlives the PAM
        // transaction, which ends with pam_end below.
        unsafe {
            if start(service.as_ptr(), data.user.as_ptr(), &conv, &mut handle) != PAM_SUCCESS || handle.is_null() {
                return Err(AuthError::Unavailable("pam_start failed".into()));
            }
            let mut status = authenticate(handle, 0);
            if status == PAM_SUCCESS {
                status = acct_mgmt(handle, 0);
            }
            if status == PAM_SUCCESS {
                // Refresh e.g. Kerberos tickets; failures here don't block.
                let _ = setcred(handle, PAM_REFRESH_CRED);
            }
            end(handle, status);
            if status == PAM_SUCCESS {
                Ok(())
            } else {
                Err(AuthError::Denied)
            }
        }
    }
}

/// Development-only authenticator that accepts the password `helix-test`.
/// Only compiled with the `insecure-test-auth` feature; never in normal
/// builds.
#[cfg(feature = "insecure-test-auth")]
pub struct TestAuth;

#[cfg(feature = "insecure-test-auth")]
impl Authenticator for TestAuth {
    fn available(&self) -> Result<(), String> {
        Ok(())
    }

    fn authenticate(&self, _user: &str, password: &Secret) -> Result<(), AuthError> {
        if password.0 == b"helix-test" {
            Ok(())
        } else {
            Err(AuthError::Denied)
        }
    }
}

/// The authenticator this build uses.
pub fn authenticator() -> std::sync::Arc<dyn Authenticator> {
    #[cfg(feature = "insecure-test-auth")]
    {
        tracing::warn!("INSECURE TEST BUILD: the lock screen accepts the password 'helix-test'");
        std::sync::Arc::new(TestAuth)
    }
    #[cfg(not(feature = "insecure-test-auth"))]
    {
        std::sync::Arc::new(Pam)
    }
}

/// The current user's login name.
pub fn current_user() -> String {
    // SAFETY: getpwuid returns a pointer to static storage or null; the name
    // is copied immediately.
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() || (*pw).pw_name.is_null() {
            return std::env::var("USER").unwrap_or_default();
        }
        CStr::from_ptr((*pw).pw_name).to_string_lossy().into_owned()
    }
}

struct ConvData {
    user: CString,
    password: CString,
}

impl ConvData {
    fn new(user: &str, password: &Secret) -> Option<Self> {
        Some(Self {
            user: CString::new(user).ok()?,
            password: CString::new(password.0.clone()).ok()?,
        })
    }
}

impl Drop for ConvData {
    fn drop(&mut self) {
        let bytes = std::mem::take(&mut self.password).into_bytes_with_nul();
        drop(Secret(bytes));
    }
}

/// PAM conversation: answer password prompts with the password and visible
/// prompts with the user name; ignore informational messages.
unsafe extern "C" fn conversation(
    num_msg: c_int,
    msg: *mut *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata: *mut c_void,
) -> c_int {
    if num_msg <= 0 || msg.is_null() || resp.is_null() || appdata.is_null() {
        return PAM_CONV_ERR;
    }
    let data = &*(appdata as *const ConvData);
    let replies = libc::calloc(num_msg as usize, std::mem::size_of::<PamResponse>()) as *mut PamResponse;
    if replies.is_null() {
        return PAM_BUF_ERR;
    }
    for i in 0..num_msg as usize {
        let message = *msg.add(i);
        let reply = &mut *replies.add(i);
        match (*message).msg_style {
            PAM_PROMPT_ECHO_OFF => reply.resp = libc::strdup(data.password.as_ptr()),
            PAM_PROMPT_ECHO_ON => reply.resp = libc::strdup(data.user.as_ptr()),
            PAM_ERROR_MSG | PAM_TEXT_INFO => {}
            _ => {
                free_replies(replies, i);
                return PAM_CONV_ERR;
            }
        }
    }
    *resp = replies;
    PAM_SUCCESS
}

/// Free the first `count` replies (wiping answers) and the array.
unsafe fn free_replies(replies: *mut PamResponse, count: usize) {
    for i in 0..count {
        let r = &mut *replies.add(i);
        if !r.resp.is_null() {
            let len = libc::strlen(r.resp);
            std::ptr::write_bytes(r.resp, 0, len);
            libc::free(r.resp as *mut c_void);
        }
    }
    libc::free(replies as *mut c_void);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_password_stays_in_one_wiped_buffer() {
        let mut t = SecretText::default();
        let start = t.0.as_ptr();
        for _ in 0..TYPED_CAPACITY / 2 {
            t.push('é');
        }
        assert_eq!(t.0.as_ptr(), start, "no reallocation within the reserved room");
        t.pop();
        assert_eq!(t.chars(), TYPED_CAPACITY / 2 - 1);
        // SAFETY: bytes within the capacity, written before.
        let tail = unsafe { std::slice::from_raw_parts(start.add(t.0.len()), 2) };
        assert_eq!(tail, [0, 0], "erased characters are wiped");
        let secret = t.take();
        assert_eq!(secret.bytes(), "é".repeat(TYPED_CAPACITY / 2 - 1).as_bytes());
        assert!(t.is_empty());
        // SAFETY: as above.
        let all = unsafe { std::slice::from_raw_parts(start, TYPED_CAPACITY) };
        assert!(all.iter().all(|&b| b == 0), "taking wipes the field");
        // Growing past the room keeps the text.
        for _ in 0..TYPED_CAPACITY + 10 {
            t.push('x');
        }
        assert_eq!(t.chars(), TYPED_CAPACITY + 10);
    }

    #[test]
    fn conversation_answers_prompts() {
        let data = ConvData::new("alice", &Secret::new("s3cret".into())).unwrap();
        let prompts = [
            PamMessage { msg_style: PAM_PROMPT_ECHO_OFF, msg: c"Password: ".as_ptr() },
            PamMessage { msg_style: PAM_PROMPT_ECHO_ON, msg: c"Login: ".as_ptr() },
            PamMessage { msg_style: PAM_TEXT_INFO, msg: c"Hello".as_ptr() },
        ];
        let mut ptrs: Vec<*const PamMessage> = prompts.iter().map(|m| m as *const _).collect();
        let mut out: *mut PamResponse = std::ptr::null_mut();
        // SAFETY: valid inputs; the replies are freed below.
        unsafe {
            let rc = conversation(3, ptrs.as_mut_ptr(), &mut out, &data as *const ConvData as *mut c_void);
            assert_eq!(rc, PAM_SUCCESS);
            assert_eq!(CStr::from_ptr((*out).resp).to_str().unwrap(), "s3cret");
            assert_eq!(CStr::from_ptr((*out.add(1)).resp).to_str().unwrap(), "alice");
            assert!((*out.add(2)).resp.is_null());
            free_replies(out, 3);
        }
    }

    #[test]
    fn conversation_rejects_unknown_styles() {
        let data = ConvData::new("alice", &Secret::new("x".into())).unwrap();
        let prompt = PamMessage { msg_style: 99, msg: c"?".as_ptr() };
        let mut ptrs = [&prompt as *const PamMessage];
        let mut out: *mut PamResponse = std::ptr::null_mut();
        // SAFETY: valid inputs; nothing is returned on error.
        let rc = unsafe { conversation(1, ptrs.as_mut_ptr(), &mut out, &data as *const ConvData as *mut c_void) };
        assert_eq!(rc, PAM_CONV_ERR);
        assert!(out.is_null());
    }

    #[test]
    fn current_user_is_known() {
        assert!(!current_user().is_empty());
    }
}
