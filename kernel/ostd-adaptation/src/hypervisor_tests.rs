//! Tests of `cpu/hypervisor.rs` and `timer/pit/presence.rs` (work item 1.8): the
//! hypervisor found and the clock rates taken, over CPUID and MSR answers recorded for
//! each kind of machine, and the PIT's presence over counts read from its ports.

use std::cell::RefCell;
use std::collections::BTreeMap;

use moruna_abi::boot_line::{CORE, Fatal, HYPERV, Hypervisor as Line, KVM};

use super::hypervisor::{
    APIC_TIMER_DIVIDE, Cpu, GUEST_OS_ID, HYPERV_LINE, Hypervisor, KVM_LINE, TscSource,
    apic_timer_count, hypercall_msr, hypercall_msrs, hypervisor, tsc_rate,
};
use super::pit_presence::{NO_PIT_FOR_APIC_TIMER, NO_PIT_FOR_TSC, READS, counts};

/// OSTD's timer rate, `TIMER_FREQ`: the APIC timer ticks this many times a second.
const TIMER_FREQ: u64 = 1000;

/// A CPU that answers from a recording. A leaf above the highest its range reports reads
/// as `None`, as OSTD's `cpuid` makes it; an MSR it was not given is a fault, which fails
/// the test, since `hypervisor.rs` must not read an MSR the CPU does not have.
#[derive(Default)]
struct Recorded {
    leaves: BTreeMap<u32, [u32; 4]>,
    msrs: BTreeMap<u32, u64>,
    read: RefCell<Vec<u32>>,
}

impl Recorded {
    fn leaf(mut self, leaf: u32, registers: [u32; 4]) -> Self {
        self.leaves.insert(leaf, registers);
        self
    }

    fn msr(mut self, msr: u32, value: u64) -> Self {
        self.msrs.insert(msr, value);
        self
    }

    fn highest(&self, base: u32) -> u32 {
        self.leaves.get(&base).map_or(0, |registers| registers[0])
    }
}

impl Cpu for Recorded {
    fn cpuid(&self, leaf: u32, subleaf: u32) -> Option<[u32; 4]> {
        assert_eq!(subleaf, 0, "every leaf read has one subleaf");
        let base = if leaf < 0x4000_0000 { 0 } else { 0x4000_0000 };
        if leaf > self.highest(base) {
            return None;
        }
        Some(self.leaves.get(&leaf).copied().unwrap_or_default())
    }

    fn rdmsr(&self, msr: u32) -> u64 {
        self.read.borrow_mut().push(msr);
        *self
            .msrs
            .get(&msr)
            .unwrap_or_else(|| panic!("#GP: MSR {msr:#x} read, and this CPU has none"))
    }
}

/// The four bytes of a signature as a register holds them.
fn word(bytes: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*bytes)
}

/// CPUID leaf 1's ECX with or without the hypervisor-present bit.
fn leaf_1(hypervisor: bool) -> [u32; 4] {
    [
        0x000a_06a4,
        0,
        0x7ffa_3203 | (u32::from(hypervisor) << 31),
        0x1f8b_fbff,
    ]
}

const TSC_FREQUENCY: u32 = 0x4000_0022;
const APIC_FREQUENCY: u32 = 0x4000_0023;

/// Leaf `0x40000003` with the hypercall MSRs (EAX bit 5), the frequency MSRs (EAX bit
/// 11) and the frequencies available (EDX bit 8), each as asked.
fn hyperv_features(hypercall: bool, frequency_access: bool, frequency_available: bool) -> [u32; 4] {
    // The other privileges Azure grants a guest: the reference counter, SynIC, timers,
    // the APIC MSRs and the VP index, PostMessages and SignalEvents.
    let eax = 0x0000_005e | u32::from(hypercall) << 5 | u32::from(frequency_access) << 11;
    let ebx = 0x0000_0030;
    let edx = 0x0000_0006 | u32::from(frequency_available) << 8;
    [eax, ebx, 0, edx]
}

