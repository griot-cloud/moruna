//! The data plane in memory, for testing a function on a host.
//!
//! [`Plane::run`] plays the engine: it lays each input batch out as a record, as the engine does,
//! posts it on the task's input ring, gives the task its windows, takes back each record the task
//! posts (through the engine's record check, then read back with arrow-ipc), and reads the exit
//! code the task says. The task runs on a thread of its own, where [`crate::serve`] finds these
//! rings as on the core it finds the task's own. The two sides speak the core's ring protocol,
//! `moruna_plane::ring`, the same source `moruna_sys::plane` builds: only the memory differs, two
//! page-aligned allocations here and two shared objects there.
//!
//! ```
//! use std::process::ExitCode;
//! use std::sync::Arc;
//!
//! use moruna_function::host::Plane;
//! use moruna_function::{RecordBatch, serve};
//! use arrow_array::{ArrayRef, Int64Array};
//!
//! let a: ArrayRef = Arc::new(Int64Array::from(vec![1, 2, 3]));
//! let input = RecordBatch::try_from_iter([("a", a)]).unwrap();
//! let outcome = Plane::new()
//!     .run([input.clone()], || serve(|batch, out| out.batch(&batch)))
//!     .unwrap();
//! assert_eq!(outcome.said, Some(0));
//! assert_eq!(outcome.batches(), [input]);
//! ```

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt;
use std::io::Cursor;
use std::process::ExitCode;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use arrow_array::RecordBatch;
use arrow_ipc::reader::StreamReader;
use moruna_ipc_check::frame::END_OF_STREAM;
use moruna_ipc_check::record::{Need, check_record, framing_need};
use moruna_plane::ring::{Engine, Geometry, InEntry, Mode, OutEntry, Shared, Task, layout};

use crate::code;
use crate::error::FunctionError;
use crate::lane::{Lane, Mapped, Posted, Unattached, Window};

const PAGE: usize = 4096;

/// How long a sleep on a bell lasts at most before it looks again: the host's stand-in for the
/// core's futex, which also notices a side that went away.
const NAP: Duration = Duration::from_millis(20);

/// Memory both sides map: a region or a rings object, page aligned and zeroed.
pub(crate) struct Memory {
    ptr: NonNull<u8>,
    layout: Layout,
    bell: Mutex<()>,
    rung: Condvar,
    gone: AtomicBool,
}

// SAFETY: the engine's thread and the task's share the memory as the core's two processes share
// an object: its words through atomics, and its byte ranges each owned by one side at a time
// under the ring protocol, never through a reference to the whole.
unsafe impl Send for Memory {}
// SAFETY: as above.
unsafe impl Sync for Memory {}

impl Memory {
    fn new(len: usize) -> Memory {
        let size = len.max(PAGE).next_multiple_of(PAGE);
        let layout = Layout::from_size_align(size, PAGE).expect("a page-aligned layout");
        // SAFETY: the layout's size is not zero.
        let ptr = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(ptr).unwrap_or_else(|| std::alloc::handle_alloc_error(layout));
        Memory {
            ptr,
            layout,
            bell: Mutex::new(()),
            rung: Condvar::new(),
            gone: AtomicBool::new(false),
        }
    }

    fn len(&self) -> usize {
        self.layout.size()
    }

    fn check(&self, at: usize, n: usize) {
        assert!(
            at.checked_add(n).is_some_and(|end| end <= self.len()),
            "{n} bytes at {at} are past the memory's {}",
            self.len()
        );
    }

    /// The address of byte `at`.
    fn at(&self, at: usize) -> NonNull<u8> {
        self.check(at, 0);
        // SAFETY: `at` is within the allocation, or one past its end.
        unsafe { self.ptr.add(at) }
    }

    fn write(&self, at: usize, data: &[u8]) {
        self.check(at, data.len());
        // SAFETY: the range is inside the allocation, and the protocol gives it to this side.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), self.at(at).as_ptr(), data.len()) }
    }

    fn read(&self, at: usize, out: &mut [u8]) {
        self.check(at, out.len());
        // SAFETY: as in `write`.
        unsafe { std::ptr::copy_nonoverlapping(self.at(at).as_ptr(), out.as_mut_ptr(), out.len()) }
    }

