//! `PlacementStats`, `QueueStats` and the engine's own `PlacementDetail` (section j).

use std::sync::atomic::Ordering;

use amoru_kernel::{PlacementStats, QueueStats, Seq, StageId, TIER_COUNT, Tier};

use crate::PlacementEngine;
use crate::state::{STATE_COUNT, State};

impl PlacementEngine {
    /// Every entry of a queue with the state it is in (e.1), head first. The state machine
    /// is what several of the section k tests assert on, and it is not derivable from
    /// `PlacementStats`.
    pub fn entry_states(&self, stage: StageId) -> Vec<(Seq, State)> {
        let index = usize::from(stage);
        if index >= self.queues.len() {
            return Vec::new();
        }
        let queue = self.lock_queue(index);
        queue
            .order
            .iter()
            .map(|entry| (entry.seq, entry.state.clone()))
            .collect()
    }

    /// The head of a queue and its state (PL-I1).
    pub fn head_state(&self, stage: StageId) -> Option<(Seq, State)> {
        let index = usize::from(stage);
        if index >= self.queues.len() {
            return None;
        }
        let queue = self.lock_queue(index);
        queue
            .order
            .front()
            .map(|entry| (entry.seq, entry.state.clone()))
    }
}

/// One queue as `detailed_stats` reports it (j).
#[derive(Clone, Debug)]
pub struct QueueDetail {
    /// Resident bytes per `TierKind::index()`.
    pub bytes_by_tier: [u64; TIER_COUNT],
    /// Entries per state: the seven states of e.1 plus `OnRemote`, always 0 in v1.
    pub entries_by_state: [u64; STATE_COUNT],
    /// The segment this queue appends to.
    pub active_segment: Option<u32>,
    /// Entries dropped without writing (PL-I6).
    pub evictions: u64,
    /// Evicted entries a `replace` brought back.
    pub evictions_replaced: u64,
    /// Moves re-issued after a first failure (f.10).
    pub move_retries: u64,
    /// True while a failed move of the head is waiting for the next `pop`.
    pub head_error: bool,
}

/// Everything the controller and the tests read that `PlacementStats` does not carry (j).
#[derive(Clone, Debug)]
pub struct PlacementDetail {
    /// One entry per queue, by stage.
    pub per_queue: Vec<QueueDetail>,
    /// Segment files that exist.
    pub segments_live: u64,
    /// Of those, the ones every record of which is released (f.7).
    pub segments_reclaimable: u64,
    /// The next number the global segment counter will hand out.
    pub next_segment: u32,
    /// Moves issued and not yet seen complete.
    pub moves_in_flight: u64,
    /// Bytes claimed against each tier for a move in flight (PL-I3).
    pub reservations: [u64; TIER_COUNT],
    /// Bytes charged to the staging directory (PL-I7).
    pub disk_bytes: u64,
    /// The disk budget in force, which a failed preallocation lowers (h).
    pub disk_budget_effective: u64,
    /// Uncommitted morsels the engine is tracking (PL-I11).
    pub lineage_len: u64,
    /// The committed watermark.
    pub committed_seq: Option<Seq>,
    /// Manifests written since `new`.
    pub manifests_written: u64,
    /// How long the last manifest write took.
    pub last_manifest_us: u64,
    /// The run's one host tier (contracts e.1).
    pub host_tier: Tier,
}

/// `Placement::stats` (contracts d.10).
pub fn stats(engine: &PlacementEngine) -> PlacementStats {
    let mut queues = Vec::with_capacity(engine.queues.len());
    for (index, counters) in engine.counters.iter().enumerate() {
        let mut bytes_by_tier = [0u64; TIER_COUNT];
        for (slot, cell) in counters.bytes.iter().enumerate() {
            bytes_by_tier[slot] = cell.load(Ordering::Acquire);
        }
        queues.push(QueueStats {
            stage: index as u16,
            bytes_by_tier,
            count: counters.count.load(Ordering::Relaxed),
            misses: counters.misses.load(Ordering::Relaxed),
            miss_wait_us: counters.miss_wait_us.load(Ordering::Relaxed),
            demotions: counters.demotions.load(Ordering::Relaxed),
            promotions: counters.promotions.load(Ordering::Relaxed),
        });
    }
    PlacementStats {
        queues,
        in_flight_bytes: engine.in_flight_bytes.load(Ordering::Acquire),
    }
}

/// `PlacementEngine::detailed_stats` (j).
pub fn detailed(engine: &PlacementEngine) -> PlacementDetail {
    let (segments_live, segments_reclaimable, active) = {
        let staging = engine.lock_staging();
        let live = staging.segments.len() as u64;
        let reclaimable = staging
            .segments
            .values()
            .filter(|segment| segment.reclaimable())
            .count() as u64;
        (live, reclaimable, staging.active.clone())
    };
    let mut per_queue = Vec::with_capacity(engine.queues.len());
    for index in 0..engine.queues.len() {
        let counters = &engine.counters[index];
        let mut bytes_by_tier = [0u64; TIER_COUNT];
        for (slot, cell) in counters.bytes.iter().enumerate() {
            bytes_by_tier[slot] = cell.load(Ordering::Acquire);
        }
        let queue = engine.lock_queue(index);
        let mut entries_by_state = [0u64; STATE_COUNT];
        for entry in &queue.order {
            entries_by_state[entry.state.index()] += 1;
        }
        per_queue.push(QueueDetail {
            bytes_by_tier,
            entries_by_state,
            active_segment: active.get(&(index as u16)).copied(),
            evictions: counters.evictions.load(Ordering::Relaxed),
            evictions_replaced: counters.evictions_replaced.load(Ordering::Relaxed),
            move_retries: counters.move_retries.load(Ordering::Relaxed),
            head_error: queue.head_error.is_some(),
        });
    }
    let moves_in_flight = engine.lock_moves().len() as u64;
    let lineage_len = engine.lineage_len();
    let mut reservations = [0u64; TIER_COUNT];
    for (slot, cell) in engine.reserved.iter().enumerate() {
        reservations[slot] = cell.load(Ordering::Acquire);
    }
    PlacementDetail {
        per_queue,
        segments_live,
        segments_reclaimable,
        next_segment: engine.next_segment.load(Ordering::Acquire),
        moves_in_flight,
        reservations,
        disk_bytes: engine.disk_bytes.load(Ordering::Acquire),
        disk_budget_effective: engine.disk_budget.load(Ordering::Acquire),
        lineage_len,
        committed_seq: engine.committed_seq(),
        manifests_written: engine.manifests_written.load(Ordering::Relaxed),
        last_manifest_us: engine.last_manifest_us.load(Ordering::Relaxed),
        host_tier: engine.host_tier,
    }
}
