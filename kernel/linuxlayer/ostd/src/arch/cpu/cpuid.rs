// SPDX-License-Identifier: MPL-2.0

//! CPU information from the CPUID instruction.

use core::arch::x86_64::CpuidResult;

use spin::Once;

static MAX_LEAF: Once<u32> = Once::new();
static MAX_HYPERVISOR_LEAF: Once<u32> = Once::new();
static MAX_EXTENDED_LEAF: Once<u32> = Once::new();

#[repr(u32)]
enum Leaf {
    Base = 0x00,
    Xstate = 0x0d,

    HypervisorBase = 0x40000000,
    ExtBase = 0x80000000,
}

/// Executes the CPUID instruction for the given leaf and subleaf.
///
/// This method will return `None` if the leaf is not supported.
pub fn cpuid(leaf: u32, subleaf: u32) -> Option<CpuidResult> {
    fn raw_cpuid(leaf: u32, subleaf: u32) -> CpuidResult {
        core::arch::x86_64::__cpuid_count(leaf, subleaf)
    }

    let max_leaf = if leaf < Leaf::HypervisorBase as u32 {
        // Standard leaves (0x0000_0000 - 0x3FFF_FFFF)
        *MAX_LEAF.call_once(|| raw_cpuid(Leaf::Base as u32, 0).eax)
    } else if leaf < Leaf::ExtBase as u32 {
        // Hypervisor leaves (0x4000_0000 - 0x7FFF_FFFF)
        *MAX_HYPERVISOR_LEAF.call_once(|| raw_cpuid(Leaf::HypervisorBase as u32, 0).eax)
    } else {
        // Extended leaves (0x8000_0000 - 0xFFFF_FFFF)
        *MAX_EXTENDED_LEAF.call_once(|| raw_cpuid(Leaf::ExtBase as u32, 0).eax)
    };

    if leaf > max_leaf {
        None
    } else {
        Some(raw_cpuid(leaf, subleaf))
    }
}

/// Queries the supported XSTATE features, i.e., the supported bits of `XCR0` and `IA32_XSS`.
pub(crate) fn query_xstate_max_features() -> Option<u64> {
    let res0 = cpuid(Leaf::Xstate as u32, 0)?;
    let res1 = cpuid(Leaf::Xstate as u32, 1)?;

    // Supported bits in `XCR0`.
    let xcr_bits = (res0.eax as u64) | ((res0.edx as u64) << 32);
    // Supported bits in `IA32_XSS`.
    let xss_bits = (res1.ecx as u64) | ((res1.edx as u64) << 32);

    Some(xcr_bits | xss_bits)
}

/// Queries the size in bytes of the XSAVE area containing states enabled by `XCRO` and `IA32_XSS`.
pub(crate) fn query_xsave_area_size() -> Option<u32> {
    cpuid(Leaf::Xstate as u32, 1).map(|res| res.ebx)
}