/// An Azure machine (Generation 2): Microsoft's hypervisor interface up to leaf
/// `0x4000000c`, leaf 0x15 there and empty, the TSC at 2.59 GHz and the APIC timer's clock
/// at 200 MHz.
fn azure(features: [u32; 4]) -> Recorded {
    Recorded::default()
        .leaf(0, [0x15, word(b"Genu"), word(b"ntel"), word(b"ineI")])
        .leaf(1, leaf_1(true))
        .leaf(0x15, [0, 0, 0, 0])
        .leaf(
            0x4000_0000,
            [0x4000_000c, word(b"Micr"), word(b"osof"), word(b"t Hv")],
        )
        .leaf(0x4000_0001, [word(b"Hv#1"), 0, 0, 0])
        .leaf(0x4000_0003, features)
        .msr(TSC_FREQUENCY, 2_593_906_000)
        .msr(APIC_FREQUENCY, 200_000_000)
}

/// QEMU on KVM: KVM's signature and features, no leaf 0x15.
fn kvm() -> Recorded {
    Recorded::default()
        .leaf(0, [0xd, word(b"Auth"), word(b"cAMD"), word(b"enti")])
        .leaf(1, leaf_1(true))
        .leaf(
            0x4000_0000,
            [0x4000_0001, word(b"KVMK"), word(b"VMKV"), word(b"M\0\0\0")],
        )
        .leaf(0x4000_0001, [0x0100_7afb, 0, 0, 0])
}

/// A machine with no hypervisor whose leaf 0x15 gives the crystal: 24 MHz, and the TSC
/// 216/2 of it, 2.592 GHz.
fn bare_metal_with_crystal() -> Recorded {
    Recorded::default()
        .leaf(0, [0x20, word(b"Genu"), word(b"ntel"), word(b"ineI")])
        .leaf(1, leaf_1(false))
        .leaf(0x15, [2, 216, 24_000_000, 0])
}

/// A machine with nothing to give the TSC's rate: no hypervisor, no leaf 0x15.
fn nothing_at_all() -> Recorded {
    Recorded::default()
        .leaf(0, [0xd, word(b"Auth"), word(b"cAMD"), word(b"enti")])
        .leaf(1, leaf_1(false))
}

fn kvmclock(hz: u64) -> impl FnOnce() -> Option<u64> {
    move || Some(hz)
}

fn no_kvmclock() -> Option<u64> {
    None
}

#[test]
fn kvm_is_found_by_its_signature_and_its_clock_gives_the_tsc_rate() {
    let cpu = kvm();
    assert_eq!(hypervisor(&cpu), Some(Hypervisor::Kvm));
    assert_eq!(Hypervisor::Kvm.line(), Some("moruna: hypervisor kvm"));
    assert_eq!(
        tsc_rate(&cpu, kvmclock(2_995_200_000)),
        Some((2_995_200_000, TscSource::Kvmclock))
    );
    // KVM without its clock, and with no leaf 0x15: the PIT.
    assert_eq!(tsc_rate(&cpu, no_kvmclock), None);
    // No Hyper-V MSR is read off Hyper-V.
    assert_eq!(apic_timer_count(&cpu, TIMER_FREQ), None);
    assert!(!hypercall_msrs(&cpu));
    assert!(cpu.read.borrow().is_empty());
}

#[test]
fn hyper_v_is_found_by_its_interface_and_gives_both_rates() {
    let cpu = azure(hyperv_features(true, true, true));
    assert_eq!(hypervisor(&cpu), Some(Hypervisor::HyperV));
    assert_eq!(Hypervisor::HyperV.line(), Some("moruna: hypervisor hyperv"));
    // kvmclock first, which answers nothing off KVM; leaf 0x15, which is empty; the MSR.
    assert_eq!(
        tsc_rate(&cpu, no_kvmclock),
        Some((2_593_906_000, TscSource::HyperV))
    );
    // 200 MHz, divided by 64, a thousand ticks a second.
    assert_eq!(APIC_TIMER_DIVIDE, 64);
    assert_eq!(apic_timer_count(&cpu, TIMER_FREQ), Some(3125));
    assert_eq!(*cpu.read.borrow(), [TSC_FREQUENCY, APIC_FREQUENCY]);
    assert!(hypercall_msrs(&cpu));
}

