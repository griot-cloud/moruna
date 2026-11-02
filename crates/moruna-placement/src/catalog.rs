//! The buffer catalog (09 f.18, adaptive-runtime 9.1): the one record of every batch the engine
//! holds, its size, where its bytes are, who produced it, which consumers have still to take it,
//! which running units hold it, and what bringing it back after an eviction would cost.
//!
//! A queue entry names its batch by [`BufferId`] and holds nothing else about it: the payload,
//! the tier, the disk copy and the move in flight are the batch's, kept here once however many
//! queues it was fanned out to (PL-I18). The catalog does no IO and takes no lock of its own; the
//! engine holds it behind one mutex (preamble 4.2 position 3, after the lineage index and before
//! the roll guard, the moves map and the segments map), and every change of a batch goes through
//! [`Catalog::update`], which takes the batch's bytes out of every total before the change and
//! puts them back after it, so the totals cannot drift from the records (PL-I3, PL-I21).
//!
//! The eviction order is a port ([`EvictionOrder`]): the catalog says which batches may leave
//! memory and what each would cost, an order ranks them, and [`KnownFuture`] is the order the
//! engine ships (f.18, PL-I20).

use std::collections::{BTreeSet, HashMap};

pub use moruna_kernel::{BufferId, CatalogEvent, CatalogObserver, Future};
use moruna_kernel::{
    CatalogStats, MorunaError, Payload, PayloadKind, Producer, SegmentRef, Seq, StageId,
    TIER_COUNT, TierKind,
};

use crate::state::{EntryPayload, State};

/// One batch (f.18).
pub struct Buffer {
    /// Its name.
    pub id: BufferId,
    /// `payload.bytes()` at push: the figure every total moves by.
    pub bytes: u64,
    /// Table or tensor.
    pub kind: PayloadKind,
    /// What produced it: a source or a kernel stage's output (contracts d.16).
    pub owner: Producer,
    /// The bytes, while memory holds them for a consumer still to take them.
    pub(crate) payload: Option<EntryPayload>,
    /// Where the bytes are and what is in flight (e.1).
    pub state: State,
    /// The segment record holding a copy of these bytes, when there is one.
    pub disk: Option<SegmentRef>,
    /// The move in flight for this batch, when there is one (PL-I2).
    pub move_id: Option<u64>,
    /// The bytes can be read again from the source (the batch is a source's, D5).
    pub recomputable: bool,
    /// The consumers still to take it: one `(queue, seq)` per queue it waits in. Its length is
    /// the batch's remaining consumer count, which the run's DAG fixed when it was pushed.
    pub consumers: Vec<(StageId, Seq)>,
    /// The consumers that took it and have not released it: running units, or sink writes in
    /// flight (PL-I19).
    pub holders: Vec<(StageId, Seq)>,
    /// The catalog's tick when it arrived.
    pub arrived: u64,
    /// The catalog's tick when it was last pushed, taken, brought back or replaced.
    pub touched: u64,
    /// A miss has been counted for the bytes being out of memory since they last left it, so a
    /// head waiting on one reload is one miss however often it is planned.
    pub missed: bool,
    /// Memory could not admit the batch and it is on its way straight to disk (KD7b,
    /// `push_spilled`): its bytes count in no tier until the record lands, when it is `OnDisk`.
    pub outside: bool,
}

impl Buffer {
    /// The tier slot its bytes count in, `None` for a batch with no bytes anywhere. A batch whose
    /// only copy is a segment record counts in the `Disk` slot; one with a move in flight counts
    /// where its bytes are until the move lands (RE-I1).
    pub fn slot(&self) -> Option<usize> {
        match &self.state {
            State::Resident(tier) | State::ResidentOnDisk(tier) => Some(tier.index()),
            State::Promoting(from, _) | State::Demoting(from, _) => Some(from.index()),
            State::OnDisk(_) => Some(TierKind::Disk.index()),
            State::Evicted | State::Consumed | State::OnRemote(_, _) => None,
        }
    }

    /// True when its bytes are in a memory tier.
    pub fn in_memory(&self) -> bool {
        self.slot()
            .is_some_and(|slot| slot != TierKind::Disk.index())
    }

    /// The disk bytes bringing it back moves once it has left memory: a read of the record, or a
    /// read from the source for a batch dropped without one.
    pub fn reload_cost(&self) -> u64 {
        self.bytes
    }
}

/// What one queue tells the catalog about the future of the batches waiting in it (f.18). A
/// queue loses entries only at its head (PL-I5), so an entry's position is the number it was
/// pushed as on that queue less the number of takes the queue has had: no queue is walked.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct QueueView {
    /// Takes from the queue so far.
    pub taken: u64,
    /// Its promotion window: an entry this close to the head is about to be taken.
    pub window: u16,
    /// Its staging flag (`set_staging`).
    pub staging: bool,
}

/// How a batch leaves memory (f.18).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Shed {
    /// Drop the resident copy of a batch that already has a disk copy: no IO now.
    DropCopy,
    /// Drop a source's batch to be read again from the source: no IO now (PL-I6).
    Recompute,
    /// Write a record and drop the resident copy when it lands (f.5).
    Spill,
    /// Copy a device-resident batch down to the host tier (e.4).
    CopyDown,
}

/// One batch that may leave memory, with what the eviction order weighs (f.18).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// The batch.
    pub id: BufferId,
    /// What leaving frees.
    pub bytes: u64,
    /// [`Future::next_use`].
    pub next_use: u64,
    /// The disk bytes leaving and coming back moves: a spill writes and reads the bytes, every
    /// other way only reads them back.
    pub reload: u64,
    /// When it arrived.
    pub arrived: u64,
    /// When it was last touched.
    pub touched: u64,
    /// How it would leave.
    pub how: Shed,
}

