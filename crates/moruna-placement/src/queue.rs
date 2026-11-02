//! The queue: its structure (e.2), `push` (f.1), `pop` and `peek_resident` (f.3),
//! `pop_blocking` (f.4), `release` (f.18), `is_full` (f.14), and `evicted`/`replace` (f.16).
//! A queue holds entries in order; each names its batch in the catalog, which holds the bytes.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam::sync::{Parker, Unparker};
use moruna_kernel::{
    Locality, Morsel, MorunaError, Origin, PayloadKind, PayloadSpec, Seq, StageId, TIER_COUNT,
    Tier, TierPref,
};

use crate::PlacementEngine;
use crate::catalog::{Buffer, NewBuffer};
use crate::state::{Entry, EntryPayload, State, kind_satisfies, satisfies};

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
    /// Entries pushed so far: the next entry is placed as this number (f.18).
    pub pushed: u64,
    /// Entries taken so far. An entry leaves only at the head (PL-I5), so its position is the
    /// number it was placed as less this.
    pub taken: u64,
    /// Planning passes so far. A pass's finding that nothing could leave (`blocked`) is recorded
    /// only if no later pass has run since, so a stale finding never outlives the queue's
    /// emptying (f.14).
    pub epoch: u64,
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
            promotion_window: moruna_kernel::DEFAULT_PROMOTION_WINDOW,
            head_error: None,
            blocked: false,
            closed: false,
            waiters: Vec::new(),
            pushed: 0,
            taken: 0,
            epoch: 0,
        }
    }

    /// The position of the entry with this sequence number.
    pub fn position(&self, seq: Seq) -> Option<usize> {
        self.order.iter().position(|entry| entry.seq == seq)
    }
}

/// How a pushed batch's bytes come in (f.18).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Admit {
    /// Counted as they come; the tier is shed back within its budget after (the contract's
    /// `push`).
    Unclaimed,
    /// Under an admission claim of `(slot, bytes)`, which becomes their residency.
    Claimed(usize, u64),
    /// Not admitted to memory: written straight to disk (`push_spilled`).
    Outside,
}

/// Per-queue counters read without the queue lock (preamble 4.2 names them a lock-free path).
/// The bytes a queue holds are not among them: they are the catalog's (f.18).
#[derive(Default)]
pub struct QueueCounters {
    /// Entries in the queue.
    pub count: AtomicU64,
    /// Pops that waited on a move (PL-I9).
    pub misses: AtomicU64,
    /// Microseconds waited on moves, summed (PL-I9).
    pub miss_wait_us: AtomicU64,
    /// Promotions completed, counted in the queue a batch's move is attributed to (f.18).
    pub promotions: AtomicU64,
    /// Demotions completed, counted the same way.
    pub demotions: AtomicU64,
    /// Batches dropped without writing (PL-I6).
    pub evictions: AtomicU64,
    /// Evicted batches a `replace` brought back (f.16).
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

    /// Check a morsel's tier before it enters a queue (f.1): resident, and in the run's one host
    /// tier when it is a host tier.
    fn admit_tier(&self, morsel: &Morsel) -> moruna_kernel::Result<Tier> {
        let tier = morsel.payload.tier();
        match tier {
            Tier::Remote(_, _) => Err(MorunaError::Unsupported("rdma")),
            Tier::Disk(_) => Err(MorunaError::Staging(format!(
                "push: morsel {} has no resident bytes",
                morsel.seq
            ))),
            Tier::PinnedHost | Tier::Host if tier != self.host_tier => {
                Err(MorunaError::Staging(format!(
                    "push: wrong host tier {tier:?}; this run's host tier is {:?}",
                    self.host_tier
                )))
            }
            Tier::PinnedHost | Tier::Host | Tier::Device(_) => Ok(tier),
        }
    }

