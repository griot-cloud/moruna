//! The memory a plan's own operators hold, inside the run's budget (MH 4.5).
//!
//! A DataFusion plan's sorts, aggregations and joins hold their working state outside Moruna's
//! arena, in reservations against the session's `MemoryPool`; its scans and exchanges hold the
//! batches in flight between its operators, which no pool sees. A `PlanSource` runs its plan in
//! a session whose pool is a [`BudgetPool`]: the facade gives it a share of the run's budget
//! beside the arena, charges that share to the controller, and moves it when the machine's
//! limits move. The share is divided three ways:
//!
//! - a tenth is the pool the operators reserve from ([`BudgetPool::capacity`]);
//! - four tenths are held back for what the operators hold beyond what they reserve. DataFusion
//!   counts an aggregation's groups but not the copies it makes of them to sort and spill, nor
//!   the batches a spill merge reads back: measured on a grouped aggregation that spills (F8.9),
//!   the process held three to five times the pool above its arena and its batches in flight,
//!   which is where this figure comes from ([`BudgetPool::unaccounted`]);
//! - half is what the plan's batches in flight may hold, and the scans that decode them
//!   ([`BudgetPool::in_flight`]); `PlanSource` sizes the session's batches and partitions to it.
//!
//! An operator that can spill and finds the pool full spills to [`PlanMemory::spill_dir`], which
//! is the run's staging directory; one that cannot is refused with DataFusion's
//! `ResourcesExhausted` rather than taking the process past its ceiling.
//!
//! The pool divides itself as DataFusion's `FairSpillPool` does: consumers that cannot spill take
//! what they ask for while it lasts, and the rest is shared equally among the consumers that can.
//! Unlike that pool its limit moves. A lowered limit refuses growth at once; what is already
//! reserved is given back as the operators holding it spill or finish, so a shrink is eventual,
//! as the arena's is.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use datafusion::error::{DataFusionError, Result};
use datafusion::execution::memory_pool::{
    MemoryConsumer, MemoryLimit, MemoryPool, MemoryReservation,
};

/// A DataFusion memory pool over a share of the run's budget that the run sets and moves.
#[derive(Debug)]
pub struct BudgetPool {
    share: AtomicU64,
    refusals: AtomicU64,
    state: Mutex<Reserved>,
}

#[derive(Debug, Default)]
struct Reserved {
    /// Consumers registered that can spill.
    spillers: usize,
    /// Bytes reserved by consumers that can spill.
    spillable: usize,
    /// Bytes reserved by consumers that cannot.
    unspillable: usize,
    /// The most ever reserved at once.
    peak: usize,
}

impl BudgetPool {
    /// A pool over a share of `share` bytes.
    pub fn new(share: u64) -> BudgetPool {
        BudgetPool {
            share: AtomicU64::new(share),
            refusals: AtomicU64::new(0),
            state: Mutex::new(Reserved::default()),
        }
    }

    /// Move the share. Growth past the new capacity is refused from now on; what is reserved
    /// above it is given back as its holders spill or finish. The batches in flight are sized
    /// when the plan starts and keep that size.
    pub fn set_limit(&self, bytes: u64) {
        self.share.store(bytes, Ordering::SeqCst);
    }

    /// The share in force: the pool, what the operators hold beyond it, and the batches in
    /// flight together.
    pub fn limit(&self) -> u64 {
        self.share.load(Ordering::SeqCst)
    }

    /// What the operators may reserve: a tenth of the share.
    pub fn capacity(&self) -> u64 {
        self.limit() / 10
    }

    /// What the operators are expected to hold beyond their reservations, four times the pool.
    pub fn unaccounted(&self) -> u64 {
        self.capacity() * 4
    }

    /// What the plan's batches in flight and its scans may hold: the rest, half the share.
    pub fn in_flight(&self) -> u64 {
        self.limit() - self.capacity() - self.unaccounted()
    }

    /// Requests the pool refused: each one an operator that spilled, or failed for want of
    /// memory when it could not.
    pub fn refusals(&self) -> u64 {
        self.refusals.load(Ordering::SeqCst)
    }

    /// The most the plan's operators have held at once.
    pub fn peak(&self) -> u64 {
        self.held().peak as u64
    }

    fn held(&self) -> std::sync::MutexGuard<'_, Reserved> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn reservable(&self) -> usize {
        usize::try_from(self.capacity()).unwrap_or(usize::MAX)
    }
}

impl Reserved {
    fn add(&mut self, can_spill: bool, bytes: usize) {
        if can_spill {
            self.spillable += bytes;
        } else {
            self.unspillable += bytes;
        }
        self.peak = self.peak.max(self.spillable + self.unspillable);
    }
}

