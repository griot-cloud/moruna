//! Issuing moves and observing them (e.4, f.2, f.5, f.6, f.10, f.18).
//!
//! A move is a batch's, not an entry's: it is keyed by the batch's id in the catalog, and when it
//! lands every queue the batch waits in is woken and planned (PL-I18). The planner decides under
//! the queue lock and the catalog lock; issuing happens with no lock held, because
//! every reactor call may resolve on the calling thread and its `then` callback takes the
//! queue lock (preamble 4.2, g). A completion runs the callback on the reactor thread that
//! resolved it, which is how the engine observes moves without a thread of its own (PL-I14).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use moruna_kernel::{
    Buffer, BufferView, CopyDst, CopySrc, MorselFeatures, MorunaError, Payload, PayloadKind,
    SegmentRef, Seq, StageId, Tier, TierKind,
};

use crate::PlacementEngine;
use crate::catalog::{BufferId, Catalog};
use crate::plan::{EntryView, PlanInput, plan};
use crate::queue::Queue;
use crate::staging::read::decode_record;
use crate::staging::segment::{RecordHeader, round_up};
use crate::staging::write::{Piece, draft, pieces};
use crate::state::{EntryPayload, PayloadRef, State};

/// A placeholder `Disk` tier for an entry whose record has not been placed yet; the real
/// `SegmentRef` is known only once the write lands (f.5).
pub(crate) const PENDING_DISK: SegmentRef = SegmentRef {
    segment: 0,
    offset: 0,
    len: 0,
};

/// What an in-flight move is doing, kept so that a first failure can re-issue the identical
/// operation exactly once (f.10).
pub enum InFlight {
    /// A demotion writing a record.
    Write {
        /// The segment file.
        path: PathBuf,
        /// Its global number.
        segment: u32,
        /// Where the body begins.
        body_offset: u64,
        /// Body bytes.
        payload_len: u64,
        /// The tier the bytes are in, and stay in until the write lands (RE-I1).
        from: Tier,
        /// The record's pieces; a retry writes the same bytes at the same offsets.
        list: Arc<Vec<Piece>>,
    },
    /// A DMA between two resident tiers.
    Copy {
        /// Where the bytes are.
        from: Tier,
        /// Where they are going.
        to: Tier,
        /// The payload, so a retry rebuilds the identical source view.
        payload: PayloadRef,
    },
    /// A promotion out of a segment.
    Read {
        /// The segment file.
        path: PathBuf,
        /// Where the record's header page starts.
        record_start: u64,
        /// Bytes to read: the header page and the page-padded body.
        len: u64,
        /// Bytes the record actually occupies: the header page and the body. A segment file
        /// is preallocated, so a real read returns `len`; an in-memory fake file ends at the
        /// last byte written, which is `need`.
        need: u64,
        /// The tier this read lands in.
        into: Tier,
        /// The tier the whole promotion ends in; a device destination is a second step.
        to: Tier,
    },
}

/// One move the engine has issued and not yet seen complete (e.2).
pub struct MoveInFlight {
    /// The queue the move is counted in: the batch's first consumer's (f.18).
    pub stage: usize,
    /// The batch.
    pub buffer: BufferId,
    /// What it is doing.
    pub op: InFlight,
    /// Completions still outstanding.
    pub pieces_left: usize,
    /// True once any completion of this move has resolved `Err`.
    pub failed: bool,
    /// 0 for the first attempt, 1 for the one retry f.10 allows.
    pub attempt: u8,
    /// The reservation to release when the move settles, if any.
    pub reservation: Option<(usize, u64)>,
    /// The batch's payload bytes.
    pub bytes: u64,
    /// True for a promotion, false for a demotion.
    pub promote: bool,
}

/// What one planning pass of a queue decided (f.2, f.18).
struct Pass {
    /// The promotions to issue.
    tasks: Vec<Task>,
    /// Per tier, the bytes the catalog is asked to shed from the queue's batches.
    pressure: Vec<(TierKind, u64)>,
    /// The first reservation that found no room: the tier slot and the bytes.
    short: Option<(usize, u64, Option<BufferId>)>,
}

/// What a planning or shedding pass hands the issuer: everything a move needs, and no lock.
pub struct Task {
    pub(crate) stage: usize,
    pub(crate) stage_id: StageId,
    pub(crate) seq: Seq,
    pub(crate) buffer: BufferId,
    pub(crate) move_id: u64,
    pub(crate) bytes: u64,
    pub(crate) promote: bool,
    pub(crate) reservation: Option<(usize, u64)>,
    pub(crate) kind: TaskKind,
}

/// Why a move could not be issued at all. Only the disk bound of f.2 step 4 turns a
/// recomputable entry into an `Evicted` one; any other failure leaves the bytes where they
/// are and is reported like a failed move (f.10).
enum Refusal {
    /// No segment could open within the disk budget (PL-I7).
    DiskBound(MorunaError),
    /// Anything else.
    Other(MorunaError),
}

