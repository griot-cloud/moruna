//! Issuing moves and observing them (e.4, f.2, f.5, f.6, f.10).
//!
//! The planner decides under the queue lock; issuing happens with no lock held, because
//! every reactor call may resolve on the calling thread and its `then` callback takes the
//! queue lock (preamble 4.2, g). A completion runs the callback on the reactor thread that
//! resolved it, which is how the engine observes moves without a thread of its own (PL-I14).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use amoru_kernel::{
    AmoruError, Buffer, BufferView, CopyDst, CopySrc, MorselFeatures, Payload, PayloadKind,
    SegmentRef, Seq, StageId, Tier, TierKind,
};

use crate::PlacementEngine;
use crate::plan::{EntryView, Intent, PlanInput, plan};
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
    /// The queue.
    pub stage: usize,
    /// The entry.
    pub seq: Seq,
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
    /// The entry's payload bytes.
    pub bytes: u64,
    /// True for a promotion, false for a demotion.
    pub promote: bool,
}

/// What `plan_locked` hands the issuer: everything a move needs, and no lock.
pub struct Task {
    stage: usize,
    stage_id: StageId,
    seq: Seq,
    move_id: u64,
    bytes: u64,
    promote: bool,
    reservation: Option<(usize, u64)>,
    kind: TaskKind,
}

/// Why a move could not be issued at all. Only the disk bound of f.2 step 4 turns a
/// recomputable entry into an `Evicted` one; any other failure leaves the bytes where they
/// are and is reported like a failed move (f.10).
enum Refusal {
    /// No segment could open within the disk budget (PL-I7).
    DiskBound(AmoruError),
    /// Anything else.
    Other(AmoruError),
}

