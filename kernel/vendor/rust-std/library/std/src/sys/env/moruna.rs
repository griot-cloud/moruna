//! moruna: a native task's environment comes from its start page, which holds none: it is empty,
//! and setting a variable is unsupported.

pub use super::common::Env;
use crate::ffi::{OsStr, OsString};
use crate::io;

pub fn env() -> Env {
    Env::new(Vec::new())
}

pub fn getenv(_: &OsStr) -> Option<OsString> {
    None
}

pub unsafe fn setenv(_: &OsStr, _: &OsStr) -> io::Result<()> {
    Err(io::const_error!(io::ErrorKind::Unsupported, "a native task has no environment to set"))
}

pub unsafe fn unsetenv(_: &OsStr) -> io::Result<()> {
    Err(io::const_error!(io::ErrorKind::Unsupported, "a native task has no environment to unset"))
}