impl Refusal {
    fn error(&self) -> &MorunaError {
        match self {
            Refusal::DiskBound(error) | Refusal::Other(error) => error,
        }
    }
}

/// The identical operation a retry re-issues (f.10).
enum Redo {
    /// The record's pieces, at the same offsets.
    Write(PathBuf, Arc<Vec<Piece>>, usize),
    /// The same source view over the same bytes, into the same tier.
    Copy(PayloadRef, Tier),
    /// The same record range of the same segment.
    Read(PathBuf, u64, u64),
}

pub(crate) enum TaskKind {
    Write {
        from: Tier,
        payload: PayloadRef,
        kind: PayloadKind,
    },
    Copy {
        from: Tier,
        to: Tier,
        payload: PayloadRef,
    },
    Read {
        seg: SegmentRef,
        to: Tier,
    },
}

impl PlacementEngine {
    /// Take the moves map lock (preamble 4.2 position 3).
    pub(crate) fn lock_moves(
        &self,
    ) -> crate::queue::Guarded<'_, std::collections::HashMap<u64, MoveInFlight>> {
        crate::queue::Guarded::new(
            crate::locks::Held::enter(crate::locks::MOVES),
            self.moves.lock().unwrap_or_else(|e| e.into_inner()),
        )
    }

    /// Plan one queue and issue what it decided (f.2), then ask the catalog to shed what the
    /// queue holds above its water marks and to make room for a promotion that found none
    /// (f.18). No lock is held across a reactor or allocator call, without exception
    /// (preamble 4.2).
    pub(crate) fn plan_and_issue(&self, index: usize) {
        if self.is_shutting_down() {
            return;
        }
        let (
            Pass {
                tasks,
                pressure,
                short,
            },
            target,
            epoch,
        ) = {
            let mut queue = self.lock_queue(index);
            queue.epoch += 1;
            let pass = self.plan_locked(index, &mut queue);
            (pass, queue.target, queue.epoch)
        };
        for task in tasks {
            self.issue(task);
        }
        let stage = index as StageId;
        for (kind, need) in pressure {
            let found = self.shed(kind.index(), need, Some(stage), None);
            if !found && kind == target.kind() {
                // Recorded only when no later pass has looked at the queue since this one did:
                // a pop or a landing in between planned it again, and its finding stands (f.14).
                let mut queue = self.lock_queue(index);
                if queue.epoch == epoch {
                    queue.blocked = true;
                }
            }
        }
        if let Some((slot, bytes, demand)) = short {
            self.make_room(slot, bytes, demand);
        }
    }

    /// Plan every queue: what a budget or a knob change triggers (f.8).
    pub(crate) fn plan_every_queue(&self) {
        for index in 0..self.queues.len() {
            self.plan_and_issue(index);
        }
    }

    /// Build the planner's input from a queue and the catalog (f.2).
    fn plan_input(&self, index: usize, queue: &Queue, catalog: &Catalog) -> PlanInput {
        // Where the controller has not set a mark, f.8's default: high is the tier's budget
        // divided by the number of queues, low is half of that.
        let queues = self.queues.len() as u64;
        let mut water = queue.water;
        for (slot, mark) in water.iter_mut().enumerate() {
            if !queue.water_set[slot] {
                let high = self.budget_of(slot) / queues.max(1);
                *mark = (high / 2, high);
            }
        }
        // The planner reads the head and the promotion window and nothing behind them: which
        // batches leave memory is the catalog's choice (f.18), so a plan costs the window, not
        // the queue.
        let window = usize::from(queue.promotion_window).max(1);
        PlanInput {
            entries: queue
                .order
                .iter()
                .take(window)
                .map(|entry| match catalog.get(entry.buffer) {
                    Some(buffer) => EntryView {
                        tier: buffer.state.resident_tier(),
                        on_disk: matches!(buffer.state, State::OnDisk(_)),
                        in_flight: buffer.state.in_flight(),
                        evicted: matches!(buffer.state, State::Evicted),
                        on_remote: matches!(buffer.state, State::OnRemote(_, _)),
                    },
                    None => {
                        // A queued entry always names a batch the catalog holds (PL-I18); an
                        // entry that does not is inert here and refused by `pop`.
                        tracing::error!(target: "placement.catalog", stage = index, seq = entry.seq, "an entry names no batch");
                        EntryView {
                            tier: None,
                            on_disk: false,
                            in_flight: true,
                            evicted: false,
                            on_remote: false,
                        }
                    }
                })
                .collect(),
            target: queue.target,
            want: queue.consumer,
            host_tier: self.host_tier,
            water,
            bytes: {
                // What is already on its way out does not count toward the pressure again.
                let held = catalog.queue_bytes(index as StageId);
                let leaving = catalog.queue_leaving(index as StageId);
                std::array::from_fn(|slot| held[slot].saturating_sub(leaving[slot]))
            },
            window: queue.promotion_window,
        }
    }

    /// Run the planner and claim what each promotion needs; return the promotions to issue
    /// once the locks are released, the pressure the catalog must relieve, and the first
    /// reservation that found no room (f.2, f.18).
    fn plan_locked(&self, index: usize, queue: &mut Queue) -> Pass {
        let mut catalog = self.lock_catalog();
        let input = self.plan_input(index, queue, &catalog);
        let decided = plan(&input);
        queue.blocked = false;
        if decided.head_evicted
            && let Some(head) = queue.order.front()
        {
            // The consumer has reached a batch eviction dropped: a miss (f.18).
            catalog.miss(head.buffer);
        }
        let stage_id = queue.stage;
        let mut tasks = Vec::new();
        let mut short = None;
        let mut window_stopped = false;
        for intent in decided.intents {
            if !intent.head && window_stopped {
                continue;
            }
            let Some(entry) = queue.order.get(intent.index) else {
                continue;
            };
            let (id, seq) = (entry.buffer, entry.seq);
            let Some(bytes) = catalog.get(id).map(|buffer| buffer.bytes) else {
                continue;
            };
            let to = intent.to;
            let slot = to.index();
            let budget = self.budget_of(slot);
            if !catalog.reserve(slot, bytes, budget) {
                if intent.head && bytes > budget {
                    queue.head_error = Some(MorunaError::Staging(format!(
                        "head cannot fit in {to:?}: need {bytes}, budget {budget}"
                    )));
                } else {
                    catalog.wait_for_room(index);
                    // A head's room may come from batches about to be taken (f.18 rule 13).
                    short.get_or_insert((slot, bytes, intent.head.then_some(id)));
                }
                if !intent.head {
                    window_stopped = true;
                }
                continue;
            }
            let Some(buffer) = catalog.get(id) else {
                catalog.unreserve(slot, bytes);
                continue;
            };
            let state = buffer.state.clone();
            let reference = buffer.payload.as_ref().map(EntryPayload::reference);
            let (kind, from) = match &state {
                State::OnDisk(seg) => (TaskKind::Read { seg: *seg, to }, Tier::Disk(*seg)),
                State::Resident(at) | State::ResidentOnDisk(at) => {
                    let at = *at;
                    match reference {
                        Some(payload) if at != to => (
                            TaskKind::Copy {
                                from: at,
                                to,
                                payload,
                            },
                            at,
                        ),
                        Some(_) | None => {
                            catalog.unreserve(slot, bytes);
                            continue;
                        }
                    }
                }
                State::Promoting(_, _)
                | State::Demoting(_, _)
                | State::Evicted
                | State::Consumed
                | State::OnRemote(_, _) => {
                    catalog.unreserve(slot, bytes);
                    continue;
                }
            };
            if intent.head && matches!(from, Tier::Disk(_)) {
                // The consumer has reached a batch eviction put on disk: a reload on demand,
                // and a miss weighted by its cost (f.18).
                catalog.miss(id);
            }
            let move_id = self.next_move_id();
            catalog.update(id, |buffer| {
                buffer.state = State::Promoting(from, to);
                buffer.move_id = Some(move_id);
            });
            tracing::debug!(target: "placement.promote", stage = stage_id, seq, from = ?from, to = ?to, bytes);
            tasks.push(Task {
                stage: index,
                stage_id,
                seq,
                buffer: id,
                move_id,
                bytes,
                promote: true,
                reservation: Some((slot, bytes)),
                kind,
            });
        }
        Pass {
            tasks,
            pressure: decided.pressure,
            short,
        }
    }

    /// Issue one move. No engine lock is held here (g).
    pub(crate) fn issue(&self, task: Task) {
        let outcome = match &task.kind {
            TaskKind::Write {
                from,
                payload,
                kind,
            } => self.issue_write(&task, *from, payload.clone(), *kind),
            TaskKind::Copy { from, to, payload } => self
                .issue_copy(&task, *from, *to, payload.clone())
                .map_err(Refusal::Other),
            TaskKind::Read { seg, to } => self.issue_read(&task, *seg, *to).map_err(Refusal::Other),
        };
        if let Err(refusal) = outcome {
            self.abandon(&task, refusal);
        }
    }

    /// Demotion to disk (f.5): lay the record out, find room for it, then write its pieces.
    fn issue_write(
        &self,
        task: &Task,
        from: Tier,
        payload: PayloadRef,
        kind: PayloadKind,
    ) -> Result<(), Refusal> {
        let page = self.page_bytes;
        let laid_out = draft(&payload, page, &*self.alloc).map_err(Refusal::Other)?;
        let total = laid_out.total(page);
        let (segment, path, record_start) = self
            .reserve_record(task.stage_id, total)
            .map_err(Refusal::DiskBound)?;
        let body_offset = record_start + page;
        let payload_len = laid_out.payload_len;
        let mut header = self
            .alloc
            .alloc(page as usize, self.host_tier)
            .map_err(Refusal::Other)?;
        RecordHeader {
            seq: task.seq,
            stage: task.stage_id,
            kind,
            codec: self.cfg.codec,
            payload_len,
            body_offset,
        }
        .write(&mut header)
        .map_err(Refusal::Other)?;
        let list = Arc::new(pieces(laid_out, header, record_start, page));
        let count = list.len();
        self.install_move(
            task,
            InFlight::Write {
                path: path.clone(),
                segment,
                body_offset,
                payload_len,
                from,
                list: Arc::clone(&list),
            },
            count,
        );
        self.submit_write(task.move_id, &path, &list)
            .map_err(Refusal::Other)
    }

    /// A DMA between two resident tiers (e.4).
    fn issue_copy(
        &self,
        task: &Task,
        from: Tier,
        to: Tier,
        payload: PayloadRef,
    ) -> moruna_kernel::Result<()> {
        self.refuse_remote(from)?;
        self.refuse_remote(to)?;
        self.install_move(
            task,
            InFlight::Copy {
                from,
                to,
                payload: payload.clone(),
            },
            1,
        );
        self.submit_copy(task.move_id, &payload, to)
    }

    /// A promotion out of a segment (f.6). A device destination is the two rows
    /// `Disk -> host tier` and `host tier -> Device`, the second issued from the first's
    /// `then` (e.4).
    fn issue_read(&self, task: &Task, seg: SegmentRef, to: Tier) -> moruna_kernel::Result<()> {
        self.refuse_remote(to)?;
        let page = self.page_bytes;
        let Some(path) = self.segment_path(seg.segment) else {
            return Err(MorunaError::Staging(
                "promotion from disk without a staging directory".into(),
            ));
        };
        if seg.offset < page {
            return Err(MorunaError::Staging(format!(
                "record at offset {} has no header page before it",
                seg.offset
            )));
        }
        let record_start = seg.offset - page;
        let len = page + round_up(seg.len, page);
        self.install_move(
            task,
            InFlight::Read {
                path: path.clone(),
                record_start,
                len,
                need: page + seg.len,
                into: self.host_tier,
                to,
            },
            1,
        );
        self.submit_read(task.move_id, &path, record_start, len)
    }

    /// Reserved (E11): no v1 path produces a `Remote` endpoint.
    fn refuse_remote(&self, tier: Tier) -> moruna_kernel::Result<()> {
        match tier {
            Tier::Remote(_, _) => Err(MorunaError::Unsupported("rdma")),
            Tier::Device(_) | Tier::PinnedHost | Tier::Host | Tier::Disk(_) => Ok(()),
        }
    }

    /// Put a move in the moves map before its first completion can arrive (g).
    fn install_move(&self, task: &Task, op: InFlight, pieces_left: usize) {
        let mut moves = self.lock_moves();
        moves.insert(
            task.move_id,
            MoveInFlight {
                stage: task.stage,
                buffer: task.buffer,
                op,
                pieces_left,
                failed: false,
                attempt: 0,
                reservation: task.reservation,
                bytes: task.bytes,
                promote: task.promote,
            },
        );
        drop(moves);
        self.in_flight_bytes.fetch_add(task.bytes, Ordering::AcqRel);
    }

    fn submit_write(&self, move_id: u64, path: &Path, list: &[Piece]) -> moruna_kernel::Result<()> {
        let mut views = Vec::with_capacity(list.len());
        for piece in list {
            views.push((piece.offset, piece.view(&*self.alloc)?));
        }
        for (offset, view) in views {
            let completion = self.reactor.write_file(path, offset, view);
            let engine = self.handle();
            completion.then(Box::new(move |result| {
                if let Some(engine) = engine {
                    engine.on_piece(move_id, result);
                }
            }));
        }
        Ok(())
    }

    fn submit_read(
        &self,
        move_id: u64,
        path: &Path,
        offset: u64,
        len: u64,
    ) -> moruna_kernel::Result<()> {
        let buffer = self.alloc.alloc(len as usize, self.host_tier)?;
        let completion = self.reactor.read_file_opt(path, offset, buffer, true);
        let engine = self.handle();
        completion.then(Box::new(move |result| {
            if let Some(engine) = engine {
                engine.on_read(move_id, result);
            }
        }));
        Ok(())
    }

    fn submit_copy(
        &self,
        move_id: u64,
        payload: &PayloadRef,
        to: Tier,
    ) -> moruna_kernel::Result<()> {
        let source = Self::source_view(payload)?;
        let bytes = source.len();
        let destination = self.alloc.alloc(bytes, to)?;
        let completion = self
            .reactor
            .copy(CopySrc::View(source), CopyDst::Buffer(destination));
        let engine = self.handle();
        completion.then(Box::new(move |result| {
            if let Some(engine) = engine {
                engine.on_copy(move_id, result);
            }
        }));
        Ok(())
    }

    /// The DMA source for a payload moving between resident tiers.
    ///
    /// A tensor's bytes are one contiguous range and come back over the destination buffer
    /// with `ManagedTensor::from_buffer` (contracts d.4). A table cannot: rebuilding a
    /// `RecordBatch` over buffers the engine allocated needs either `unsafe` in this crate,
    /// which section l forbids, or a safe constructor the contracts do not have. That is the
    /// E10 item this crate reports; the disk rows are unaffected, because a record goes
    /// through `ipc::encode_framing` and `ipc::decode`, which rebuild the batch for us.
    fn source_view(payload: &PayloadRef) -> moruna_kernel::Result<BufferView> {
        match payload {
            PayloadRef::Tensor(tensor) => BufferView::of_tensor(tensor),
            PayloadRef::Table(_) => Err(MorunaError::Staging(
                "a device move of a table needs a safe way to rebuild a RecordBatch over new \
                 buffers (09 e.4, reported as E10)"
                    .into(),
            )),
        }
    }

    /// A move that could not even be issued: put the batch back where its bytes are, and
    /// handle the disk bound of f.2 step 4.
    fn abandon(&self, task: &Task, refusal: Refusal) {
        let error = refusal.error();
        let disk_bound = matches!(refusal, Refusal::DiskBound(_));
        {
            let mut moves = self.lock_moves();
            if moves.remove(&task.move_id).is_some() {
                drop(moves);
                self.in_flight_bytes
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                        Some(held.saturating_sub(task.bytes))
                    })
                    .ok();
            }
        }
        let is_write = matches!(task.kind, TaskKind::Write { .. });
        let (consumers, evicted, outside, events) = {
            let mut catalog = self.lock_catalog();
            if let Some((slot, bytes)) = task.reservation {
                catalog.unreserve(slot, bytes);
            }
            let current = catalog
                .get(task.buffer)
                .is_some_and(|buffer| buffer.move_id == Some(task.move_id));
            if !current {
                return;
            }
            let recomputable = catalog
                .get(task.buffer)
                .is_some_and(|buffer| buffer.recomputable);
            let evicted = is_write && disk_bound && recomputable;
            let outside = catalog
                .get(task.buffer)
                .is_some_and(|buffer| buffer.outside);
            catalog.update(task.buffer, |buffer| {
                buffer.move_id = None;
                if evicted {
                    // No room for the record and the bytes can be read again (f.2 step 4).
                    buffer.payload = None;
                    buffer.state = State::Evicted;
                    buffer.disk = None;
                    buffer.missed = false;
                } else {
                    buffer.state = buffer.state.source_state(buffer.disk);
                }
            });
            if evicted {
                catalog.dropped(task.buffer, task.bytes);
            }
            let consumers = consumer_stages(&catalog, task.buffer);
            (consumers, evicted, outside, catalog.drain_events())
        };
        if evicted {
            self.counters[task.stage]
                .evictions
                .fetch_add(1, Ordering::Relaxed);
        } else {
            for &stage in &consumers {
                let mut queue = self.lock_queue(usize::from(stage));
                let head = queue
                    .order
                    .front()
                    .is_some_and(|entry| entry.buffer == task.buffer);
                if (is_write && disk_bound) || outside {
                    // Not recomputable and no room: the run cannot continue on the queues this
                    // batch waits in, wherever it is in them. `is_full` holds and the next `pop`
                    // carries the totals (PL-I7, f.2 step 4, f.14).
                    queue.blocked = true;
                    if queue.head_error.is_none() {
                        queue.head_error = Some(head_error_of(error));
                    }
                } else if head && queue.head_error.is_none() {
                    queue.head_error = Some(head_error_of(error));
                }
            }
        }
        tracing::warn!(target: "placement.move_failed", stage = task.stage, seq = task.seq, error = %error, issued = false);
        self.deliver(events);
        for stage in consumers {
            self.unpark_all(usize::from(stage));
        }
    }

    /// Take a move out of the map once its last completion has arrived (g). Returns the
    /// move only when nothing of it is outstanding.
    fn settle(&self, move_id: u64, failed: bool) -> Option<MoveInFlight> {
        let mut moves = self.lock_moves();
        let entry = moves.get_mut(&move_id)?;
        if failed {
            entry.failed = true;
        }
        entry.pieces_left = entry.pieces_left.saturating_sub(1);
        if entry.pieces_left > 0 {
            return None;
        }
        let done = moves.remove(&move_id);
        drop(moves);
        if let Some(done) = &done {
            self.in_flight_bytes
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                    Some(held.saturating_sub(done.bytes))
                })
                .ok();
        }
        done
    }

    /// One piece of a record write resolved (f.5).
    fn on_piece(&self, move_id: u64, result: moruna_kernel::Result<()>) {
        let Some(done) = self.settle(move_id, result.is_err()) else {
            return;
        };
        if done.failed {
            self.after_failure(move_id, done);
            return;
        }
        let InFlight::Write {
            segment,
            body_offset,
            payload_len,
            from,
            ..
        } = &done.op
        else {
            return;
        };
        let seg = SegmentRef {
            segment: *segment,
            offset: *body_offset,
            len: *payload_len,
        };
        let from = *from;
        let (stage, id, bytes) = (done.stage, done.buffer, done.bytes);
        // The record's pieces hold a shared handle on the batch's own payload. It is released
        // before the batch is touched, so nothing outlives the write (d.3, d.4).
        drop(done);
        // Pops may have moved the batch into a consumer's promotion window while its record was
        // in flight. Its bytes are still resident (the reactor was given a view, RE-I1), so the
        // record is kept and so is the resident copy: the batch becomes `Resident + OnDisk` and
        // the consumer never waits on a read of bytes that are already here (PL-I1, G-I3). The
        // next shedding pass drops the resident copy for free if it leaves the window (f.7).
        let hot = self.is_hot(id);
        let (consumers, waiting, events) = {
            let mut catalog = self.lock_catalog();
            let current = catalog
                .get(id)
                .is_some_and(|buffer| buffer.move_id == Some(move_id));
            if !current {
                return;
            }
            self.record_written(seg.segment);
            catalog.update(id, |buffer| {
                buffer.move_id = None;
                buffer.disk = Some(seg);
                // A batch memory never admitted lands on disk whatever its position: its
                // consumer reads it back under a claim (KD7b).
                let outside = std::mem::replace(&mut buffer.outside, false);
                if hot && !outside {
                    buffer.state = State::ResidentOnDisk(from);
                } else {
                    buffer.payload = None;
                    buffer.state = State::OnDisk(seg);
                    buffer.missed = false;
                }
            });
            catalog.spilled(id, bytes);
            let waiting = if hot {
                Vec::new()
            } else {
                catalog.take_waiting()
            };
            (
                consumer_stages(&catalog, id),
                waiting,
                catalog.drain_events(),
            )
        };
        self.counters[stage]
            .demotions
            .fetch_add(1, Ordering::Relaxed);
        self.deliver(events);
        self.wake_and_plan(&consumers, &waiting);
    }

    /// A promotion's read resolved (f.6).
    fn on_read(&self, move_id: u64, result: moruna_kernel::Result<(Buffer, usize)>) {
        let failed = result.is_err();
        let Some(done) = self.settle(move_id, failed) else {
            return;
        };
        if done.failed {
            self.after_failure(move_id, done);
            return;
        }
        let Ok((buffer, taken)) = result else { return };
        let InFlight::Read { need, to, .. } = &done.op else {
            return;
        };
        let (need, to) = (*need, *to);
        if (taken as u64) < need {
            self.after_failure(
                move_id,
                MoveInFlight {
                    failed: true,
                    ..done
                },
            );
            return;
        }
        let record = match decode_record(buffer, self.page_bytes) {
            Ok(record) => record,
            Err(error) => {
                self.fail_settled(&done, move_id, error);
                return;
            }
        };
        if to.is_host() {
            self.land_promotion(move_id, &done, record.payload);
        } else {
            // Disk -> host tier -> Device: the second row of e.4, issued from this
            // callback, on the reactor thread that resolved the read.
            self.second_step(move_id, done, record.payload, to);
        }
    }

    /// A DMA between resident tiers resolved (e.4).
    fn on_copy(&self, move_id: u64, result: moruna_kernel::Result<Option<Buffer>>) {
        let failed = result.is_err();
        let Some(done) = self.settle(move_id, failed) else {
            return;
        };
        if done.failed {
            self.after_failure(move_id, done);
            return;
        }
        let Ok(Some(buffer)) = result else {
            self.fail_settled(
                &done,
                move_id,
                MorunaError::Staging("a copy resolved without its destination buffer".into()),
            );
            return;
        };
        let InFlight::Copy { payload, .. } = &done.op else {
            return;
        };
        let rebuilt = match payload {
            PayloadRef::Tensor(tensor) => moruna_kernel::ManagedTensor::from_buffer(
                buffer,
                0,
                tensor.dtype(),
                tensor.shape().to_vec(),
            )
            .and_then(Payload::tensor),
            PayloadRef::Table(_) => Err(MorunaError::Staging(
                "a device move of a table needs a safe way to rebuild a RecordBatch over new \
                 buffers (09 e.4, reported as E10)"
                    .into(),
            )),
        };
        match rebuilt {
            Ok(payload) => self.land_promotion(move_id, &done, payload),
            Err(error) => self.fail_settled(&done, move_id, error),
        }
    }

    /// Issue the second row of a two-step promotion out of a segment (e.4).
    fn second_step(&self, move_id: u64, done: MoveInFlight, payload: Payload, to: Tier) {
        let holder = EntryPayload::from(payload);
        let reference = holder.reference();
        drop(holder);
        let bytes = done.bytes;
        let (stage, buffer) = (done.stage, done.buffer);
        let reservation = done.reservation;
        {
            let mut moves = self.lock_moves();
            moves.insert(
                move_id,
                MoveInFlight {
                    stage,
                    buffer,
                    op: InFlight::Copy {
                        from: self.host_tier,
                        to,
                        payload: reference.clone(),
                    },
                    pieces_left: 1,
                    failed: false,
                    attempt: done.attempt,
                    reservation,
                    bytes,
                    promote: true,
                },
            );
        }
        self.in_flight_bytes.fetch_add(bytes, Ordering::AcqRel);
        if let Err(error) = self.submit_copy(move_id, &reference, to) {
            let Some(done) = self.settle(move_id, true) else {
                return;
            };
            self.fail_settled(&done, move_id, error);
        }
    }

    /// A promotion or a copy down landed: the batch is resident in its new tier, and its disk
    /// copy (if any) stays valid until its last consumer takes it (f.6, f.7).
    fn land_promotion(&self, move_id: u64, done: &MoveInFlight, payload: Payload) {
        let (stage, id) = (done.stage, done.buffer);
        let tier = payload.tier();
        let promote = done.promote;
        let from_disk = matches!(done.op, InFlight::Read { .. });
        // A batch a `restore` brought back has entries with no features yet; they are read off
        // the first payload that lands (f.13).
        let features = from_disk.then(|| MorselFeatures::from_payload(&payload));
        let (consumers, waiting, events) = {
            let mut catalog = self.lock_catalog();
            if let Some((slot, bytes)) = done.reservation {
                catalog.unreserve(slot, bytes);
            }
            let current = catalog
                .get(id)
                .is_some_and(|buffer| buffer.move_id == Some(move_id));
            if !current {
                return;
            }
            catalog.update(id, |buffer| {
                buffer.move_id = None;
                buffer.payload = Some(EntryPayload::from(payload));
                buffer.state = match buffer.disk {
                    Some(_) => State::ResidentOnDisk(tier),
                    None => State::Resident(tier),
                };
            });
            catalog.touch(id);
            if from_disk {
                catalog.reloaded(id, done.bytes);
            }
            // A copy down frees the tier it left.
            let waiting = if promote {
                Vec::new()
            } else {
                catalog.take_waiting()
            };
            (
                consumer_entries(&catalog, id),
                waiting,
                catalog.drain_events(),
            )
        };
        if let Some(features) = features {
            for &(stage, seq) in &consumers {
                let mut queue = self.lock_queue(usize::from(stage));
                if let Some(position) = queue.position(seq)
                    && queue.order[position].features.bytes == 0
                {
                    queue.order[position].features = features.clone();
                }
            }
        }
        if promote {
            self.counters[stage]
                .promotions
                .fetch_add(1, Ordering::Relaxed);
        } else {
            self.counters[stage]
                .demotions
                .fetch_add(1, Ordering::Relaxed);
        }
        self.deliver(events);
        let stages: Vec<StageId> = consumers.iter().map(|(stage, _)| *stage).collect();
        self.wake_and_plan(&stages, &waiting);
    }

    /// A move failed. The reactor has already applied its own fallback (06 f.9); the engine
    /// re-issues the identical operation exactly once, from this callback, with a fresh move
    /// id, and then gives up (f.10).
    fn after_failure(&self, move_id: u64, done: MoveInFlight) {
        if done.attempt > 0 {
            let error = MorunaError::Staging(format!(
                "move of batch {} counted in queue {} failed twice",
                done.buffer.0, done.stage
            ));
            self.fail_settled(&done, move_id, error);
            return;
        }
        let MoveInFlight {
            stage,
            buffer,
            op,
            reservation,
            bytes,
            promote,
            ..
        } = done;
        let retry_id = self.next_move_id();
        {
            let mut catalog = self.lock_catalog();
            let current = catalog
                .get(buffer)
                .is_some_and(|b| b.move_id == Some(move_id));
            if !current {
                if let Some((slot, held)) = reservation {
                    catalog.unreserve(slot, held);
                }
                return;
            }
            catalog.update(buffer, |b| b.move_id = Some(retry_id));
        }
        self.counters[stage]
            .move_retries
            .fetch_add(1, Ordering::Relaxed);
        tracing::warn!(target: "placement.move_failed", stage, buffer = buffer.0, attempt = 1, retrying = true);
        // The identical operation: the same pieces at the same offsets, the same source view
        // over the same bytes, the same record range (f.10).
        let again = match &op {
            InFlight::Write { path, list, .. } => {
                Redo::Write(path.clone(), Arc::clone(list), list.len())
            }
            InFlight::Copy { payload, to, .. } => Redo::Copy(payload.clone(), *to),
            InFlight::Read {
                path,
                record_start,
                len,
                ..
            } => Redo::Read(path.clone(), *record_start, *len),
        };
        let pieces_left = match &again {
            Redo::Write(_, _, count) => *count,
            Redo::Copy(_, _) | Redo::Read(_, _, _) => 1,
        };
        {
            let mut moves = self.lock_moves();
            moves.insert(
                retry_id,
                MoveInFlight {
                    stage,
                    buffer,
                    op,
                    pieces_left,
                    failed: false,
                    attempt: 1,
                    reservation,
                    bytes,
                    promote,
                },
            );
        }
        self.in_flight_bytes.fetch_add(bytes, Ordering::AcqRel);
        let outcome = match again {
            Redo::Write(path, list, _) => self.submit_write(retry_id, &path, &list),
            Redo::Copy(payload, to) => self.submit_copy(retry_id, &payload, to),
            Redo::Read(path, offset, len) => self.submit_read(retry_id, &path, offset, len),
        };
        if let Err(error) = outcome
            && let Some(done) = self.settle(retry_id, true)
        {
            self.fail_settled(&done, retry_id, error);
        }
    }

    /// Settle a move that will not be retried: the batch stays where its bytes were, the
    /// reservation is released, and a failed promotion of a head is reported through the next
    /// `pop` of each queue it heads (f.10).
    fn fail_settled(&self, done: &MoveInFlight, move_id: u64, error: MorunaError) {
        let id = done.buffer;
        let (consumers, waiting, events, outside) = {
            let mut catalog = self.lock_catalog();
            let outside = catalog.get(id).is_some_and(|buffer| buffer.outside);
            if let Some((slot, bytes)) = done.reservation {
                catalog.unreserve(slot, bytes);
            }
            let current = catalog
                .get(id)
                .is_some_and(|buffer| buffer.move_id == Some(move_id));
            if !current {
                return;
            }
            catalog.update(id, |buffer| {
                buffer.move_id = None;
                buffer.state = buffer.state.source_state(buffer.disk);
            });
            let waiting = if done.reservation.is_some() {
                catalog.take_waiting()
            } else {
                Vec::new()
            };
            (
                consumer_stages(&catalog, id),
                waiting,
                catalog.drain_events(),
                outside,
            )
        };
        if outside {
            // Memory never admitted it and disk would not take it: fail loud on every queue it
            // waits in (KD7b).
            for &stage in &consumers {
                let mut queue = self.lock_queue(usize::from(stage));
                queue.blocked = true;
                if queue.head_error.is_none() {
                    queue.head_error = Some(MorunaError::Staging(format!(
                        "a batch memory could not admit could not be written to disk: {error}"
                    )));
                }
            }
        }
        if done.promote {
            for &stage in &consumers {
                let mut queue = self.lock_queue(usize::from(stage));
                let head = queue.order.front().is_some_and(|entry| entry.buffer == id);
                if head && queue.head_error.is_none() {
                    let seq = queue.order.front().map(|entry| entry.seq).unwrap_or(0);
                    queue.head_error = Some(MorunaError::Staging(format!(
                        "morsel {seq} on stage {stage} could not be promoted: {error}"
                    )));
                }
            }
        }
        tracing::warn!(target: "placement.move_failed", stage = done.stage, buffer = id.0, error = %error, attempts = 2);
        self.deliver(events);
        for &stage in &consumers {
            self.unpark_all(usize::from(stage));
        }
        for queue in waiting {
            self.plan_and_issue(queue);
        }
    }

    /// Wake every queue a batch waits in and plan it, then plan the queues that were waiting for
    /// the room a landing freed.
    fn wake_and_plan(&self, stages: &[StageId], waiting: &[usize]) {
        for &stage in stages {
            self.unpark_all(usize::from(stage));
        }
        for &stage in stages {
            self.plan_and_issue(usize::from(stage));
        }
        for &queue in waiting {
            if !stages.iter().any(|&stage| usize::from(stage) == queue) {
                self.plan_and_issue(queue);
            }
        }
    }
}

/// The queues a batch waits in, lowest first and each once.
pub(crate) fn consumer_stages(catalog: &Catalog, id: BufferId) -> Vec<StageId> {
    let mut stages: Vec<StageId> = consumer_entries(catalog, id)
        .into_iter()
        .map(|(stage, _)| stage)
        .collect();
    stages.sort_unstable();
    stages.dedup();
    stages
}

/// The `(queue, seq)` of every entry naming a batch.
pub(crate) fn consumer_entries(catalog: &Catalog, id: BufferId) -> Vec<(StageId, Seq)> {
    catalog
        .get(id)
        .map(|buffer| buffer.consumers.clone())
        .unwrap_or_default()
}

/// A copy of an error for the queue's `head_error`, which `pop` returns once (f.10);
/// `MorunaError` is not `Clone`, because it carries a morsel's features (CT-I10).
fn head_error_of(error: &MorunaError) -> MorunaError {
    MorunaError::Staging(error.to_string())
}