impl Refusal {
    fn error(&self) -> &AmoruError {
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

enum TaskKind {
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

    /// Plan one queue and issue what it decided (f.2). The lock is released before the
    /// first reactor or allocator call, without exception (preamble 4.2).
    pub(crate) fn plan_and_issue(&self, index: usize) {
        if self.is_shutting_down() {
            return;
        }
        let tasks = {
            let mut queue = self.lock_queue(index);
            self.plan_locked(index, &mut queue)
        };
        for task in tasks {
            self.issue(task);
        }
    }

    /// Plan every queue: what a budget or a knob change triggers (f.8).
    pub(crate) fn plan_every_queue(&self) {
        for index in 0..self.queues.len() {
            self.plan_and_issue(index);
        }
    }

    /// Build the planner's input from a queue (f.2).
    fn plan_input(&self, index: usize, queue: &Queue) -> PlanInput {
        let mut bytes = [0u64; amoru_kernel::TIER_COUNT];
        for (slot, cell) in self.counters[index].bytes.iter().enumerate() {
            bytes[slot] = cell.load(Ordering::Acquire);
        }
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
        PlanInput {
            entries: queue
                .order
                .iter()
                .map(|entry| EntryView {
                    bytes: entry.bytes,
                    tier: entry.state.resident_tier(),
                    has_disk: entry.disk.is_some(),
                    on_disk: matches!(entry.state, State::OnDisk(_)),
                    in_flight: entry.state.in_flight(),
                    evicted: matches!(entry.state, State::Evicted),
                    recomputable: entry.recomputable,
                    on_remote: matches!(entry.state, State::OnRemote(_, _)),
                })
                .collect(),
            target: queue.target,
            want: queue.consumer,
            host_tier: self.host_tier,
            water,
            bytes,
            // A disk budget of zero means the run has no disk tier at all (preamble section 5:
            // "0 disables the disk tier"), so no queue may plan a demotion to it. Without this
            // the planner demoted, the segment roll refused the charge, and a run on a host
            // with no writable staging directory died mid pass with `Staging` instead of
            // running without spill (PM, 2026-09-22; PL-I7, f.2 step 4).
            staging_enabled: queue.staging_enabled
                && self.disk_budget.load(std::sync::atomic::Ordering::Acquire) > 0,
            window: queue.promotion_window,
        }
    }

    /// Run the planner and apply everything that needs no external call; return the moves
    /// to issue once the lock is released.
    fn plan_locked(&self, index: usize, queue: &mut Queue) -> Vec<Task> {
        let input = self.plan_input(index, queue);
        let decided = plan(&input);
        queue.blocked = decided.blocked;
        let stage_id = queue.stage;
        let counters = &self.counters[index];
        let mut tasks = Vec::new();
        let mut window_stopped = false;
        for intent in decided.intents {
            match intent {
                Intent::Promote {
                    index: position,
                    from: _,
                    to,
                    head,
                } => {
                    if !head && window_stopped {
                        continue;
                    }
                    let Some(entry) = queue.order.get(position) else {
                        continue;
                    };
                    let bytes = entry.bytes;
                    let seq = entry.seq;
                    let slot = to.index();
                    if !self.reserve(slot, bytes) {
                        if head && bytes > self.budget_of(slot) {
                            queue.head_error = Some(AmoruError::Staging(format!(
                                "head cannot fit in {to:?}: need {bytes}, budget {}",
                                self.budget_of(slot)
                            )));
                        }
                        if !head {
                            window_stopped = true;
                        }
                        continue;
                    }
                    let (kind, from) = match &entry.state {
                        State::OnDisk(seg) => (TaskKind::Read { seg: *seg, to }, Tier::Disk(*seg)),
                        State::Resident(at) | State::ResidentOnDisk(at) => {
                            let at = *at;
                            match entry.payload.as_ref().map(EntryPayload::reference) {
                                Some(payload) if at != to => (
                                    TaskKind::Copy {
                                        from: at,
                                        to,
                                        payload,
                                    },
                                    at,
                                ),
                                Some(_) | None => {
                                    self.release(slot, bytes);
                                    continue;
                                }
                            }
                        }
                        State::Promoting(_, _)
                        | State::Demoting(_, _)
                        | State::Evicted
                        | State::Consumed
                        | State::OnRemote(_, _) => {
                            self.release(slot, bytes);
                            continue;
                        }
                    };
                    let move_id = self.next_move_id();
                    if let Some(entry) = queue.order.get_mut(position) {
                        entry.state = State::Promoting(from, to);
                        entry.move_id = Some(move_id);
                    }
                    tracing::debug!(target: "placement.promote", stage = stage_id, seq, from = ?from, to = ?to, bytes);
                    tasks.push(Task {
                        stage: index,
                        stage_id,
                        seq,
                        move_id,
                        bytes,
                        promote: true,
                        reservation: Some((slot, bytes)),
                        kind,
                    });
                }
                Intent::Demote {
                    index: position,
                    from,
                    to,
                } => {
                    let Some(entry) = queue.order.get(position) else {
                        continue;
                    };
                    let bytes = entry.bytes;
                    let seq = entry.seq;
                    let payload_kind = entry.kind;
                    let Some(payload) = entry.payload.as_ref().map(EntryPayload::reference) else {
                        continue;
                    };
                    let (kind, target, reservation) = match to {
                        TierKind::Disk => (
                            TaskKind::Write {
                                from,
                                payload,
                                kind: payload_kind,
                            },
                            Tier::Disk(PENDING_DISK),
                            None,
                        ),
                        TierKind::Device
                        | TierKind::PinnedHost
                        | TierKind::Host
                        | TierKind::Remote => {
                            let down = self.host_tier;
                            let slot = down.index();
                            if !self.reserve(slot, bytes) {
                                continue;
                            }
                            (
                                TaskKind::Copy {
                                    from,
                                    to: down,
                                    payload,
                                },
                                down,
                                Some((slot, bytes)),
                            )
                        }
                    };
                    let move_id = self.next_move_id();
                    if let Some(entry) = queue.order.get_mut(position) {
                        entry.state = State::Demoting(from, target);
                        entry.move_id = Some(move_id);
                    }
                    tracing::debug!(target: "placement.demote", stage = stage_id, seq, from = ?from, to = ?target, bytes);
                    tasks.push(Task {
                        stage: index,
                        stage_id,
                        seq,
                        move_id,
                        bytes,
                        promote: false,
                        reservation,
                        kind,
                    });
                }
                Intent::DropResident { index: position } => {
                    let Some(entry) = queue.order.get_mut(position) else {
                        continue;
                    };
                    let (Some(seg), Some(tier)) = (entry.disk, entry.state.resident_tier()) else {
                        continue;
                    };
                    entry.payload = None;
                    entry.state = State::OnDisk(seg);
                    let (seq, bytes) = (entry.seq, entry.bytes);
                    self.sub_bytes(index, tier, bytes);
                    counters.demotions.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(target: "placement.demote", stage = stage_id, seq, from = ?tier, to = "Disk", bytes, no_io = true);
                }
                Intent::Evict { index: position } => {
                    let Some(entry) = queue.order.get_mut(position) else {
                        continue;
                    };
                    let Some(tier) = entry.state.resident_tier() else {
                        continue;
                    };
                    entry.payload = None;
                    entry.state = State::Evicted;
                    entry.disk = None;
                    let (seq, bytes) = (entry.seq, entry.bytes);
                    self.sub_bytes(index, tier, bytes);
                    counters.evictions.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(target: "placement.evict", stage = stage_id, seq, from = ?tier, bytes);
                }
            }
        }
        tasks
    }

    /// Issue one move. No engine lock is held here (g).
    fn issue(&self, task: Task) {
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
    ) -> amoru_kernel::Result<()> {
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
    fn issue_read(&self, task: &Task, seg: SegmentRef, to: Tier) -> amoru_kernel::Result<()> {
        self.refuse_remote(to)?;
        let page = self.page_bytes;
        let Some(path) = self.segment_path(seg.segment) else {
            return Err(AmoruError::Staging(
                "promotion from disk without a staging directory".into(),
            ));
        };
        if seg.offset < page {
            return Err(AmoruError::Staging(format!(
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
    fn refuse_remote(&self, tier: Tier) -> amoru_kernel::Result<()> {
        match tier {
            Tier::Remote(_, _) => Err(AmoruError::Unsupported("rdma")),
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
                seq: task.seq,
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

    fn submit_write(&self, move_id: u64, path: &Path, list: &[Piece]) -> amoru_kernel::Result<()> {
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
    ) -> amoru_kernel::Result<()> {
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
    ) -> amoru_kernel::Result<()> {
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
    fn source_view(payload: &PayloadRef) -> amoru_kernel::Result<BufferView> {
        match payload {
            PayloadRef::Tensor(tensor) => BufferView::of_tensor(tensor),
            PayloadRef::Table(_) => Err(AmoruError::Staging(
                "a device move of a table needs a safe way to rebuild a RecordBatch over new \
                 buffers (09 e.4, reported as E10)"
                    .into(),
            )),
        }
    }

    /// A move that could not even be issued: put the entry back where its bytes are, and
    /// handle the disk bound of f.2 step 4.
    fn abandon(&self, task: &Task, refusal: Refusal) {
        let error = refusal.error();
        let disk_bound = matches!(refusal, Refusal::DiskBound(_));
        if let Some((slot, bytes)) = task.reservation {
            self.release(slot, bytes);
        }
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
        let mut evicted = false;
        {
            let mut queue = self.lock_queue(task.stage);
            let Some(position) = queue.position(task.seq) else {
                return;
            };
            let head = position == 0;
            let entry = &mut queue.order[position];
            if entry.move_id != Some(task.move_id) {
                return;
            }
            entry.move_id = None;
            if is_write && disk_bound && entry.recomputable {
                // No room for the record and the bytes can be read again (f.2 step 4).
                if let Some(tier) = entry.state.resident_tier() {
                    self.sub_bytes(task.stage, tier, entry.bytes);
                }
                entry.payload = None;
                entry.state = State::Evicted;
                entry.disk = None;
                evicted = true;
                self.counters[task.stage]
                    .evictions
                    .fetch_add(1, Ordering::Relaxed);
            } else {
                entry.state = entry.state.source_state(entry.disk);
                if is_write && disk_bound {
                    // Not recomputable and no room: the run cannot continue on this queue,
                    // whichever entry could not be written. `is_full` holds and the next
                    // `pop` carries the totals (PL-I7, f.2 step 4, f.14).
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
        if evicted {
            self.lineage_set_disk(task.seq, None);
        }
        self.unpark_all(task.stage);
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
    fn on_piece(&self, move_id: u64, result: amoru_kernel::Result<()>) {
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
        let (segment, body_offset, payload_len, from) =
            (*segment, *body_offset, *payload_len, *from);
        let seg = SegmentRef {
            segment,
            offset: body_offset,
            len: payload_len,
        };
        let stage = done.stage;
        let seq = done.seq;
        // The record's pieces hold a shared handle on the entry's own payload. It must be
        // released before the entry can be popped again, or `pop` would find the payload
        // still shared and could not hand the morsel back (d.3, d.4).
        drop(done);
        {
            let mut queue = self.lock_queue(stage);
            let Some(position) = queue.position(seq) else {
                return;
            };
            let window = usize::from(queue.promotion_window).max(1);
            let entry = &mut queue.order[position];
            if entry.move_id != Some(move_id) {
                return;
            }
            entry.move_id = None;
            entry.disk = Some(seg);
            if position < window {
                // Pops moved this entry into the promotion window while its record was in
                // flight. Its bytes are still resident (the reactor was given a view, RE-I1),
                // so the record is kept and the resident copy is kept too: the entry becomes
                // `Resident + OnDisk` and the consumer never waits on a read of bytes that
                // are already here (PL-I1, G-I3). The next pressure pass drops the resident
                // copy for free if the entry leaves the window again (f.7).
                entry.state = State::ResidentOnDisk(from);
            } else {
                entry.payload = None;
                entry.state = State::OnDisk(seg);
                self.sub_bytes(stage, from, entry.bytes);
            }
        }
        self.counters[stage]
            .demotions
            .fetch_add(1, Ordering::Relaxed);
        self.record_written(segment);
        self.lineage_set_disk(seq, Some(seg));
        self.unpark_all(stage);
        self.plan_and_issue(stage);
    }

    /// A promotion's read resolved (f.6).
    fn on_read(&self, move_id: u64, result: amoru_kernel::Result<(Buffer, usize)>) {
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
    fn on_copy(&self, move_id: u64, result: amoru_kernel::Result<Option<Buffer>>) {
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
                AmoruError::Staging("a copy resolved without its destination buffer".into()),
            );
            return;
        };
        let InFlight::Copy { payload, .. } = &done.op else {
            return;
        };
        let rebuilt = match payload {
            PayloadRef::Tensor(tensor) => amoru_kernel::ManagedTensor::from_buffer(
                buffer,
                0,
                tensor.dtype(),
                tensor.shape().to_vec(),
            )
            .and_then(Payload::tensor),
            PayloadRef::Table(_) => Err(AmoruError::Staging(
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
        let (stage, seq) = (done.stage, done.seq);
        let reservation = done.reservation;
        {
            let mut moves = self.lock_moves();
            moves.insert(
                move_id,
                MoveInFlight {
                    stage,
                    seq,
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

    /// A promotion landed: the entry is resident again, and its disk copy (if any) stays
    /// valid until the entry is consumed (f.6, f.7).
    fn land_promotion(&self, move_id: u64, done: &MoveInFlight, payload: Payload) {
        let stage = done.stage;
        let seq = done.seq;
        let tier = payload.tier();
        let promote = done.promote;
        {
            let mut queue = self.lock_queue(stage);
            let Some(position) = queue.position(seq) else {
                if let Some((slot, bytes)) = done.reservation {
                    self.release(slot, bytes);
                }
                return;
            };
            let entry = &mut queue.order[position];
            if entry.move_id != Some(move_id) {
                if let Some((slot, bytes)) = done.reservation {
                    self.release(slot, bytes);
                }
                return;
            }
            let previous = entry.state.resident_tier();
            entry.move_id = None;
            if entry.features.bytes == 0 {
                entry.features = MorselFeatures::from_payload(&payload);
            }
            entry.payload = Some(EntryPayload::from(payload));
            entry.state = match entry.disk {
                Some(_) => State::ResidentOnDisk(tier),
                None => State::Resident(tier),
            };
            if let Some(previous) = previous {
                self.sub_bytes(stage, previous, entry.bytes);
            }
            self.add_bytes(stage, tier, entry.bytes);
        }
        if let Some((slot, bytes)) = done.reservation {
            self.release(slot, bytes);
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
        self.unpark_all(stage);
        self.plan_and_issue(stage);
    }

    /// A move failed. The reactor has already applied its own fallback (06 f.9); the engine
    /// re-issues the identical operation exactly once, from this callback, with a fresh move
    /// id, and then gives up (f.10).
    fn after_failure(&self, move_id: u64, done: MoveInFlight) {
        if done.attempt > 0 {
            let error = AmoruError::Staging(format!(
                "move of morsel {} on stage {} failed twice",
                done.seq, done.stage
            ));
            self.fail_settled(&done, move_id, error);
            return;
        }
        let MoveInFlight {
            stage,
            seq,
            op,
            reservation,
            bytes,
            promote,
            ..
        } = done;
        let retry_id = self.next_move_id();
        {
            let mut queue = self.lock_queue(stage);
            match queue.position(seq) {
                Some(position) if queue.order[position].move_id == Some(move_id) => {
                    queue.order[position].move_id = Some(retry_id);
                }
                Some(_) | None => return,
            }
        }
        self.counters[stage]
            .move_retries
            .fetch_add(1, Ordering::Relaxed);
        tracing::warn!(target: "placement.move_failed", stage, seq, attempt = 1, retrying = true);
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
                    seq,
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

    /// Settle a move that will not be retried: the entry stays where its bytes were, the
    /// reservation is released, and a failed head is reported through the next `pop`
    /// (f.10).
    fn fail_settled(&self, done: &MoveInFlight, move_id: u64, error: AmoruError) {
        if let Some((slot, bytes)) = done.reservation {
            self.release(slot, bytes);
        }
        let stage = done.stage;
        let seq = done.seq;
        {
            let mut queue = self.lock_queue(stage);
            let Some(position) = queue.position(seq) else {
                return;
            };
            if queue.order[position].move_id != Some(move_id) {
                return;
            }
            let disk = queue.order[position].disk;
            let entry = &mut queue.order[position];
            entry.move_id = None;
            entry.state = entry.state.source_state(disk);
            if position == 0 && done.promote && queue.head_error.is_none() {
                queue.head_error = Some(AmoruError::Staging(format!(
                    "morsel {seq} on stage {stage} could not be promoted: {error}"
                )));
            }
        }
        tracing::warn!(target: "placement.move_failed", stage, seq, error = %error, attempts = 2);
        self.unpark_all(stage);
    }
}

/// A copy of an error for the queue's `head_error`, which `pop` returns once (f.10);
/// `AmoruError` is not `Clone`, because it carries a morsel's features (CT-I10).
fn head_error_of(error: &AmoruError) -> AmoruError {
    AmoruError::Staging(error.to_string())
}