#[test]
fn hyper_v_without_the_frequency_msrs_leaves_the_pit() {
    // Each of the two bits alone is not enough: the partition must hold the privilege and
    // the hypervisor must say the frequencies are there.
    for (access, available) in [(false, false), (true, false), (false, true)] {
        let cpu = azure(hyperv_features(true, access, available));
        assert_eq!(hypervisor(&cpu), Some(Hypervisor::HyperV));
        assert_eq!(tsc_rate(&cpu, no_kvmclock), None, "{access} {available}");
        assert_eq!(apic_timer_count(&cpu, TIMER_FREQ), None);
        assert!(cpu.read.borrow().is_empty(), "no frequency MSR is read");
    }
}

#[test]
fn a_frequency_msr_that_reads_zero_gives_no_rate() {
    let cpu = azure(hyperv_features(true, true, true))
        .msr(TSC_FREQUENCY, 0)
        .msr(APIC_FREQUENCY, 0);
    assert_eq!(tsc_rate(&cpu, no_kvmclock), None);
    assert_eq!(apic_timer_count(&cpu, TIMER_FREQ), None);
}

#[test]
fn an_apic_timer_count_that_does_not_fit_its_register_gives_no_rate() {
    // Below 64 kHz the count for a millisecond is zero; above 274 THz it is more than 32
    // bits. Neither is a clock, and both leave the calibration as before.
    let slow = azure(hyperv_features(true, true, true)).msr(APIC_FREQUENCY, 63_999);
    assert_eq!(apic_timer_count(&slow, TIMER_FREQ), None);
    let edge = azure(hyperv_features(true, true, true)).msr(APIC_FREQUENCY, 64_000);
    assert_eq!(apic_timer_count(&edge, TIMER_FREQ), Some(1));
    let fast = azure(hyperv_features(true, true, true)).msr(APIC_FREQUENCY, u64::MAX);
    assert_eq!(apic_timer_count(&fast, TIMER_FREQ), None);
    let largest = (u64::from(u32::MAX) + 1) * 64 * 1000 - 1;
    let top = azure(hyperv_features(true, true, true)).msr(APIC_FREQUENCY, largest);
    assert_eq!(apic_timer_count(&top, TIMER_FREQ), Some(u32::MAX));
}

#[test]
fn leaf_0x15_comes_before_hyper_vs_msr() {
    // The system design's order: kvmclock, leaf 0x15, then Hyper-V's MSR.
    let cpu = azure(hyperv_features(true, true, true)).leaf(0x15, [2, 200, 25_000_000, 0]);
    assert_eq!(
        tsc_rate(&cpu, no_kvmclock),
        Some((2_500_000_000, TscSource::Crystal))
    );
    assert!(cpu.read.borrow().is_empty());
    // And kvmclock comes before both, whatever else the machine says.
    assert_eq!(
        tsc_rate(&cpu, kvmclock(1_000_000_000)),
        Some((1_000_000_000, TscSource::Kvmclock))
    );
}

#[test]
fn bare_metal_has_no_hypervisor_and_takes_the_rate_from_leaf_0x15() {
    let cpu = bare_metal_with_crystal();
    assert_eq!(hypervisor(&cpu), None);
    assert_eq!(
        tsc_rate(&cpu, no_kvmclock),
        Some((2_592_000_000, TscSource::Crystal))
    );
    assert_eq!(apic_timer_count(&cpu, TIMER_FREQ), None);
    assert!(!hypercall_msrs(&cpu));
}

