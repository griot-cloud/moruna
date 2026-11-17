//! The Moruna microkernel (SDD 20).
//!
//! The kernel holds processes, threads, memory, interrupts, timers, messages and
//! capability handles, and nothing else. Every other service is a process speaking a
//! published protocol, started from the boot manifest. OSTD is the hardware layer: it
//! holds all of the unsafe code (boot, page tables, interrupts, timers, the frame
//! allocator, task switching, user mode entry). This crate is safe Rust.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod account;
mod boot;
mod channel;
mod clock;
mod datapath;
mod foreign;
mod futex;
mod interrupt;
mod memory;
mod object;
mod personality;
mod plane;
mod process;
mod sched;
mod signal;
mod syscall;
mod usercopy;
mod vm;

/// The kernel's entry point, called by the boot crate from OSTD's `#[ostd::main]`.
/// It starts the manifest's processes and returns, which hands the CPU to them.
pub fn main() {
    boot::boot();
}

/// Says the core's fatal line for a panic in kernel mode (`moruna: fatal core: <reason>`).
/// The boot crate's panic handler calls it, then OSTD's handler, which powers the machine
/// off.
pub fn panicked(info: &core::panic::PanicInfo) {
    boot::panicked(info);
}