    fn word(&self, at: usize) -> Option<&AtomicU64> {
        if !at.is_multiple_of(8) || at.checked_add(8).is_none_or(|end| end > self.len()) {
            return None;
        }
        // SAFETY: an aligned word inside the allocation, which outlives the borrow; both sides
        // reach a protocol word only atomically.
        Some(unsafe { AtomicU64::from_ptr(self.at(at).as_ptr().cast()) })
    }

    /// The low half of the word at `at`: a bell's futex word.
    fn bell_word(&self, at: usize) -> Option<&AtomicU32> {
        self.word(at)?;
        // SAFETY: as in `word`; the low half of an aligned word is aligned.
        Some(unsafe { AtomicU32::from_ptr(self.at(at).as_ptr().cast()) })
    }

    fn is_gone(&self) -> bool {
        self.gone.load(Ordering::Acquire)
    }

    /// The engine has gone: a sleeper wakes and no window will come.
    fn leave(&self) {
        self.gone.store(true, Ordering::Release);
        Shared::wake(self, 0);
    }
}

impl Drop for Memory {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with this layout.
        unsafe { dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

impl Shared for Memory {
    fn load(&self, at: usize) -> u64 {
        self.word(at).map_or(0, |w| w.load(Ordering::Acquire))
    }

    fn store(&self, at: usize, value: u64) {
        if let Some(w) = self.word(at) {
            w.store(value, Ordering::Release);
        }
    }

    fn fence(&self) {
        std::sync::atomic::fence(Ordering::SeqCst);
    }

    /// A futex: sleeps while the bell holds `expected`, until a wake or a moment passes. A
    /// wake is sent under the same lock the word is read under here, so none is lost.
    fn wait(&self, at: usize, expected: u32, _deadline: u64) -> bool {
        let Some(word) = self.bell_word(at) else {
            return false;
        };
        let guard = self.bell.lock().unwrap_or_else(PoisonError::into_inner);
        if word.load(Ordering::Acquire) == expected && !self.is_gone() {
            let _ = self
                .rung
                .wait_timeout(guard, NAP)
                .unwrap_or_else(PoisonError::into_inner);
        }
        true
    }

    fn wake(&self, _at: usize) {
        let _guard = self.bell.lock().unwrap_or_else(PoisonError::into_inner);
        self.rung.notify_all();
    }
}

type Lines = Arc<Mutex<Vec<String>>>;

fn push(log: &Lines, line: &str) {
    let mut log = log.lock().unwrap_or_else(PoisonError::into_inner);
    if line.is_empty() {
        log.push(String::new());
    }
    log.extend(line.lines().map(String::from));
}

/// The task's half of rings in memory.
pub(crate) struct HostLane {
    name: String,
    region: Arc<Memory>,
    rings: Arc<Memory>,
    half: Task,
    log: Lines,
}

impl Lane for HostLane {
    fn name(&self) -> &str {
        &self.name
    }

    fn batch(&mut self) -> Result<Option<Posted>, String> {
        let Some(e) = self.half.next(&*self.rings).map_err(|b| b.0.to_string())? else {
            return Ok(None);
        };
        let (Ok(offset), Ok(len)) = (usize::try_from(e.offset), usize::try_from(e.len)) else {
            return Err(String::from("an input past the region"));
        };
        if offset
            .checked_add(len)
            .is_none_or(|end| end > self.region.len())
        {
            return Err(String::from("an input past the region"));
        }
        let keep: Arc<dyn Mapped> = self.region.clone();
        Ok(Some(Posted {
            cookie: e.cookie,
            addr: self.region.at(offset),
            len,
            keep: Some(keep),
        }))
    }

    fn ended(&mut self) -> bool {
        self.half.ended(&*self.rings) || self.rings.is_gone()
    }

    fn wait_input(&mut self) {
        let rings = &*self.rings;
        if !rings.is_gone() {
            self.half.sleep(rings, u64::MAX, |t| t.has_input(rings));
        }
    }

    fn window_bytes(&self) -> u64 {
        self.half.geometry.window_bytes
    }

    fn window(&mut self) -> Result<Option<Window>, String> {
        let Some(w) = self
            .half
            .window(&*self.rings)
            .map_err(|b| b.0.to_string())?
        else {
            return Ok(None);
        };
        let g = self.half.geometry;
        let at = usize::try_from(g.window_at(w)).map_err(|_| String::from("a window"))?;
        let len = usize::try_from(g.window_bytes).map_err(|_| String::from("a window"))?;
        self.rings.check(at, len);
        Ok(Some(Window {
            number: w,
            addr: self.rings.at(at),
            len,
        }))
    }

    fn wait_window(&mut self) -> bool {
        let rings = &*self.rings;
        if !rings.is_gone() {
            self.half.sleep(rings, u64::MAX, |t| t.has_window(rings));
        }
        !rings.is_gone()
    }

    fn post(&mut self, window: u64, len: u64, cookie: u64) -> bool {
        let entry = OutEntry {
            window,
            offset: 0,
            len,
            cookie,
        };
        let posted = self.half.post(&*self.rings, entry);
        if posted {
            self.half.ring(&*self.rings);
        }
        posted
    }

    fn release(&mut self) {
        if self.half.release(&*self.rings) {
            self.half.ring(&*self.rings);
        }
    }

    fn exit(&mut self, code: i64) -> i64 {
        self.half.exit(&*self.rings, code);
        code
    }

    fn log(&mut self, line: &str) {
        push(&self.log, line);
    }
}

thread_local! {
    /// The lane [`Plane::run`] gave this thread, until `serve` takes it.
    static LANE: RefCell<Option<HostLane>> = const { RefCell::new(None) };
    /// The log of the run this thread is the task of.
    static LOG: RefCell<Option<Lines>> = const { RefCell::new(None) };
}

fn program() -> String {
    std::env::args()
        .next()
        .and_then(|a| {
            std::path::Path::new(&a)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| String::from("function"))
}

/// The lane [`Plane::run`] gave this thread, once.
pub(crate) fn attach() -> Result<HostLane, Unattached> {
    LANE.with(|l| l.borrow_mut().take())
        .ok_or_else(|| Unattached {
            code: code::NO_PLANE,
            line: format!(
                "{}: no plane: a function is served on the data plane, the core's or the one \
             moruna_function::host::Plane::run makes",
                program()
            ),
        })
}

/// A line of the log: the run's, on its task's thread, and standard error elsewhere.
pub(crate) fn log_line(line: &str) {
    LOG.with(|l| match &*l.borrow() {
        Some(log) => push(log, line),
        None => eprintln!("{line}"),
    })
}

/// The data-plane record of `batch`, laid out as the engine lays one out: what a task is given.
pub fn record(batch: &RecordBatch) -> Result<Vec<u8>, FunctionError> {
    crate::record::encode(batch)?.to_vec()
}

/// One input of [`Plane::run`].
#[derive(Clone, Debug)]
pub enum Input {
    /// A batch, posted as its record ([`record`]).
    Batch(RecordBatch),
    /// A record's bytes, posted as they are: a damaged one, say.
    Record(Vec<u8>),
}

impl From<RecordBatch> for Input {
    fn from(batch: RecordBatch) -> Input {
        Input::Batch(batch)
    }
}

/// What the task posted in answer to one input.
#[derive(Clone, Debug, Default)]
pub struct Answer {
    /// Each record, as the task wrote it.
    pub records: Vec<Vec<u8>>,
    /// Each record that passed the engine's check, read back.
    pub batches: Vec<RecordBatch>,
    /// Whether the task released the input, saying it was done with it.
    pub released: bool,
}

/// How a run went.
#[derive(Debug)]
pub struct Outcome {
    /// What the task returned (`main`'s exit code).
    pub returned: ExitCode,
    /// The exit code the task said on its rings, if it said one.
    pub said: Option<i64>,
    /// Each input's answer, in the inputs' order.
    pub answers: Vec<Answer>,
    /// The task's log, a line an entry.
    pub log: Vec<String>,
    /// Each record the engine's check refused, with its sentence. On the core the first ends
    /// the task.
    pub refused: Vec<String>,
}

impl Outcome {
    /// Every batch the task wrote, in order.
    pub fn batches(&self) -> Vec<RecordBatch> {
        self.answers
            .iter()
            .flat_map(|a| a.batches.iter().cloned())
            .collect()
    }
}

/// Why a run did not complete.
#[derive(Debug)]
pub enum HostError {
    /// The plane's sizes cannot be laid out.
    Geometry(String),
    /// An input batch has no record.
    Input(FunctionError),
    /// An input record is larger than the region.
    TooBig { record: u64, region: u64 },
    /// The task wrote a word of its rings that could not be true.
    Rings(String),
    /// The task panicked.
    Panicked(String),
    /// The task neither answered nor ended within the plane's timeout.
    TimedOut,
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HostError::Geometry(m) => write!(f, "the plane's sizes: {m}"),
            HostError::Input(e) => write!(f, "an input batch: {e}"),
            HostError::TooBig { record, region } => write!(
                f,
                "an input record of {record} bytes is larger than the region of {region}"
            ),
            HostError::Rings(m) => write!(f, "the task broke its rings: {m}"),
            HostError::Panicked(m) => write!(f, "the task panicked: {m}"),
            HostError::TimedOut => {
                f.write_str("the task neither answered nor ended within the plane's timeout")
            }
        }
    }
}

impl std::error::Error for HostError {}

/// The engine's side of the data plane, in memory: an input region and a rings object with its
/// windows.
#[derive(Clone, Debug)]
pub struct Plane {
    name: String,
    region_bytes: u64,
    windows: u64,
    window_bytes: u64,
    timeout: Duration,
}

impl Default for Plane {
    fn default() -> Plane {
        Plane::new()
    }
}

/// Where the next record of `len` bytes goes in a region of `region` bytes, with `flight` (the
/// records posted and not released, oldest first) in it: after the newest, wrapping to the start
/// once the end has no room.
fn place(flight: &VecDeque<(usize, usize)>, len: usize, region: usize) -> Option<usize> {
    let need = len.max(1).next_multiple_of(PAGE);
    let (Some(&(oldest, _)), Some(&(newest, newest_len))) = (flight.front(), flight.back()) else {
        return (need <= region).then_some(0);
    };
    let head = (newest + newest_len.max(1)).next_multiple_of(PAGE);
    if newest >= oldest {
        if head + need <= region {
            Some(head)
        } else {
            (need <= oldest).then_some(0)
        }
    } else {
        (head + need <= oldest).then_some(head)
    }
}

/// A record the task posted, through the engine's check and read back.
fn read_back(bytes: &[u8]) -> Result<RecordBatch, String> {
    let end = match framing_need(bytes)? {
        Need::Framing(n) if n <= bytes.len() => n,
        _ => return Err(String::from("the record is shorter than its framing")),
    };
    check_record(&bytes[..end], bytes.len() as u64)?;
    let mut stream = bytes.to_vec();
    stream.extend_from_slice(&END_OF_STREAM);
    let mut reader = StreamReader::try_new(Cursor::new(stream), None).map_err(|e| e.to_string())?;
    match reader.next() {
        Some(b) => b.map_err(|e| e.to_string()),
        None => Err(String::from("the record holds no batch")),
    }
}

fn panic_message(p: Box<dyn std::any::Any + Send>) -> String {
    match p.downcast::<String>() {
        Ok(s) => *s,
        Err(p) => match p.downcast::<&str>() {
            Ok(s) => String::from(*s),
            Err(_) => String::from("a panic"),
        },
    }
}

fn bad(b: moruna_plane::ring::Bad) -> HostError {
    HostError::Rings(b.0.to_string())
}

/// One run's engine.
struct Run<'a> {
    geometry: Geometry,
    engine: Engine,
    region: &'a Memory,
    rings: &'a Memory,
    records: Vec<Vec<u8>>,
    rows: Vec<u64>,
    answers: Vec<Answer>,
    refused: Vec<String>,
    /// The records posted and not released, oldest first: offset and length.
    flight: VecDeque<(usize, usize)>,
    /// The oldest input not released.
    oldest: usize,
    /// The next input to post.
    next: usize,
    closed: bool,
}