#[test]
fn leaf_0x15_without_the_crystal_or_a_ratio_gives_nothing() {
    for registers in [
        [2, 216, 0, 0],
        [0, 216, 24_000_000, 0],
        [2, 0, 24_000_000, 0],
    ] {
        let cpu = bare_metal_with_crystal().leaf(0x15, registers);
        assert_eq!(tsc_rate(&cpu, no_kvmclock), None, "{registers:?}");
    }
}

#[test]
fn a_machine_with_nothing_at_all_leaves_the_pit() {
    let cpu = nothing_at_all();
    assert_eq!(hypervisor(&cpu), None);
    assert_eq!(tsc_rate(&cpu, no_kvmclock), None);
    assert_eq!(apic_timer_count(&cpu, TIMER_FREQ), None);
    assert!(!hypercall_msrs(&cpu));
    assert!(cpu.read.borrow().is_empty());
    // And one that does not even answer leaf 1.
    assert_eq!(hypervisor(&Recorded::default()), None);
}

#[test]
fn another_hypervisor_is_present_and_unnamed() {
    // QEMU's emulator, as CI's boots and this Mac's run.
    let tcg = nothing_at_all().leaf(1, leaf_1(true)).leaf(
        0x4000_0000,
        [0x4000_0001, word(b"TCGT"), word(b"CGTC"), word(b"GTCG")],
    );
    assert_eq!(hypervisor(&tcg), Some(Hypervisor::Other));
    assert_eq!(Hypervisor::Other.line(), None);
    assert_eq!(tsc_rate(&tcg, no_kvmclock), None);
    // A hypervisor bit with no hypervisor leaves at all.
    let bare = nothing_at_all().leaf(1, leaf_1(true));
    assert_eq!(hypervisor(&bare), Some(Hypervisor::Other));
}

#[test]
fn hv1_needs_the_leaves_microsofts_interface_promises() {
    // The TLFS asks for leaves up to 0x40000005 with `Hv#1`; fewer is not its interface.
    let short = azure(hyperv_features(true, true, true)).leaf(
        0x4000_0000,
        [0x4000_0004, word(b"Micr"), word(b"osof"), word(b"t Hv")],
    );
    assert_eq!(hypervisor(&short), Some(Hypervisor::Other));
    assert_eq!(tsc_rate(&short, no_kvmclock), None);
    assert!(!hypercall_msrs(&short));
    // The vendor name alone is not the interface.
    let other =
        azure(hyperv_features(true, true, true)).leaf(0x4000_0001, [word(b"Hv#2"), 0, 0, 0]);
    assert_eq!(hypervisor(&other), Some(Hypervisor::Other));
    // And without the hypervisor bit, the leaves are not looked at.
    let hidden = azure(hyperv_features(true, true, true)).leaf(1, leaf_1(false));
    assert_eq!(hypervisor(&hidden), None);
}

#[test]
fn kvm_offering_microsofts_interface_is_hyper_v_to_its_guest() {
    // QEMU's `hv-*` enlightenments: Microsoft's interface at 0x40000000, KVM's moved up.
    let cpu = kvm()
        .leaf(
            0x4000_0000,
            [0x4000_000a, word(b"Micr"), word(b"osof"), word(b"t Hv")],
        )
        .leaf(0x4000_0001, [word(b"Hv#1"), 0, 0, 0])
        .leaf(0x4000_0003, hyperv_features(true, true, true))
        .msr(TSC_FREQUENCY, 3_000_000_000);
    assert_eq!(hypervisor(&cpu), Some(Hypervisor::HyperV));
    assert_eq!(
        tsc_rate(&cpu, no_kvmclock),
        Some((3_000_000_000, TscSource::HyperV))
    );
}

#[test]
fn the_hypercall_msrs_need_their_privilege() {
    assert!(hypercall_msrs(&azure(hyperv_features(true, false, false))));
    assert!(!hypercall_msrs(&azure(hyperv_features(false, true, true))));
}

