// SPDX-License-Identifier: MPL-2.0

//! The core of the Asterinas kernel.
//!
//! This crate implements the Linux ABI and the core kernel mechanisms.

#![no_std]
#![deny(unsafe_code)]
#![feature(alloc_io)]
#![feature(btree_cursors)]
#![feature(debug_closure_helpers)]
#![feature(format_args_nl)]
#![feature(linked_list_cursors)]
#![feature(linked_list_retain)]
#![feature(panic_can_unwind)]
#![feature(min_specialization)]
#![feature(thin_box)]
#![feature(unique_rc_arc)]
#![feature(custom_inner_attributes)]

extern crate alloc;
extern crate lru;
#[macro_use]
extern crate ostd_pod;

// Set this crate's log prefix for `ostd::log`.
macro_rules! __log_prefix {
    () => {
        ""
    };
}

#[cfg_attr(target_arch = "x86_64", path = "arch/x86/mod.rs")]
#[cfg_attr(target_arch = "riscv64", path = "arch/riscv/mod.rs")]
#[cfg_attr(target_arch = "loongarch64", path = "arch/loongarch/mod.rs")]
#[cfg_attr(target_arch = "aarch64", path = "arch/arm/mod.rs")]
mod arch;

pub mod context;
mod cpu;
pub mod device;
pub mod error;
pub mod events;
pub mod fs;
mod init;
mod ipc;
mod net;
pub mod power;
pub mod prelude;
pub mod process;
mod sched;
pub mod security;
mod syscall;
mod thread;
mod time;
pub mod util;
// User space: no vDSO is mapped (vdso.rs).
mod vdso;
pub mod vm;

// Host tests: the clocks a kernel's boot starts before its tests run.
#[cfg(ktest)]
ostd::ktest_setup!(time::clocks::init_for_ktest);

/// Boots the Asterinas core kernel.
pub fn boot() {
    init::main();
}
