//! Item 3.1, the full-year hang (`architecture/work/3.1-full-year-hang.md`): a native stage of
//! eight ring tasks under a memory bound, driven by the real scheduler, stops making progress.
//!
//! `moruna_micro::task::TaskState` runs only on the Moruna kernel, so the stage here is a model
//! of its ring mode at `feature/arrow-core-0.7`, step for step: `pipelined`, `feed`,
//! `take_oldest`, `answers`, `output`, `land`, `join`, `gather`, `step`, `wait_any` and
//! `finish`, with `layout::IN_FLIGHT`, `layout::WINDOWS`, `layout::QUEUED_MAX`,
//! `layout::keep_in_place` and `layout::cut` as they are there.
//!
//! Its tasks are threads, as `images/gate/tasks` runs on `moruna_sys::plane::Rings`: a region of
//! `IN_FLIGHT + 1` pieces and `WINDOWS` windows; each batch answered in order with a record in a
//! window, then released; asleep on the task's bell while it has no batch or no window
//! (`Rings::wait_input`, `Rings::wait_window`), never waiting on its port. The plane is
//! `moruna_plane::RingPlane` as the stage meets it: a record the stage keeps in place holds its
//! window until downstream drops the last buffer over it and the stage reaps it; a landed one is
//! dropped at once; a dropped record's window is idle, and idle windows go back to the task only
//! in `RingPlane::next`'s pump, by its rule (`gives_idle`). The run's arena is the
//! `FakeAllocator`, capped.

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
    Allocator, CancelToken, Fingerprint, InitCtx, Kernel, KernelKind, KernelState, MorunaError,
    Payload, PayloadKind, PayloadSpec, Result, SourceSchema, Tier, TierPref,
};
use moruna_testkit::{FakeAllocator, FakeSource};

use super::common::RigBuilder;

/// `layout::IN_FLIGHT`.
const IN_FLIGHT: usize = 3;
/// `layout::WINDOWS` (two before `6b6375f8`).
const WINDOWS: usize = 3;
/// `layout::QUEUED_MAX` since `c512249c` (`2 * IN_FLIGHT` before).
const QUEUED_MAX: usize = 0;
/// A task's region: `IN_FLIGHT` pieces and one more (`layout::ring_sizes`).
const REGION: usize = IN_FLIGHT + 1;
/// `task.rs`'s `LOOK_NS`, shortened: how long a wait on the task looks before it looks again.
const LOOK: Duration = Duration::from_millis(20);
/// The framing `encode_framing` takes from the run's arena for each record it lays out.
const FRAMING: usize = 4096;

/// `RingPlane::pump`'s rule for giving a task its idle windows back
/// (`kernel/plane/core/src/ringed.rs`, `low`): when at least half its windows are idle, or when
/// it waits for a window on its port. A native task never waits on its port: it sleeps on its
/// bell (`Rings::wait_window`), so `port_wait` is always false for it.
fn gives_idle(idle: usize, windows: usize, port_wait: bool) -> bool {
    idle > 0 && (idle * 2 >= windows || port_wait)
}

/// `layout::keep_in_place`.
fn keep_in_place(held: usize, windows: usize) -> bool {
    held + 1 < windows
}

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
    /// The engine's side as it last began to wait on the task (`Look`).
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

    /// The pump's regrant of idle windows (`gives_idle`).
    fn pump(&self, w: &mut Wire) {
        if gives_idle(w.idle, WINDOWS, false) {
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

    /// The task (`images/gate/tasks`): each batch in turn, answered with a record in a window
    /// and released; asleep on its bell when it has no batch or no window.
    fn task(&self, work: Duration) {
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
            let answer = heap_copy(&batch);
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
            w.holding -= 1;
            w.events.push_back(Ev::Released(id));
            self.bell.notify_all();
        }
    }
}

/// The task's answer to a batch: its rows, written in a window (the plane's memory).
fn heap_copy(batch: &RecordBatch) -> RecordBatch {
    let values: Vec<i64> = batch
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .map(|a| a.values().to_vec())
        .unwrap_or_default();
    RecordBatch::try_new(batch.schema(), vec![Arc::new(Int64Array::from(values))])
        .unwrap_or_else(|e| panic!("the task's answer: {e}"))
}

/// `batch` copied into the run's arena (`moruna_adapters::land::land_table`).
fn land_into(batch: &RecordBatch, alloc: &dyn Allocator) -> Result<RecordBatch> {
    let values = batch
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| kernel_error("not an Int64 column"))?;
    let bytes: Vec<u8> = values
        .values()
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
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
    /// Answers the unit under way had taken.
    taken: usize,
}

