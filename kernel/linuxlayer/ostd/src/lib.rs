// SPDX-License-Identifier: MPL-2.0

//! OSTD's API for Asterinas's Linux layer in user space.
//!
//! Asterinas's Linux layer is written against OSTD, its kernel framework. Carried into a
//! user process as Moruna's Linux personality (`architecture/data-os.md` §4), it is
//! built against this crate instead, imported under OSTD's name, so the carried files
//! keep their `use ostd::...` lines. The mechanical swaps (`kernel-is-the-machine.md`
//! §4.2) are here:
//!
//! - OSTD's `Mutex`, `SpinLock`, `RwLock`, `RwMutex`, `WaitQueue` and `Waiter` are the same
//!   types built on the core's futex (`sync`);
//! - `VmReader` and `VmWriter` over the program's memory go through a fault-tolerant copy
//!   (`mm::io`).
//!
//! What touches the entry, clones, the pager and thread state sits behind the program
//! driver (`user`), and is stream B1's mechanism. Files carried from OSTD keep its
//! licence, MPL-2.0, with their headers.

#![no_std]
#![feature(core_intrinsics)]
#![feature(negative_impls)]
#![expect(internal_features)]
#![warn(missing_docs)]

extern crate alloc;
#[macro_use]
extern crate ostd_pod;

// Set this crate's log prefix for `ostd::log`.
macro_rules! __log_prefix {
    () => {
        ""
    };
}

pub mod arch;
pub mod backend;
pub mod boot;
pub mod console;
pub mod cpu;
mod error;
pub mod irq;
pub mod log;
pub mod mm;
pub mod panic;
pub mod power;
pub mod prelude;
pub mod sync;
pub mod task;
pub mod timer;
pub mod user;
pub mod util;

#[cfg(ktest)]
mod test;

pub use ostd_macros::early_cmdline_parser;
pub use ostd_pod::Pod;

// OSDK's heap allocator's per-CPU boxes (`osdk_heap_allocator`), for aster-util.
pub use self::cpu::local::{CpuLocalBox, alloc_cpu_local};
pub use self::{error::Error, prelude::Result};
