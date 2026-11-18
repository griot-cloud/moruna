//! moruna: a native task's environment is its start slot `env` (item 5.5), which std's start read
//! once (`moruna_sys::rt::env`); setting a variable is unsupported.

pub use super::common::Env;
use crate::ffi::{OsStr, OsString};
use crate::io;

/// Bytes of the environment as an `OsString`.
fn os_string(bytes: &[u8]) -> OsString {
    // SAFETY: an `OsStr` on Moruna is bytes (`sys::os_str::bytes`): any bytes are one.
    unsafe { OsString::from_encoded_bytes_unchecked(bytes.to_vec()) }
}

pub fn env() -> Env {
    let entries = moruna_sys::rt::env::entries();
    Env::new(entries.map(|(k, v)| (os_string(k), os_string(v))).collect())
}

pub fn getenv(key: &OsStr) -> Option<OsString> {
    moruna_sys::rt::env::get(key.as_encoded_bytes()).map(os_string)
}

pub unsafe fn setenv(_: &OsStr, _: &OsStr) -> io::Result<()> {
    Err(io::const_error!(
        io::ErrorKind::Unsupported,
        "a native task's environment is the one it was started with",
    ))
}

pub unsafe fn unsetenv(_: &OsStr) -> io::Result<()> {
    Err(io::const_error!(
        io::ErrorKind::Unsupported,
        "a native task's environment is the one it was started with",
    ))
}
