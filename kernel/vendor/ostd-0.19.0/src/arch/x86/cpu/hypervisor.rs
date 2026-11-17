// SPDX-License-Identifier: MPL-2.0

//! moruna: the hypervisor the machine runs on, and the clock rates it gives (Moruna's
//! system design, section 1F, "The core on Hyper-V"; work item 1.8). It names nothing of
//! OSTD: it reads the CPU through [`Cpu`], so Moruna's host tests compile this same file
//! (`kernel/ostd-adaptation`) and answer for the CPU with recorded CPUID and MSR values.
//!
//! Every Hyper-V constant is the vendored `hvdef`'s. The two it does not have, the
//! hypervisor-present bit and the interface signature, are written here with the place
//! Microsoft's Hypervisor Top Level Functional Specification (TLFS) defines them.

use hvdef::{
    HV_CPUID_FUNCTION_HV_INTERFACE, HV_CPUID_FUNCTION_HV_VENDOR_AND_MAX_FUNCTION,
    HV_CPUID_FUNCTION_MS_HV_FEATURES, HV_CPUID_FUNCTION_MS_HV_IMPLEMENTATION_LIMITS,
    HV_CPUID_FUNCTION_VERSION_AND_FEATURES, HV_PAGE_SHIFT, HV_X64_MSR_APIC_FREQUENCY,
    HV_X64_MSR_TSC_FREQUENCY, HvFeatures,
    hypercall::{HvGuestOsOpenSource, HvGuestOsOpenSourceType, MsrHypercallContents},
};

/// What this file reads of the CPU.
pub(crate) trait Cpu {
    /// CPUID's EAX, EBX, ECX and EDX for `leaf` and `subleaf`; `None` for a leaf above
    /// the highest the CPU reports in that leaf's range (standard, hypervisor, extended).
    fn cpuid(&self, leaf: u32, subleaf: u32) -> Option<[u32; 4]>;

    /// An MSR's value. This file reads an MSR only once CPUID has said it is there and
    /// the partition may read it.
    fn rdmsr(&self, msr: u32) -> u64;
}

/// A hypervisor this file can name.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Hypervisor {
    /// Microsoft's hypervisor interface, `Hv#1`: Hyper-V, and so Azure.
    HyperV,
    /// KVM, by its signature.
    Kvm,
    /// Another: QEMU's TCG, Xen, VMware and the rest.
    Other,
}

/// CPUID leaf 1, ECX bit 31: a hypervisor is present (TLFS, "Establishing the Hypercall
/// Interface (x86/x64)", step 1; "Feature and Interface Discovery", "Hypervisor
/// Discovery").
const HYPERVISOR_PRESENT: u32 = 1 << 31;

/// Leaf `0x40000001`'s EAX on Microsoft's hypervisor interface: `Hv#1` (TLFS,
/// "Establishing the Hypercall Interface (x86/x64)", step 2). `hvdef` names the leaf and
/// not the signature.
const HV_INTERFACE_SIGNATURE: u32 = u32::from_le_bytes(*b"Hv#1");

/// KVM's signature in leaf `0x40000000`'s EBX, ECX and EDX, as OSTD's `cpuid.rs` reads it.
const KVM_SIGNATURE: [u8; 12] = *b"KVMKVMKVM\0\0\0";

/// The CPUID leaf of the Time Stamp Counter and Nominal Core Crystal Clock (Intel SDM,
/// volume 2A, CPUID, leaf 15H). Not a Hyper-V leaf, so not `hvdef`'s.
const TSC_CRYSTAL_LEAF: u32 = 0x15;

/// The console line for each kind of hypervisor (`moruna_abi::boot_line`, which the host
/// tests hold these to). A machine with no hypervisor has none.
pub(crate) const HYPERV_LINE: &str = "moruna: hypervisor hyperv";
pub(crate) const KVM_LINE: &str = "moruna: hypervisor kvm";
pub(crate) const OTHER_LINE: &str = "moruna: hypervisor other";

impl Hypervisor {
    /// The console line for this hypervisor.
    pub(crate) fn line(self) -> &'static str {
        match self {
            Hypervisor::HyperV => HYPERV_LINE,
            Hypervisor::Kvm => KVM_LINE,
            Hypervisor::Other => OTHER_LINE,
        }
    }
}

