//! What the KB1 test programs share.

#![no_std]
#![forbid(unsafe_code)]

use moruna_sys::{Console, exit, start};

/// The exit code of a program whose manifest entry lacks a slot it needs.
pub const MISSING_SLOT: i64 = 90;

/// The console slot, or the process ends with [`MISSING_SLOT`].
pub fn console() -> Console {
    match start::take("console") {
        Some(h) => Console(h),
        None => exit(MISSING_SLOT),
    }
}

/// The handle in `slot`, or the process ends with [`MISSING_SLOT`].
pub fn slot(name: &str) -> moruna_sys::Handle {
    match start::take(name) {
        Some(h) => h,
        None => exit(MISSING_SLOT),
    }
}

/// Milliseconds as nanoseconds.
pub const fn ms(n: u64) -> u64 {
    n * 1_000_000
}

/// The machine a program runs on, as the CPUID hypervisor leaf (0x4000_0000) names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Machine {
    /// KVM's signature, `KVMKVMKVM\0\0\0`: the guest runs on the host's CPU.
    Kvm,
    /// QEMU's TCG signature, `TCGTCGTCGTCG`: every guest instruction is emulated.
    Tcg,
    /// No hypervisor bit, or a signature neither of the above; the twelve bytes it read.
    Other([u8; 12]),
}

impl Machine {
    /// What the CPU says it is. `cpuid` is unprivileged, so a program reads it itself.
    pub fn detect() -> Machine {
        use core::arch::x86_64::__cpuid;
        let mut signature = [0u8; 12];
        if __cpuid(1).ecx & (1 << 31) == 0 {
            return Machine::Other(signature);
        }
        let leaf = __cpuid(0x4000_0000);
        signature[..4].copy_from_slice(&leaf.ebx.to_le_bytes());
        signature[4..8].copy_from_slice(&leaf.ecx.to_le_bytes());
        signature[8..].copy_from_slice(&leaf.edx.to_le_bytes());
        match &signature {
            b"KVMKVMKVM\0\0\0" => Machine::Kvm,
            b"TCGTCGTCGTCG" => Machine::Tcg,
            _ => Machine::Other(signature),
        }
    }
}
