//! moruna: a native task's arguments come from its start page, which holds its name and no
//! other argument: the name is the first argument, as a program's own name is elsewhere.

pub use super::common::Args;
use crate::ffi::OsString;

pub fn args() -> Args {
    Args::new(vec![OsString::from(moruna_sys::start::process_name())])
}
