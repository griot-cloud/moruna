//! The user-space personality's mechanisms (ABI 2.6; `kernel-is-the-machine.md` §4.1,
//! `architecture/data-os.md` §4): stream B1's module.
//!
//! The personality entry, copy-on-write snapshots and clones, the user pager, and thread
//! state and interruption are reserved here and answered `BadSyscall` until stream B1
//! implements them (`architecture/data-os-streams.md`). Only stream B1 writes this file.

use alloc::sync::Arc;

use moruna_abi::Error;

use crate::process::{Process, Thread};

/// What a call returns: `rax` and `rdx`, or the error.
pub(crate) type Answer = Result<(u64, u64), Error>;

/// `personality_set`: reserved.
pub(crate) fn personality_set(_process: &Process, _args: [u64; 6]) -> Answer {
    Err(Error::BadSyscall)
}

/// `personality_return`: reserved.
pub(crate) fn personality_return(_caller: &Arc<Thread>, _args: [u64; 6]) -> Answer {
    Err(Error::BadSyscall)
}

/// `memory_snapshot`: reserved.
pub(crate) fn memory_snapshot(_process: &Process, _args: [u64; 6]) -> Answer {
    Err(Error::BadSyscall)
}

/// `process_clone`: reserved.
pub(crate) fn process_clone(_process: &Process, _args: [u64; 6]) -> Answer {
    Err(Error::BadSyscall)
}

/// `pager_create`: reserved.
pub(crate) fn pager_create(_process: &Process, _args: [u64; 6]) -> Answer {
    Err(Error::BadSyscall)
}

/// `pager_supply`: reserved.
pub(crate) fn pager_supply(_process: &Process, _args: [u64; 6]) -> Answer {
    Err(Error::BadSyscall)
}

/// `thread_state_read`: reserved.
pub(crate) fn thread_state_read(_process: &Process, _args: [u64; 6]) -> Answer {
    Err(Error::BadSyscall)
}

/// `thread_state_write`: reserved.
pub(crate) fn thread_state_write(_process: &Process, _args: [u64; 6]) -> Answer {
    Err(Error::BadSyscall)
}

/// `thread_interrupt`: reserved.
pub(crate) fn thread_interrupt(_process: &Process, _args: [u64; 6]) -> Answer {
    Err(Error::BadSyscall)
}
