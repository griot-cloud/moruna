//! The queue: its structure (e.2), `push` (f.1), `pop` and `peek_resident` (f.3),
//! `pop_blocking` (f.4), `is_full` (f.14), and `evicted`/`replace` (f.16).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam::sync::{Parker, Unparker};
use moruna_kernel::{
    Locality, Morsel, MorunaError, Origin, PayloadKind, PayloadSpec, Seq, StageId, TIER_COUNT,
    Tier, TierPref,
};

use crate::PlacementEngine;
use crate::state::{Entry, State, kind_satisfies, satisfies};

/// A lock guard that also records its position in the preamble's lock order (4.2).
pub struct Guarded<'a, T> {
    inner: std::sync::MutexGuard<'a, T>,
    _held: crate::locks::Held,
}

impl<'a, T> Guarded<'a, T> {
    /// Pair a lock-order record with the guard it belongs to.
    pub fn new(held: crate::locks::Held, inner: std::sync::MutexGuard<'a, T>) -> Guarded<'a, T> {
        Guarded { inner, _held: held }
    }
}

impl<T> std::ops::Deref for Guarded<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T> std::ops::DerefMut for Guarded<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

/// How long `pop_blocking` parks before looking again, so `close` and `shutdown` are
/// observed without a second wake-up path (f.4).
const PARK_SLICE: Duration = Duration::from_millis(10);

/// One queue's mutable state, behind the queue mutex (preamble 4.2 position 2).
pub struct Queue {
    /// The stage whose output this queue holds.
    pub stage: StageId,
    /// FIFO; head at the front (PL-I5).
    pub order: VecDeque<Entry>,
    /// What the consumer declared through `set_consumer`.
    pub consumer: PayloadSpec,
    /// The tier promotions aim at (b).
    pub target: Tier,
    /// `(low, high)` per `TierKind::index()`, where the controller has set them.
    pub water: [(u64, u64); TIER_COUNT],
    /// Which marks the controller has set; the rest fall back to f.8's defaults.
    pub water_set: [bool; TIER_COUNT],
    /// `set_staging`; false for a queue a source gives, true otherwise (b).
    pub staging_enabled: bool,
    /// The promotion window `k`.
    pub promotion_window: u16,
    /// A failed move of the head, surfaced by the next `pop` (f.10) and by `is_full` (f.14).
    pub head_error: Option<MorunaError>,
    /// The last plan found the target tier over its high water and no candidate (f.14).
    pub blocked: bool,
    /// No more pushes will arrive (PL-I10).
    pub closed: bool,
    /// Blocked `pop_blocking` callers, by waiter id.
    pub waiters: Vec<(u64, Unparker)>,
}

impl Queue {
    /// A queue with the defaults of b: staging off for a queue a source gives (its entries are
    /// `recomputable`) and on for every other, a promotion window of 2 (preamble section 5),
    /// and a host consumer.
    pub fn new(stage: StageId, host_tier: Tier, recomputable: bool) -> Queue {
        Queue {
            stage,
            order: VecDeque::new(),
            consumer: PayloadSpec {
                kind: PayloadKind::Either,
                tier: TierPref::Host,
            },
            target: host_tier,
            water: [(0, u64::MAX); TIER_COUNT],
            water_set: [false; TIER_COUNT],
            staging_enabled: !recomputable,
            promotion_window: 2,
            head_error: None,
            blocked: false,
            closed: false,
            waiters: Vec::new(),
        }
    }

    /// The position of the entry with this sequence number.
    pub fn position(&self, seq: Seq) -> Option<usize> {
        self.order.iter().position(|entry| entry.seq == seq)
    }
}

/// Per-queue counters read without the queue lock (preamble 4.2 names the tier byte
/// counters a lock-free path).
#[derive(Default)]
pub struct QueueCounters {
    /// Resident bytes per `TierKind::index()`.
    pub bytes: [AtomicU64; TIER_COUNT],
    /// Entries in the queue.
    pub count: AtomicU64,
    /// Pops that waited on a move (PL-I9).
    pub misses: AtomicU64,
    /// Microseconds waited on moves, summed (PL-I9).
    pub miss_wait_us: AtomicU64,
    /// Promotions completed.
    pub promotions: AtomicU64,
    /// Demotions completed.
    pub demotions: AtomicU64,
    /// Entries dropped without writing (PL-I6).
    pub evictions: AtomicU64,
    /// Evicted entries a `replace` brought back (f.16).
    pub evictions_replaced: AtomicU64,
    /// Moves re-issued after a first failure (f.10).
    pub move_retries: AtomicU64,
}

impl PlacementEngine {
    /// Take one queue's lock (preamble 4.2 position 2).
    pub(crate) fn lock_queue(&self, index: usize) -> Guarded<'_, Queue> {
        let held = crate::locks::Held::enter(crate::locks::QUEUE);
        Guarded {
            inner: self.queues[index].lock().unwrap_or_else(|e| e.into_inner()),
            _held: held,
        }
    }

