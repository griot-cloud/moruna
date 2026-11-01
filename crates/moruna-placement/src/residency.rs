//! The engine's side of the buffer catalog (09 f.18): shedding memory by the eviction order,
//! making room for a promotion or an admission, delivering the catalog's events, and the
//! contracts' [`Residency`] port (d.17) the scheduler and its policy drive (adaptive-runtime
//! 9.2, 9.4).
//!
//! Shedding reads the future from the queues (one queue lock at a time, released before the
//! next), then chooses and marks its victims under the catalog lock, then issues the moves with
//! no lock held (preamble 4.2, PL-I14).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use moruna_kernel::{
    BatchView, CatalogFloor, CatalogSnapshot, ClaimHolder, Head, Location, MorunaError,
    Reservation, Reserve, Residency, StageId, Tier, TierKind,
};

use crate::PlacementEngine;
use crate::catalog::{BufferId, Catalog, CatalogEvent, CatalogObserver, QueueView, Shed, pick};
use crate::moves::{PENDING_DISK, Task, TaskKind, consumer_stages};
use crate::queue::Admit;
use crate::state::{EntryPayload, State};

impl PlacementEngine {
    /// Take the catalog lock (preamble 4.2 position 3).
    pub(crate) fn lock_catalog(&self) -> crate::queue::Guarded<'_, Catalog> {
        crate::queue::Guarded::new(
            crate::locks::Held::enter(crate::locks::CATALOG),
            self.catalog.lock().unwrap_or_else(|e| e.into_inner()),
        )
    }

    /// Hand events to the observer, outside every lock (f.18).
    pub(crate) fn deliver(&self, events: Vec<CatalogEvent>) {
        if events.is_empty() {
            return;
        }
        let observer = self
            .observer
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for event in &events {
            tracing::trace!(target: "placement.catalog", event = ?event);
            if let Some(observer) = &observer {
                observer.event(event);
            }
        }
    }

    /// What every queue tells the catalog about its batches' future (f.18), one queue lock at a
    /// time.
    pub(crate) fn views(&self) -> Vec<QueueView> {
        (0..self.queues.len())
            .map(|index| {
                let queue = self.lock_queue(index);
                QueueView {
                    taken: queue.taken,
                    window: queue.promotion_window,
                    staging: queue.staging_enabled,
                }
            })
            .collect()
    }

    /// True when one of the batch's entries is inside its queue's promotion window.
    pub(crate) fn is_hot(&self, id: BufferId) -> bool {
        let views = self.views();
        self.lock_catalog()
            .future(id, &views)
            .is_some_and(|future| future.hot)
    }

    /// The run has a disk tier: a staging directory and a disk budget (PL-I7).
    fn has_disk(&self) -> bool {
        self.run_dir.is_some() && self.disk_budget.load(Ordering::Acquire) > 0
    }

    /// Free at least `need` bytes of tier slot `slot`, from the batches queue `scope` holds when
    /// given, else from any, in the eviction order (f.18). Batches that leave without IO are
    /// gone when this returns; spills and copies down are issued and free their bytes when they
    /// land. Returns whether any batch left or is on its way out.
    pub(crate) fn shed(
        &self,
        slot: usize,
        need: u64,
        scope: Option<StageId>,
        demand: Option<BufferId>,
    ) -> bool {
        if need == 0 || self.is_shutting_down() {
            return false;
        }
        let views = self.views();
        let disk = self.has_disk();
        let mut tasks = Vec::new();
        let (found, waiting, events, dropped) = {
            let mut catalog = self.lock_catalog();
            let mut candidates = catalog.candidates(slot, scope, &views, disk, demand);
            if candidates.is_empty() {
                return false;
            }
            self.order.order(&mut candidates);
            let mut dropped: Vec<(usize, bool)> = Vec::new();
            let mut freed_now = false;
            let mut moved = 0usize;
            for chosen in pick(&candidates, need) {
                let id = chosen.id;
                let stages = consumer_stages(&catalog, id);
                let Some(&first) = stages.first() else {
                    continue;
                };
                let attributed = usize::from(first);
                match chosen.how {
                    Shed::DropCopy => {
                        catalog.update(id, |buffer| {
                            if let Some(seg) = buffer.disk {
                                buffer.payload = None;
                                buffer.state = State::OnDisk(seg);
                                buffer.missed = false;
                            }
                        });
                        catalog.dropped(id, chosen.bytes);
                        dropped.push((attributed, false));
                        freed_now = true;
                        moved += 1;
                    }
                    Shed::Recompute => {
                        catalog.update(id, |buffer| {
                            buffer.payload = None;
                            buffer.state = State::Evicted;
                            buffer.disk = None;
                            buffer.missed = false;
                        });
                        catalog.dropped(id, chosen.bytes);
                        dropped.push((attributed, true));
                        freed_now = true;
                        moved += 1;
                    }
                    Shed::Spill | Shed::CopyDown => {
                        let Some(buffer) = catalog.get(id) else {
                            continue;
                        };
                        let Some(from) = buffer.state.resident_tier() else {
                            continue;
                        };
                        let Some(payload) = buffer.payload.as_ref().map(EntryPayload::reference)
                        else {
                            continue;
                        };
                        let (kind, bytes) = (buffer.kind, buffer.bytes);
                        let seq = buffer
                            .consumers
                            .iter()
                            .find(|(stage, _)| *stage == first)
                            .map(|(_, seq)| *seq)
                            .unwrap_or(0);
                        let (task_kind, target, reservation) = match chosen.how {
                            Shed::CopyDown => {
                                let down = self.host_tier;
                                let host = down.index();
                                if !catalog.reserve(host, bytes, self.budget_of(host)) {
                                    continue;
                                }
                                (
                                    TaskKind::Copy {
                                        from,
                                        to: down,
                                        payload,
                                    },
                                    down,
                                    Some((host, bytes)),
                                )
                            }
                            Shed::Spill | Shed::DropCopy | Shed::Recompute => (
                                TaskKind::Write {
                                    from,
                                    payload,
                                    kind,
                                },
                                Tier::Disk(PENDING_DISK),
                                None,
                            ),
                        };
                        let move_id = self.next_move_id();
                        catalog.update(id, |buffer| {
                            buffer.state = State::Demoting(from, target);
                            buffer.move_id = Some(move_id);
                        });
                        catalog.chosen();
                        tracing::debug!(target: "placement.demote", stage = first, seq, buffer = id.0, from = ?from, to = ?target, bytes, next_use = chosen.next_use);
                        moved += 1;
                        tasks.push(Task {
                            stage: attributed,
                            stage_id: first,
                            seq,
                            buffer: id,
                            move_id,
                            bytes,
                            promote: false,
                            reservation,
                            kind: task_kind,
                        });
                    }
                }
            }
            let waiting = if freed_now {
                catalog.take_waiting()
            } else {
                Vec::new()
            };
            (moved > 0, waiting, catalog.drain_events(), dropped)
        };
        for (queue, evicted) in dropped {
            let counter = if evicted {
                &self.counters[queue].evictions
            } else {
                &self.counters[queue].demotions
            };
            counter.fetch_add(1, Ordering::Relaxed);
        }
        self.deliver(events);
        for task in tasks {
            self.issue(task);
        }
        for queue in waiting {
            self.plan_and_issue(queue);
        }
        found
    }

    /// Bring tier slot `slot` back within its budget after bytes arrived (f.18): a push, a
    /// replace, a budget lowered.
    pub(crate) fn relieve(&self, slot: usize) {
        let over = {
            let catalog = self.lock_catalog();
            let held = catalog
                .resident(slot)
                .saturating_add(catalog.reserved(slot));
            held.saturating_sub(catalog.leaving(slot))
                .saturating_sub(self.budget_of(slot))
        };
        if over > 0 {
            self.shed(slot, over, None, None);
        }
    }

    /// Make room for `bytes` in tier slot `slot`: what a promotion or an admission that found
    /// no room asks for (f.18). Bytes a demotion in flight is taking out count as room on its
    /// way. True when the room is there or on its way.
    pub(crate) fn make_room(&self, slot: usize, bytes: u64, demand: Option<BufferId>) -> bool {
        let need = {
            let catalog = self.lock_catalog();
            let held = catalog
                .resident(slot)
                .saturating_add(catalog.reserved(slot));
            held.saturating_add(bytes)
                .saturating_sub(catalog.leaving(slot))
                .saturating_sub(self.budget_of(slot))
        };
        need == 0 || self.shed(slot, need, None, demand)
    }

    /// A claim this engine granted, which gives its bytes back here if it is dropped unused.
    fn claim(&self, slot: usize, bytes: u64) -> Reservation {
        let holder: std::sync::Weak<dyn ClaimHolder> = self.this.clone();
        Reservation::new(slot, bytes, holder)
    }

    /// A reservation given back unused.
    pub(crate) fn give_back(&self, slot: usize, bytes: u64) {
        let waiting = {
            let mut catalog = self.lock_catalog();
            catalog.unreserve(slot, bytes);
            catalog.take_waiting()
        };
        for queue in waiting {
            self.plan_and_issue(queue);
        }
    }

    /// The checks every claimed push and replace makes before it takes the claim over.
    fn claimed(
        &self,
        stages: &[StageId],
        morsel: &moruna_kernel::Morsel,
        reservation: &Reservation,
        what: &str,
    ) -> moruna_kernel::Result<()> {
        if stages.iter().any(|&s| usize::from(s) >= self.queues.len()) {
            return Err(MorunaError::Staging(format!(
                "{what}: a queue of {stages:?} is outside the run's {} queues",
                self.queues.len()
            )));
        }
        if morsel.payload.tier().index() != reservation.slot() {
            return Err(MorunaError::Staging(format!(
                "{what}: the batch is in {:?} and its reservation is in tier slot {}",
                morsel.payload.tier(),
                reservation.slot()
            )));
        }
        Ok(())
    }
}