impl Run<'_> {
    /// Takes every record the task posted, giving each window back.
    fn take(&mut self) -> Result<bool, HostError> {
        let mut took = false;
        while let Some(e) = self.engine.take(self.rings).map_err(bad)? {
            took = true;
            let at = usize::try_from(self.geometry.window_at(e.window) + e.offset)
                .map_err(|_| HostError::Rings(String::from("a record past the rings")))?;
            let mut bytes = vec![0u8; e.len as usize];
            self.rings.read(at, &mut bytes);
            if !self.engine.give(self.rings, e.window).map_err(bad)? {
                return Err(HostError::Rings(String::from("a window given twice")));
            }
            self.engine.ring(self.rings);
            let answer = usize::try_from(e.cookie)
                .ok()
                .filter(|i| (self.oldest..self.next).contains(i))
                .and_then(|i| self.answers.get_mut(i))
                .ok_or_else(|| {
                    HostError::Rings(format!(
                        "a record answering {}, which is not an input the task holds",
                        e.cookie
                    ))
                })?;
            match read_back(&bytes) {
                Ok(b) => answer.batches.push(b),
                Err(why) => self.refused.push(why),
            }
            answer.records.push(bytes);
        }
        Ok(took)
    }

    /// Notes `n` more inputs released, oldest first.
    fn release(&mut self, n: u64) -> bool {
        for _ in 0..n {
            self.flight.pop_front();
            self.answers[self.oldest].released = true;
            self.oldest += 1;
        }
        n > 0
    }

    /// Posts inputs while the input ring and the region have room, and closes after the last.
    fn post(&mut self) -> bool {
        let mut posted = false;
        while self.next < self.records.len() && self.engine.can_post() {
            let record = &self.records[self.next];
            let Some(offset) = place(&self.flight, record.len(), self.region.len()) else {
                break;
            };
            self.region.write(offset, record);
            let entry = InEntry {
                offset: offset as u64,
                len: record.len() as u64,
                cookie: self.next as u64,
                rows: self.rows[self.next],
            };
            if !self.engine.post(self.rings, entry) {
                break;
            }
            self.engine.ring(self.rings);
            self.flight.push_back((offset, record.len()));
            self.next += 1;
            posted = true;
        }
        if self.next == self.records.len() && !self.closed {
            self.engine.close(self.rings);
            self.engine.ring(self.rings);
            self.closed = true;
            posted = true;
        }
        posted
    }

    /// Serves the task until it ends.
    fn drive(&mut self, task: &JoinHandle<ExitCode>, timeout: Duration) -> Result<(), HostError> {
        let deadline = Instant::now() + timeout;
        loop {
            // Whether the task had ended before this look: then the look sees all it did.
            let ended = task.is_finished();
            self.engine.look(self.rings);
            // The releases first, then the records: every record answering a released input was
            // posted before its release, so this take sees it, before the input leaves.
            let released = self.engine.released(self.rings).map_err(bad)?;
            let took = self.take()?;
            let released = self.release(released);
            let posted = self.post();
            if took || released || posted {
                continue;
            }
            if ended {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(HostError::TimedOut);
            }
            if let Some(seq) = self.engine.sleep(self.rings).map_err(bad)? {
                Shared::wait(self.rings, layout::ENGINE_BELL, seq, u64::MAX);
            }
            self.engine.awake(self.rings);
        }
    }
}