    /// Add `bytes` to a queue's counter for `tier`.
    pub(crate) fn add_bytes(&self, index: usize, tier: Tier, bytes: u64) {
        self.counters[index].bytes[tier.index()].fetch_add(bytes, Ordering::AcqRel);
    }

    /// An entry's bytes leave `tier`: the queue's counter falls, and when the entry was one of a
    /// fan-out's (d.16) it leaves its group, so a batch other queues still hold stays counted
    /// once (f.9).
    pub(crate) fn drop_resident(&self, index: usize, tier: Tier, entry: &mut Entry) {
        self.sub_bytes(index, tier, entry.bytes);
        if let Some(group) = entry.group.take() {
            group.leave(&self.shared_bytes);
        }
    }

    /// Subtract `bytes` from a queue's counter for `tier`.
    pub(crate) fn sub_bytes(&self, index: usize, tier: Tier, bytes: u64) {
        self.counters[index].bytes[tier.index()]
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                Some(held.saturating_sub(bytes))
            })
            .ok();
    }

    /// `push` (f.1) and `replace` (f.16): the two ways bytes enter a queue.
    pub(crate) fn push_entry(
        &self,
        index: usize,
        morsel: Morsel,
        replacing: bool,
    ) -> moruna_kernel::Result<()> {
        if self.is_shutting_down() {
            return Err(MorunaError::Cancelled);
        }
        let tier = morsel.payload.tier();
        match tier {
            Tier::Remote(_, _) => return Err(MorunaError::Unsupported("rdma")),
            Tier::Disk(_) => {
                return Err(MorunaError::Staging(format!(
                    "push: morsel {} has no resident bytes",
                    morsel.seq
                )));
            }
            Tier::PinnedHost | Tier::Host => {
                if tier != self.host_tier {
                    return Err(MorunaError::Staging(format!(
                        "push: wrong host tier {tier:?}; this run's host tier is {:?}",
                        self.host_tier
                    )));
                }
            }
            Tier::Device(_) => {}
        }
        let stage = index as StageId;
        let seq = morsel.seq;
        let origin = morsel.origin.clone();
        let bytes = morsel.bytes;
        let kind = morsel.payload.kind();
        let waiters = {
            let mut queue = self.lock_queue(index);
            if queue.closed && !replacing {
                return Err(MorunaError::Staging(format!(
                    "push: stage {stage} is closed"
                )));
            }
            if replacing {
                let Some(position) = queue.position(seq) else {
                    return Err(MorunaError::Staging("replace: no evicted entry".into()));
                };
                let entry = &mut queue.order[position];
                if !matches!(entry.state, State::Evicted) {
                    return Err(MorunaError::Staging("replace: no evicted entry".into()));
                }
                let mut fresh = Entry::new(morsel, entry.recomputable, None);
                fresh.disk = entry.disk;
                *entry = fresh;
                self.counters[index]
                    .evictions_replaced
                    .fetch_add(1, Ordering::Relaxed);
            } else {
                let recomputable = self.cfg.topology.source_of(stage).is_some();
                let entry = Entry::new(morsel, recomputable, None);
                queue.order.push_back(entry);
                self.counters[index].count.fetch_add(1, Ordering::Relaxed);
            }
            self.add_bytes(index, tier, bytes);
            queue.waiters.clone()
        };
        self.lineage_push(&[stage], seq, kind, bytes, origin);
        for (_, waiter) in waiters {
            waiter.unpark();
        }
        self.plan_and_issue(index);
        Ok(())
    }

    /// `push_to` for a fan-out (f.1, d.16): one morsel into each of `stages`, its payload shared
    /// by every entry and counted once, and its lineage recorded at every queue in one step.
    /// Every queue is checked before any is pushed into, so a refusal pushes nothing.
    pub(crate) fn push_shared(
        &self,
        stages: &[StageId],
        morsel: Morsel,
    ) -> moruna_kernel::Result<()> {
        if self.is_shutting_down() {
            return Err(MorunaError::Cancelled);
        }
        let tier = morsel.payload.tier();
        if !matches!(tier, Tier::PinnedHost | Tier::Host | Tier::Device(_))
            || (tier.is_host() && tier != self.host_tier)
        {
            return Err(MorunaError::Staging(format!(
                "push: morsel {} is in {tier:?}, which no queue of this run holds",
                morsel.seq
            )));
        }
        for &stage in stages {
            if self.lock_queue(usize::from(stage)).closed {
                return Err(MorunaError::Staging(format!(
                    "push: stage {stage} is closed"
                )));
            }
        }
        let (seq, bytes, kind, origin) = (
            morsel.seq,
            morsel.bytes,
            morsel.payload.kind(),
            morsel.origin.clone(),
        );
        let group = crate::state::Group::new(bytes, tier.index());
        let mut waiters = Vec::new();
        let mut morsel = Some(morsel);
        for (at, &stage) in stages.iter().enumerate() {
            let index = usize::from(stage);
            let this = if at + 1 == stages.len() {
                morsel.take()
            } else {
                morsel.as_ref().map(|m| Morsel {
                    seq: m.seq,
                    stage: m.stage,
                    payload: m.payload.share(),
                    bytes: m.bytes,
                    origin: m.origin.clone(),
                    features: m.features.clone(),
                })
            };
            let Some(this) = this else {
                break;
            };
            let recomputable = self.cfg.topology.source_of(stage).is_some();
            let mut queue = self.lock_queue(index);
            // Counted into the queue first and into the group after, so the tier's total never
            // dips below what is resident between the two.
            self.add_bytes(index, tier, bytes);
            let entry = Entry::new(this, recomputable, Some(group.join(&self.shared_bytes)));
            queue.order.push_back(entry);
            self.counters[index].count.fetch_add(1, Ordering::Relaxed);
            waiters.extend(queue.waiters.iter().map(|(_, w)| w.clone()));
        }
        self.lineage_push(stages, seq, kind, bytes, origin);
        for waiter in waiters {
            waiter.unpark();
        }
        for &stage in stages {
            self.plan_and_issue(usize::from(stage));
        }
        Ok(())
    }

    /// `pop` (f.3). Never blocks; `Ok(None)` when the head is not yet resident.
    pub(crate) fn pop_entry(
        &self,
        index: usize,
        want: &PayloadSpec,
        locality: Locality,
    ) -> moruna_kernel::Result<Option<Morsel>> {
        let taken = {
            let mut queue = self.lock_queue(index);
            if let Some(error) = queue.head_error.take() {
                queue.blocked = false;
                return Err(error);
            }
            let Some(head) = queue.order.front() else {
                return Ok(None);
            };
            if let State::OnRemote(_, _) = head.state {
                debug_assert!(false, "v1 never produces an OnRemote entry (E11)");
                return Err(MorunaError::Unsupported("rdma"));
            }
            let _ = locality;
            if !head_is_ready(head, want) {
                return Ok(None);
            }
            // The morsel is built while the entry is still in the queue, so a refusal loses
            // nothing: a payload whose DMA view a completion has not released yet is a
            // transient condition the next pop clears (d.3, d.4).
            let Some(head) = queue.order.front_mut() else {
                return Ok(None);
            };
            let tier = head.state.resident_tier();
            let morsel = match head.take_morsel() {
                Ok(morsel) => morsel,
                Err(error) => {
                    tracing::trace!(target: "placement.miss", stage = index, reason = %error);
                    return Ok(None);
                }
            };
            let Some(mut entry) = queue.order.pop_front() else {
                return Ok(None);
            };
            entry.state = State::Consumed;
            if let Some(tier) = tier {
                self.drop_resident(index, tier, &mut entry);
            }
            self.counters[index]
                .count
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                    Some(held.saturating_sub(1))
                })
                .ok();
            (morsel, entry.disk, entry.seq)
        };
        let (morsel, disk, seq) = taken;
        self.lineage_consumed(index as StageId, seq);
        if let Some(seg) = disk {
            self.release_record(seg.segment);
        }
        self.plan_and_issue(index);
        Ok(Some(morsel))
    }

    /// `pop_blocking` (f.4): the only blocking call the engine offers (PL-I14).
    pub(crate) fn pop_blocking_entry(
        &self,
        index: usize,
        want: &PayloadSpec,
        locality: Locality,
    ) -> moruna_kernel::Result<Option<(Morsel, u64)>> {
        let parker = Parker::new();
        let waiter_id = self.next_move.fetch_add(1, Ordering::Relaxed);
        let mut waiting_since: Option<Instant> = None;
        let result = loop {
            if self.is_shutting_down() {
                break Err(MorunaError::Cancelled);
            }
            match self.pop_entry(index, want, locality) {
                Err(e) => break Err(e),
                Ok(Some(morsel)) => {
                    let waited = match waiting_since {
                        Some(start) => {
                            let micros = start.elapsed().as_micros() as u64;
                            self.counters[index].misses.fetch_add(1, Ordering::Relaxed);
                            self.counters[index]
                                .miss_wait_us
                                .fetch_add(micros, Ordering::Relaxed);
                            tracing::trace!(target: "placement.miss", stage = index, seq = morsel.seq, wait_us = micros);
                            micros
                        }
                        None => 0,
                    };
                    break Ok(Some((morsel, waited)));
                }
                Ok(None) => {}
            }
            {
                let mut queue = self.lock_queue(index);
                if queue.order.is_empty() && queue.closed {
                    break Ok(None);
                }
                if !queue.waiters.iter().any(|(id, _)| *id == waiter_id) {
                    queue.waiters.push((waiter_id, parker.unparker().clone()));
                }
            }
            // The waiter is registered before the plan, so a move that lands between the two
            // wakes this caller rather than leaving it parked. Planning here is what makes a
            // head that nothing else has touched (the first pop after a `restore`, or after
            // a `close`) start moving.
            self.plan_and_issue(index);
            if waiting_since.is_none() {
                waiting_since = Some(Instant::now());
            }
            parker.park_timeout(PARK_SLICE);
        };
        let mut queue = self.lock_queue(index);
        queue.waiters.retain(|(id, _)| *id != waiter_id);
        result
    }

    /// `peek_resident` (f.3): the same test as `pop`, without the removal.
    pub(crate) fn peek(&self, index: usize, want: &PayloadSpec, _locality: Locality) -> bool {
        let queue = self.lock_queue(index);
        queue
            .order
            .front()
            .is_some_and(|head| head_is_ready(head, want))
    }

    /// `evicted` (f.16): the `(seq, origin)` of every `Evicted` entry, in position order.
    pub(crate) fn evicted_entries(&self, index: usize) -> Vec<(Seq, Origin)> {
        let queue = self.lock_queue(index);
        queue
            .order
            .iter()
            .filter(|entry| matches!(entry.state, State::Evicted))
            .map(|entry| (entry.seq, entry.origin.clone()))
            .collect()
    }

    /// `is_full` (f.14). O(1): it reads what the last plan recorded.
    pub(crate) fn queue_is_full(&self, index: usize) -> bool {
        let queue = self.lock_queue(index);
        queue.head_error.is_some() || queue.closed || queue.blocked
    }

    /// Wake every caller parked on a queue (f.1, f.15, and every move completion).
    pub(crate) fn unpark_all(&self, index: usize) {
        let waiters = {
            let queue = self.lock_queue(index);
            queue.waiters.clone()
        };
        for (_, waiter) in waiters {
            waiter.unpark();
        }
    }
}

/// True when the head may be returned to a consumer wanting `want` (f.3): its bytes are
/// resident in a satisfying tier and its payload is of a satisfying kind.
fn head_is_ready(head: &Entry, want: &PayloadSpec) -> bool {
    if head.payload.is_none() {
        return false;
    }
    if !kind_satisfies(head.kind, want) {
        return false;
    }
    match &head.state {
        State::Resident(tier) | State::ResidentOnDisk(tier) => satisfies(*tier, want),
        State::Promoting(_, _)
        | State::Demoting(_, _)
        | State::OnDisk(_)
        | State::Evicted
        | State::Consumed
        | State::OnRemote(_, _) => false,
    }
}