/// The order in which candidates leave memory: a port, so a policy can replace the order without
/// touching the catalog (adaptive-runtime 1.4, f.18).
pub trait EvictionOrder: Send + Sync + std::fmt::Debug {
    /// The order's name, for the report.
    fn name(&self) -> &'static str;
    /// Sort `candidates` so the first leaves first.
    fn order(&self, candidates: &mut [Candidate]);
}

/// Belady's rule over the known future, weighed by reload cost against size (f.18, PL-I20): the
/// candidate with the largest `(next_use + 1) × bytes / reload` leaves first. A batch used later
/// goes before one used sooner, and of two used equally soon the one that is cheaper to bring
/// back per byte freed goes first, so a batch with a disk copy already (reload = bytes) goes
/// before one that must be written (reload = 2 × bytes). Ties go to the larger batch, then to the
/// lower id.
#[derive(Copy, Clone, Debug, Default)]
pub struct KnownFuture;

impl KnownFuture {
    /// The score's comparison, exact in integers: `a` leaves before `b`.
    fn before(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
        let score = |c: &Candidate, other: &Candidate| {
            u128::from(c.next_use + 1) * u128::from(c.bytes) * u128::from(other.reload.max(1))
        };
        score(b, a)
            .cmp(&score(a, b))
            .then(b.bytes.cmp(&a.bytes))
            .then(a.id.cmp(&b.id))
    }
}

impl EvictionOrder for KnownFuture {
    fn name(&self) -> &'static str {
        "known_future"
    }

    fn order(&self, candidates: &mut [Candidate]) {
        candidates.sort_by(KnownFuture::before);
    }
}

/// The leading candidates of an ordered list whose bytes reach `need`; all of them when they do
/// not.
pub fn pick(ordered: &[Candidate], need: u64) -> Vec<Candidate> {
    let mut freed = 0u64;
    let mut chosen = Vec::new();
    for candidate in ordered {
        if freed >= need {
            break;
        }
        freed = freed.saturating_add(candidate.bytes);
        chosen.push(*candidate);
    }
    chosen
}

/// A batch the engine is about to record (f.1).
pub(crate) struct NewBuffer {
    pub bytes: u64,
    pub kind: PayloadKind,
    pub owner: Producer,
    pub payload: Option<EntryPayload>,
    pub state: State,
    pub disk: Option<SegmentRef>,
    pub recomputable: bool,
    pub consumers: Vec<(StageId, Seq)>,
    pub outside: bool,
}

/// What one take handed out (f.3).
pub(crate) struct Taken {
    /// The consumer's share of the bytes.
    pub payload: Payload,
    /// The batch's disk record, released because no consumer is left to read it (f.7).
    pub record: Option<SegmentRef>,
}

#[derive(Default)]
struct Figures {
    spilled_bytes: u64,
    spills: u64,
    reloaded_bytes: u64,
    reloads: u64,
    evictions: u64,
    misses: u64,
    miss_reload_bytes: u64,
    released: u64,
    freed: u64,
    overrun_bytes: u64,
    overflows: u64,
    overflow_bytes: u64,
}

/// The catalog (f.18).
pub struct Catalog {
    buffers: HashMap<BufferId, Buffer>,
    /// Every waiting or held `(queue, seq)`, its batch and, once its queue has placed it, the
    /// number it was pushed as on that queue.
    by_entry: HashMap<(StageId, Seq), (BufferId, Option<u64>)>,
    /// The batches whose bytes count in each tier slot, so a shedding pass reads only those.
    members: Vec<BTreeSet<BufferId>>,
    next_id: u64,
    tick: u64,
    resident: [u64; TIER_COUNT],
    peak: [u64; TIER_COUNT],
    reserved: [u64; TIER_COUNT],
    per_queue: Vec<[u64; TIER_COUNT]>,
    held: u64,
    /// Bytes per tier slot with a demotion in flight out of it: resident until the move lands,
    /// and already on their way out, so no pass sheds them twice.
    leaving: [u64; TIER_COUNT],
    per_queue_leaving: Vec<[u64; TIER_COUNT]>,
    /// Bytes of batches on their way straight to disk, admitted to no tier.
    outside: u64,
    figures: Figures,
    events: Vec<CatalogEvent>,
    waiting: BTreeSet<usize>,
}

impl Catalog {
    /// An empty catalog for a run of `queues` queues.
    pub fn new(queues: usize) -> Catalog {
        Catalog {
            buffers: HashMap::new(),
            by_entry: HashMap::new(),
            members: vec![BTreeSet::new(); TIER_COUNT],
            next_id: 1,
            tick: 0,
            resident: [0; TIER_COUNT],
            peak: [0; TIER_COUNT],
            reserved: [0; TIER_COUNT],
            per_queue: vec![[0; TIER_COUNT]; queues],
            held: 0,
            leaving: [0; TIER_COUNT],
            per_queue_leaving: vec![[0; TIER_COUNT]; queues],
            outside: 0,
            figures: Figures::default(),
            events: Vec::new(),
            waiting: BTreeSet::new(),
        }
    }

