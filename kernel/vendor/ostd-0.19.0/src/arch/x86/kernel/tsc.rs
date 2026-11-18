// SPDX-License-Identifier: MPL-2.0

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::{
    arch::{
        timer::pit::{self, OperatingMode},
        trap::TrapFrame,
    },
    info,
    irq::IrqLine,
    timer::TIMER_FREQ,
};

// moruna: the deadline of the boot waits (Moruna's system design, section 1B); this wait,
// the APIC timer's (`timer/apic.rs`) and the wait for the other CPUs to start
// (`boot/smp.rs`) use it, and the power-off after it.
pub(crate) mod boot_wait;

/// The frequency in Hz of the Time Stamp Counter (TSC).
pub(in crate::arch) static TSC_FREQ: AtomicU64 = AtomicU64::new(0);

pub(in crate::arch) fn init_tsc_freq() {
    use super::kvm_clock::determine_tsc_freq as determine_tsc_freq_via_kvmclock;

    // moruna: kvmclock, then CPUID leaf 0x15, then Hyper-V's `HV_X64_MSR_TSC_FREQUENCY`,
    // and the PIT last (Moruna's system design, section 1F, "Clock rates"; work item 1.8).
    // Upstream tried kvmclock, CPUID leaf 0x15 (`cpuid::query_tsc_freq`, now in
    // `cpu/hypervisor.rs`) and the PIT.
    let tsc_freq = match crate::arch::cpu::hypervisor::tsc_rate(
        &super::hyperv::ThisCpu,
        determine_tsc_freq_via_kvmclock,
    ) {
        Some((tsc_freq, source)) => {
            info!("TSC frequency from {:?}", source);
            tsc_freq
        }
        None => determine_tsc_freq_via_pit(),
    };
    TSC_FREQ.store(tsc_freq, Ordering::Relaxed);
    info!("TSC frequency: {:?} Hz", tsc_freq);
}

/// Determines the TSC frequency with the help of the Programmable Interval Timer (PIT).
///
/// When the TSC frequency is not enumerated in the results of the CPUID instruction, it can
/// leverage the PIT to calculate the TSC frequency.
pub(super) fn determine_tsc_freq_via_pit() -> u64 {
    // moruna: the PIT is looked for before its interrupts are waited for (Moruna's system
    // design, section 1F; work item 1.8). A machine with no PIT says so and powers off at
    // once, where it would otherwise wait out the deadline below.
    if !pit::is_present() {
        crate::early_println!("{}", pit::presence::NO_PIT_FOR_TSC);
        crate::power::poweroff(crate::power::ExitCode::Failure);
    }

    // Allocate IRQ
    let mut irq = IrqLine::alloc().unwrap();
    irq.on_active(pit_callback);

    // Enable PIT
    pit::init(OperatingMode::RateGenerator);
    let irq = pit::enable_interrupt(irq);

    static IS_FINISH: AtomicBool = AtomicBool::new(false);
    static FREQUENCY: AtomicU64 = AtomicU64::new(0);

    // Wait until `FREQUENCY` is ready
    //
    // moruna: the wait has a deadline (Moruna's system design, section 1B). Upstream halted
    // until the next interrupt, which never comes on a machine with no PIT; a halted CPU
    // could not look at a deadline, so the wait pauses with local IRQs enabled instead. It
    // ends, as upstream's did, with local IRQs disabled.
    let measured = boot_wait::wait(
        crate::arch::read_tsc,
        || {
            crate::arch::irq::enable_local();
            core::hint::spin_loop();
            crate::arch::irq::disable_local();
        },
        || IS_FINISH.load(Ordering::Acquire),
    );
    if !measured {
        crate::early_println!("{}", boot_wait::PIT_TIMED_OUT);
        crate::power::poweroff(crate::power::ExitCode::Failure);
    }

    // Disable PIT
    drop(irq);

    return FREQUENCY.load(Ordering::Acquire);

    fn pit_callback(_trap_frame: &TrapFrame) {
        static IN_TIME: AtomicU64 = AtomicU64::new(0);
        static TSC_FIRST_COUNT: AtomicU64 = AtomicU64::new(0);
        // Set a certain times of callbacks to calculate the frequency
        const CALLBACK_TIMES: u64 = TIMER_FREQ / 10;

        let tsc_current_count = crate::arch::read_tsc();

        if IN_TIME.load(Ordering::Relaxed) < CALLBACK_TIMES || IS_FINISH.load(Ordering::Acquire) {
            if IN_TIME.load(Ordering::Relaxed) == 0 {
                TSC_FIRST_COUNT.store(tsc_current_count, Ordering::Relaxed);
            }
            IN_TIME.fetch_add(1, Ordering::Relaxed);
            return;
        }

        let tsc_first_count = TSC_FIRST_COUNT.load(Ordering::Relaxed);
        let freq = (tsc_current_count - tsc_first_count) * (TIMER_FREQ / CALLBACK_TIMES);
        FREQUENCY.store(freq, Ordering::Release);
        IS_FINISH.store(true, Ordering::Release);
    }
}