impl Plane {
    /// A region of 32 MiB and four windows of 8 MiB, the task named `function`, and five
    /// minutes for a run.
    pub fn new() -> Plane {
        Plane {
            name: String::from("function"),
            region_bytes: 32 << 20,
            windows: 4,
            window_bytes: 8 << 20,
            timeout: Duration::from_secs(300),
        }
    }

    /// The task's name, which its log's lines start with.
    pub fn named(mut self, name: impl Into<String>) -> Plane {
        self.name = name.into();
        self
    }

    /// The input region's size, whole pages.
    pub fn region(mut self, bytes: u64) -> Plane {
        self.region_bytes = bytes;
        self
    }

    /// `count` windows of `bytes` each, whole pages.
    pub fn windows(mut self, count: u64, bytes: u64) -> Plane {
        self.windows = count;
        self.window_bytes = bytes;
        self
    }

    /// How long the task may go without answering or ending before the run gives up on it.
    pub fn timeout(mut self, timeout: Duration) -> Plane {
        self.timeout = timeout;
        self
    }

    /// Runs `task` on a thread of its own, on rings in memory that carry `inputs` to it in order,
    /// and returns what it answered, said and logged. `task` is what a function's `main` is: a
    /// call of [`crate::serve`], which finds these rings.
    pub fn run<I, T>(&self, inputs: I, task: T) -> Result<Outcome, HostError>
    where
        I: IntoIterator,
        I::Item: Into<Input>,
        T: FnOnce() -> ExitCode + Send + 'static,
    {
        let geometry = Geometry::new(self.region_bytes, self.windows, self.window_bytes)
            .map_err(|b| HostError::Geometry(b.0.to_string()))?;
        let object = geometry
            .object_bytes()
            .and_then(|b| usize::try_from(b).ok())
            .ok_or_else(|| HostError::Geometry(String::from("the rings object is too big")))?;
        let region_len = usize::try_from(self.region_bytes)
            .map_err(|_| HostError::Geometry(String::from("the region is too big")))?;
        let (mut records, mut rows) = (Vec::new(), Vec::new());
        for input in inputs {
            match input.into() {
                Input::Batch(b) => {
                    rows.push(b.num_rows() as u64);
                    records.push(record(&b).map_err(HostError::Input)?);
                }
                Input::Record(r) => {
                    rows.push(0);
                    records.push(r);
                }
            }
        }
        if let Some(r) = records.iter().find(|r| r.len() > region_len) {
            return Err(HostError::TooBig {
                record: r.len() as u64,
                region: self.region_bytes,
            });
        }
        let region = Arc::new(Memory::new(region_len));
        let rings = Arc::new(Memory::new(object));
        let mut engine = Engine::format(&*rings, geometry, Mode::Protect, 0);
        for w in 0..geometry.windows {
            engine.give(&*rings, w).map_err(bad)?;
        }
        let log: Lines = Arc::default();
        let task = {
            let (name, region, rings, log) = (
                self.name.clone(),
                region.clone(),
                rings.clone(),
                log.clone(),
            );
            thread::Builder::new()
                .name(name.clone())
                .spawn(move || {
                    if let Ok(half) = Task::attach(&*rings) {
                        let lane = HostLane {
                            name,
                            region,
                            rings,
                            half,
                            log: log.clone(),
                        };
                        LANE.with(|l| *l.borrow_mut() = Some(lane));
                    }
                    LOG.with(|l| *l.borrow_mut() = Some(log));
                    let returned = task();
                    LANE.with(|l| l.borrow_mut().take());
                    LOG.with(|l| l.borrow_mut().take());
                    returned
                })
                .map_err(|e| HostError::Rings(format!("no thread for the task: {e}")))?
        };
        let count = records.len();
        let mut run = Run {
            geometry,
            engine,
            region: &region,
            rings: &rings,
            records,
            rows,
            answers: vec![Answer::default(); count],
            refused: Vec::new(),
            flight: VecDeque::new(),
            oldest: 0,
            next: 0,
            closed: false,
        };
        if let Err(e) = run.drive(&task, self.timeout) {
            rings.leave();
            region.leave();
            // A task stuck in its own code is left to itself; any other ends now it sees the
            // engine gone.
            if !matches!(e, HostError::TimedOut) {
                let _ = task.join();
            }
            return Err(e);
        }
        let said = run.engine.exit(&*rings);
        let (answers, refused) = (run.answers, run.refused);
        let returned = task
            .join()
            .map_err(|p| HostError::Panicked(panic_message(p)))?;
        let log = std::mem::take(&mut *log.lock().unwrap_or_else(PoisonError::into_inner));
        Ok(Outcome {
            returned,
            said,
            answers,
            log,
            refused,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_go_after_the_newest_and_wrap_once_the_end_is_full() {
        let mut f = VecDeque::new();
        assert_eq!(place(&f, 10, 4 * PAGE), Some(0));
        assert_eq!(
            place(&f, 0, 4 * PAGE),
            Some(0),
            "an empty record takes a page"
        );
        assert_eq!(place(&f, 5 * PAGE, 4 * PAGE), None);
        f.push_back((0, PAGE + 1));
        assert_eq!(place(&f, PAGE, 4 * PAGE), Some(2 * PAGE));
        f.push_back((2 * PAGE, 2 * PAGE));
        assert_eq!(
            place(&f, 1, 4 * PAGE),
            None,
            "full, and nothing free at the start"
        );
        f.pop_front();
        assert_eq!(place(&f, PAGE, 4 * PAGE), Some(0), "wraps to the start");
        assert_eq!(place(&f, 3 * PAGE, 4 * PAGE), None, "not before the oldest");
        f.push_back((0, PAGE));
        assert_eq!(
            place(&f, PAGE, 4 * PAGE),
            Some(PAGE),
            "after the newest, before the oldest"
        );
        assert_eq!(place(&f, 2 * PAGE, 4 * PAGE), None);
    }

    #[test]
    fn memory_is_page_aligned_zeroed_and_bounded() {
        let m = Memory::new(10);
        assert_eq!(m.len(), PAGE);
        assert_eq!(m.at(0).as_ptr() as usize % PAGE, 0);
        let mut b = [1u8; 4];
        m.read(PAGE - 4, &mut b);
        assert_eq!(b, [0; 4]);
        m.write(8, &[1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(m.load(8), 1);
        assert_eq!(m.load(3), 0, "an unaligned word reads as zero");
        assert_eq!(m.load(PAGE), 0, "past the end reads as zero");
        m.store(PAGE, 5);
        assert!(!m.wait(PAGE, 0, 0), "no bell past the end");
        assert!(m.wait(8, 0, 0), "the word moved on: no sleep");
        m.leave();
        assert!(m.wait(8, 1, 0), "the engine left: no sleep");
        let r = std::panic::catch_unwind(|| Memory::new(PAGE).write(PAGE, &[1]));
        assert!(r.is_err(), "a write past the end is refused");
    }

    /// The engine's side against a task that does not keep the protocol: a record the check
    /// refuses is noted, and one answering an input the task does not hold is the task's fault.
    #[test]
    fn records_are_judged_as_the_engine_judges_them() {
        let g = Geometry::new(4 * PAGE as u64, 2, PAGE as u64).unwrap();
        let region = Memory::new(4 * PAGE);
        let rings = Memory::new(g.object_bytes().unwrap() as usize);
        let mut engine = Engine::format(&rings, g, Mode::Protect, 0);
        for w in 0..2 {
            assert!(engine.give(&rings, w).unwrap());
        }
        let mut task = Task::attach(&rings).unwrap();
        let mut run = Run {
            geometry: g,
            engine,
            region: &region,
            rings: &rings,
            records: vec![vec![0u8; 8]],
            rows: vec![0],
            answers: vec![Answer::default()],
            refused: Vec::new(),
            flight: VecDeque::new(),
            oldest: 0,
            next: 0,
            closed: false,
        };
        assert!(run.post(), "the one input posted, and the ring closed");
        assert_eq!(task.next(&rings).unwrap().map(|e| e.cookie), Some(0));
        let mut post = |cookie| {
            let w = task.window(&rings).unwrap().unwrap();
            assert!(task.post(
                &rings,
                OutEntry {
                    window: w,
                    offset: 0,
                    len: 8,
                    cookie
                }
            ));
        };
        post(0);
        assert!(run.take().unwrap());
        assert_eq!(run.answers[0].records, [vec![0u8; 8]]);
        assert_eq!(run.refused.len(), 1, "eight zero bytes are no record");
        post(99);
        assert!(matches!(run.take(), Err(HostError::Rings(_))));
    }

    #[test]
    fn errors_say_what_happened() {
        let all = [
            HostError::Geometry(String::from("g")),
            HostError::Input(FunctionError::new("i")),
            HostError::TooBig {
                record: 9,
                region: 4,
            },
            HostError::Rings(String::from("r")),
            HostError::Panicked(String::from("p")),
            HostError::TimedOut,
        ];
        let said: Vec<String> = all.iter().map(|e| e.to_string()).collect();
        assert_eq!(
            said,
            [
                "the plane's sizes: g",
                "an input batch: i",
                "an input record of 9 bytes is larger than the region of 4",
                "the task broke its rings: r",
                "the task panicked: p",
                "the task neither answered nor ended within the plane's timeout",
            ]
        );
        assert_eq!(panic_message(Box::new(String::from("s"))), "s");
        assert_eq!(panic_message(Box::new("t")), "t");
        assert_eq!(panic_message(Box::new(3)), "a panic");
        assert!(read_back(&[]).is_err());
    }
}
