//! The busy slots: how many workers may be running a task, and how many are (SC-I7, G-I5).
//!
//! The workers that take tasks are `min(workers.active, cpu_limit)` (MH 4.4). The knob, the
//! limit and the busy count share one atomic word so that a worker's claim on a busy slot is
//! checked against the bound in force at the instant it is made: a claim is a compare-and-swap
//! that succeeds only while the worker's index and the busy count are both below the bound, and
//! a lowering of either the knob or the limit is a write to the same word. A check made at the
//! loop head and acted on after the pick could not promise that: a lowering landing in between
//! let a worker outside the new bound start a task after the others had parked, and let a worker
//! inside it start one while a task from before the lowering still ran (found by
//! `cpu_limit_bounds_active`, 2026-09-30). A task running when the bound is lowered keeps its
//! slot until it ends (parking is between tasks), and until then its slot counts against the
//! new bound, so the busy count only falls towards it and never climbs back over it.

use std::sync::atomic::{AtomicU64, Ordering};

const FIELD: u32 = 16;
const MASK: u64 = 0xffff;
const BUSY: u32 = 0;
const KNOB: u32 = FIELD;
const CPU: u32 = 2 * FIELD;

fn field(word: u64, at: u32) -> u16 {
    ((word >> at) & MASK) as u16
}

fn with_field(word: u64, at: u32, value: u16) -> u64 {
    (word & !(MASK << at)) | (u64::from(value) << at)
}

/// The bound a word encodes: the knob, bounded by the CPU limit, and never below one worker.
fn bound(word: u64) -> u16 {
    field(word, KNOB).min(field(word, CPU)).max(1)
}

/// `workers.active`, the CPU limit as a worker count, and the busy count, in one word.
pub(crate) struct WorkerSlots {
    word: AtomicU64,
}

impl WorkerSlots {
    pub(crate) fn new(knob: u16, cpu_bound: u16) -> WorkerSlots {
        let word = with_field(with_field(0, KNOB, knob), CPU, cpu_bound);
        WorkerSlots {
            word: AtomicU64::new(word),
        }
    }

    /// The workers that may take a task: `min(workers.active, cpu_limit)`, at least one.
    pub(crate) fn active(&self) -> u16 {
        bound(self.word.load(Ordering::SeqCst))
    }

    /// The CPU limit as a worker count.
    pub(crate) fn cpu_bound(&self) -> u16 {
        field(self.word.load(Ordering::SeqCst), CPU)
    }

    /// The workers holding a busy slot now.
    pub(crate) fn busy(&self) -> u16 {
        field(self.word.load(Ordering::SeqCst), BUSY)
    }

    /// Store the `workers.active` knob. Every claim after this returns sees it.
    pub(crate) fn set_knob(&self, workers: u16) {
        self.store(KNOB, workers);
    }

    /// Store the CPU limit. Every claim after this returns sees it.
    pub(crate) fn set_cpu_bound(&self, workers: u16) {
        self.store(CPU, workers);
    }

    fn store(&self, at: u32, value: u16) {
        let mut word = self.word.load(Ordering::SeqCst);
        loop {
            match self.word.compare_exchange_weak(
                word,
                with_field(word, at, value),
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return,
                Err(now) => word = now,
            }
        }
    }

    /// Claim a busy slot for `worker`, or `None` when the worker is outside the bound or the
    /// bound's slots are all held. The slot is released when the returned guard drops.
    pub(crate) fn try_claim(&self, worker: u16) -> Option<BusySlot<'_>> {
        let mut word = self.word.load(Ordering::SeqCst);
        loop {
            let bound = bound(word);
            if worker >= bound || field(word, BUSY) >= bound {
                return None;
            }
            match self.word.compare_exchange_weak(
                word,
                word + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return Some(BusySlot { slots: self }),
                Err(now) => word = now,
            }
        }
    }

    /// Claim a busy slot whatever the bound: the probe's, which runs alone on the worker it was
    /// given once every other worker's task has ended (f.9).
    pub(crate) fn claim(&self) -> BusySlot<'_> {
        self.word.fetch_add(1, Ordering::SeqCst);
        BusySlot { slots: self }
    }
}

/// A held busy slot; dropping it frees the slot.
pub(crate) struct BusySlot<'a> {
    slots: &'a WorkerSlots,
}

impl Drop for BusySlot<'_> {
    fn drop(&mut self) {
        self.slots.word.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod unit {
    use super::WorkerSlots;

    #[test]
    fn a_claim_is_bounded_by_index_and_count() {
        let slots = WorkerSlots::new(4, 16);
        assert_eq!(slots.active(), 4);
        let held: Vec<_> = (0..4).filter_map(|w| slots.try_claim(w)).collect();
        assert_eq!(held.len(), 4);
        assert_eq!(slots.busy(), 4);
        assert!(
            slots.try_claim(4).is_none(),
            "worker 4 is outside the bound"
        );
        drop(held);
        assert_eq!(slots.busy(), 0);

        // Lowered with two slots held: they stay held, and no claim succeeds until the busy
        // count is under the new bound, even for a worker inside it.
        let first = slots.try_claim(0);
        let second = slots.try_claim(3);
        assert!(first.is_some() && second.is_some());
        slots.set_cpu_bound(1);
        assert_eq!((slots.active(), slots.cpu_bound(), slots.busy()), (1, 1, 2));
        assert!(slots.try_claim(0).is_none(), "two held, bound one");
        drop(second);
        assert!(slots.try_claim(0).is_none(), "one held, bound one");
        drop(first);
        assert!(
            slots.try_claim(3).is_none(),
            "worker 3 is outside the bound"
        );
        let again = slots.try_claim(0);
        assert!(again.is_some());
        drop(again);

        // The knob and the limit are separate fields; the bound is their minimum, at least one.
        slots.set_cpu_bound(8);
        slots.set_knob(2);
        assert_eq!(slots.active(), 2);
        slots.set_knob(0);
        assert_eq!(slots.active(), 1);

        // The probe's claim ignores the bound and is released like any other.
        let probe = slots.claim();
        assert_eq!(slots.busy(), 1);
        drop(probe);
        assert_eq!(slots.busy(), 0);
    }
}
