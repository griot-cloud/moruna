//! Item 3.1, the full-year hang (`architecture/work/3.1-full-year-hang.md`): a native stage of
//! eight ring tasks under a memory bound, driven by the real scheduler.
//!
//! `moruna_micro::task::TaskState` runs only on the Moruna kernel, so the stage here is a model
//! of its ring mode, step for step: `pipelined`, `feed`, `take_oldest`, `answers`, `part`,
//! `output`, `land`, `join`, `took`, `give`, `gather`, `step`, `wait_any` and `finish_morsels`,
//! on the stage's own layout (`moruna_micro::layout`: `IN_FLIGHT`, `WINDOWS`, `QUEUED_MAX`,
//! `cut`, `to_post`, `answers_whole`, `waits_for_answer`, `keep_in_place`, and item 6.3m's
//! `Morsels`, by which a unit gives each morsel whole under its own number).
//!
//! Its tasks are threads, as `images/gate/tasks` runs on `moruna_sys::plane::Rings`: a region of
//! `IN_FLIGHT + 1` pieces and `WINDOWS` windows; each batch answered in turn with its records,
//! each in a window, then released; asleep on the task's bell while it has no batch or no
//! window (`Rings::wait_input`, `Rings::wait_window`), never waiting on its port. The plane is
//! `moruna_plane::RingPlane` as the stage meets it: a record the stage keeps in place holds its
//! window until downstream drops the last buffer over it and the stage reaps it; a landed one is
//! dropped once it has landed; a dropped record's window is idle, and idle windows go back to the
//! task only in `RingPlane::next`'s pump, by the plane's own rule
//! (`moruna_plane::ringed::gives_idle`). The run's arena is the `FakeAllocator`, capped, and the
//! stage's own allocations in it (a record's framing, a landing, a join) can be refused on
//! purpose ([`Faults`]), as the arena refuses them under memory pressure (RB5).

use std::collections::{HashMap, VecDeque};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use moruna_kernel::arrow::array::{Array, Int64Array};
use moruna_kernel::arrow::buffer::{Buffer as ArrowBuffer, ScalarBuffer};
use moruna_kernel::arrow::datatypes::SchemaRef;
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{
    Allocator, BoxFuture, CancelToken, Completed, Fingerprint, InitCtx, Kernel, KernelKind,
    KernelState, MorunaError, Payload, PayloadKind, PayloadSpec, Result, Seq, Sink, SinkSummary,
    SourceSchema, Tier, TierPref,
};
use moruna_micro::layout;
use moruna_testkit::{FakeAllocator, FakeSink, FakeSource};

use super::common::RigBuilder;

/// Batches a task holds at once, and its windows (`layout::IN_FLIGHT`, `layout::WINDOWS`).
const IN_FLIGHT: usize = layout::IN_FLIGHT as usize;
const WINDOWS: usize = layout::WINDOWS as usize;
/// A task's region: `IN_FLIGHT` pieces and one more (`layout::ring_sizes`).
const REGION: usize = IN_FLIGHT + 1;
/// `task.rs`'s `LOOK_NS`, shortened: how long a wait on the task looks before it looks again.
const LOOK: Duration = Duration::from_millis(20);
/// The framing `encode_framing` takes from the run's arena for each record it lays out.
const FRAMING: usize = 4096;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn kernel_error(msg: &str) -> MorunaError {
    MorunaError::Kernel {
        stage: 1,
        seq: 0,
        msg: msg.to_string(),
        features: None,
    }
}

/// What a task says on its rings.
enum Ev {
    Put(u64),
    Released(u64),
    Ended,
}

/// One task's region and rings, both sides' view of them.
#[derive(Default)]
struct Wire {
    /// Batches posted and not yet taken by the task.
    input: VecDeque<(u64, RecordBatch)>,
    /// Batches the task has taken and not released.
    holding: usize,
    closed: bool,
    stop: bool,
    /// Windows given to the task and free for a record.
    given: usize,
    /// Windows whose record the engine dropped, not yet given back.
    idle: usize,
    /// Records put and not yet dropped by the engine: each holds its window.
    records: HashMap<u64, RecordBatch>,
    events: VecDeque<Ev>,
    next_batch: u64,
    next_record: u64,
    /// The task sleeps for want of a window.
    no_window: bool,
    /// The engine's side as it last began to wait on the task.
    engine: Option<EngineLook>,
}

