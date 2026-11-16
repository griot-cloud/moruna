// SPDX-License-Identifier: MPL-2.0

//! x86-64, from a user process.

pub mod cpu;
pub mod trap;

/// Reads the time-stamp counter.
pub fn read_tsc() -> u64 {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: RDTSC reads a counter and touches no memory.
    unsafe {
        core::arch::x86_64::_rdtsc()
    }
    #[cfg(not(target_arch = "x86_64"))]
    crate::backend::now()
}

/// The time-stamp counter's frequency, from CPUID; zero when the CPU does not say.
pub fn tsc_freq() -> u64 {
    cpu::cpuid::query_tsc_freq().unwrap_or(0)
}

/// A random 64-bit value from the core's random source.
pub fn read_random() -> Option<u64> {
    let mut buf = [0u8; 8];
    crate::backend::random(&mut buf);
    Some(u64::from_ne_bytes(buf))
}
