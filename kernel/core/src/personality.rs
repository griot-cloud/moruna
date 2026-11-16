//! The user-space personality's mechanisms (ABI 2.6; `kernel-is-the-machine.md` §4.1,
//! `architecture/data-os.md` §4): stream B1's module.
//!
//! The personality entry, copy-on-write snapshots and clones, the user pager, and thread
//! state and interruption. Their arguments are `moruna_sys::personality`'s. Until each is
//! implemented it answers `BadSyscall`. Only stream B1 writes this file.

use alloc::sync::Arc;

use moruna_abi::Error;

use crate::process::{Process, Thread};

/// What a call returns: `rax` and `rdx`, or the error.
pub(crate) type Answer = Result<(u64, u64), Error>;

/// `personality_set(process, base, len)`: a created process's personality region.
pub(crate) fn personality_set(_process: &Process, _args: [u64; 6]) -> Answer {
    Err(Error::BadSyscall)
}

/// `personality_return(frame) -> reason`: runs the program from `frame` until it enters.
pub(crate) fn personality_return(_caller: &Arc<Thread>, _args: [u64; 6]) -> Answer {
    Err(Error::BadSyscall)
}

/// `memory_snapshot(memory, offset, len, account) -> memory`.
pub(crate) fn memory_snapshot(_process: &Process, _args: [u64; 6]) -> Answer {
    Err(Error::BadSyscall)
}

/// `process_clone(resource, process, account) -> process`.
pub(crate) fn process_clone(_process: &Process, _args: [u64; 6]) -> Answer {
    Err(Error::BadSyscall)
}

/// `pager_create(port, size, key, account) -> memory`.
pub(crate) fn pager_create(_process: &Process, _args: [u64; 6]) -> Answer {
    Err(Error::BadSyscall)
}

/// `pager_supply(memory, offset, len, source, source_offset)`; a refusal when `source` is 0.
pub(crate) fn pager_supply(_process: &Process, _args: [u64; 6]) -> Answer {
    Err(Error::BadSyscall)
}

/// `thread_state_read(thread, frame, len)`.
pub(crate) fn thread_state_read(_process: &Process, _args: [u64; 6]) -> Answer {
    Err(Error::BadSyscall)
}

/// `thread_state_write(thread, frame, len)`.
pub(crate) fn thread_state_write(_process: &Process, _args: [u64; 6]) -> Answer {
    Err(Error::BadSyscall)
}

/// `thread_interrupt(thread)`.
pub(crate) fn thread_interrupt(_process: &Process, _args: [u64; 6]) -> Answer {
    Err(Error::BadSyscall)
}