    fn tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// Take a batch's bytes out of the totals (`sign` false) or put them in (`sign` true).
    fn charge(&mut self, id: BufferId, add: bool) {
        let Some(buffer) = self.buffers.get(&id) else {
            return;
        };
        let bytes = buffer.bytes;
        if buffer.outside {
            // Not admitted to memory: counted only as on its way to disk (KD7b).
            self.outside = if add {
                self.outside.saturating_add(bytes)
            } else {
                self.outside.saturating_sub(bytes)
            };
            return;
        }
        let apply = |cell: &mut u64| {
            *cell = if add {
                cell.saturating_add(bytes)
            } else {
                cell.saturating_sub(bytes)
            };
        };
        if let Some(slot) = buffer.slot() {
            if add {
                self.members[slot].insert(id);
            } else {
                self.members[slot].remove(&id);
            }
            apply(&mut self.resident[slot]);
            for (stage, _) in &buffer.consumers {
                if let Some(queue) = self.per_queue.get_mut(usize::from(*stage)) {
                    apply(&mut queue[slot]);
                }
            }
            if add && self.resident[slot] > self.peak[slot] {
                self.peak[slot] = self.resident[slot];
            }
        }
        if let State::Demoting(from, _) = buffer.state {
            let slot = from.index();
            apply(&mut self.leaving[slot]);
            for (stage, _) in &buffer.consumers {
                if let Some(queue) = self.per_queue_leaving.get_mut(usize::from(*stage)) {
                    apply(&mut queue[slot]);
                }
            }
        }
        if !buffer.holders.is_empty() && buffer.in_memory() {
            apply(&mut self.held);
        }
    }

    /// Record a batch (f.1). A `(queue, seq)` the catalog already holds is refused: the queues of
    /// a run never hold one number twice (contracts d.16).
    pub(crate) fn insert(&mut self, new: NewBuffer) -> moruna_kernel::Result<BufferId> {
        if new.consumers.is_empty() {
            return Err(MorunaError::Plan(
                "a batch is pushed into at least one queue".into(),
            ));
        }
        for (at, entry) in new.consumers.iter().enumerate() {
            if new.consumers[..at].contains(entry) {
                return Err(MorunaError::Staging(format!(
                    "push: morsel {} names queue {} twice",
                    entry.1, entry.0
                )));
            }
        }
        for entry in &new.consumers {
            if self.by_entry.contains_key(entry) {
                return Err(MorunaError::Staging(format!(
                    "push: morsel {} is already on queue {}",
                    entry.1, entry.0
                )));
            }
        }
        let id = BufferId(self.next_id);
        self.next_id += 1;
        let now = self.tick();
        for entry in &new.consumers {
            self.by_entry.insert(*entry, (id, None));
        }
        self.buffers.insert(
            id,
            Buffer {
                id,
                bytes: new.bytes,
                kind: new.kind,
                owner: new.owner,
                payload: new.payload,
                state: new.state,
                disk: new.disk,
                move_id: None,
                recomputable: new.recomputable,
                consumers: new.consumers,
                holders: Vec::new(),
                arrived: now,
                touched: now,
                missed: false,
                outside: new.outside,
            },
        );
        self.charge(id, true);
        Ok(id)
    }

    /// The batch, to read.
    pub fn get(&self, id: BufferId) -> Option<&Buffer> {
        self.buffers.get(&id)
    }

    /// The batch the entry `(stage, seq)` names, while it waits or is held.
    pub fn of_entry(&self, stage: StageId, seq: Seq) -> Option<BufferId> {
        self.by_entry.get(&(stage, seq)).map(|(id, _)| *id)
    }

    /// The queue placed entry `(stage, seq)` as the `ordinal`-th it was pushed (f.18).
    pub(crate) fn place(&mut self, stage: StageId, seq: Seq, ordinal: u64) {
        if let Some(entry) = self.by_entry.get_mut(&(stage, seq)) {
            entry.1 = Some(ordinal);
        }
    }

    /// The batch's future from the queues' views (f.18): `None` while one of its entries is not
    /// placed in its queue yet, or when no consumer waits for it.
    pub fn future(&self, id: BufferId, views: &[QueueView]) -> Option<Future> {
        let buffer = self.buffers.get(&id)?;
        let mut future = Future {
            next_use: u64::MAX,
            hot: false,
            all_staging: true,
            any_staging: false,
        };
        for entry in &buffer.consumers {
            let ordinal = self.by_entry.get(entry)?.1?;
            let view = views.get(usize::from(entry.0))?;
            let position = ordinal.saturating_sub(view.taken);
            future.next_use = future.next_use.min(position);
            future.hot |= position < u64::from(view.window.max(1));
            future.all_staging &= view.staging;
            future.any_staging |= view.staging;
        }
        (!buffer.consumers.is_empty()).then_some(future)
    }

    /// Every batch, in id order.
    pub fn buffers(&self) -> Vec<&Buffer> {
        let mut all: Vec<&Buffer> = self.buffers.values().collect();
        all.sort_by_key(|buffer| buffer.id);
        all
    }

    /// Change one batch: the one way a record changes, so the totals follow it exactly
    /// (PL-I21). `None` when the catalog does not hold it.
    pub(crate) fn update<R>(
        &mut self,
        id: BufferId,
        change: impl FnOnce(&mut Buffer) -> R,
    ) -> Option<R> {
        if !self.buffers.contains_key(&id) {
            return None;
        }
        self.charge(id, false);
        let result = self.buffers.get_mut(&id).map(change);
        self.charge(id, true);
        result
    }

    /// Mark a batch touched now, for an order that reads recency.
    pub(crate) fn touch(&mut self, id: BufferId) {
        let now = self.tick();
        if let Some(buffer) = self.buffers.get_mut(&id) {
            buffer.touched = now;
        }
    }

