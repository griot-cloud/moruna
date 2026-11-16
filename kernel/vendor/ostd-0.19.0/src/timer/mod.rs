// SPDX-License-Identifier: MPL-2.0

//! The timer support.

mod jiffies;

use alloc::{boxed::Box, vec::Vec};
use core::{
    cell::RefCell,
    sync::atomic::{AtomicU32, Ordering},
};

pub use jiffies::Jiffies;

use crate::{
    arch::trap::TrapFrame,
    cpu::{CpuId, PinCurrentCpu},
    cpu_local, irq,
};

/// The timer frequency in Hz.
///
/// Here we choose 1000Hz since 1000Hz is easier for unit conversion and convenient for timer.
/// What's more, the frequency cannot be set too high or too low, 1000Hz is a modest choice.
///
/// For system performance reasons, this rate cannot be set too high, otherwise most of the time is
/// spent in executing timer code.
pub const TIMER_FREQ: u64 = 1000;

type InterruptCallback = Box<dyn Fn() + Sync + Send>;

cpu_local! {
    static INTERRUPT_CALLBACKS: RefCell<Vec<InterruptCallback>> = RefCell::new(Vec::new());
    /// How many timer periods (`1 / TIMER_FREQ`) this CPU waits between two ticks.
    static TICK_STRETCH: AtomicU32 = AtomicU32::new(1);
}

/// Makes this CPU tick once every `periods` timer periods (`1 / TIMER_FREQ` each), at least
/// one: a CPU that runs one thread, or none, needs no tick a millisecond (Moruna). A smaller
/// stretch than before re-arms the timer at once, so the CPU ticks within the new stretch;
/// a larger one takes effect from the next tick.
pub fn set_tick_stretch(periods: u32) {
    let periods = periods.max(1);
    let irq_guard = irq::disable_local();
    let old = TICK_STRETCH
        .get_with(&irq_guard)
        .swap(periods, Ordering::Relaxed);
    if periods < old {
        crate::arch::timer::rearm(u64::from(periods));
    }
}

/// This CPU's tick stretch (see [`set_tick_stretch`]).
pub(crate) fn tick_stretch() -> u64 {
    let irq_guard = irq::disable_local();
    u64::from(TICK_STRETCH.get_with(&irq_guard).load(Ordering::Relaxed))
}

/// Registers a function that will be executed during the timer interrupt on the current CPU.
pub fn register_callback_on_cpu<F>(func: F)
where
    F: Fn() + Sync + Send + 'static,
{
    let irq_guard = irq::disable_local();
    INTERRUPT_CALLBACKS
        .get_with(&irq_guard)
        .borrow_mut()
        .push(Box::new(func));
}

pub(crate) fn call_timer_callback_functions(_: &TrapFrame) {
    let irq_guard = irq::disable_local();

    if irq_guard.current_cpu() == CpuId::bsp() {
        jiffies::ELAPSED.fetch_add(1, Ordering::Relaxed);
    }

    let callbacks_guard = INTERRUPT_CALLBACKS.get_with(&irq_guard);
    for callback in callbacks_guard.borrow().iter() {
        (callback)();
    }
    drop(callbacks_guard);
}
