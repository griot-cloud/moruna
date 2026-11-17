//! Tests of `cpu/hypervisor.rs` and `timer/pit/presence.rs` (work item 1.8): the
//! hypervisor found and the clock rates taken, over CPUID and MSR answers recorded for
//! each kind of machine, and the PIT's presence over counts read from its ports; and the
//! SynIC's values and the two hypercalls of VMBus (work item 1.9c).

use std::cell::RefCell;
use std::collections::BTreeMap;

use moruna_abi::boot_line::{CORE, Fatal, HYPERV, Hypervisor as Line, KVM, OTHER};

use super::hypervisor::{
    APIC_TIMER_DIVIDE, Cpu, EVENT_FLAGS, EVENT_FLAGS_SIZE, GUEST_OS_ID, HYPERV_LINE, Hypervisor,
    KVM_LINE, MESSAGE_FLAGS, MESSAGE_SLOT, MESSAGE_TYPE, OTHER_LINE, POST_MESSAGE,
    POST_MESSAGE_SIZE, SIGNAL_EVENT, TscSource, VMBUS_SINT, VMBUS_SINT_MSR, apic_timer_count,
    hypercall_msr, hypercall_msrs, hypercall_status, hypervisor, message_pending, message_present,
    post_message_input, scontrol_msr, signal_event_input, sint_msr, synic_msrs, synic_page_msr,
    tsc_rate,
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
    assert_eq!(Hypervisor::Kvm.line(), "moruna: hypervisor kvm");
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
    assert_eq!(Hypervisor::HyperV.line(), "moruna: hypervisor hyperv");
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
fn another_hypervisor_is_present_and_called_other() {
    // QEMU's emulator, as CI's boots and this Mac's run.
    let tcg = nothing_at_all().leaf(1, leaf_1(true)).leaf(
        0x4000_0000,
        [0x4000_0001, word(b"TCGT"), word(b"CGTC"), word(b"GTCG")],
    );
    assert_eq!(hypervisor(&tcg), Some(Hypervisor::Other));
    assert_eq!(Hypervisor::Other.line(), "moruna: hypervisor other");
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
    assert_eq!(OTHER_LINE, Line(OTHER).to_string());
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

// Work item 1.9c: the SynIC on CPU 0 and the two hypercalls of VMBus.

#[test]
fn the_synic_needs_hyper_v_and_its_four_privileges() {
    let cpu = azure(hyperv_features(true, true, true));
    assert!(synic_msrs(&cpu));
    assert!(cpu.read.borrow().is_empty(), "no MSR is read to decide");
    // Each privilege taken away in turn: AccessHypercallMsrs (EAX bit 5), AccessSynicRegs
    // (EAX bit 2), PostMessages and SignalEvents (EBX bits 4 and 5).
    for (register, bit) in [(0, 5), (0, 2), (1, 4), (1, 5)] {
        let mut features = hyperv_features(true, true, true);
        features[register] &= !(1 << bit);
        assert!(!synic_msrs(&azure(features)), "{register} {bit}");
    }
    assert!(!synic_msrs(&kvm()));
}

#[test]
fn the_synic_pages_take_their_frame_and_the_enable_bit_and_keep_reserved_bits() {
    assert_eq!(synic_page_msr(0, 0x0012_3000), Some(0x0012_3001));
    // Bits 11:1 are kept as read; a page firmware left enabled is moved.
    assert_eq!(synic_page_msr(0xffe, 0x0012_3000), Some(0x0012_3fff));
    assert_eq!(synic_page_msr(0xabcd_e001, 0x0004_5000), Some(0x0004_5001));
    assert_eq!(
        synic_page_msr(0, 0xffff_ffff_ffff_f000),
        Some(0xffff_ffff_ffff_f001)
    );
    assert_eq!(synic_page_msr(0, 0x0012_3008), None);
}

#[test]
fn sint_2_is_unmasked_at_its_vector_with_the_apics_eoi() {
    assert_eq!(VMBUS_SINT, 2);
    assert_eq!(VMBUS_SINT_MSR, 0x4000_0092);
    // Masked, as the SINTs are at reset.
    assert_eq!(sint_msr(1 << 16, 0x30), 0x30);
    // Auto-EOI and polling cleared; the reserved bits 15:8 and 63:21 and the proxy bit kept.
    let current = (1 << 63) | (1 << 20) | (1 << 18) | (1 << 17) | (1 << 16) | 0xab00 | 0x41;
    assert_eq!(sint_msr(current, 0xfe), (1 << 63) | (1 << 20) | 0xabfe);
}

#[test]
fn scontrol_enables_and_keeps_its_reserved_bits() {
    assert_eq!(scontrol_msr(0), 1);
    assert_eq!(scontrol_msr(0xf0), 0xf1);
    assert_eq!(scontrol_msr(1), 1);
}

#[test]
fn the_two_hypercalls_are_post_message_slow_and_signal_event_fast() {
    // Call codes 0x5c and 0x5d (TLFS appendix A); the fast bit is bit 16.
    assert_eq!(POST_MESSAGE, 0x5c);
    assert_eq!(SIGNAL_EVENT, 0x1_005d);
    // The connection ID in bits 31:0, the flag number in bits 47:32.
    assert_eq!(signal_event_input(0x0002_0001, 7), 0x0007_0002_0001);
    assert_eq!(signal_event_input(u32::MAX, u16::MAX), 0xffff_ffff_ffff);
    // A status in RAX's low 16 bits; the rest is the reps completed.
    assert_eq!(hypercall_status(0), 0);
    assert_eq!(hypercall_status(0x0000_0fff_0000_0013), 0x13);
    assert_eq!(u64::from(hypercall_status(0x13)), moruna_abi::hyperv::BUSY);
}

#[test]
fn a_posted_message_is_laid_out_as_hvcallpostmessage_takes_it() {
    let payload: Vec<u8> = (1..=240).collect();
    let input = post_message_input(0x0002_0001, 1, &payload).unwrap();
    assert_eq!(POST_MESSAGE_SIZE, 256);
    assert_eq!(input[0..4], 0x0002_0001u32.to_le_bytes());
    assert_eq!(input[4..8], [0; 4], "padding");
    assert_eq!(input[8..12], 1u32.to_le_bytes());
    assert_eq!(input[12..16], 240u32.to_le_bytes());
    assert_eq!(input[16..], payload[..]);
    let short = post_message_input(4, 1, &[9, 8, 7]).unwrap();
    assert_eq!(short[12..20], [3, 0, 0, 0, 9, 8, 7, 0]);
    assert!(short[20..].iter().all(|b| *b == 0));
    assert!(post_message_input(4, 1, &[]).is_some());
    // The rule of item 1.9c: at most 240 bytes.
    assert_eq!(post_message_input(4, 1, &[0; 241]), None);
    assert_eq!(
        moruna_abi::hyperv::PAYLOAD_MAX,
        POST_MESSAGE_SIZE - 16,
        "the ABI's limit is the hypercall's"
    );
}

#[test]
fn sint_2s_slot_and_flags_are_where_the_abi_says() {
    assert_eq!(u32::from(VMBUS_SINT), moruna_abi::hyperv::SINT);
    assert_eq!(MESSAGE_SLOT, 512);
    assert_eq!(MESSAGE_TYPE, 0);
    assert_eq!(MESSAGE_FLAGS, 5);
    assert_eq!(EVENT_FLAGS_SIZE, 256);
    assert_eq!(EVENT_FLAGS, moruna_abi::hyperv::EVENT_FLAGS_OFFSET);
    assert_eq!(
        EVENT_FLAGS_SIZE * 8,
        moruna_abi::hyperv::EVENT_FLAGS as usize
    );
    assert_eq!(moruna_abi::hyperv::MESSAGE_SIZE, 256);
    // An empty slot's type is zero; VMBus's messages are type 1.
    assert!(!message_present(0));
    assert!(message_present(1));
    assert!(message_present(0x8000_0000));
    // Only bit 0 of the flags is MessagePending.
    assert!(message_pending(1));
    assert!(message_pending(0xff));
    assert!(!message_pending(0));
    assert!(!message_pending(0xfe));
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