    /// The consumer of `stage` takes its share of the batch it waits for as `seq` (f.3): the
    /// share moves from the batch's consumers to its holders, and once no consumer is left the
    /// catalog drops its own copy of the payload and hands back the disk record for release.
    /// The caller has checked, under the queue lock, that the batch is ready (f.3).
    pub(crate) fn take(&mut self, stage: StageId, seq: Seq) -> moruna_kernel::Result<Taken> {
        let Some(id) = self.of_entry(stage, seq) else {
            return Err(MorunaError::Staging(format!(
                "pop: morsel {seq} of queue {stage} is not in the catalog"
            )));
        };
        let now = self.tick();
        let taken = self.update(id, |buffer| {
            let Some(payload) = buffer.payload.as_ref() else {
                return Err(MorunaError::Staging(format!(
                    "pop: morsel {seq} of queue {stage} has no resident bytes"
                )));
            };
            let Some(at) = buffer.consumers.iter().position(|c| *c == (stage, seq)) else {
                return Err(MorunaError::Staging(format!(
                    "pop: morsel {seq} of queue {stage} was taken already"
                )));
            };
            let share = payload.share();
            buffer.consumers.swap_remove(at);
            buffer.holders.push((stage, seq));
            buffer.touched = now;
            let mut record = None;
            if buffer.consumers.is_empty() {
                buffer.payload = None;
                if let State::ResidentOnDisk(tier) = buffer.state {
                    buffer.state = State::Resident(tier);
                }
                record = buffer.disk.take();
            }
            Ok(Taken {
                payload: share,
                record,
            })
        });
        match taken {
            Some(taken) => taken,
            None => Err(MorunaError::Staging(format!(
                "pop: morsel {seq} of queue {stage} has no batch"
            ))),
        }
    }

    /// The consumer of `stage` released the share it took as `seq` (contracts d.10 `release`).
    /// Returns the freed batch's disk record when this was the last holder of a batch no consumer
    /// waits for, which the engine releases (f.7); the batch's bytes leave every total here.
    pub(crate) fn release(
        &mut self,
        stage: StageId,
        seq: Seq,
    ) -> moruna_kernel::Result<Option<Option<SegmentRef>>> {
        let id = match self.of_entry(stage, seq) {
            Some(id)
                if self
                    .buffers
                    .get(&id)
                    .is_some_and(|b| b.holders.contains(&(stage, seq))) =>
            {
                id
            }
            Some(_) | None => {
                return Err(MorunaError::Staging(format!(
                    "release: the consumer of queue {stage} holds no morsel {seq}"
                )));
            }
        };
        self.by_entry.remove(&(stage, seq));
        let done = self.update(id, |buffer| {
            buffer.holders.retain(|h| *h != (stage, seq));
            buffer.holders.is_empty() && buffer.consumers.is_empty()
        });
        self.figures.released += 1;
        self.events.push(CatalogEvent::Released {
            batch: id,
            stage,
            seq,
        });
        if done != Some(true) {
            return Ok(None);
        }
        self.charge(id, false);
        let Some(buffer) = self.buffers.remove(&id) else {
            return Ok(None);
        };
        self.figures.freed += 1;
        self.events.push(CatalogEvent::Freed {
            batch: id,
            bytes: buffer.bytes,
        });
        Ok(Some(buffer.disk))
    }

    /// Claim `bytes` of tier slot `slot` for a move in flight or an admission (PL-I3): granted
    /// only when what is resident, what is already claimed and these bytes all fit `budget`.
    pub(crate) fn reserve(&mut self, slot: usize, bytes: u64, budget: u64) -> bool {
        // A claim of nothing takes nothing, whatever memory holds.
        if bytes == 0 {
            return true;
        }
        let held = self.resident[slot].saturating_add(self.reserved[slot]);
        if held.saturating_add(bytes) > budget {
            return false;
        }
        self.reserved[slot] += bytes;
        true
    }

    /// Release a claim that will not become resident.
    pub(crate) fn unreserve(&mut self, slot: usize, bytes: u64) {
        self.reserved[slot] = self.reserved[slot].saturating_sub(bytes);
    }

    /// Bytes resident in a tier slot, every batch once (PL-I3, PL-I18).
    pub fn resident(&self, slot: usize) -> u64 {
        self.resident[slot]
    }

    /// Bytes claimed in a tier slot.
    pub fn reserved(&self, slot: usize) -> u64 {
        self.reserved[slot]
    }

    /// Of queue `stage`'s figure, the bytes with a demotion in flight.
    pub fn queue_leaving(&self, stage: StageId) -> [u64; TIER_COUNT] {
        self.per_queue_leaving
            .get(usize::from(stage))
            .copied()
            .unwrap_or([0; TIER_COUNT])
    }

    /// Bytes of tier slot `slot` with a demotion in flight: resident until it lands.
    pub fn leaving(&self, slot: usize) -> u64 {
        self.leaving[slot]
    }

    /// The bytes per tier the batches waiting in queue `stage` hold, for its water marks and its
    /// `QueueStats`: a fanned-out batch is in the figure of every queue it waits in.
    pub fn queue_bytes(&self, stage: StageId) -> [u64; TIER_COUNT] {
        self.per_queue
            .get(usize::from(stage))
            .copied()
            .unwrap_or([0; TIER_COUNT])
    }

    /// A queue whose reservation failed, to be planned again when room frees.
    pub(crate) fn wait_for_room(&mut self, queue: usize) {
        self.waiting.insert(queue);
    }