/// The hypervisor the machine runs on, `None` on one with none.
///
/// A hypervisor is present when CPUID leaf 1 sets ECX bit 31. It is Microsoft's when
/// leaf `0x40000000` reports leaves up to at least `0x40000005` and leaf `0x40000001`
/// answers `Hv#1`, as the TLFS asks a guest to decide (step 2), rather than by the vendor
/// name: KVM can offer the same interface, and is then Hyper-V to its guest.
pub(crate) fn hypervisor(cpu: &impl Cpu) -> Option<Hypervisor> {
    let [_, _, ecx, _] = cpu.cpuid(HV_CPUID_FUNCTION_VERSION_AND_FEATURES, 0)?;
    if ecx & HYPERVISOR_PRESENT == 0 {
        return None;
    }
    let Some([max, ebx, ecx, edx]) = cpu.cpuid(HV_CPUID_FUNCTION_HV_VENDOR_AND_MAX_FUNCTION, 0)
    else {
        return Some(Hypervisor::Other);
    };
    let interface = cpu.cpuid(HV_CPUID_FUNCTION_HV_INTERFACE, 0);
    if max >= HV_CPUID_FUNCTION_MS_HV_IMPLEMENTATION_LIMITS
        && interface.is_some_and(|[eax, ..]| eax == HV_INTERFACE_SIGNATURE)
    {
        return Some(Hypervisor::HyperV);
    }
    let mut signature = [0; 12];
    signature[0..4].copy_from_slice(&ebx.to_le_bytes());
    signature[4..8].copy_from_slice(&ecx.to_le_bytes());
    signature[8..12].copy_from_slice(&edx.to_le_bytes());
    if signature == KVM_SIGNATURE {
        Some(Hypervisor::Kvm)
    } else {
        Some(Hypervisor::Other)
    }
}

/// What Microsoft's hypervisor lets this partition do (leaf `0x40000003`), on Hyper-V.
fn hyperv_features(cpu: &impl Cpu) -> Option<HvFeatures> {
    if hypervisor(cpu)? != Hypervisor::HyperV {
        return None;
    }
    cpu.cpuid(HV_CPUID_FUNCTION_MS_HV_FEATURES, 0)
        .map(HvFeatures::from_cpuid)
}

/// Whether Hyper-V's two frequency MSRs may be read: the partition holds
/// `AccessFrequencyRegs` (leaf `0x40000003` EAX bit 11) and the hypervisor says the
/// frequencies are available (EDX bit 8), the two bits Linux checks
/// (`arch/x86/kernel/cpu/mshyperv.c`, `HV_ACCESS_FREQUENCY_MSRS` and
/// `HV_FEATURE_FREQUENCY_MSRS_AVAILABLE`).
fn frequency_msrs(cpu: &impl Cpu) -> bool {
    hyperv_features(cpu).is_some_and(|features| {
        features.privileges().access_frequency_msrs() && features.frequency_regs_available()
    })
}

/// A rate in Hz from one of Hyper-V's frequency MSRs, if they may be read and it is not
/// zero.
fn hyperv_rate(cpu: &impl Cpu, msr: u32) -> Option<u64> {
    if !frequency_msrs(cpu) {
        return None;
    }
    let hz = cpu.rdmsr(msr);
    (hz != 0).then_some(hz)
}

/// Where the TSC's rate came from.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum TscSource {
    /// KVM's paravirtual clock.
    Kvmclock,
    /// CPUID leaf `0x15`: the core crystal's rate and the TSC's ratio to it.
    Crystal,
    /// Hyper-V's `HV_X64_MSR_TSC_FREQUENCY`.
    HyperV,
}

/// The TSC's rate in Hz, and where it came from: kvmclock's (`kvmclock` answers it, or
/// `None` off KVM), then CPUID leaf `0x15`'s, then Hyper-V's MSR, in the system design's
/// order (section 1F, "Clock rates"). `None` leaves the PIT, which comes last.
pub(crate) fn tsc_rate(
    cpu: &impl Cpu,
    kvmclock: impl FnOnce() -> Option<u64>,
) -> Option<(u64, TscSource)> {
    if let Some(hz) = kvmclock() {
        return Some((hz, TscSource::Kvmclock));
    }
    if let Some(hz) = crystal_rate(cpu) {
        return Some((hz, TscSource::Crystal));
    }
    hyperv_rate(cpu, HV_X64_MSR_TSC_FREQUENCY).map(|hz| (hz, TscSource::HyperV))
}