#[derive(Default)]
struct Plane {
    wire: Mutex<Wire>,
    bell: Condvar,
}

impl Plane {
    /// Lays `batch` out in the region and posts it; `None` when the region has no room.
    fn post(&self, batch: RecordBatch) -> Option<u64> {
        let mut w = lock(&self.wire);
        if w.input.len() + w.holding >= REGION {
            return None;
        }
        let id = w.next_batch;
        w.next_batch += 1;
        w.input.push_back((id, batch));
        self.bell.notify_all();
        Some(id)
    }

    /// The pump's regrant of idle windows, by the plane's own rule. A native task never waits
    /// for a window on its port.
    fn pump(&self, w: &mut Wire) {
        if moruna_plane::ringed::gives_idle(w.idle, w.given, WINDOWS, false) {
            w.given += w.idle;
            w.idle = 0;
            self.bell.notify_all();
        }
    }

    /// The next event, waiting up to `wait` for one: `RingPlane::next`, which pumps.
    fn next(&self, wait: Duration) -> Option<Ev> {
        let deadline = std::time::Instant::now() + wait;
        let mut w = lock(&self.wire);
        loop {
            if let Some(e) = w.events.pop_front() {
                return Some(e);
            }
            self.pump(&mut w);
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return None;
            }
            w = self
                .bell
                .wait_timeout(w, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    fn record(&self, id: u64) -> Result<RecordBatch> {
        lock(&self.wire)
            .records
            .get(&id)
            .cloned()
            .ok_or_else(|| kernel_error("a record the plane does not hold"))
    }

    /// The engine is done with record `id` (`drop_batch`): its window is idle.
    fn drop_record(&self, id: u64) {
        let mut w = lock(&self.wire);
        if w.records.remove(&id).is_some() {
            w.idle += 1;
        }
    }

    fn finish(&self) {
        lock(&self.wire).closed = true;
        self.bell.notify_all();
    }

    fn stop(&self) {
        lock(&self.wire).stop = true;
        self.bell.notify_all();
    }

    /// The task (`images/gate/tasks`): each batch in turn, answered with `parts` records, each
    /// in a window, then released; asleep on its bell when it has no batch or no window.
    fn task(&self, work: Duration, parts: usize) {
        loop {
            let (id, batch) = {
                let mut w = lock(&self.wire);
                loop {
                    if w.stop {
                        return;
                    }
                    if let Some(next) = w.input.pop_front() {
                        w.holding += 1;
                        break next;
                    }
                    if w.closed {
                        w.events.push_back(Ev::Ended);
                        self.bell.notify_all();
                        return;
                    }
                    w = self
                        .bell
                        .wait(w)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
            };
            std::thread::sleep(work);
            let rows = batch.num_rows();
            let parts = parts.clamp(1, rows.max(1));
            let each = rows.div_ceil(parts).max(1);
            for at in (0..rows.max(1)).step_by(each) {
                let answer = heap_copy(&batch.slice(at.min(rows), each.min(rows - at.min(rows))));
                let mut w = lock(&self.wire);
                while w.given == 0 {
                    if w.stop {
                        return;
                    }
                    w.no_window = true;
                    w = self
                        .bell
                        .wait(w)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                w.no_window = false;
                w.given -= 1;
                let record = w.next_record;
                w.next_record += 1;
                w.records.insert(record, answer);
                w.events.push_back(Ev::Put(record));
                self.bell.notify_all();
            }
            let mut w = lock(&self.wire);
            w.holding -= 1;
            w.events.push_back(Ev::Released(id));
            self.bell.notify_all();
        }
    }
}

fn values(batch: &RecordBatch) -> Vec<i64> {
    batch
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .map(|a| a.values().to_vec())
        .unwrap_or_default()
}

/// The task's answer: rows of its batch, written in a window (the plane's memory).
fn heap_copy(batch: &RecordBatch) -> RecordBatch {
    RecordBatch::try_new(
        batch.schema(),
        vec![Arc::new(Int64Array::from(values(batch)))],
    )
    .unwrap_or_else(|e| panic!("the task's answer: {e}"))
}

/// `batch` copied into the run's arena (`moruna_adapters::land::land_table`).
fn land_into(batch: &RecordBatch, alloc: &dyn Allocator) -> Result<RecordBatch> {
    let values = values(batch);
    let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
    let mut buffer = alloc.alloc(bytes.len().max(1), Tier::Host)?;
    buffer[..bytes.len()].copy_from_slice(&bytes);
    let arrow = buffer.into_arrow_buffer()?;
    let array = Int64Array::new(ScalarBuffer::new(arrow, 0, values.len()), None);
    RecordBatch::try_new(batch.schema(), vec![Arc::new(array)])
        .map_err(|e| kernel_error(&e.to_string()))
}

/// Records whose payloads downstream has dropped (`rings::Released`).
#[derive(Clone, Default)]
struct Released(Arc<Mutex<Vec<u64>>>);

impl Released {
    fn take(&self) -> Vec<u64> {
        std::mem::take(&mut *lock(&self.0))
    }
}

/// What keeps a record kept in place (`rings::Hold`): given back when its last buffer goes.
struct Hold {
    _bytes: ArrowBuffer,
    record: u64,
    released: Released,
}

impl Drop for Hold {
    fn drop(&mut self) {
        lock(&self.released.0).push(self.record);
    }
}

/// Record `id` decoded in place and handed on as it lies (`rings::payload_over`).
fn in_place(record: &RecordBatch, id: u64, released: &Released) -> Result<RecordBatch> {
    let bytes = record.column(0).to_data().buffers()[0].clone();
    let rows = record.num_rows();
    let at = NonNull::new(bytes.as_ptr() as *mut u8)
        .ok_or_else(|| kernel_error("a record at address 0"))?;
    let len = bytes.len();
    let hold = Arc::new(Hold {
        _bytes: bytes,
        record: id,
        released: released.clone(),
    });
    // SAFETY: `at .. at + len` is the record's own buffer, which `hold` keeps alive, and nothing
    // writes it: the task wrote it before it put the record.
    let over = unsafe { ArrowBuffer::from_custom_allocation(at, len, hold) };
    let array = Int64Array::new(ScalarBuffer::new(over, 0, rows), None);
    RecordBatch::try_new(record.schema(), vec![Arc::new(array)])
        .map_err(|e| kernel_error(&e.to_string()))
}

/// The run's arena refusing the stage's own allocations on purpose: every `every`th, counted by
/// where (`framing` of a cut or of a laid-out piece, `land` of a record, `join` of answers).
#[derive(Default)]
struct Faults {
    every: u64,
    calls: AtomicU64,
    refused: Mutex<HashMap<&'static str, u64>>,
}

impl Faults {
    fn check(&self, at: &'static str, bytes: usize) -> Result<()> {
        if self.every == 0 {
            return Ok(());
        }
        let n = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
        if !n.is_multiple_of(self.every) {
            return Ok(());
        }
        *lock(&self.refused).entry(at).or_default() += 1;
        Err(MorunaError::Alloc {
            bytes: bytes as u64,
            tier: Tier::Host,
            budget: 0,
            in_use: 0,
        })
    }
}

enum Seen {
    Put(u64),
    Released(u64),
    Ended,
}

/// The engine's side of a task as it began to wait on it.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // read through `Debug`, in the failure's words
struct EngineLook {
    /// Records the stage kept in place and has not given back (`TaskState::held`).
    held: usize,
    /// Batches posted and not answered, and pieces queued (`in_flight`, `queued`).
    in_flight: usize,
    queued: usize,
    /// Answers taken and not yet given: of morsels open, and of morsels answered whole
    /// (`TaskState::morsels`, `TaskState::done`).
    ready: usize,
}

/// `task.rs`'s `Taking`: an answer being taken.
struct Taking {
    offered: u64,
    parts: Vec<Payload>,
    refused: Option<u64>,
    released: bool,
}

/// `TaskState` in ring mode.
struct Ring {
    plane: Arc<Plane>,
    thread: Option<std::thread::JoinHandle<()>>,
    alloc: Arc<dyn Allocator>,
    faults: Arc<Faults>,
    /// Inside `apply`, where the scheduler applies a refused unit again (RB5); a stage's finish
    /// is not, so its allocations are never refused here.
    in_apply: bool,
    piece: u64,
    in_flight: VecDeque<u64>,
    queued: VecDeque<RecordBatch>,
    applying: bool,
    /// The unit under way answers its whole morsel: the stage's first (6.3j).
    whole: bool,
    /// The morsels given and not answered in full, and those answered whole and not given (6.3m).
    morsels: layout::Morsels<Payload>,
    done: VecDeque<layout::Answered<Payload>>,
    /// What the function takes a piece, which the model's task reports as its time.
    work: Duration,
    taking: Option<Taking>,
    early: VecDeque<Seen>,
    schema: Option<SchemaRef>,
    stage_schema: Arc<Mutex<Option<SchemaRef>>>,
    held: usize,
    released: Released,
    /// The test's own stop, so a stalled run's threads end.
    stop: Arc<AtomicBool>,
}

impl KernelState for Ring {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        self.plane.stop();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Ring {
    /// An allocation of the stage's in the run's arena, which may be refused on purpose.
    fn arena(&self, at: &'static str, bytes: usize) -> Result<()> {
        if self.in_apply {
            self.faults.check(at, bytes)?;
        }
        Ok(())
    }

    /// `encode_framing`'s framing, from the run's arena, for one record laid out.
    fn framing(&self) -> Result<()> {
        self.arena("framing", FRAMING)?;
        drop(self.alloc.alloc(FRAMING, Tier::Host)?);
        Ok(())
    }

    /// `land_table`: a copy into the run's arena.
    fn landed(&self, batch: &RecordBatch, at: &'static str) -> Result<RecordBatch> {
        self.arena(at, batch.num_rows() * 8)?;
        land_into(batch, self.alloc.as_ref())
    }

    fn reap(&mut self) {
        for record in self.released.take() {
            self.held = self.held.saturating_sub(1);
            self.plane.drop_record(record);
        }
    }

    /// `TaskState::pieces`, by `layout::cut`.
    fn pieces(&self, batch: &RecordBatch) -> Result<Vec<RecordBatch>> {
        self.framing()?;
        let rows = batch.num_rows();
        Ok(layout::cut(rows, rows as u64 * 8, self.piece)
            .into_iter()
            .map(|(at, n)| batch.slice(at, n))
            .collect())
    }

    fn offer(&mut self, part: &RecordBatch) -> Result<u64> {
        self.framing()?;
        loop {
            match self.plane.post(part.clone()) {
                Some(id) => return Ok(id),
                None => {
                    if !self.step()? {
                        return Err(kernel_error(
                            "a record does not fit its input region and nothing to release",
                        ));
                    }
                }
            }
        }
    }

    fn step(&mut self) -> Result<bool> {
        match self.plane.next(LOOK) {
            None => {
                if self.stop.load(Ordering::SeqCst) {
                    return Err(MorunaError::Cancelled);
                }
                Ok(false)
            }
            Some(Ev::Put(r)) if !self.in_flight.is_empty() || self.taking.is_some() => {
                self.early.push_back(Seen::Put(r));
                Ok(true)
            }
            Some(Ev::Put(r)) => {
                self.plane.drop_record(r);
                Err(kernel_error(
                    "it put a record before it was offered a batch",
                ))
            }
            Some(Ev::Released(b)) if !self.in_flight.is_empty() || self.taking.is_some() => {
                self.early.push_back(Seen::Released(b));
                Ok(true)
            }
            Some(Ev::Released(_)) => Ok(true),
            Some(Ev::Ended) => Err(kernel_error("it ended while the engine offered a batch")),
        }
    }

    fn output(&mut self, out: u64) -> Result<Payload> {
        if !layout::keep_in_place(self.held as u64, WINDOWS as u64) {
            return self.land(out);
        }
        let record = self.plane.record(out)?;
        let kept = in_place(&record, out, &self.released)?;
        self.held += 1;
        Ok(Payload::Table(kept, Tier::Host))
    }

    fn land(&mut self, out: u64) -> Result<Payload> {
        let record = self.plane.record(out)?;
        let landed = self.landed(&record, "land")?;
        self.plane.drop_record(out);
        Ok(Payload::Table(landed, Tier::Host))
    }

    fn wait_any(&mut self) -> Result<Seen> {
        if let Some(seen) = self.early.pop_front() {
            return Ok(seen);
        }
        lock(&self.plane.wire).engine = Some(EngineLook {
            held: self.held,
            in_flight: self.in_flight.len(),
            queued: self.queued.len(),
            ready: self.morsels.held() + self.done.iter().map(|d| d.answers.len()).sum::<usize>(),
        });
        loop {
            self.reap();
            match self.plane.next(LOOK) {
                Some(Ev::Put(r)) => return Ok(Seen::Put(r)),
                Some(Ev::Released(b)) => return Ok(Seen::Released(b)),
                Some(Ev::Ended) => return Ok(Seen::Ended),
                None => {
                    if self.stop.load(Ordering::SeqCst) {
                        return Err(MorunaError::Cancelled);
                    }
                }
            }
        }
    }

    fn answers(&mut self, t: &mut Taking) -> Result<Payload> {
        if let Some(out) = t.refused.take() {
            self.part(t, out)?;
        }
        while !t.released {
            match self.wait_any()? {
                Seen::Put(out) => self.part(t, out)?,
                Seen::Released(b) if b == t.offered => t.released = true,
                Seen::Released(_) => {}
                Seen::Ended => return Err(kernel_error("it ended before it answered")),
            }
        }
        match t.parts.len() {
            0 => Err(kernel_error("a batch said done without a record")),
            1 => t.parts.pop().ok_or_else(|| kernel_error("no part")),
            _ => {
                let joined = self.join(&t.parts)?;
                t.parts.clear();
                self.reap();
                Ok(joined)
            }
        }
    }

    fn part(&mut self, t: &mut Taking, out: u64) -> Result<()> {
        let part = if t.parts.is_empty() {
            self.output(out)
        } else {
            self.land(out)
        };
        match part {
            Ok(part) => {
                t.parts.push(part);
                Ok(())
            }
            Err(e) => {
                t.refused = Some(out);
                Err(e)
            }
        }
    }

    fn pipelined(&mut self, batch: &RecordBatch, seq: Seq) -> Result<Vec<Completed>> {
        if self.schema.is_none() {
            self.schema = lock(&self.stage_schema).clone();
        }
        if !self.applying {
            let pieces = self.pieces(batch)?;
            self.morsels.open(seq, pieces.len());
            self.queued.extend(pieces);
            self.applying = true;
            self.whole = layout::answers_whole(self.schema.is_some());
        }
        if self.taking.is_some() {
            let answer = self.take_oldest()?;
            self.took(answer)?;
        }
        self.feed()?;
        if self.schema.is_none() {
            let answer = self.take_oldest()?;
            self.took(answer)?;
            self.feed()?;
        }
        loop {
            self.gather();
            let mut came = false;
            while let Some(&oldest) = self.in_flight.front()
                && self
                    .early
                    .iter()
                    .any(|seen| matches!(seen, Seen::Released(b) if *b == oldest))
            {
                let answer = self.take_oldest()?;
                self.took(answer)?;
                came = true;
            }
            self.feed()?;
            if layout::waits_for_answer(self.queued.len(), self.in_flight.len(), self.whole) {
                let answer = self.take_oldest()?;
                self.took(answer)?;
                self.feed()?;
            } else if !came {
                break;
            }
        }
        let given = self.give()?;
        self.applying = false;
        self.whole = false;
        Ok(given)
    }

    /// `TaskState::took`: an answer to its morsel. The model's task reports each piece's
    /// function time, its `work`, as a task that stamps it on its release would (item 6.3z).
    fn took(&mut self, answer: Payload) -> Result<()> {
        let ns = self.work.as_nanos() as u64;
        if let Some(whole) = self
            .morsels
            .answer(answer, Some((ns, ns)))
            .map_err(kernel_error)?
        {
            self.done.push_back(whole);
        }
        Ok(())
    }

    /// `TaskState::give`: each morsel answered whole, its answers joined.
    fn give(&mut self) -> Result<Vec<Completed>> {
        for at in 0..self.done.len() {
            if self.done[at].answers.len() < 2 {
                continue;
            }
            let parts = std::mem::take(&mut self.done[at].answers);
            match self.join(&parts) {
                Ok(joined) => {
                    drop(parts);
                    self.reap();
                    self.done[at].answers = vec![joined];
                }
                Err(e) => {
                    self.done[at].answers = parts;
                    return Err(e);
                }
            }
        }
        self.done
            .drain(..)
            .map(|mut whole| {
                Ok(Completed {
                    seq: whole.seq,
                    output: whole
                        .answers
                        .pop()
                        .ok_or_else(|| kernel_error("a morsel with no answer"))?,
                    time: whole.time,
                })
            })
            .collect()
    }

    fn feed(&mut self) -> Result<()> {
        for _ in 0..layout::to_post(self.in_flight.len(), self.queued.len()) {
            let Some(part) = self.queued.pop_front() else {
                break;
            };
            let offered = match self.offer(&part) {
                Ok(offered) => offered,
                Err(e) => {
                    self.queued.push_front(part);
                    return Err(e);
                }
            };
            self.in_flight.push_back(offered);
        }
        Ok(())
    }

    fn take_oldest(&mut self) -> Result<Payload> {
        let mut taking = match self.taking.take() {
            Some(taking) => taking,
            None => Taking {
                offered: self
                    .in_flight
                    .pop_front()
                    .ok_or_else(|| kernel_error("no batch in flight"))?,
                parts: Vec::new(),
                refused: None,
                released: false,
            },
        };
        let answer = match self.answers(&mut taking) {
            Ok(answer) => answer,
            Err(e) => {
                self.taking = Some(taking);
                return Err(e);
            }
        };
        if let (None, Payload::Table(batch, _)) = (&self.schema, &answer) {
            let schema = batch.schema();
            lock(&self.stage_schema).get_or_insert_with(|| Arc::clone(&schema));
            self.schema = Some(schema);
        }
        Ok(answer)
    }

    fn gather(&mut self) {
        while let Some(event) = self.plane.next(Duration::ZERO) {
            match event {
                Ev::Put(r) => self.early.push_back(Seen::Put(r)),
                Ev::Released(b) => self.early.push_back(Seen::Released(b)),
                Ev::Ended => {
                    self.early.push_back(Seen::Ended);
                    return;
                }
            }
        }
    }

    fn join(&mut self, parts: &[Payload]) -> Result<Payload> {
        let batches: Vec<RecordBatch> = parts
            .iter()
            .filter_map(|p| match p {
                Payload::Table(b, _) => Some(b.clone()),
                Payload::Tensor(..) => None,
            })
            .collect();
        let schema = batches[0].schema();
        let joined = moruna_kernel::arrow::compute::concat_batches(&schema, &batches)
            .map_err(|e| kernel_error(&e.to_string()))?;
        drop(batches);
        Ok(Payload::Table(self.landed(&joined, "join")?, Tier::Host))
    }
}

/// One task as the run left it: its windows given and idle, records the engine holds, whether
/// it sleeps for want of a window, and the engine's side.
#[derive(Debug)]
#[allow(dead_code)] // read through `Debug`, in the failure's words
struct TaskLook {
    given: usize,
    idle: usize,
    records: usize,
    no_window: bool,
    engine: Option<EngineLook>,
}

/// `ImageTask` in ring mode with `tasks` processes (`MORUNA_PROCESSES`), whose tasks answer each
/// batch with `parts` records.
pub(super) struct NativeStage {
    tasks: usize,
    piece: u64,
    work: Duration,
    parts: usize,
    faults: Arc<Faults>,
    stage_schema: Arc<Mutex<Option<SchemaRef>>>,
    stop: Arc<AtomicBool>,
    planes: Mutex<Vec<Arc<Plane>>>,
    applies: AtomicU64,
}

impl NativeStage {
    /// A stage of `tasks` tasks whose function takes `work` a piece and answers it with one
    /// record of the same rows (item 6.3m).
    pub(super) fn of(tasks: usize, piece: u64, work: Duration) -> NativeStage {
        NativeStage {
            tasks,
            work,
            ..NativeStage::new(piece, 1, Faults::default())
        }
    }

    fn new(piece: u64, parts: usize, faults: Faults) -> NativeStage {
        NativeStage {
            tasks: 8,
            piece,
            work: Duration::from_micros(200),
            parts,
            faults: Arc::new(faults),
            stage_schema: Arc::default(),
            stop: Arc::default(),
            planes: Mutex::default(),
            applies: AtomicU64::new(0),
        }
    }

    fn looks(&self) -> Vec<TaskLook> {
        lock(&self.planes)
            .iter()
            .map(|p| {
                let w = lock(&p.wire);
                TaskLook {
                    given: w.given,
                    idle: w.idle,
                    records: w.records.len(),
                    no_window: w.no_window,
                    engine: w.engine,
                }
            })
            .collect()
    }
}

impl Kernel for NativeStage {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint::compute("sc_p_native_stage", b"ring")
    }

    fn kind(&self) -> KernelKind {
        KernelKind::Stateful {
            max_instances: core::num::NonZeroUsize::new(self.tasks)
                .unwrap_or(core::num::NonZeroUsize::MIN),
        }
    }

    fn state_uses_arena(&self) -> bool {
        false
    }

    fn deals_instances(&self) -> bool {
        true
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        Ok(input.clone())
    }

    fn init(&self, ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        let plane = Arc::new(Plane::default());
        // The plane gives a task every window as it opens (`open_ring`'s `give_idle`).
        lock(&plane.wire).given = WINDOWS;
        let task = Arc::clone(&plane);
        let (work, parts) = (self.work, self.parts);
        let thread = std::thread::Builder::new()
            .name(format!("task-{}", ctx.instance))
            .spawn(move || task.task(work, parts))
            .map_err(|e| kernel_error(&e.to_string()))?;
        lock(&self.planes).push(Arc::clone(&plane));
        Ok(Box::new(Ring {
            plane,
            thread: Some(thread),
            alloc: Arc::clone(&ctx.alloc),
            faults: Arc::clone(&self.faults),
            in_apply: false,
            piece: self.piece,
            in_flight: VecDeque::new(),
            queued: VecDeque::new(),
            applying: false,
            whole: false,
            morsels: layout::Morsels::new(),
            done: VecDeque::new(),
            work: self.work,
            taking: None,
            early: VecDeque::new(),
            schema: None,
            stage_schema: Arc::clone(&self.stage_schema),
            held: 0,
            released: Released::default(),
            stop: Arc::clone(&self.stop),
        }))
    }

    /// The scheduler gives a stage of one input its morsels (`apply_morsels`); the model has no
    /// answer of one batch to give.
    fn apply(&self, _state: &mut dyn KernelState, _input: Payload) -> Result<Payload> {
        Err(kernel_error("a ring stage gives morsels (apply_morsels)"))
    }

    fn apply_morsels(
        &self,
        state: &mut dyn KernelState,
        input: Payload,
        seq: Seq,
    ) -> Result<Vec<Completed>> {
        self.applies.fetch_add(1, Ordering::Relaxed);
        let st = state
            .as_any_mut()
            .downcast_mut::<Ring>()
            .ok_or_else(|| kernel_error("a state this kernel did not make"))?;
        let Payload::Table(batch, _) = input else {
            return Err(kernel_error("not a table"));
        };
        st.in_apply = true;
        let given = st.pipelined(&batch, seq);
        st.in_apply = false;
        given
    }

    fn finish_morsels(&self, state: &mut dyn KernelState, _after: Seq) -> Result<Vec<Completed>> {
        let st = state
            .as_any_mut()
            .downcast_mut::<Ring>()
            .ok_or_else(|| kernel_error("a state this kernel did not make"))?;
        loop {
            st.feed()?;
            if st.in_flight.is_empty() && st.taking.is_none() {
                break;
            }
            let answer = st.take_oldest()?;
            st.took(answer)?;
        }
        let rest = st.give()?;
        st.plane.finish();
        loop {
            match st.wait_any()? {
                Seen::Released(_) => {}
                Seen::Put(_) => return Err(kernel_error("a record after the last batch")),
                Seen::Ended => return Ok(rest),
            }
        }
    }
}

/// A sink that keeps every value it is given, so a test can count each row.
struct Collecting {
    inner: FakeSink,
    values: Arc<Mutex<Vec<i64>>>,
}

impl Sink for Collecting {
    fn open(&mut self, schema: &SourceSchema) -> Result<()> {
        self.inner.open(schema)
    }

    fn accepts(&self) -> PayloadSpec {
        self.inner.accepts()
    }

    fn write(&self, seq: Seq, payload: Payload) -> BoxFuture<'_, Result<()>> {
        if let Payload::Table(batch, _) = &payload {
            lock(&self.values).extend(values(batch));
        }
        self.inner.write(seq, payload)
    }

    fn finish(&mut self) -> Result<SinkSummary> {
        self.inner.finish()
    }

    fn committed_seq(&self) -> Option<Seq> {
        self.inner.committed_seq()
    }

    fn skip(&self, seq: Seq) {
        self.inner.skip(seq);
    }

    fn checkpoint(&self) -> Result<Option<Vec<u8>>> {
        self.inner.checkpoint()
    }
}

/// Eight tasks, eight workers, morsels of `morsel_pieces` pieces of 8 KiB over `ROWS` rows of
/// eight bytes, an arena of `arena` bytes: every value the sink was given, sorted; or how the
/// run stopped making progress.
fn run(
    stage: &Arc<NativeStage>,
    morsel_pieces: u64,
    arena: u64,
) -> std::result::Result<Vec<i64>, String> {
    let values = Arc::new(Mutex::new(Vec::new()));
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 8;
            cfg.workers_active = 8;
            cfg.initial_morsel_target = morsel_pieces * stage.piece;
            cfg.morsel_min = 8;
            cfg.read_ahead = 8;
        })
        .source(FakeSource::new().splits(1, ROWS, ROWS * 8))
        .alloc(FakeAllocator::new().with_limit(Tier::Host, arena))
        .sink_over(Box::new(Collecting {
            inner: FakeSink::new(),
            values: Arc::clone(&values),
        }))
        .kernel(Arc::clone(stage) as Arc<dyn Kernel>)
        .go();
    rig.scheduler
        .init_instances()
        .expect("the stage's tasks start");
    let cancel = CancelToken::new();
    let token = cancel.clone();
    let (ended, outcome, looks) = std::thread::scope(|scope| {
        let handle = scope.spawn(|| rig.scheduler.run(token));
        let ended = super::common::wait_for(Duration::from_secs(30), || handle.is_finished());
        let looks = stage.looks();
        if !ended {
            stage.stop.store(true, Ordering::SeqCst);
            cancel.cancel();
        }
        (ended, handle.join().ok(), looks)
    });
    if !ended {
        return Err(format!(
            "the run stopped making progress after {} units, {} values written; the stage's \
             tasks: {looks:#?}",
            stage.applies.load(Ordering::Relaxed),
            lock(&values).len()
        ));
    }
    match outcome {
        Some(Ok(crate::RunOutcome::Completed { .. })) => {}
        other => return Err(format!("expected Completed, got {other:?}")),
    }
    let mut written = std::mem::take(&mut *lock(&values));
    written.sort_unstable();
    Ok(written)
}

/// Rows of the source: one split, values `0..ROWS` (`FakeSource::value_at`).
const ROWS: u64 = 512 * 1024;
/// The pieces a task is sent: 8 KiB, a thousand rows.
const PIECE: u64 = 8 * 1024;

/// Every row of the source once, in order.
fn every_row_once() -> Vec<i64> {
    (0..ROWS as i64).collect()
}

#[test]
fn sc_p_3_1_a_native_stage_of_eight_tasks_under_a_memory_bound_keeps_making_progress() {
    // The one-month Score run at a 2 GiB budget, scaled down by 1,024: eight tasks, eight
    // workers, morsels of 8 MiB cut into pieces of about 1.2 MiB (each task's plane is a quarter
    // of its share of the stage's budget), so a morsel is more pieces than a task holds in
    // flight and a unit takes a fourth answer while it keeps two in place. With the plane's
    // half rule alone the third window never came back and every task and every unit waited on
    // each other after eight units, nothing written.
    let stage = Arc::new(NativeStage::new(PIECE, 1, Faults::default()));
    let written = run(&stage, 6, ROWS * 8 / 2).unwrap_or_else(|why| panic!("{why}"));
    assert!(written == every_row_once(), "every row once");
}

#[test]
fn sc_p_3_1_an_arena_refusal_inside_a_unit_loses_and_repeats_no_row() {
    // Every seventh allocation the stage makes in the run's arena is refused: a cut's framing, a
    // piece's framing, a record's landing (answers of two records each, so the later part of an
    // answer lands) and a join. The scheduler applies a refused unit again with the same batch
    // (RB5), and the stage goes on from where the refusal stopped it. Before, the batch was cut
    // and posted again (rows twice), and the answers already taken, the record being landed and
    // a piece being laid out were dropped (rows lost): 532,338 or 522,758 rows for 524,288.
    // The arena is four times the data, so every refusal is one of these.
    let stage = Arc::new(NativeStage::new(
        PIECE,
        2,
        Faults {
            every: 7,
            ..Faults::default()
        },
    ));
    let written = run(&stage, 6, ROWS * 8 * 4).unwrap_or_else(|why| panic!("{why}"));
    let refused = lock(&stage.faults.refused).clone();
    for at in ["framing", "land", "join"] {
        assert!(
            refused.get(at).is_some_and(|n| *n > 0),
            "no refusal at {at}: {refused:?}"
        );
    }
    assert_eq!(written.len() as u64, ROWS, "rows: {refused:?}");
    assert!(written == every_row_once(), "every row once: {refused:?}");
}
