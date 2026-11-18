//! ptrace: a function traces no process; `ENOSYS`.

use super::{Sys, nosys};
use crate::{
    error::Result,
    platform::{
        PalPtrace,
        types::{c_int, c_void, pid_t},
    },
};

impl PalPtrace for Sys {
    unsafe fn ptrace(
        _request: c_int,
        _pid: pid_t,
        _addr: *mut c_void,
        _data: *mut c_void,
    ) -> Result<c_int> {
        nosys()
    }
}
