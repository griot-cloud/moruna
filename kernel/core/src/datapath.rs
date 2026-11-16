//! The data path's mechanisms (ABI 2.6; `architecture/data-os.md` §2): stream A's module.
//!
//! `thread_set_cpu`, `clock_page` and `windows_protect`, and the `map::HUGE` and
//! `map::POPULATE` flags, are reserved here and answered `BadSyscall` until stream A
//! implements them (`architecture/data-os-streams.md`). Only stream A writes this file.

use alloc::sync::Arc;

use moruna_abi::Error;

use crate::process::{Process, Thread};

/// What a call returns: `rax` and `rdx`, or the error.
pub(crate) type Answer = Result<(u64, u64), Error>;

/// `thread_set_cpu(thread, cpu)`: reserved.
pub(crate) fn thread_set_cpu(_caller: &Arc<Thread>, _args: [u64; 6]) -> Answer {
    Err(Error::BadSyscall)
}

/// `clock_page() -> address`: reserved.
pub(crate) fn clock_page(_process: &Process, _args: [u64; 6]) -> Answer {
    Err(Error::BadSyscall)
}

/// `windows_protect(process, ranges, count, flags)`: reserved.
pub(crate) fn windows_protect(_process: &Process, _args: [u64; 6]) -> Answer {
    Err(Error::BadSyscall)
}
