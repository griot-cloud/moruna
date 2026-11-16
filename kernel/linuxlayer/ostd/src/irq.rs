// SPDX-License-Identifier: MPL-2.0

//! Interrupts: a user process takes none. An IRQ-disabled guard is an atomic-mode
//! guard (`task::disable_preempt`), and every thread is in task context.

use crate::{
    sync::{GuardTransfer, reader_enter, reader_exit},
    task::atomic_mode::InAtomicMode,
};

/// Enters atomic mode, as disabling local IRQs would.
pub fn disable_local() -> DisabledLocalIrqGuard {
    DisabledLocalIrqGuard {
        parity: reader_enter(),
    }
}

/// A guard for a section with local IRQs disabled: an RCU read-side section here.
#[clippy::has_significant_drop]
#[must_use]
#[derive(Debug)]
pub struct DisabledLocalIrqGuard {
    parity: usize,
}

impl !Send for DisabledLocalIrqGuard {}

impl !Sync for DisabledLocalIrqGuard {}

// SAFETY: the guard keeps an RCU read-side section open while it lives.
unsafe impl InAtomicMode for DisabledLocalIrqGuard {}

impl GuardTransfer for DisabledLocalIrqGuard {
    fn transfer_to(&mut self) -> Self {
        disable_local()
    }
}

impl Drop for DisabledLocalIrqGuard {
    fn drop(&mut self) {
        reader_exit(self.parity);
    }
}

/// The level of interrupt the CPU runs at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterruptLevel {
    /// Task context.
    L0,
    /// An interrupt handler, entered from this privilege level.
    L1(crate::cpu::PrivilegeLevel),
    /// A nested interrupt.
    L2,
}

impl InterruptLevel {
    /// The current level: always task context in user space.
    pub fn current() -> Self {
        Self::L0
    }

    /// Whether this is task context.
    pub fn is_task_context(&self) -> bool {
        *self == Self::L0
    }

    /// Whether this is an interrupt context.
    pub fn is_interrupt_context(&self) -> bool {
        !self.is_task_context()
    }
}

/// The interrupt number bottom halves see for the ticker's tick.
pub const TIMER_IRQ: u8 = 0;

type BottomHalfL1 = fn(DisabledLocalIrqGuard, u8) -> DisabledLocalIrqGuard;

static BOTTOM_HALF_HANDLER_L1: spin::Once<BottomHalfL1> = spin::Once::new();
static BOTTOM_HALF_HANDLER_L2: spin::Once<fn(u8)> = spin::Once::new();

/// Registers the bottom half run after each tick, as after an interrupt.
pub fn register_bottom_half_handler_l1(func: BottomHalfL1) {
    BOTTOM_HALF_HANDLER_L1.call_once(|| func);
}

/// Registers the bottom half of a nested interrupt: never run in user space.
pub fn register_bottom_half_handler_l2(func: fn(u8)) {
    BOTTOM_HALF_HANDLER_L2.call_once(|| func);
}

/// Runs the bottom half for interrupt `irq_num`.
pub(crate) fn process_bottom_half(irq_num: u8) {
    if let Some(handler) = BOTTOM_HALF_HANDLER_L1.get() {
        drop(handler(disable_local(), irq_num));
    }
}
