// SPDX-License-Identifier: MPL-2.0

//! Atomic mode in user space.
//!
//! A user process cannot turn preemption off. An atomic-mode guard here marks an RCU
//! read-side section instead (`sync::rcu`), which is what OSTD's callers rely on it for.

use crate::{
    sync::{GuardTransfer, reader_enter, reader_exit},
    task::atomic_mode::InAtomicMode,
};

/// A guard for an atomic-mode section: an RCU read-side section here.
#[clippy::has_significant_drop]
#[must_use]
#[derive(Debug)]
pub struct DisabledPreemptGuard {
    parity: usize,
}

impl !Send for DisabledPreemptGuard {}

impl !Sync for DisabledPreemptGuard {}

// SAFETY: the guard keeps an RCU read-side section open while it lives.
unsafe impl InAtomicMode for DisabledPreemptGuard {}

impl DisabledPreemptGuard {
    fn new() -> Self {
        Self {
            parity: reader_enter(),
        }
    }
}

impl GuardTransfer for DisabledPreemptGuard {
    fn transfer_to(&mut self) -> Self {
        disable_preempt()
    }
}

impl Drop for DisabledPreemptGuard {
    fn drop(&mut self) {
        reader_exit(self.parity);
    }
}

/// Enters atomic mode.
pub fn disable_preempt() -> DisabledPreemptGuard {
    DisabledPreemptGuard::new()
}

/// Halts the CPU until something happens: in user space, sleeps for a tick, as an idle
/// CPU wakes at the next interrupt.
pub fn halt_cpu() {
    let word = core::sync::atomic::AtomicU32::new(0);
    let tick = 1_000_000_000 / crate::timer::TIMER_FREQ;
    crate::backend::futex_wait(&word, 0, crate::backend::now() + tick);
}