impl ClaimHolder for PlacementEngine {
    fn give_back(&self, slot: usize, bytes: u64) {
        PlacementEngine::give_back(self, slot, bytes);
    }
}

/// How a policy reads a batch's state (contracts d.17).
fn location(state: &State) -> Location {
    match state {
        State::Resident(tier) => Location::Memory(*tier),
        State::ResidentOnDisk(tier) => Location::MemoryAndDisk(*tier),
        State::Promoting(from, to) | State::Demoting(from, to) => Location::Moving {
            from: from.kind(),
            to: to.kind(),
        },
        State::OnDisk(_) => Location::Disk,
        State::Evicted | State::Consumed => Location::Dropped,
        State::OnRemote(_, _) => Location::Remote,
    }
}

impl Residency for PlacementEngine {
    fn snapshot(&self) -> CatalogSnapshot {
        let views = self.views();
        let budgets = std::array::from_fn(|slot| self.budget_of(slot));
        let catalog = self.lock_catalog();
        CatalogSnapshot {
            batches: catalog
                .buffers()
                .into_iter()
                .map(|buffer| BatchView {
                    id: buffer.id,
                    bytes: buffer.bytes,
                    location: location(&buffer.state),
                    owner: buffer.owner,
                    consumers: buffer.consumers.clone(),
                    holders: buffer.holders.clone(),
                    reload_cost: buffer.reload_cost(),
                    future: catalog.future(buffer.id, &views),
                })
                .collect(),
            stats: catalog.stats(budgets),
        }
    }

