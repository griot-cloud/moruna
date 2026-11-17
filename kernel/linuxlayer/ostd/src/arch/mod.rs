// SPDX-License-Identifier: MPL-2.0

//! x86-64, from a user process.

pub mod cpu;
pub mod power;
pub mod trap;

/// Reads the "time-stamp counter": in user space the core's monotonic clock in nanoseconds,
/// which a VM's CPUID may not give a frequency for when the real TSC is used.
pub fn read_tsc() -> u64 {
    crate::backend::now()
}

/// The counter's frequency: a nanosecond clock's.
pub fn tsc_freq() -> u64 {
    1_000_000_000
}

/// A random 64-bit value from the core's random source.
pub fn read_random() -> Option<u64> {
    let mut buf = [0u8; 8];
    crate::backend::random(&mut buf);
    Some(u64::from_ne_bytes(buf))
}