/// `TaskState` in ring mode.
struct Ring {
    plane: Arc<Plane>,
    thread: Option<std::thread::JoinHandle<()>>,
    alloc: Arc<dyn Allocator>,
    piece: u64,
    in_flight: VecDeque<u64>,
    queued: VecDeque<RecordBatch>,
    early: VecDeque<Seen>,
    schema: Option<SchemaRef>,
    stage_schema: Arc<Mutex<Option<SchemaRef>>>,
    held: usize,
    released: Released,
    /// The test's own stop, so a stalled run's threads end: the stage's waits are never told
    /// the run's cancel (`Packaged::bind_run_ports` has no caller in `run.rs`).
    stop: Arc<AtomicBool>,
    taken: usize,
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
    /// `encode_framing`'s framing, from the run's arena, for one record laid out.
    fn framing(&self) -> Result<()> {
        drop(self.alloc.alloc(FRAMING, Tier::Host)?);
        Ok(())
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
        let len = rows as u64 * 8;
        if len <= self.piece || rows <= 1 {
            return Ok(vec![batch.clone()]);
        }
        let target = self.piece - self.piece / 10;
        let count = usize::try_from(len.div_ceil(target.max(1)))
            .unwrap_or(rows)
            .clamp(1, rows);
        let each = rows.div_ceil(count);
        Ok((0..rows)
            .step_by(each)
            .map(|at| batch.slice(at, each.min(rows - at)))
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
            Some(Ev::Put(r)) if !self.in_flight.is_empty() => {
                self.early.push_back(Seen::Put(r));
                Ok(true)
            }
            Some(Ev::Put(r)) => {
                self.plane.drop_record(r);
                Err(kernel_error(
                    "it put a record before it was offered a batch",
                ))
            }
            Some(Ev::Released(b)) if !self.in_flight.is_empty() => {
                self.early.push_back(Seen::Released(b));
                Ok(true)
            }
            Some(Ev::Released(_)) => Ok(true),
            Some(Ev::Ended) => Err(kernel_error("it ended while the engine offered a batch")),
        }
    }

    fn output(&mut self, out: u64) -> Result<Payload> {
        if !keep_in_place(self.held, WINDOWS) {
            return self.land(out);
        }
        let record = self.plane.record(out)?;
        let kept = in_place(&record, out, &self.released)?;
        self.held += 1;
        Ok(Payload::Table(kept, Tier::Host))
    }

    fn land(&mut self, out: u64) -> Result<Payload> {
        let record = self.plane.record(out)?;
        let landed = land_into(&record, self.alloc.as_ref());
        self.plane.drop_record(out);
        Ok(Payload::Table(landed?, Tier::Host))
    }