    /// The queues waiting for room, emptied.
    pub(crate) fn take_waiting(&mut self) -> Vec<usize> {
        std::mem::take(&mut self.waiting).into_iter().collect()
    }

    /// The events since the last call, in order.
    pub(crate) fn drain_events(&mut self) -> Vec<CatalogEvent> {
        std::mem::take(&mut self.events)
    }

    /// A batch left memory without IO (f.18).
    pub(crate) fn dropped(&mut self, id: BufferId, bytes: u64) {
        self.figures.evictions += 1;
        self.events.push(CatalogEvent::Dropped { batch: id, bytes });
    }

    /// The eviction order chose a batch that leaves by a move (a spill or a copy down).
    pub(crate) fn chosen(&mut self) {
        self.figures.evictions += 1;
    }

    /// A spill landed (f.5).
    pub(crate) fn spilled(&mut self, id: BufferId, bytes: u64) {
        self.figures.spills += 1;
        self.figures.spilled_bytes += bytes;
        self.events.push(CatalogEvent::Spilled { batch: id, bytes });
    }

    /// A read from disk landed (f.6).
    pub(crate) fn reloaded(&mut self, id: BufferId, bytes: u64) {
        self.figures.reloads += 1;
        self.figures.reloaded_bytes += bytes;
        self.events
            .push(CatalogEvent::Reloaded { batch: id, bytes });
    }

    /// A consumer reached `id` while eviction had it out of memory: one miss, weighted by its
    /// reload cost, counted once until the bytes leave memory again.
    pub(crate) fn miss(&mut self, id: BufferId) {
        let Some(buffer) = self.buffers.get_mut(&id) else {
            return;
        };
        if buffer.missed {
            return;
        }
        buffer.missed = true;
        let cost = buffer.reload_cost();
        self.figures.misses += 1;
        self.figures.miss_reload_bytes += cost;
    }

    /// A batch memory could not admit was pushed straight to disk (KD7b).
    pub(crate) fn overflowed(&mut self, bytes: u64) {
        self.figures.overflows += 1;
        self.figures.overflow_bytes += bytes;
    }

    /// A batch pushed `bytes` past the reservation made for it.
    pub(crate) fn overrun(&mut self, bytes: u64) {
        self.figures.overrun_bytes += bytes;
    }

    /// Every batch that may leave tier slot `slot` now, with how it would and what it would cost
    /// (f.18, PL-I19, PL-I20). A batch qualifies only when its bytes are in that tier with no move
    /// in flight, the catalog holds its payload, no running unit holds it, a consumer still waits
    /// for it, the queues' `views` say none of its consumers is about to take it, and, with
    /// `scope`, it waits in that queue. `host_slot` is the run's host tier; `disk` says whether
    /// the run has a disk tier at all (PL-I7).
    pub fn candidates(
        &self,
        slot: usize,
        scope: Option<StageId>,
        views: &[QueueView],
        disk: bool,
        demand: Option<BufferId>,
    ) -> Vec<Candidate> {
        let mut out = Vec::new();
        let Some(members) = self.members.get(slot) else {
            return out;
        };
        for buffer in members.iter().filter_map(|id| self.buffers.get(id)) {
            if buffer.payload.is_none() {
                continue;
            }
            let resident_only = match buffer.state {
                State::Resident(_) => true,
                State::ResidentOnDisk(_) => false,
                State::Promoting(_, _)
                | State::Demoting(_, _)
                | State::OnDisk(_)
                | State::Evicted
                | State::Consumed
                | State::OnRemote(_, _) => continue,
            };
            if !buffer.holders.is_empty() || buffer.consumers.is_empty() {
                continue;
            }
            if let Some(stage) = scope
                && !buffer.consumers.iter().any(|(s, _)| *s == stage)
            {
                continue;
            }
            let Some(future) = self.future(buffer.id, views) else {
                continue;
            };
            // A head a consumer waits for may take the room of batches about to be taken, whose
            // next use is later by definition (f.18 rule 13): every batch but itself and the
            // heads already read back on demand and not yet taken, which would otherwise take
            // each other's room in turn.
            let protected = match demand {
                Some(head) => buffer.id == head || buffer.missed,
                None => future.hot,
            };
            if protected {
                continue;
            }
            let device = slot == TierKind::Device.index();
            let how = if !resident_only && buffer.disk.is_some() {
                Shed::DropCopy
            } else if device {
                Shed::CopyDown
            } else if buffer.recomputable && !(future.all_staging && disk) {
                Shed::Recompute
            } else if future.any_staging && disk {
                Shed::Spill
            } else {
                continue;
            };
            let reload = match how {
                Shed::Spill => buffer.bytes.saturating_mul(2),
                Shed::DropCopy | Shed::Recompute | Shed::CopyDown => buffer.reload_cost(),
            };
            out.push(Candidate {
                id: buffer.id,
                bytes: buffer.bytes,
                next_use: future.next_use,
                reload,
                arrived: buffer.arrived,
                touched: buffer.touched,
                how,
            });
        }
        out
    }

    /// The figures of `CatalogStats` (contracts d.10), beside the budgets in force.
    pub fn stats(&self, budget_by_tier: [u64; TIER_COUNT]) -> CatalogStats {
        let f = &self.figures;
        CatalogStats {
            resident_by_tier: self.resident,
            peak_by_tier: self.peak,
            reserved_by_tier: self.reserved,
            budget_by_tier,
            held_bytes: self.held,
            buffers: self.buffers.len() as u64,
            spilled_bytes: f.spilled_bytes,
            spills: f.spills,
            reloaded_bytes: f.reloaded_bytes,
            reloads: f.reloads,
            evictions: f.evictions,
            misses: f.misses,
            miss_reload_bytes: f.miss_reload_bytes,
            released: f.released,
            freed: f.freed,
            overrun_bytes: f.overrun_bytes,
            overflows: f.overflows,
            overflow_bytes: f.overflow_bytes,
            outside_bytes: self.outside,
        }
    }
}