fn exhausted(
    reservation: &MemoryReservation,
    additional: usize,
    available: usize,
) -> DataFusionError {
    DataFusionError::ResourcesExhausted(format!(
        "the run's budget for its plan: {} asked for {additional} more bytes with {} held, and \
         {available} are available to it",
        reservation.consumer().name(),
        reservation.size()
    ))
}

impl MemoryPool for BudgetPool {
    fn name(&self) -> &str {
        "moruna-budget"
    }

    fn register(&self, consumer: &MemoryConsumer) {
        if consumer.can_spill() {
            self.held().spillers += 1;
        }
    }

    fn unregister(&self, consumer: &MemoryConsumer) {
        if consumer.can_spill() {
            let mut state = self.held();
            state.spillers = state.spillers.saturating_sub(1);
        }
    }

    fn grow(&self, reservation: &MemoryReservation, additional: usize) {
        self.held()
            .add(reservation.consumer().can_spill(), additional);
    }

    fn shrink(&self, reservation: &MemoryReservation, shrink: usize) {
        let mut state = self.held();
        if reservation.consumer().can_spill() {
            state.spillable = state.spillable.saturating_sub(shrink);
        } else {
            state.unspillable = state.unspillable.saturating_sub(shrink);
        }
    }

    fn try_grow(&self, reservation: &MemoryReservation, additional: usize) -> Result<()> {
        let capacity = self.reservable();
        let mut state = self.held();
        let can_spill = reservation.consumer().can_spill();
        let available = if can_spill {
            // Each spiller may hold its equal share of what the others cannot give back.
            let shared = capacity.saturating_sub(state.unspillable);
            let each = shared / state.spillers.max(1);
            each.saturating_sub(reservation.size())
                .min(shared.saturating_sub(state.spillable))
        } else {
            capacity.saturating_sub(state.unspillable + state.spillable)
        };
        if additional > available {
            self.refusals.fetch_add(1, Ordering::SeqCst);
            return Err(exhausted(reservation, additional, available));
        }
        state.add(can_spill, additional);
        Ok(())
    }

    fn reserved(&self) -> usize {
        let state = self.held();
        state.spillable + state.unspillable
    }

    fn memory_limit(&self) -> MemoryLimit {
        MemoryLimit::Finite(self.reservable())
    }
}

impl std::fmt::Display for BudgetPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}(capacity: {} bytes)", self.name(), self.capacity())
    }
}

/// Where a plan's operators may hold memory and spill: the pool the run sizes, and the directory
/// a spilling operator writes to (`None`: nowhere, so an operator that must spill is refused).
#[derive(Clone, Debug)]
pub struct PlanMemory {
    /// The pool every operator of the plan reserves from.
    pub pool: Arc<BudgetPool>,
    /// The run's staging directory.
    pub spill_dir: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reservation(pool: &Arc<BudgetPool>, name: &str, can_spill: bool) -> MemoryReservation {
        let dynamic: Arc<dyn MemoryPool> = Arc::clone(pool) as Arc<dyn MemoryPool>;
        MemoryConsumer::new(name)
            .with_can_spill(can_spill)
            .register(&dynamic)
    }

    #[test]
    fn spillers_share_what_the_others_leave_and_the_share_moves() {
        let pool = Arc::new(BudgetPool::new(10000));
        let fixed = reservation(&pool, "fixed", false);
        fixed.try_grow(200).expect("room");
        let a = reservation(&pool, "a", true);
        let b = reservation(&pool, "b", true);
        // 800 shared by two spillers: 400 each.
        a.try_grow(400).expect("a's share");
        let refused = a.try_grow(1).expect_err("past a's share");
        assert_eq!(pool.refusals(), 1);
        assert!(
            refused.to_string().contains("budget for its plan"),
            "{refused}"
        );
        b.try_grow(400).expect("b's share");
        assert_eq!(pool.reserved(), 1000);
        assert!(fixed.try_grow(1).is_err(), "the pool is full");
        assert_eq!(pool.peak(), 1000);

        // Lowered: nothing grows until the holders give back.
        pool.set_limit(5000);
        assert!(matches!(pool.memory_limit(), MemoryLimit::Finite(500)));
        a.shrink(400);
        assert!(b.try_grow(1).is_err(), "still above the new limit");
        b.shrink(400);
        b.try_grow(100).expect("under the new limit");
        // Raised: room again.
        pool.set_limit(20000);
        fixed
            .try_grow(1000)
            .expect("unspillable takes what is free");
        a.grow(5);
        assert_eq!(pool.reserved(), 200 + 100 + 1000 + 5);
        drop(a);
        drop(b);
        drop(fixed);
        assert_eq!(pool.reserved(), 0);
        assert_eq!(pool.limit(), 20000);
        assert_eq!(
            (pool.capacity(), pool.unaccounted(), pool.in_flight()),
            (2000, 8000, 10000)
        );
        assert_eq!(pool.name(), "moruna-budget");
        assert!(pool.to_string().contains("2000"));
    }
}