    fn reserve(&self, tier: Tier, bytes: u64) -> moruna_kernel::Result<Reserve> {
        if !tier.is_resident() {
            return Err(MorunaError::Staging(format!(
                "reserve: {tier:?} is not a memory tier"
            )));
        }
        if tier.is_host() && tier != self.host_tier {
            return Err(MorunaError::Staging(format!(
                "reserve: wrong host tier {tier:?}; this run's host tier is {:?}",
                self.host_tier
            )));
        }
        let slot = tier.index();
        let budget = self.budget_of(slot);
        if bytes > budget {
            return Ok(Reserve::Refused {
                need: bytes,
                budget,
            });
        }
        let claim = |engine: &PlacementEngine| {
            let mut catalog = engine.lock_catalog();
            let granted = catalog.reserve(slot, bytes, budget);
            let free = budget
                .saturating_sub(catalog.resident(slot))
                .saturating_sub(catalog.reserved(slot));
            (granted, bytes.saturating_sub(free))
        };
        let (granted, _) = claim(self);
        if granted {
            return Ok(Reserve::Granted(self.claim(slot, bytes)));
        }
        let shed = self.make_room(slot, bytes, None);
        let (granted, need) = claim(self);
        if granted {
            return Ok(Reserve::Granted(self.claim(slot, bytes)));
        }
        if shed {
            Ok(Reserve::Pending { need })
        } else {
            Ok(Reserve::Refused { need, budget })
        }
    }

    fn head(&self, stage: StageId) -> Option<Head> {
        if usize::from(stage) >= self.queues.len() {
            return None;
        }
        self.head_of(usize::from(stage))
    }

    fn push_reserved(
        &self,
        stages: &[StageId],
        morsel: moruna_kernel::Morsel,
        reservation: Reservation,
    ) -> moruna_kernel::Result<()> {
        self.claimed(stages, &morsel, &reservation, "push")?;
        let (slot, bytes) = reservation.into_claim();
        let result = self.push_into(stages, morsel, Admit::Claimed(slot, bytes));
        if result.is_err() {
            self.give_back(slot, bytes);
        }
        result
    }

    fn replace_reserved(
        &self,
        stage: StageId,
        morsel: moruna_kernel::Morsel,
        reservation: Reservation,
    ) -> moruna_kernel::Result<()> {
        self.claimed(&[stage], &morsel, &reservation, "replace")?;
        let (slot, bytes) = reservation.into_claim();
        let result = self.replace_entry(usize::from(stage), morsel, Some((slot, bytes)));
        if result.is_err() {
            self.give_back(slot, bytes);
        }
        result
    }