#[cfg(test)]
mod tests {
    //! PL-T29 to PL-T31 over the catalog alone: the eviction order, who may leave memory and
    //! how, and the totals (09 k).

    use std::sync::Arc;

    use moruna_kernel::arrow::array::{ArrayRef, Int32Array};
    use moruna_kernel::arrow::datatypes::{DataType, Field, Schema};
    use moruna_kernel::arrow::record_batch::RecordBatch;
    use moruna_kernel::{DeviceId, Tier};

    use super::*;

    const HOST: usize = 2;
    const DISK: usize = 3;

    fn payload(tier: Tier) -> EntryPayload {
        let column: ArrayRef = Arc::new(Int32Array::from(vec![1, 2, 3]));
        let schema = Arc::new(Schema::new(vec![Field::new("i", DataType::Int32, false)]));
        match RecordBatch::try_new(schema, vec![column]) {
            Ok(batch) => EntryPayload::Table(batch, tier),
            Err(e) => panic!("batch: {e}"),
        }
    }

    fn resident(bytes: u64, consumers: &[(StageId, Seq)]) -> NewBuffer {
        NewBuffer {
            bytes,
            kind: PayloadKind::Table,
            owner: Producer::Kernel(1),
            payload: Some(payload(Tier::Host)),
            state: State::Resident(Tier::Host),
            disk: None,
            recomputable: false,
            consumers: consumers.to_vec(),
            outside: false,
        }
    }

    fn placed(catalog: &mut Catalog, new: NewBuffer, ordinals: &[u64]) -> BufferId {
        let consumers = new.consumers.clone();
        let id = match catalog.insert(new) {
            Ok(id) => id,
            Err(e) => panic!("insert: {e}"),
        };
        for ((stage, seq), ordinal) in consumers.iter().zip(ordinals) {
            catalog.place(*stage, *seq, *ordinal);
        }
        id
    }

    fn views(queues: usize, window: u16, staging: bool) -> Vec<QueueView> {
        vec![
            QueueView {
                taken: 0,
                window,
                staging,
            };
            queues
        ]
    }

    fn candidate(id: u64, bytes: u64, next_use: u64, reload: u64) -> Candidate {
        Candidate {
            id: BufferId(id),
            bytes,
            next_use,
            reload,
            arrived: id,
            touched: id,
            how: Shed::Spill,
        }
    }

    /// PL-T29 known_future_order (PL-I20): furthest next use first, a cheaper reload per byte
    /// first among equals, then the larger batch, then the lower id; `pick` stops once the
    /// bytes reach the need.
    #[test]
    fn pl_t29_known_future_order() {
        let mut ranked = vec![
            candidate(1, 100, 2, 200),
            candidate(2, 100, 9, 200),
            candidate(3, 100, 5, 200),
            candidate(4, 100, 5, 100),
            candidate(5, 300, 2, 600),
            candidate(6, 100, 2, 200),
        ];
        KnownFuture.order(&mut ranked);
        let order: Vec<u64> = ranked.iter().map(|c| c.id.0).collect();
        // 4 (score 6) > 2 (score 5) > 3 (score 3) > 5 = 1 = 6 (score 1.5): the larger first,
        // then the lower id.
        assert_eq!(order, vec![4, 2, 3, 5, 1, 6]);
        assert_eq!(KnownFuture.name(), "known_future");
        // A batch with a disk copy already (reload = bytes) used as soon as one that must be
        // written (reload = 2 x bytes) goes first.
        let mut pair = vec![candidate(1, 100, 3, 200), candidate(2, 100, 3, 100)];
        KnownFuture.order(&mut pair);
        assert_eq!(pair[0].id, BufferId(2));
        // A zero reload cost does not divide by zero.
        let mut odd = vec![candidate(1, 0, 0, 0), candidate(2, 10, 0, 10)];
        KnownFuture.order(&mut odd);
        assert_eq!(odd.len(), 2);

        let ids = |chosen: Vec<Candidate>| chosen.iter().map(|c| c.id.0).collect::<Vec<_>>();
        assert_eq!(
            ids(pick(&ranked, 0)),
            Vec::<u64>::new(),
            "no need, nothing chosen"
        );
        assert_eq!(ids(pick(&ranked, 100)), vec![4]);
        assert_eq!(ids(pick(&ranked, 101)), vec![4, 2]);
        assert_eq!(
            ids(pick(&ranked, 10_000)).len(),
            6,
            "all of them when they do not reach"
        );
    }

