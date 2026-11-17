//! The usage build's evidence (stream U).
//!
//! The usage build is an untimed build of the boot and its programs, compiled with LLVM's
//! coverage instrumentation, that runs the gate's jobs once to record what they used. No
//! production build carries any of it.
//!
//! - [`dump`]: a program's counters as console lines (the guest's encoder allocates
//!   nothing), and the host's decoder of a serial log.
//! - [`elf`]: the sections, segments and symbols of an ELF64 file (host).
//! - [`profraw`]: the LLVM raw profile rebuilt from a program's ELF and its counters, which
//!   is what `llvm-profdata` merges (host).
//!
//! The counters are the only part of a raw profile that changes while a program runs. The
//! rest (the per-function records and the names) is in the ELF file, so the guest sends
//! only counters: the kernel's are read out of guest memory by the host, and a user
//! program prints its own.

#![no_std]

#[cfg(feature = "alloc")]
extern crate alloc;

pub mod dump;
#[cfg(feature = "alloc")]
pub mod elf;
#[cfg(feature = "alloc")]
pub mod profraw;