#[test]
fn the_lines_are_the_consoles() {
    assert_eq!(HYPERV_LINE, Line(HYPERV).to_string());
    assert_eq!(KVM_LINE, Line(KVM).to_string());
    assert_eq!(
        NO_PIT_FOR_TSC,
        Fatal {
            stage: CORE,
            reason: "no PIT to calibrate the TSC"
        }
        .to_string()
    );
    assert_eq!(
        NO_PIT_FOR_APIC_TIMER,
        Fatal {
            stage: CORE,
            reason: "no PIT to calibrate the APIC timer"
        }
        .to_string()
    );
}

#[test]
fn the_guest_os_id_is_an_open_source_os_of_undefined_type() {
    assert_eq!(GUEST_OS_ID, 0x8000_0000_0000_0000);
    // TLFS, "Guest OS Identity MSR for Open Source Operating Systems": bit 63 open source,
    // 62:56 the OS type (0, undefined), 55:48 the OS ID, 47:16 the version, 15:0 the build.
    assert_eq!(GUEST_OS_ID >> 63, 1);
    assert_eq!((GUEST_OS_ID >> 56) & 0x7f, 0);
    assert_eq!(GUEST_OS_ID & ((1 << 56) - 1), 0);
    // Not zero, which would leave the hypercall page disabled.
    assert_ne!(GUEST_OS_ID, 0);
    let id = hvdef::hypercall::HvGuestOsId::from(GUEST_OS_ID);
    let open = id.open_source().expect("an open source OS");
    assert_eq!(
        open.os_type(),
        hvdef::hypercall::HvGuestOsOpenSourceType::UNDEFINED.0
    );
}

#[test]
fn the_hypercall_msr_takes_the_page_and_the_enable_bit_and_keeps_its_reserved_bits() {
    // Off at boot: the page's frame number in bits 63:12, enable in bit 0.
    assert_eq!(hypercall_msr(0, 0x0012_3000), Some(0x0012_3001));
    assert_eq!(hypercall_msr(0, 0), Some(1));
    // Bits 11:2 are kept as read, and a page left enabled elsewhere is moved.
    assert_eq!(hypercall_msr(0x7fc, 0x0012_3000), Some(0x0012_37fd));
    assert_eq!(hypercall_msr(0xabcd_e001, 0x0004_5000), Some(0x0004_5001));
    // The highest page a 64-bit address holds.
    assert_eq!(
        hypercall_msr(0, 0xffff_ffff_ffff_f000),
        Some(0xffff_ffff_ffff_f001)
    );
    // A locked MSR cannot be moved, and a page address must be a page's.
    assert_eq!(hypercall_msr(0b10, 0x0012_3000), None);
    assert_eq!(hypercall_msr(0xabcd_e003, 0x0012_3000), None);
    assert_eq!(hypercall_msr(0, 0x0012_3008), None);
}

#[test]
fn a_pit_is_there_when_its_count_moves() {
    // Channel 0 counting down from 1193, read as fast as the ports answer.
    assert!(counts([1193, 1193, 1191, 1188]));
    assert!(counts([5, 4]));
    // The count wrapping to its reload is a move too.
    assert!(counts([0, 1193]));
}

#[test]
fn ports_with_nothing_behind_them_read_the_same_and_are_no_pit() {
    assert_eq!(READS, 1024);
    assert!(!counts(core::iter::repeat_n(0xffff, READS)));
    // A counter that never moves is no clock either.
    assert!(!counts(core::iter::repeat_n(0x1234, READS)));
    assert!(!counts([7]));
    assert!(!counts([]));
}

#[test]
fn the_reads_stop_at_the_first_move() {
    let reads = RefCell::new(0);
    let counted = [9u16, 9, 8, 7, 6]
        .into_iter()
        .inspect(|_| *reads.borrow_mut() += 1);
    assert!(counts(counted));
    assert_eq!(*reads.borrow(), 3);
}