    /// PL-T30 who_may_leave (PL-I19, PL-I20): a held batch, a batch no consumer waits for, a
    /// batch a consumer is about to take, one with a move in flight and one outside the scope
    /// never leave; how a batch leaves follows its disk copy, its tier, its recomputability, the
    /// queues' staging flags and whether the run has a disk tier.
    #[test]
    fn pl_t30_who_may_leave() {
        let mut catalog = Catalog::new(4);
        let v = views(4, 1, true);
        // The head of queue 0 is hot; position 3 of queue 0 is not.
        let head = placed(&mut catalog, resident(10, &[(0, 0)]), &[0]);
        let cold = placed(&mut catalog, resident(10, &[(0, 3)]), &[3]);
        let unplaced = match catalog.insert(resident(10, &[(1, 0)])) {
            Ok(id) => id,
            Err(e) => panic!("{e}"),
        };
        let ids = |c: &Catalog, scope| {
            c.candidates(HOST, scope, &v, true, None)
                .iter()
                .map(|c| c.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(&catalog, None),
            vec![cold],
            "the head is hot and an entry its queue has not placed has no future yet"
        );
        assert!(catalog.future(unplaced, &v).is_none());
        let f = catalog.future(cold, &v).expect("a future");
        assert_eq!((f.next_use, f.hot), (3, false));
        assert_eq!(catalog.future(head, &v).map(|f| f.hot), Some(true));

        // A shared batch is hot when any of its entries is: at the head of queue 2, deep in 3.
        let shared = placed(&mut catalog, resident(10, &[(2, 0), (3, 7)]), &[0, 7]);
        assert!(!ids(&catalog, None).contains(&shared));
        // Once queue 2 has taken past it ... it is no longer there; take it and it is held.
        let taken = catalog.take(2, 0).expect("take");
        assert!(
            taken.record.is_none(),
            "queue 3 still waits: nothing released"
        );
        assert!(
            !ids(&catalog, None).contains(&shared),
            "a batch a running unit holds never leaves (PL-I19)"
        );
        assert!(catalog.release(2, 0).expect("release").is_none());
        assert!(
            ids(&catalog, None).contains(&shared),
            "released, its next use is position 7 of queue 3"
        );
        assert_eq!(
            ids(&catalog, Some(3)),
            vec![shared],
            "scope: queue 3's alone"
        );
        assert_eq!(ids(&catalog, Some(1)), Vec::<BufferId>::new());

        // A move in flight: not a candidate.
        catalog.update(cold, |b| b.state = State::Demoting(Tier::Host, Tier::Host));
        assert!(!ids(&catalog, None).contains(&cold));
        catalog.update(cold, |b| b.state = State::Resident(Tier::Host));

        // How: a disk copy is dropped; a recomputable batch is dropped unless every queue stages
        // and there is a disk; a written one is spilled when a queue stages and there is a disk.
        let seg = SegmentRef {
            segment: 1,
            offset: 4096,
            len: 12,
        };
        catalog.update(cold, |b| {
            b.state = State::ResidentOnDisk(Tier::Host);
            b.disk = Some(seg);
        });
        let how = |c: &Catalog, v: &[QueueView], disk: bool, id: BufferId| {
            c.candidates(HOST, None, v, disk, None)
                .into_iter()
                .find(|c| c.id == id)
                .map(|c| (c.how, c.reload))
        };
        assert_eq!(how(&catalog, &v, true, cold), Some((Shed::DropCopy, 10)));
        assert_eq!(how(&catalog, &v, true, shared), Some((Shed::Spill, 20)));
        assert_eq!(
            how(&catalog, &v, false, shared),
            None,
            "no disk tier, not recomputable"
        );
        let off = views(4, 1, false);
        assert_eq!(
            how(&catalog, &off, true, shared),
            None,
            "staging off, not recomputable"
        );
        catalog.update(shared, |b| b.recomputable = true);
        assert_eq!(
            how(&catalog, &off, true, shared),
            Some((Shed::Recompute, 10))
        );
        assert_eq!(
            how(&catalog, &v, false, shared),
            Some((Shed::Recompute, 10))
        );
        assert_eq!(how(&catalog, &v, true, shared), Some((Shed::Spill, 20)));
        let mut mixed = views(4, 1, true);
        mixed[3].staging = false;
        assert_eq!(
            how(&catalog, &mixed, true, shared),
            Some((Shed::Recompute, 10)),
            "a source's batch is written only when every queue it waits in stages (PL-I6)"
        );

        // A device batch is copied down.
        let device = placed(
            &mut catalog,
            NewBuffer {
                state: State::Resident(Tier::Device(DeviceId(0))),
                payload: Some(payload(Tier::Device(DeviceId(0)))),
                ..resident(10, &[(1, 5)])
            },
            &[5],
        );
        let on_device = catalog.candidates(TierKind::Device.index(), None, &v, true, None);
        assert_eq!(on_device.len(), 1);
        assert_eq!(
            (on_device[0].id, on_device[0].how),
            (device, Shed::CopyDown)
        );

        // A head a consumer waits for on disk may take the room of hot batches (rule 13): the
        // hot head of queue 0 becomes a candidate, but not the waiting head itself, and not a
        // head already read back on demand and not yet taken.
        let for_demand = |c: &Catalog, demand| {
            c.candidates(HOST, None, &v, true, Some(demand))
                .iter()
                .map(|c| c.id)
                .collect::<Vec<_>>()
        };
        assert!(for_demand(&catalog, BufferId(9_999)).contains(&head));
        assert!(!for_demand(&catalog, head).contains(&head));
        catalog.update(head, |b| b.missed = true);
        assert!(!for_demand(&catalog, BufferId(9_999)).contains(&head));

        // A batch with no payload in the catalog (its last consumer took it) never leaves.
        let gone = placed(&mut catalog, resident(10, &[(1, 9)]), &[9]);
        catalog.take(1, 9).expect("take");
        assert!(!ids(&catalog, None).contains(&gone));
    }

    /// PL-T31 totals_follow_the_records (PL-I3, PL-I18, PL-I21): the tier totals, each queue's
    /// figure, the held bytes and the peak follow every insert, take, move and release; a batch
    /// is freed once, after its last holder releases it; the events come in order; a reservation
    /// never passes the budget; mistakes are refused by name.
    #[test]
    fn pl_t31_totals_follow_the_records() {
        let mut catalog = Catalog::new(3);
        let id = placed(&mut catalog, resident(100, &[(0, 4), (1, 4)]), &[0, 0]);
        assert_eq!(catalog.resident(HOST), 100, "a fan-out is resident once");
        assert_eq!(catalog.queue_bytes(0)[HOST], 100);
        assert_eq!(catalog.queue_bytes(1)[HOST], 100);
        assert_eq!(
            catalog.queue_bytes(9),
            [0; TIER_COUNT],
            "a queue that is not there"
        );
        assert!(
            catalog.insert(resident(5, &[(1, 4)])).is_err(),
            "a number is on a queue once"
        );
        assert!(
            catalog.insert(resident(5, &[(2, 8), (2, 8)])).is_err(),
            "one push names a queue once"
        );
        assert!(
            catalog.insert(resident(5, &[])).is_err(),
            "a batch has a consumer"
        );
        assert!(
            catalog.of_entry(2, 8).is_none(),
            "a refused insert records nothing"
        );

        // Spilled: the bytes move to the Disk slot.
        let seg = SegmentRef {
            segment: 2,
            offset: 4096,
            len: 100,
        };
        catalog.update(id, |b| {
            b.payload = None;
            b.state = State::OnDisk(seg);
            b.disk = Some(seg);
        });
        catalog.spilled(id, 100);
        assert_eq!((catalog.resident(HOST), catalog.resident(DISK)), (0, 100));
        assert_eq!(catalog.queue_bytes(0)[DISK], 100);
        // Reloaded: back in memory, the disk copy kept.
        catalog.update(id, |b| {
            b.payload = Some(payload(Tier::Host));
            b.state = State::ResidentOnDisk(Tier::Host);
        });
        catalog.reloaded(id, 100);
        catalog.miss(id);
        catalog.miss(id);
        assert_eq!((catalog.resident(HOST), catalog.resident(DISK)), (100, 0));

        let first = catalog.take(0, 4).expect("queue 0 takes");
        assert!(first.record.is_none(), "queue 1 still needs the disk copy");
        assert!(catalog.take(0, 4).is_err(), "a share is taken once");
        let stats = catalog.stats([0; TIER_COUNT]);
        assert_eq!((stats.held_bytes, catalog.queue_bytes(0)[HOST]), (100, 0));
        let last = catalog.take(1, 4).expect("queue 1 takes");
        assert_eq!(last.record, Some(seg), "the last take releases the record");
        assert_eq!(catalog.resident(HOST), 100, "held, still resident");
        assert!(catalog.release(2, 4).is_err(), "queue 2 holds nothing");
        assert_eq!(
            catalog.release(0, 4).expect("release"),
            None,
            "queue 1 holds it still"
        );
        assert!(catalog.release(0, 4).is_err(), "released once");
        assert_eq!(catalog.release(1, 4).expect("release"), Some(None), "freed");
        assert_eq!(catalog.resident(HOST), 0);
        assert!(catalog.get(id).is_none() && catalog.of_entry(1, 4).is_none());
        let events = catalog.drain_events();
        assert_eq!(
            events,
            vec![
                CatalogEvent::Spilled {
                    batch: id,
                    bytes: 100
                },
                CatalogEvent::Reloaded {
                    batch: id,
                    bytes: 100
                },
                CatalogEvent::Released {
                    batch: id,
                    stage: 0,
                    seq: 4
                },
                CatalogEvent::Released {
                    batch: id,
                    stage: 1,
                    seq: 4
                },
                CatalogEvent::Freed {
                    batch: id,
                    bytes: 100
                },
            ]
        );
        assert!(catalog.drain_events().is_empty());
        let stats = catalog.stats([1; TIER_COUNT]);
        assert_eq!(stats.peak_by_tier[HOST], 100);
        assert_eq!(stats.peak_by_tier[DISK], 100);
        assert_eq!(
            (
                stats.spills,
                stats.spilled_bytes,
                stats.reloads,
                stats.reloaded_bytes
            ),
            (1, 100, 1, 100)
        );
        assert_eq!(
            (stats.misses, stats.miss_reload_bytes),
            (1, 100),
            "one miss, counted once"
        );
        assert_eq!((stats.released, stats.freed, stats.buffers), (2, 1, 0));
        assert_eq!(stats.budget_by_tier, [1; TIER_COUNT]);
        assert!(catalog.take(1, 4).is_err(), "nothing is left to take");

        // Reservations: never past the budget.
        assert!(catalog.reserve(HOST, 60, 100));
        assert!(!catalog.reserve(HOST, 41, 100));
        assert!(catalog.reserve(HOST, 40, 100));
        catalog.unreserve(HOST, 100);
        assert_eq!(catalog.reserved(HOST), 0);
        catalog.overrun(7);
        catalog.dropped(BufferId(99), 3);
        catalog.chosen();
        let stats = catalog.stats([0; TIER_COUNT]);
        assert_eq!((stats.overrun_bytes, stats.evictions), (7, 2));
        catalog.wait_for_room(2);
        catalog.wait_for_room(1);
        catalog.wait_for_room(2);
        assert_eq!(catalog.take_waiting(), vec![1, 2]);
        assert!(catalog.take_waiting().is_empty());
        assert!(catalog.update(BufferId(12345), |_| ()).is_none());
        catalog.touch(BufferId(12345));
        catalog.miss(BufferId(12345));
        assert_eq!(catalog.buffers().len(), 0);
    }
}
