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

mod boot;
mod channel;
mod clock;
mod interrupt;
mod memory;
mod object;
mod process;
mod signal;
mod syscall;
mod usercopy;

/// The kernel's entry point, called by the boot crate from OSTD's `#[ostd::main]`.
/// It starts the manifest's processes and returns, which hands the CPU to them.
pub fn main() {
    boot::boot();
}