/// The TSC's rate from CPUID leaf `0x15`, if the leaf gives the crystal's rate.
///
/// moruna: moved here from `cpuid.rs` (`query_tsc_freq`), its arithmetic and its reasons
/// unchanged, so that the host tests can answer for the leaf.
fn crystal_rate(cpu: &impl Cpu) -> Option<u64> {
    let [denominator, numerator, crystal_freq, _] = cpu.cpuid(TSC_CRYSTAL_LEAF, 0)?;

    if denominator == 0 || numerator == 0 {
        return None;
    }

    // If the nominal core crystal clock frequency is not enumerated, we can either obtain that
    // information from a hardcoded table or rely on the processor base frequency. The Intel
    // documentation recommends the first approach [1], but Linux uses the second approach because
    // the first approach is difficult to implement correctly for all corner cases [2]. However,
    // the second approach does not provide 100% accurate frequencies, so Linux must adjust them at
    // runtime [2]. For now, we avoid these headaches by faithfully reporting that the TSC
    // frequency is unavailable.
    //
    // [1]: Intel(R) 64 and IA-32 Architectures Software Developer's Manual,
    //      Section 20.7.3, Determining the Processor Base Frequency
    // [2]: https://github.com/torvalds/linux/commit/604dc9170f2435d27da5039a3efd757dceadc684
    if crystal_freq == 0 {
        return None;
    }

    Some((crystal_freq as u64) * (numerator as u64) / (denominator as u64))
}

/// The local APIC timer's divider in periodic mode: OSTD's `DivideConfig::Divide64`.
pub(crate) const APIC_TIMER_DIVIDE: u64 = 64;

/// The local APIC timer's initial count for one tick at `ticks_per_second` in periodic
/// mode, from Hyper-V's `HV_X64_MSR_APIC_FREQUENCY` (the timer's input clock, before the
/// divider). `None` off Hyper-V, without the frequency MSRs, or when the count would not
/// fit the 32-bit register or would be zero: the timer is then calibrated as before.
pub(crate) fn apic_timer_count(cpu: &impl Cpu, ticks_per_second: u64) -> Option<u32> {
    let hz = hyperv_rate(cpu, HV_X64_MSR_APIC_FREQUENCY)?;
    let count = hz / APIC_TIMER_DIVIDE / ticks_per_second;
    u32::try_from(count).ok().filter(|count| *count != 0)
}

/// Whether the guest OS ID and hypercall MSRs may be written: Hyper-V, and the partition
/// holds `AccessHypercallMsrs` (leaf `0x40000003` EAX bit 5), which Linux requires too
/// (`HV_MSR_HYPERCALL_AVAILABLE`).
pub(crate) fn hypercall_msrs(cpu: &impl Cpu) -> bool {
    hyperv_features(cpu).is_some_and(|features| features.privileges().access_hypercall_msrs())
}

/// The guest OS ID Moruna reports in `HV_X64_MSR_GUEST_OS_ID` before it enables the
/// hypercall page, in the TLFS's encoding for open source operating systems ("Reporting
/// the Guest OS Identity", "Guest OS Identity MSR for Open Source Operating Systems"):
/// bit 63 set, an open source OS; OS type, bits 62:56, undefined (0), since Microsoft
/// allocates the types and has none for Moruna; OS ID, version and build, bits 55:0,
/// zero. Proposed in work item 1.8, for the PM's approval.
pub(crate) const GUEST_OS_ID: u64 = HvGuestOsOpenSource::new()
    .with_is_open_source(true)
    .with_os_type(HvGuestOsOpenSourceType::UNDEFINED.0)
    .with_os_id(0)
    .with_version(0)
    .with_build_no(0)
    .into_bits();

/// The value to write to `HV_X64_MSR_HYPERCALL` to place the hypercall page at the guest
/// physical address `page` and enable it, from the MSR's `current` value: its reserved
/// bits kept, as the TLFS asks ("Establishing the Hypercall Interface (x86/x64)"). A page
/// already enabled is moved, never kept (`kernel/hyperv.rs` says why).
/// `None` when the MSR is locked, so the page cannot be placed, or `page` is not a page
/// address the MSR can hold.
pub(crate) fn hypercall_msr(current: u64, page: u64) -> Option<u64> {
    let contents = MsrHypercallContents::from_bits(current);
    if contents.locked() || page & ((1 << HV_PAGE_SHIFT) - 1) != 0 {
        return None;
    }
    contents
        .with_enable(true)
        .with_gpn_checked(page >> HV_PAGE_SHIFT)
        .ok()
        .map(MsrHypercallContents::into_bits)
}