    fn wait_any(&mut self) -> Result<Seen> {
        if let Some(seen) = self.early.pop_front() {
            return Ok(seen);
        }
        lock(&self.plane.wire).engine = Some(EngineLook {
            held: self.held,
            in_flight: self.in_flight.len(),
            queued: self.queued.len(),
            taken: self.taken,
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

    fn answers(&mut self, offered: u64) -> Result<Payload> {
        let mut parts = Vec::new();
        loop {
            match self.wait_any()? {
                Seen::Put(out) => {
                    let part = if parts.is_empty() {
                        self.output(out)
                    } else {
                        self.land(out)
                    };
                    parts.push(part?);
                }
                Seen::Released(b) if b == offered => break,
                Seen::Released(_) => {}
                Seen::Ended => return Err(kernel_error("it ended before it answered")),
            }
        }
        match parts.len() {
            0 => Err(kernel_error("a batch said done without a record")),
            1 => Ok(parts.remove(0)),
            _ => self.join(parts),
        }
    }

    fn pipelined(&mut self, batch: &RecordBatch) -> Result<Payload> {
        if self.schema.is_none() {
            self.schema = lock(&self.stage_schema).clone();
        }
        let mut taken: Vec<Payload> = Vec::new();
        self.taken = 0;
        let pieces = self.pieces(batch)?;
        self.queued.extend(pieces);
        self.feed()?;
        if self.schema.is_none() && taken.is_empty() {
            taken.push(self.take_oldest()?);
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
                self.taken = taken.len();
                taken.push(self.take_oldest()?);
                came = true;
            }
            self.feed()?;
            // `layout::holds_worker`.
            if self.queued.len() > QUEUED_MAX {
                self.taken = taken.len();
                taken.push(self.take_oldest()?);
                self.feed()?;
            } else if !came {
                break;
            }
        }
        match taken.len() {
            0 => {
                let schema = self
                    .schema
                    .clone()
                    .ok_or_else(|| kernel_error("no answer's schema yet"))?;
                Ok(Payload::Table(RecordBatch::new_empty(schema), Tier::Host))
            }
            1 => Ok(taken.remove(0)),
            _ => self.join(taken),
        }
    }

    /// `TaskState::feed`, by `layout::to_post`.
    fn feed(&mut self) -> Result<()> {
        let room = IN_FLIGHT.saturating_sub(self.in_flight.len());
        for _ in 0..room.min(self.queued.len()) {
            let Some(part) = self.queued.pop_front() else {
                break;
            };
            let offered = self.offer(&part)?;
            self.in_flight.push_back(offered);
        }
        Ok(())
    }

    fn take_oldest(&mut self) -> Result<Payload> {
        let oldest = self
            .in_flight
            .pop_front()
            .ok_or_else(|| kernel_error("no batch in flight"))?;
        let answer = self.answers(oldest)?;
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

    fn join(&mut self, parts: Vec<Payload>) -> Result<Payload> {
        let batches: Vec<RecordBatch> = parts
            .into_iter()
            .filter_map(|p| match p {
                Payload::Table(b, _) => Some(b),
                Payload::Tensor(..) => None,
            })
            .collect();
        let schema = batches[0].schema();
        let joined = moruna_kernel::arrow::compute::concat_batches(&schema, &batches)
            .map_err(|e| kernel_error(&e.to_string()))?;
        drop(batches);
        self.reap();
        Ok(Payload::Table(
            land_into(&joined, self.alloc.as_ref())?,
            Tier::Host,
        ))
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

/// `ImageTask` in ring mode with `tasks` processes (`MORUNA_PROCESSES`).
struct NativeStage {
    tasks: usize,
    piece: u64,
    work: Duration,
    stage_schema: Arc<Mutex<Option<SchemaRef>>>,
    stop: Arc<AtomicBool>,
    planes: Mutex<Vec<Arc<Plane>>>,
    applies: AtomicU64,
}

impl NativeStage {
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
        let work = self.work;
        let thread = std::thread::Builder::new()
            .name(format!("task-{}", ctx.instance))
            .spawn(move || task.task(work))
            .map_err(|e| kernel_error(&e.to_string()))?;
        lock(&self.planes).push(Arc::clone(&plane));
        Ok(Box::new(Ring {
            plane,
            thread: Some(thread),
            alloc: Arc::clone(&ctx.alloc),
            piece: self.piece,
            in_flight: VecDeque::new(),
            queued: VecDeque::new(),
            early: VecDeque::new(),
            schema: None,
            stage_schema: Arc::clone(&self.stage_schema),
            held: 0,
            released: Released::default(),
            stop: Arc::clone(&self.stop),
            taken: 0,
        }))
    }

    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        self.applies.fetch_add(1, Ordering::Relaxed);
        let st = state
            .as_any_mut()
            .downcast_mut::<Ring>()
            .ok_or_else(|| kernel_error("a state this kernel did not make"))?;
        let Payload::Table(batch, _) = input else {
            return Err(kernel_error("not a table"));
        };
        st.pipelined(&batch)
    }

    fn finish(&self, state: &mut dyn KernelState) -> Result<Option<Payload>> {
        let st = state
            .as_any_mut()
            .downcast_mut::<Ring>()
            .ok_or_else(|| kernel_error("a state this kernel did not make"))?;
        let mut rest = Vec::new();
        loop {
            st.feed()?;
            if st.in_flight.is_empty() {
                break;
            }
            rest.push(st.take_oldest()?);
        }
        let rest = match rest.len() {
            0 => None,
            1 => rest.pop(),
            _ => Some(st.join(rest)?),
        };
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

#[test]
#[ignore = "item 3.1, the full-year hang: fails until its fix; architecture/work/3.1-full-year-hang.md"]
fn sc_p_3_1_a_native_stage_of_eight_tasks_under_a_memory_bound_keeps_making_progress() {
    // The one-month Score run at a 2 GiB budget, scaled down by 1,024: eight tasks, eight
    // workers, morsels of 8 MiB cut into pieces of about 1.2 MiB (each task's plane is a quarter
    // of its share of the stage's budget), so a morsel is more pieces than a task holds in
    // flight; the arena is half the data.
    const PIECE: u64 = 8 * 1024;
    const ROWS: u64 = 512 * 1024;
    let stage = Arc::new(NativeStage {
        tasks: 8,
        piece: PIECE,
        work: Duration::from_micros(200),
        stage_schema: Arc::default(),
        stop: Arc::default(),
        planes: Mutex::default(),
        applies: AtomicU64::new(0),
    });
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 8;
            cfg.workers_active = 8;
            cfg.initial_morsel_target = 6 * PIECE;
            cfg.morsel_min = 8;
            cfg.read_ahead = 8;
        })
        .source(FakeSource::new().splits(1, ROWS, ROWS * 8))
        .alloc(FakeAllocator::new().with_limit(Tier::Host, ROWS * 8 / 2))
        .kernel(Arc::clone(&stage) as Arc<dyn Kernel>)
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
            // The stage's waits never see the run's cancel; the model's stop ends them.
            stage.stop.store(true, Ordering::SeqCst);
            cancel.cancel();
        }
        (ended, handle.join().ok(), looks)
    });
    assert!(
        ended,
        "the run stopped making progress after {} units, {} batches written by the sink; the \
         stage's tasks: {looks:#?}",
        stage.applies.load(Ordering::Relaxed),
        rig.sink.written().len()
    );
    match outcome {
        Some(Ok(crate::RunOutcome::Completed { sinks })) => {
            let rows: u64 = sinks.iter().map(|s| s.rows).sum();
            assert_eq!(rows, ROWS, "every row once");
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}
