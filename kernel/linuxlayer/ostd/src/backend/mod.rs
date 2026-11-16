// SPDX-License-Identifier: MPL-2.0

//! What the personality needs from the machine under it.
//!
//! On the target this is Moruna's core, through `moruna-sys`: futexes, the clock, threads
//! and the fault-tolerant copy. In host tests it is the host's threads, so the carried
//! logic is tested on the host with the same locks and wait queues.

#[cfg(target_os = "none")]
mod moruna;
mod shared;
#[cfg(target_os = "none")]
pub(crate) use moruna::*;
#[cfg(target_os = "none")]
pub use moruna::{
    clone_process, copy_through_core, personality_set, self_process, set_self_process,
    shared_chunk, shared_map, shared_unmap, start_thread_block, task_join_empty, task_leave,
    user_copy_range,
};
#[cfg_attr(
    not(test),
    expect(unused_imports, reason = "the target's binding uses it")
)]
pub(crate) use shared::{Core, TaskMemory};

#[cfg(not(target_os = "none"))]
mod host;
#[cfg(not(target_os = "none"))]
pub use host::fault_range;
#[cfg(not(target_os = "none"))]
pub(crate) use host::*;

/// No deadline.
pub(crate) const FOREVER: u64 = u64::MAX;

/// Which side of a fallible copy is the program's memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dir {
    /// The destination.
    ToProgram,
    /// The source.
    FromProgram,
    /// Both.
    Both,
}