    /// `push` and `push_to` (f.1, d.16): one batch into every queue of `stages`, recorded once
    /// in the catalog with one consumer per queue, its payload shared by every entry and never
    /// copied, and its lineage recorded at every queue in one step. Every queue is checked before
    /// any is pushed into, so a refusal pushes nothing. `admit` says how the batch's bytes come
    /// in: unclaimed, under an admission claim, or straight to disk (f.18).
    pub(crate) fn push_into(
        &self,
        stages: &[StageId],
        morsel: Morsel,
        admit: Admit,
    ) -> moruna_kernel::Result<()> {
        if self.is_shutting_down() {
            return Err(MorunaError::Cancelled);
        }
        let Some(&first) = stages.first() else {
            return Err(MorunaError::Plan(
                "a morsel is pushed into at least one queue".into(),
            ));
        };
        let tier = self.admit_tier(&morsel)?;
        if admit == Admit::Outside && !tier.is_host() {
            return Err(MorunaError::Staging(format!(
                "push: only a host-tier batch goes straight to disk; morsel {} is in {tier:?}",
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
        let Morsel {
            seq,
            stage: _,
            payload,
            bytes,
            origin,
            features,
        } = morsel;
        let kind = payload.kind();
        let owner = match self.cfg.topology.link(first) {
            Some(link) => link.from,
            None => {
                return Err(MorunaError::Staging(format!(
                    "push: queue {first} is not in the run's topology"
                )));
            }
        };
        let entry_payload = EntryPayload::from(payload);
        // Straight to disk: the batch is born `Demoting` with its write issued once its entries
        // are in place, and its bytes count in no tier until the record lands (KD7b).
        let write = (admit == Admit::Outside).then(|| {
            (
                self.next_move_id(),
                crate::moves::TaskKind::Write {
                    from: tier,
                    payload: entry_payload.reference(),
                    kind,
                },
            )
        });
        let (id, events) = {
            let mut catalog = self.lock_catalog();
            let id = catalog.insert(NewBuffer {
                bytes,
                kind,
                owner,
                payload: Some(entry_payload),
                state: match write {
                    Some(_) => State::Demoting(tier, Tier::Disk(crate::moves::PENDING_DISK)),
                    None => State::Resident(tier),
                },
                disk: None,
                recomputable: self.cfg.topology.source_of(first).is_some(),
                consumers: stages.iter().map(|&stage| (stage, seq)).collect(),
                outside: write.is_some(),
            })?;
            match admit {
                Admit::Claimed(slot, claimed) => {
                    catalog.unreserve(slot, claimed);
                    if bytes > claimed {
                        catalog.overrun(bytes - claimed);
                    }
                }
                Admit::Outside => {
                    if let Some((move_id, _)) = &write {
                        let move_id = *move_id;
                        catalog.update(id, |buffer| buffer.move_id = Some(move_id));
                    }
                    catalog.overflowed(bytes);
                }
                Admit::Unclaimed => {}
            }
            (id, catalog.drain_events())
        };
        let mut waiters = Vec::new();
        let mut features = Some(features);
        for (at, &stage) in stages.iter().enumerate() {
            let index = usize::from(stage);
            // The last entry takes the features; the others carry copies (they are small).
            let features = if at + 1 == stages.len() {
                features.take().unwrap_or_default()
            } else {
                features.clone().unwrap_or_default()
            };
            let mut queue = self.lock_queue(index);
            self.enqueue(
                &mut queue,
                Entry {
                    seq,
                    stage,
                    origin: origin.clone(),
                    features,
                    buffer: id,
                },
            );
            waiters.extend(queue.waiters.iter().map(|(_, w)| w.clone()));
        }
        self.lineage_push(stages, seq, kind, bytes, origin);
        self.deliver(events);
        if let Some((move_id, kind)) = write {
            self.issue(crate::moves::Task {
                stage: usize::from(first),
                stage_id: first,
                seq,
                buffer: id,
                move_id,
                bytes,
                promote: false,
                reservation: None,
                kind,
            });
        }
        for waiter in waiters {
            waiter.unpark();
        }
        for &stage in stages {
            self.plan_and_issue(usize::from(stage));
        }
        self.relieve(tier.index());
        Ok(())
    }

    /// Append an entry to its queue and tell the catalog the number it was placed as, under the
    /// queue's lock so the number and the position agree (f.18).
    pub(crate) fn enqueue(&self, queue: &mut Queue, entry: Entry) {
        let ordinal = queue.pushed;
        queue.pushed += 1;
        let (stage, seq) = (queue.stage, entry.seq);
        queue.order.push_back(entry);
        self.counters[usize::from(stage)]
            .count
            .fetch_add(1, Ordering::Relaxed);
        self.lock_catalog().place(stage, seq, ordinal);
    }

    /// `replace` (f.16): an evicted batch's bytes come back, for every queue it waits in.
    pub(crate) fn replace_entry(
        &self,
        index: usize,
        morsel: Morsel,
        reservation: Option<(usize, u64)>,
    ) -> moruna_kernel::Result<()> {
        if self.is_shutting_down() {
            return Err(MorunaError::Cancelled);
        }
        let tier = self.admit_tier(&morsel)?;
        let Morsel {
            seq,
            payload,
            bytes,
            features,
            ..
        } = morsel;
        let (consumers, events) = {
            let mut queue = self.lock_queue(index);
            let Some(position) = queue.position(seq) else {
                return Err(MorunaError::Staging("replace: no evicted entry".into()));
            };
            let id = queue.order[position].buffer;
            let mut catalog = self.lock_catalog();
            let evicted = catalog
                .get(id)
                .is_some_and(|buffer| matches!(buffer.state, State::Evicted));
            if !evicted {
                return Err(MorunaError::Staging("replace: no evicted entry".into()));
            }
            catalog.update(id, |buffer| {
                buffer.bytes = bytes;
                buffer.payload = Some(EntryPayload::from(payload));
                buffer.state = State::Resident(tier);
            });
            if let Some((slot, claimed)) = reservation {
                catalog.unreserve(slot, claimed);
                if bytes > claimed {
                    catalog.overrun(bytes - claimed);
                }
            }
            catalog.touch(id);
            queue.order[position].features = features;
            let consumers: Vec<StageId> = catalog
                .get(id)
                .map(|b| b.consumers.iter().map(|(s, _)| *s).collect())
                .unwrap_or_default();
            (consumers, catalog.drain_events())
        };
        self.counters[index]
            .evictions_replaced
            .fetch_add(1, Ordering::Relaxed);
        self.deliver(events);
        for consumer in consumers {
            let at = usize::from(consumer);
            self.unpark_all(at);
            self.plan_and_issue(at);
        }
        self.relieve(tier.index());
        Ok(())
    }

    /// `pop` (f.3). Never blocks; `Ok(None)` when the head is not yet resident. The consumer
    /// takes its share of the batch and holds it until it calls `release` (f.18).
    pub(crate) fn pop_entry(
        &self,
        index: usize,
        want: &PayloadSpec,
        locality: Locality,
    ) -> moruna_kernel::Result<Option<Morsel>> {
        let stage = index as StageId;
        let taken = {
            let mut queue = self.lock_queue(index);
            if let Some(error) = queue.head_error.take() {
                queue.blocked = false;
                return Err(error);
            }
            let Some(head) = queue.order.front() else {
                return Ok(None);
            };
            let _ = locality;
            let (seq, id) = (head.seq, head.buffer);
            let taken = {
                let mut catalog = self.lock_catalog();
                let Some(buffer) = catalog.get(id) else {
                    return Err(MorunaError::Staging(format!(
                        "pop: morsel {seq} of queue {stage} names no batch"
                    )));
                };
                if let State::OnRemote(_, _) = buffer.state {
                    debug_assert!(false, "v1 never produces an OnRemote batch (E11)");
                    return Err(MorunaError::Unsupported("rdma"));
                }
                if !is_ready(buffer, want) {
                    return Ok(None);
                }
                catalog.take(stage, seq)?
            };
            let Some(entry) = queue.order.pop_front() else {
                return Ok(None);
            };
            queue.taken += 1;
            self.counters[index]
                .count
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                    Some(held.saturating_sub(1))
                })
                .ok();
            (entry.into_morsel(taken.payload), taken.record)
        };
        let (morsel, record) = taken;
        self.lineage_consumed(stage, morsel.seq);
        if let Some(seg) = record {
            self.release_record(seg.segment);
        }
        self.plan_and_issue(index);
        Ok(Some(morsel))
    }

    /// `release` (contracts d.10, f.18): the consumer of `stage` is done with what it took as
    /// `seq`. A batch with no consumer and no holder left is freed.
    pub(crate) fn release_take(&self, index: usize, seq: Seq) -> moruna_kernel::Result<()> {
        let stage = index as StageId;
        let (freed, events, waiting) = {
            let mut catalog = self.lock_catalog();
            let freed = catalog.release(stage, seq)?;
            let waiting = if freed.is_some() {
                catalog.take_waiting()
            } else {
                Vec::new()
            };
            (freed, catalog.drain_events(), waiting)
        };
        if let Some(Some(seg)) = freed {
            self.release_record(seg.segment);
        }
        self.deliver(events);
        for queue in waiting {
            self.plan_and_issue(queue);
        }
        Ok(())
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
        let Some(head) = queue.order.front() else {
            return false;
        };
        let catalog = self.lock_catalog();
        catalog
            .get(head.buffer)
            .is_some_and(|buffer| is_ready(buffer, want))
    }

    /// The batch at the head of a queue (contracts d.17 `head`).
    pub(crate) fn head_of(&self, index: usize) -> Option<moruna_kernel::Head> {
        let stage = index as StageId;
        let queue = self.lock_queue(index);
        let entry = queue.order.front()?;
        let catalog = self.lock_catalog();
        let buffer = catalog.get(entry.buffer)?;
        Some(moruna_kernel::Head {
            id: buffer.id,
            seq: entry.seq,
            bytes: buffer.bytes,
            last: buffer.consumers.len() == 1 && buffer.consumers[0] == (stage, entry.seq),
            arrived: buffer.arrived,
        })
    }

    /// `evicted` (f.16): the `(seq, origin)` of every entry whose batch is `Evicted`, in position
    /// order. A batch fanned out to several queues is listed once, under the lowest of them, so
    /// it is read again once (contracts d.10).
    pub(crate) fn evicted_entries(&self, index: usize) -> Vec<(Seq, Origin)> {
        let stage = index as StageId;
        let queue = self.lock_queue(index);
        let catalog = self.lock_catalog();
        queue
            .order
            .iter()
            .filter(|entry| {
                catalog.get(entry.buffer).is_some_and(|buffer| {
                    matches!(buffer.state, State::Evicted)
                        && buffer.consumers.iter().map(|(s, _)| *s).min() == Some(stage)
                })
            })
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

/// True when a batch at the head may be taken by a consumer wanting `want` (f.3): its bytes are
/// resident in a satisfying tier with no move in flight, the catalog holds them, and its payload
/// is of a satisfying kind.
pub(crate) fn is_ready(buffer: &Buffer, want: &PayloadSpec) -> bool {
    if buffer.payload.is_none() || !kind_satisfies(buffer.kind, want) {
        return false;
    }
    match &buffer.state {
        State::Resident(tier) | State::ResidentOnDisk(tier) => satisfies(*tier, want),
        State::Promoting(_, _)
        | State::Demoting(_, _)
        | State::OnDisk(_)
        | State::Evicted
        | State::Consumed
        | State::OnRemote(_, _) => false,
    }
}