    fn push_spilled(
        &self,
        stages: &[StageId],
        morsel: moruna_kernel::Morsel,
    ) -> moruna_kernel::Result<()> {
        if stages.iter().any(|&s| usize::from(s) >= self.queues.len()) {
            return Err(MorunaError::Staging(format!(
                "push: a queue of {stages:?} is outside the run's {} queues",
                self.queues.len()
            )));
        }
        if !self.has_disk() {
            return Err(MorunaError::Staging(format!(
                "push: morsel {} ({} bytes) does not fit the memory budget and the run has no \
                 disk tier to write it to",
                morsel.seq, morsel.bytes
            )));
        }
        self.push_into(stages, morsel, Admit::Outside)
    }

    fn shed(&self, tier: TierKind, bytes: u64) -> bool {
        PlacementEngine::shed(self, tier.index(), bytes, None, None)
    }

    fn spill(&self, batch: BufferId) -> moruna_kernel::Result<()> {
        if !self.has_disk() {
            return Err(MorunaError::Staging(
                "spill: the run has no disk tier".into(),
            ));
        }
        let task = {
            let mut catalog = self.lock_catalog();
            let Some(buffer) = catalog.get(batch) else {
                return Err(MorunaError::Staging(format!("spill: no batch {}", batch.0)));
            };
            if !buffer.holders.is_empty() {
                return Err(MorunaError::Staging(format!(
                    "spill: batch {} is held by a running unit",
                    batch.0
                )));
            }
            // A batch with neither a consumer nor a holder left is freed when its last holder
            // releases it, so the catalog holds none to spill (PL-I19).
            let from = match buffer.state {
                State::Resident(tier) if tier.is_host() => tier,
                ref other => {
                    return Err(MorunaError::Staging(format!(
                        "spill: batch {} is {other:?}, not resident in the host tier alone",
                        batch.0
                    )));
                }
            };
            let Some(payload) = buffer.payload.as_ref().map(EntryPayload::reference) else {
                return Err(MorunaError::Staging(format!(
                    "spill: batch {} has no bytes in memory",
                    batch.0
                )));
            };
            let (first, seq) = buffer.consumers.iter().min().copied().unwrap_or((0, 0));
            let (kind, bytes) = (buffer.kind, buffer.bytes);
            let move_id = self.next_move_id();
            catalog.update(batch, |buffer| {
                buffer.state = State::Demoting(from, Tier::Disk(PENDING_DISK));
                buffer.move_id = Some(move_id);
            });
            catalog.chosen();
            Task {
                stage: usize::from(first),
                stage_id: first,
                seq,
                buffer: batch,
                move_id,
                bytes,
                promote: false,
                reservation: None,
                kind: TaskKind::Write {
                    from,
                    payload,
                    kind,
                },
            }
        };
        self.issue(task);
        Ok(())
    }

    fn reload(&self, batch: BufferId) -> moruna_kernel::Result<()> {
        let to = self.host_tier;
        let slot = to.index();
        let task = {
            let mut catalog = self.lock_catalog();
            let Some(buffer) = catalog.get(batch) else {
                return Err(MorunaError::Staging(format!(
                    "reload: no batch {}",
                    batch.0
                )));
            };
            let State::OnDisk(seg) = buffer.state else {
                return Err(MorunaError::Staging(format!(
                    "reload: batch {} is {:?}, not on disk",
                    batch.0, buffer.state
                )));
            };
            let bytes = buffer.bytes;
            let (first, seq) = buffer.consumers.iter().min().copied().unwrap_or((0, 0));
            if !catalog.reserve(slot, bytes, self.budget_of(slot)) {
                return Err(MorunaError::Staging(format!(
                    "reload: batch {} needs {bytes} bytes of {to:?} and they are not free",
                    batch.0
                )));
            }
            let move_id = self.next_move_id();
            catalog.update(batch, |buffer| {
                buffer.state = State::Promoting(Tier::Disk(seg), to);
                buffer.move_id = Some(move_id);
            });
            Task {
                stage: usize::from(first),
                stage_id: first,
                seq,
                buffer: batch,
                move_id,
                bytes,
                promote: true,
                reservation: Some((slot, bytes)),
                kind: TaskKind::Read { seg, to },
            }
        };
        self.issue(task);
        Ok(())
    }

    fn floor(&self, tier: Tier, batch: u64) -> CatalogFloor {
        let windows: u64 = (0..self.queues.len())
            .map(|index| u64::from(self.lock_queue(index).promotion_window.max(1)))
            .sum();
        CatalogFloor::of(
            self.queues.len() as u64,
            windows,
            batch,
            self.budget_of(tier.index()),
        )
    }

    fn observe(&self, observer: Arc<dyn CatalogObserver>) {
        let mut held = self.observer.write().unwrap_or_else(|e| e.into_inner());
        *held = Some(observer);
    }
}
