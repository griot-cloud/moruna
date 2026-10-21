//! What a run's tasks print, kept in the run's output store (MH 4.3 `log` and `logs`).
//!
//! Output arrives attributed ([`moruna_kernel::output`]): a stage's bytes on its streams, a
//! logging framework's records with their level, and, from the process's own standard output
//! and error ([`ProcessStreams`]), the bytes no stage can be named for, which are the engine's.
//! Handing it over never waits: [`Capture`] queues what it is given, at most [`QUEUE_BYTES`] of
//! it, and counts what it cannot hold as dropped.
//!
//! One thread, apart from every worker and from the thread that writes the run's events, turns
//! the bytes into records and writes them:
//!
//! - lines are kept whole: a line is one record without its newline; a line longer than
//!   [`RECORD_MAX`] is several, each but the last `partial`, each but the first `continued`,
//!   never split inside a UTF-8 character; past [`LINE_MAX`] the rest of the line is cut and
//!   counted (`truncated`); a line still unfinished after [`PARTIAL_AFTER`] goes out as it is,
//!   `partial`;
//! - a Python traceback written to standard error, from its `Traceback (most recent call
//!   last):` to its exception line, chained ones included, is one record, at most
//!   [`GROUP_MAX`] bytes;
//! - each producer (a task, or the engine) is held to [`RATE_RECORDS`] records and
//!   [`RATE_BYTES`] bytes a second, with bursts, and to [`PRODUCER_BYTES_MAX`] bytes in all; what
//!   is past that is dropped and counted, and where the drops end a `platform` record says how
//!   many there were;
//! - records are written in segments of the producer's file tree (`<logs>/a<attempt>-<start>/
//!   <task>/<first seq>.jsonl`, the engine's under `_engine`), each written whole and renamed
//!   into place, so a reader never sees half a segment;
//! - at most every [`LOGS_EVERY`] the thread tells the peer how far each producer's records
//!   reach now (`logs`), and once more when the run is over, with `drained`.
//!
//! The peer hears counts. What the tasks printed stays in the store.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use moruna_kernel::StageId;
use moruna_kernel::output::{Level, OutputSink, Stream};

use super::protocol::{EVENTS_VERSION, Event, LogLine, LogReach, LogStream};

/// The most text one record holds; a longer line is several.
pub const RECORD_MAX: usize = 16 << 10;
/// The most of one line that is kept; the rest is cut and counted.
pub const LINE_MAX: usize = 1 << 20;
/// The most one grouped traceback holds; the rest is cut and counted.
pub const GROUP_MAX: usize = 64 << 10;
/// The most output waiting to be written before more is dropped.
pub const QUEUE_BYTES: usize = 8 << 20;
/// A producer's segment is written once it holds this much.
pub const SEGMENT_BYTES: usize = 256 << 10;
/// A producer's segment is written once its first record has waited this long.
pub const SEGMENT_AGE: Duration = Duration::from_millis(1000);
/// An unfinished line, or a traceback that stopped coming, goes out after this long.
pub const PARTIAL_AFTER: Duration = Duration::from_millis(1000);
/// The peer hears the store's reach at most this often.
pub const LOGS_EVERY: Duration = Duration::from_millis(2000);
/// Records a producer may write a second.
pub const RATE_RECORDS: u64 = 2_000;
/// Records a producer may write at once, its second's worth included.
pub const BURST_RECORDS: u64 = 10_000;
/// Bytes a producer may write a second.
pub const RATE_BYTES: u64 = 1 << 20;
/// Bytes a producer may write at once, its second's worth included.
pub const BURST_BYTES: u64 = 4 << 20;
/// The most of one producer's output the store keeps in one process.
pub const PRODUCER_BYTES_MAX: u64 = 256 << 20;
/// How long the end of a run waits for its output to be written.
pub const FINISH_WAIT: Duration = Duration::from_secs(10);
/// How much a reader of a stream takes at once.
pub const READ_CHUNK: usize = 64 << 10;
/// How often the writer looks at its clocks when nothing arrives.
const TICK: Duration = Duration::from_millis(100);

/// The directory name of the engine's own output: no task id starts with `_`.
pub const ENGINE: &str = "_engine";

/// The directory of one attempt's output under `logs`.
pub fn attempt_dir(logs: &Path, attempt: u32, start: &str) -> PathBuf {
    logs.join(format!("a{attempt}-{start}"))
}

/// A segment's file name: the first record it holds, zero-padded so names sort as numbers do.
pub fn segment_name(first_seq: u64) -> String {
    format!("{first_seq:020}.jsonl")
}

enum Msg {
    Bytes {
        producer: usize,
        stream: Stream,
        bytes: Vec<u8>,
        t_us: u64,
    },
    Record {
        producer: usize,
        level: Option<Level>,
        text: String,
        t_us: u64,
    },
    Close,
}

impl Msg {
    fn size(&self) -> usize {
        match self {
            Msg::Bytes { bytes, .. } => bytes.len(),
            Msg::Record { text, .. } => text.len(),
            Msg::Close => 0,
        }
    }
}

/// Drops counted where output is handed over, before the writer saw it.
#[derive(Default)]
struct Overflow {
    records: AtomicU64,
    bytes: AtomicU64,
}

struct Shared {
    tx: Mutex<Option<mpsc::Sender<Msg>>>,
    queued: AtomicUsize,
    overflow: Vec<Overflow>,
    holds: AtomicUsize,
    clock: Instant,
}

impl Shared {
    fn producer(&self, stage: Option<StageId>) -> usize {
        match stage {
            Some(s) if (s as usize) + 1 < self.overflow.len() => s as usize + 1,
            _ => 0,
        }
    }

    fn give(&self, msg: Msg, producer: usize) {
        let size = msg.size();
        let refuse = |shared: &Shared| {
            let o = &shared.overflow[producer];
            o.records.fetch_add(1, Ordering::Relaxed);
            o.bytes.fetch_add(size as u64, Ordering::Relaxed);
        };
        if self.queued.fetch_add(size, Ordering::AcqRel) + size > QUEUE_BYTES {
            self.queued.fetch_sub(size, Ordering::AcqRel);
            refuse(self);
            return;
        }
        let tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
        let sent = tx.as_ref().is_some_and(|tx| tx.send(msg).is_ok());
        drop(tx);
        if !sent {
            self.queued.fetch_sub(size, Ordering::AcqRel);
            refuse(self);
        }
    }

    fn t_us(&self) -> u64 {
        self.clock.elapsed().as_micros() as u64
    }
}

/// The run's output sink, as the scheduler and every reader of a stream hold it.
struct Sink(Arc<Shared>);

struct Hold(Arc<Shared>);

impl Drop for Hold {
    fn drop(&mut self) {
        self.0.holds.fetch_sub(1, Ordering::AcqRel);
    }
}

impl OutputSink for Sink {
    fn bytes(&self, stage: Option<StageId>, stream: Stream, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let producer = self.0.producer(stage);
        // A stream that says it is structured is still a stream: only `record` carries a level.
        let stream = match stream {
            Stream::Structured => Stream::Stderr,
            other => other,
        };
        self.0.give(
            Msg::Bytes {
                producer,
                stream,
                bytes: bytes.to_vec(),
                t_us: self.0.t_us(),
            },
            producer,
        );
    }

    fn record(&self, stage: Option<StageId>, level: Option<Level>, text: &str) {
        let producer = self.0.producer(stage);
        self.0.give(
            Msg::Record {
                producer,
                level,
                text: text.to_string(),
                t_us: self.0.t_us(),
            },
            producer,
        );
    }

    fn hold(&self) -> Box<dyn Send> {
        self.0.holds.fetch_add(1, Ordering::AcqRel);
        Box::new(Hold(Arc::clone(&self.0)))
    }
}

/// Where the writer tells its facts: the run's events, and the report's notes.
pub struct Tell {
    /// One event, at the microsecond it became true.
    pub event: Box<dyn Fn(u64, Event) + Send>,
    /// A note for the report: why output was not kept.
    pub note: Box<dyn Fn(String) + Send>,
}

/// One attempt's output capture: the sink the run writes to, and the thread that keeps it.
pub struct Capture {
    shared: Arc<Shared>,
    done: Mutex<Option<mpsc::Receiver<()>>>,
}

impl Capture {
    /// Capture the output of a chain whose tasks, by stage, are `tasks` (source first), into
    /// `dir`, the attempt's directory of the store, on `clock`; tell what it keeps through
    /// `tell`. A store that cannot be made is told as a note, and everything is then dropped
    /// and counted.
    pub fn start(
        dir: PathBuf,
        attempt: u32,
        start: String,
        tasks: Vec<String>,
        clock: Instant,
        tell: Tell,
    ) -> Capture {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            tx: Mutex::new(Some(tx)),
            queued: AtomicUsize::new(0),
            overflow: (0..=tasks.len()).map(|_| Overflow::default()).collect(),
            holds: AtomicUsize::new(0),
            clock,
        });
        let (done_tx, done_rx) = mpsc::channel();
        let mut writer = Writer::new(dir, attempt, start, tasks, Arc::clone(&shared), tell);
        let spawned = std::thread::Builder::new()
            .name("moruna-logs".to_string())
            .spawn(move || {
                writer.run(&rx);
                let _ = done_tx.send(());
            });
        let done = match spawned {
            Ok(_) => Some(done_rx),
            Err(error) => {
                // No writer: nothing is queued, so everything handed over is dropped and
                // counted, and the run says why.
                *shared.tx.lock().unwrap_or_else(|e| e.into_inner()) = None;
                tracing::error!(target: "logs", %error, "no thread for the run's output");
                None
            }
        };
        Capture {
            shared,
            done: Mutex::new(done),
        }
    }

    /// The sink the scheduler and every reader of a stream write to.
    pub fn sink(&self) -> Arc<dyn OutputSink> {
        Arc::new(Sink(Arc::clone(&self.shared)))
    }

    /// The run is over: wait (at most `wait`) for every writer still holding the sink, then
    /// write what is left and tell the store's last reach, every producer `drained`. `false`
    /// when the store did not finish in time: its last reach is then not told.
    pub fn finish(&self, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        while self.shared.holds.load(Ordering::Acquire) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let tx = self
            .shared
            .tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let Some(tx) = tx else {
            return false;
        };
        let _ = tx.send(Msg::Close);
        drop(tx);
        let done = self.done.lock().unwrap_or_else(|e| e.into_inner()).take();
        match done {
            Some(done) => done
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .is_ok(),
            None => false,
        }
    }
}

/// A token bucket: `rate` a second, at most `burst` at once.
struct Bucket {
    tokens: f64,
    rate: f64,
    burst: f64,
    at: Instant,
}

impl Bucket {
    fn new(rate: u64, burst: u64) -> Bucket {
        Bucket {
            tokens: burst as f64,
            rate: rate as f64,
            burst: burst as f64,
            at: Instant::now(),
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate).min(self.burst);
        self.at = now;
    }
}

/// Why a run of records was dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cause {
    Rate,
    Queue,
    Quota,
    Store,
}

/// One record as an assembler makes it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Piece {
    text: String,
    partial: bool,
    continued: bool,
    truncated: u64,
    t_us: u64,
}

/// How many of the first `max` bytes of `bytes` (all of them, when fewer) can go out without
/// splitting a UTF-8 character: a character the cut, or the end of what has arrived, would split
/// is left for the next piece. Invalid bytes are cut where they fall.
fn boundary(bytes: &[u8], max: usize) -> usize {
    let n = bytes.len().min(max);
    let mut i = n;
    let mut back = 0;
    while back < 3 && i > 0 && (bytes[i - 1] & 0b1100_0000) == 0b1000_0000 {
        i -= 1;
        back += 1;
    }
    if i == 0 {
        return n;
    }
    let lead = bytes[i - 1];
    let width = match lead {
        0xF0..=0xF7 => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    };
    if i - 1 + width > n { i - 1 } else { n }
}

/// `bytes` as text: invalid UTF-8 replaced, never dropped.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A traceback being grouped: its lines so far, and whether its exception line has come
/// (`closed`: a chained traceback may still follow).
struct Group {
    text: String,
    truncated: u64,
    t_us: u64,
    at: Instant,
    closed: bool,
}

impl Group {
    fn add(&mut self, line: &str, now: Instant) {
        let room = GROUP_MAX.saturating_sub(self.text.len() + 1);
        if line.len() < room {
            self.text.push('\n');
            self.text.push_str(line);
        } else {
            self.truncated += line.len() as u64 + 1;
        }
        self.at = now;
    }
}

const TRACEBACK: &str = "Traceback (most recent call last):";

fn chains(line: &str) -> bool {
    line.starts_with("During handling of the above exception")
        || line.starts_with("The above exception was the direct cause")
}

/// Turns one stream's bytes into records: whole lines, long lines in pieces, tracebacks as one.
struct Assembler {
    group_tracebacks: bool,
    buf: Vec<u8>,
    since: Option<(Instant, u64)>,
    /// Bytes of the current line already out in earlier pieces.
    line_bytes: usize,
    /// Bytes of the current line cut past [`LINE_MAX`].
    cut: u64,
    group: Option<Group>,
}

impl Assembler {
    fn new(group_tracebacks: bool) -> Assembler {
        Assembler {
            group_tracebacks,
            buf: Vec::new(),
            since: None,
            line_bytes: 0,
            cut: 0,
            group: None,
        }
    }

    fn push(&mut self, mut bytes: &[u8], t_us: u64, now: Instant, out: &mut Vec<Piece>) {
        while !bytes.is_empty() {
            let (segment, ended) = match bytes.iter().position(|b| *b == b'\n') {
                Some(at) => (&bytes[..at], true),
                None => (bytes, false),
            };
            bytes = &bytes[segment.len() + usize::from(ended)..];
            if self.since.is_none() && (!segment.is_empty() || ended) {
                self.since = Some((now, t_us));
            }
            let room = LINE_MAX.saturating_sub(self.line_bytes + self.buf.len());
            let keep = segment.len().min(room);
            self.buf.extend_from_slice(&segment[..keep]);
            self.cut += (segment.len() - keep) as u64;
            // A long line goes out in pieces as it comes.
            while self.buf.len() >= RECORD_MAX {
                self.piece(true, out);
            }
            if ended {
                self.end_line(now, out);
            }
        }
    }

    /// One piece of the current line from the front of the buffer; `partial` when the line
    /// goes on.
    fn piece(&mut self, partial: bool, out: &mut Vec<Piece>) {
        let n = if partial {
            boundary(&self.buf, RECORD_MAX)
        } else {
            self.buf.len()
        };
        let t_us = self.since.map_or(0, |(_, t)| t);
        let rest = self.buf.split_off(n);
        let body = std::mem::replace(&mut self.buf, rest);
        out.push(Piece {
            text: text(&body),
            partial,
            continued: self.line_bytes > 0,
            truncated: if partial { 0 } else { self.cut },
            t_us,
        });
        self.line_bytes += n;
    }

    fn end_line(&mut self, now: Instant, out: &mut Vec<Piece>) {
        let t_us = self.since.map_or(0, |(_, t)| t);
        let whole = self.line_bytes == 0 && self.cut == 0 && self.buf.len() <= RECORD_MAX;
        if self.group_tracebacks && whole {
            let line = text(&std::mem::take(&mut self.buf));
            self.line(line, t_us, now, out);
        } else {
            self.close_group(out);
            while self.buf.len() > RECORD_MAX {
                self.piece(true, out);
            }
            self.piece(false, out);
        }
        self.line_bytes = 0;
        self.cut = 0;
        self.since = None;
    }

    /// A whole line of a stream whose tracebacks are grouped.
    fn line(&mut self, line: String, t_us: u64, now: Instant, out: &mut Vec<Piece>) {
        if let Some(group) = self.group.as_mut() {
            let indented = line.starts_with(' ') || line.starts_with('\t');
            if group.closed {
                // After its exception line a traceback may chain another: an empty line, the
                // chain's sentence, and the next traceback are still the group's.
                if line.is_empty() || chains(&line) {
                    group.add(&line, now);
                    return;
                }
                if line == TRACEBACK {
                    group.add(&line, now);
                    group.closed = false;
                    return;
                }
            } else if indented || line.is_empty() || chains(&line) || line == TRACEBACK {
                group.add(&line, now);
                return;
            } else {
                // The exception line ends the traceback.
                group.add(&line, now);
                group.closed = true;
                return;
            }
            self.close_group(out);
        }
        if line == TRACEBACK {
            self.group = Some(Group {
                text: line,
                truncated: 0,
                t_us,
                at: now,
                closed: false,
            });
            return;
        }
        out.push(Piece {
            text: line,
            partial: false,
            continued: false,
            truncated: 0,
            t_us,
        });
    }

    fn close_group(&mut self, out: &mut Vec<Piece>) {
        if let Some(mut group) = self.group.take() {
            while group.text.ends_with('\n') {
                group.text.pop();
            }
            out.push(Piece {
                text: group.text,
                partial: false,
                continued: false,
                truncated: group.truncated,
                t_us: group.t_us,
            });
        }
    }

    /// What has waited too long goes out: an unfinished line as a partial piece, a traceback
    /// that stopped coming as it is.
    fn tick(&mut self, now: Instant, out: &mut Vec<Piece>) {
        if self
            .group
            .as_ref()
            .is_some_and(|g| now.saturating_duration_since(g.at) >= PARTIAL_AFTER)
        {
            self.close_group(out);
        }
        if let Some((since, _)) = self.since
            && !self.buf.is_empty()
            && self.cut == 0
            && now.saturating_duration_since(since) >= PARTIAL_AFTER
        {
            self.close_group(out);
            let n = boundary(&self.buf, self.buf.len());
            if n > 0 {
                let t_us = self.since.map_or(0, |(_, t)| t);
                let rest = self.buf.split_off(n);
                let body = std::mem::replace(&mut self.buf, rest);
                out.push(Piece {
                    text: text(&body),
                    partial: true,
                    continued: self.line_bytes > 0,
                    truncated: 0,
                    t_us,
                });
                self.line_bytes += n;
                self.since = Some((now, t_us));
            }
        }
    }

    /// The stream ended: what is left is the last record, a line with no newline.
    fn eof(&mut self, out: &mut Vec<Piece>) {
        self.close_group(out);
        if !self.buf.is_empty() || self.cut > 0 {
            while self.buf.len() > RECORD_MAX {
                self.piece(true, out);
            }
            self.piece(false, out);
        }
        self.line_bytes = 0;
        self.cut = 0;
        self.since = None;
    }
}

/// One producer's state in the writer.
struct Producer {
    task: Option<String>,
    dir: String,
    stdout: Assembler,
    stderr: Assembler,
    /// Records numbered so far.
    seq: u64,
    /// Records `1..=durable` are in the store.
    durable: u64,
    bytes: u64,
    truncated: u64,
    dropped: u64,
    dropped_bytes: u64,
    kept_bytes: u64,
    /// Drops since the last record written, and why the last one was.
    dropping: Option<(u64, u64, Cause)>,
    pending: Vec<u8>,
    pending_first: u64,
    pending_records: u64,
    pending_bytes: u64,
    pending_truncated: u64,
    pending_since: Option<Instant>,
    records: Bucket,
    byte_rate: Bucket,
    dirty: bool,
}

impl Producer {
    fn new(task: Option<String>) -> Producer {
        let dir = task.clone().unwrap_or_else(|| ENGINE.to_string());
        Producer {
            task,
            dir,
            stdout: Assembler::new(false),
            stderr: Assembler::new(true),
            seq: 0,
            durable: 0,
            bytes: 0,
            truncated: 0,
            dropped: 0,
            dropped_bytes: 0,
            kept_bytes: 0,
            dropping: None,
            pending: Vec::new(),
            pending_first: 0,
            pending_records: 0,
            pending_bytes: 0,
            pending_truncated: 0,
            pending_since: None,
            records: Bucket::new(RATE_RECORDS, BURST_RECORDS),
            byte_rate: Bucket::new(RATE_BYTES, BURST_BYTES),
            dirty: false,
        }
    }

    fn drop_one(&mut self, bytes: u64, cause: Cause) {
        self.dropped += 1;
        self.dropped_bytes += bytes;
        let (n, b, _) = self.dropping.unwrap_or((0, 0, cause));
        self.dropping = Some((n + 1, b + bytes, cause));
        self.dirty = true;
    }

    fn reach(&self, drained: bool) -> LogReach {
        LogReach {
            task: self.task.clone(),
            records: self.durable,
            bytes: self.bytes,
            truncated: self.truncated,
            dropped: self.dropped,
            dropped_bytes: self.dropped_bytes,
            drained,
        }
    }
}

fn dropped_sentence(records: u64, bytes: u64, cause: Cause) -> String {
    let why = match cause {
        Cause::Rate => format!(
            "the output came faster than {RATE_RECORDS} records or {RATE_BYTES} bytes a second"
        ),
        Cause::Queue => format!(
            "more output was waiting to be written than the engine holds ({QUEUE_BYTES} bytes)"
        ),
        Cause::Quota => {
            format!("the store keeps at most {PRODUCER_BYTES_MAX} bytes of one producer's output")
        }
        Cause::Store => "the output store could not be written".to_string(),
    };
    format!("{records} records ({bytes} bytes) of this output were dropped here: {why}")
}

struct Writer {
    dir: PathBuf,
    attempt: u32,
    start: String,
    producers: Vec<Producer>,
    shared: Arc<Shared>,
    tell: Tell,
    /// The store failed: why. Everything after is dropped and counted.
    failed: Option<String>,
    told_at: Instant,
    made: Vec<bool>,
}

impl Writer {
    fn new(
        dir: PathBuf,
        attempt: u32,
        start: String,
        tasks: Vec<String>,
        shared: Arc<Shared>,
        tell: Tell,
    ) -> Writer {
        let producers: Vec<Producer> = std::iter::once(Producer::new(None))
            .chain(tasks.into_iter().map(|t| Producer::new(Some(t))))
            .collect();
        let made = vec![false; producers.len()];
        Writer {
            dir,
            attempt,
            start,
            producers,
            shared,
            tell,
            failed: None,
            told_at: Instant::now(),
            made,
        }
    }

    fn run(&mut self, rx: &mpsc::Receiver<Msg>) {
        if let Err(error) = std::fs::create_dir_all(&self.dir) {
            self.fail(format!(
                "the output store {} could not be made: {error}",
                self.dir.display()
            ));
        }
        let mut pieces = Vec::new();
        loop {
            let msg = rx.recv_timeout(TICK);
            let now = Instant::now();
            match msg {
                Ok(Msg::Close) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Ok(msg) => {
                    self.shared.queued.fetch_sub(msg.size(), Ordering::AcqRel);
                    self.take(msg, now, &mut pieces);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            self.tick(now, &mut pieces);
        }
        // Whatever was queued before the close is written, then the streams end.
        for p in 0..self.producers.len() {
            pieces.clear();
            self.producers[p].stdout.eof(&mut pieces);
            self.emit_pieces(p, LogStream::Stdout, &mut pieces);
            self.producers[p].stderr.eof(&mut pieces);
            self.emit_pieces(p, LogStream::Stderr, &mut pieces);
        }
        self.overflow();
        for p in 0..self.producers.len() {
            self.mark_drops(p);
            self.flush(p);
        }
        let reach = self.producers.iter().map(|p| p.reach(true)).collect();
        self.event(Event::Logs { producers: reach });
    }

    fn take(&mut self, msg: Msg, now: Instant, pieces: &mut Vec<Piece>) {
        match msg {
            Msg::Bytes {
                producer,
                stream,
                bytes,
                t_us,
            } => {
                pieces.clear();
                let (assembler, stream) = match stream {
                    Stream::Stdout => (&mut self.producers[producer].stdout, LogStream::Stdout),
                    _ => (&mut self.producers[producer].stderr, LogStream::Stderr),
                };
                assembler.push(&bytes, t_us, now, pieces);
                self.emit_pieces(producer, stream, pieces);
            }
            Msg::Record {
                producer,
                level,
                text,
                t_us,
            } => {
                let (keep, cut) = if text.len() > LINE_MAX {
                    (boundary(text.as_bytes(), LINE_MAX), text.len())
                } else {
                    (text.len(), text.len())
                };
                let truncated = (cut - keep) as u64;
                self.emit(
                    producer,
                    LogStream::Structured,
                    level,
                    Piece {
                        text: text[..keep].to_string(),
                        partial: false,
                        continued: false,
                        truncated,
                        t_us,
                    },
                );
            }
            Msg::Close => {}
        }
    }

    fn emit_pieces(&mut self, producer: usize, stream: LogStream, pieces: &mut Vec<Piece>) {
        for piece in pieces.drain(..) {
            self.emit(producer, stream, None, piece);
        }
    }

    fn tick(&mut self, now: Instant, pieces: &mut Vec<Piece>) {
        for p in 0..self.producers.len() {
            pieces.clear();
            self.producers[p].stdout.tick(now, pieces);
            self.emit_pieces(p, LogStream::Stdout, pieces);
            self.producers[p].stderr.tick(now, pieces);
            self.emit_pieces(p, LogStream::Stderr, pieces);
            if self.producers[p]
                .pending_since
                .is_some_and(|s| now.saturating_duration_since(s) >= SEGMENT_AGE)
            {
                self.flush(p);
            }
        }
        if now.saturating_duration_since(self.told_at) >= LOGS_EVERY {
            // Where drops happened since the last look, a `platform` record says so: at most one
            // per producer each time, however long a flood lasts.
            self.overflow();
            for p in 0..self.producers.len() {
                self.mark_drops(p);
            }
            let reach: Vec<LogReach> = self
                .producers
                .iter_mut()
                .filter_map(|p| std::mem::take(&mut p.dirty).then(|| p.reach(false)))
                .collect();
            if !reach.is_empty() {
                self.event(Event::Logs { producers: reach });
            }
            self.told_at = now;
        }
    }

    /// Drops counted where output was handed over.
    fn overflow(&mut self) {
        for (p, o) in self.shared.overflow.iter().enumerate() {
            let records = o.records.swap(0, Ordering::AcqRel);
            let bytes = o.bytes.swap(0, Ordering::AcqRel);
            if records > 0 {
                let producer = &mut self.producers[p];
                producer.dropped += records;
                producer.dropped_bytes += bytes;
                let (n, b, _) = producer.dropping.unwrap_or((0, 0, Cause::Queue));
                producer.dropping = Some((n + records, b + bytes, Cause::Queue));
                producer.dirty = true;
            }
        }
    }

    /// Where a run of drops ends, a `platform` record says how many there were.
    fn mark_drops(&mut self, p: usize) {
        if self.failed.is_some() {
            return;
        }
        if let Some((records, bytes, cause)) = self.producers[p].dropping.take() {
            let t_us = self.shared.t_us();
            self.append(
                p,
                LogStream::Platform,
                None,
                Piece {
                    text: dropped_sentence(records, bytes, cause),
                    partial: false,
                    continued: false,
                    truncated: 0,
                    t_us,
                },
            );
        }
    }

    /// One record of a task's (or the engine's) output, held to its producer's limits.
    fn emit(&mut self, p: usize, stream: LogStream, level: Option<Level>, piece: Piece) {
        let size = piece.text.len() as u64;
        if self.failed.is_some() {
            self.producers[p].drop_one(size, Cause::Store);
            return;
        }
        let now = Instant::now();
        let producer = &mut self.producers[p];
        producer.records.refill(now);
        producer.byte_rate.refill(now);
        if producer.kept_bytes + size > PRODUCER_BYTES_MAX {
            producer.drop_one(size, Cause::Quota);
            return;
        }
        if producer.records.tokens < 1.0 || producer.byte_rate.tokens < size as f64 {
            producer.drop_one(size, Cause::Rate);
            return;
        }
        producer.records.tokens -= 1.0;
        producer.byte_rate.tokens -= size as f64;
        self.append(p, stream, level, piece);
    }

    fn append(&mut self, p: usize, stream: LogStream, level: Option<Level>, piece: Piece) {
        let (attempt, start) = (self.attempt, self.start.clone());
        let producer = &mut self.producers[p];
        producer.seq += 1;
        let seq = producer.seq;
        let text_bytes = piece.text.len() as u64;
        let line = LogLine {
            kind_of: "event",
            v: EVENTS_VERSION,
            attempt,
            start: start.clone(),
            seq,
            t_us: piece.t_us,
            kind: "log",
            id: format!("{start}/{}/{seq}", producer.dir),
            task: producer.task.clone(),
            stream,
            level: level.map(Level::name),
            bytes: piece.text.len() as u64,
            text: piece.text,
            partial: piece.partial,
            continued: piece.continued,
            truncated: piece.truncated,
        }
        .line();
        if producer.pending.is_empty() {
            producer.pending_first = seq;
            producer.pending_since = Some(Instant::now());
        }
        producer.pending.extend_from_slice(line.as_bytes());
        producer.pending_records += 1;
        producer.pending_bytes += text_bytes;
        producer.pending_truncated += u64::from(piece.truncated > 0);
        producer.kept_bytes += line.len() as u64;
        if producer.pending.len() >= SEGMENT_BYTES {
            self.flush(p);
        }
    }

    /// The producer's pending records, written as one segment.
    fn flush(&mut self, p: usize) {
        if self.producers[p].pending.is_empty() {
            return;
        }
        if self.failed.is_some() {
            let producer = &mut self.producers[p];
            producer.dropped += producer.pending_records;
            producer.dropped_bytes += producer.pending_bytes;
            clear_pending(producer);
            return;
        }
        let dir = self.dir.join(&self.producers[p].dir);
        if !self.made[p] {
            if let Err(error) = std::fs::create_dir_all(&dir) {
                self.fail(format!("{} could not be made: {error}", dir.display()));
                return self.flush(p);
            }
            self.made[p] = true;
        }
        let producer = &mut self.producers[p];
        let name = segment_name(producer.pending_first);
        let written = write_whole(&dir, &name, &producer.pending);
        match written {
            Ok(()) => {
                producer.durable = producer.seq;
                producer.bytes += producer.pending_bytes;
                producer.truncated += producer.pending_truncated;
                producer.dirty = true;
                clear_pending(producer);
            }
            Err(error) => {
                let why = format!("{}: {error}", dir.join(name).display());
                self.fail(why);
                self.flush(p);
            }
        }
    }

    fn fail(&mut self, why: String) {
        if self.failed.is_none() {
            tracing::error!(target: "logs", %why, "the run's output store failed");
            (self.tell.note)(format!(
                "the run's output is not all kept: {why}; what came after is dropped and counted"
            ));
            self.failed = Some(why);
        }
    }

    fn event(&self, event: Event) {
        (self.tell.event)(self.shared.t_us(), event);
    }
}

fn clear_pending(producer: &mut Producer) {
    producer.pending.clear();
    producer.pending_records = 0;
    producer.pending_bytes = 0;
    producer.pending_truncated = 0;
    producer.pending_since = None;
}

/// Write `bytes` as `dir/name`, whole: through a hidden temporary file, synced, then renamed.
fn write_whole(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = dir.join(format!(".{name}.tmp"));
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, dir.join(name))
}

/// Read a producer's records back from the store, in order: what a host's reader and a test
/// do with the files this module writes.
pub fn read_producer(dir: &Path) -> std::io::Result<Vec<serde_json::Value>> {
    let mut names: Vec<String> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".jsonl") && !n.starts_with('.'))
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    names.sort();
    let mut records = VecDeque::new();
    for name in names {
        let text = std::fs::read_to_string(dir.join(&name))?;
        for line in text.lines() {
            records.push_back(serde_json::from_str(line).map_err(std::io::Error::other)?);
        }
    }
    Ok(records.into())
}

/// The process's own standard output and error, captured into the run's output as the
/// engine's while the run lasts (Unix): whatever writes to descriptors 1 and 2 (a Rust kernel's
/// `println!`, a library's warning, the engine's own diagnostics) can name no stage.
#[cfg(unix)]
pub struct ProcessStreams {
    saved: Vec<(i32, i32)>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    readers: Vec<std::thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl ProcessStreams {
    /// Point descriptors 1 and 2 at pipes read into `sink`, until [`Self::restore`].
    pub fn redirect(sink: Arc<dyn OutputSink>) -> std::io::Result<ProcessStreams> {
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut streams = ProcessStreams {
            saved: Vec::new(),
            stop: Arc::clone(&stop),
            readers: Vec::new(),
        };
        for (fd, stream) in [(1, Stream::Stdout), (2, Stream::Stderr)] {
            let mut ends = [0i32; 2];
            // SAFETY: `ends` is two descriptors' room; pipe writes both or neither.
            if unsafe { libc::pipe(ends.as_mut_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let [read, write] = ends;
            // SAFETY: plain descriptor calls on descriptors this function owns or the process's
            // own 1 and 2; each result is checked.
            unsafe {
                libc::fcntl(read, libc::F_SETFD, libc::FD_CLOEXEC);
                libc::fcntl(write, libc::F_SETFD, libc::FD_CLOEXEC);
                libc::fcntl(read, libc::F_SETFL, libc::O_NONBLOCK);
                let saved = libc::dup(fd);
                if saved < 0 {
                    libc::close(read);
                    libc::close(write);
                    return Err(std::io::Error::last_os_error());
                }
                libc::fcntl(saved, libc::F_SETFD, libc::FD_CLOEXEC);
                if libc::dup2(write, fd) < 0 {
                    let error = std::io::Error::last_os_error();
                    libc::close(saved);
                    libc::close(read);
                    libc::close(write);
                    return Err(error);
                }
                libc::close(write);
                streams.saved.push((fd, saved));
            }
            let (sink, stop) = (Arc::clone(&sink), Arc::clone(&stop));
            let reader = std::thread::Builder::new()
                .name(format!("moruna-fd{fd}"))
                .spawn(move || read_fd(read, stream, sink.as_ref(), &stop))?;
            streams.readers.push(reader);
        }
        Ok(streams)
    }

    /// Give descriptors 1 and 2 back, then read what was left in the pipes.
    pub fn restore(mut self) {
        self.give_back();
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }

    fn give_back(&mut self) {
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        for (fd, saved) in self.saved.drain(..) {
            // SAFETY: `saved` is this struct's own copy of the process's descriptor.
            unsafe {
                libc::dup2(saved, fd);
                libc::close(saved);
            }
        }
        self.stop.store(true, Ordering::Release);
    }
}

#[cfg(unix)]
impl Drop for ProcessStreams {
    fn drop(&mut self) {
        self.give_back();
    }
}

/// Read descriptor `fd` into `sink` until asked to stop, then read what is left and close it.
#[cfg(unix)]
fn read_fd(fd: i32, stream: Stream, sink: &dyn OutputSink, stop: &std::sync::atomic::AtomicBool) {
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        let stopping = stop.load(Ordering::Acquire);
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one pollfd, owned here.
        unsafe { libc::poll(&mut poll, 1, 50) };
        loop {
            // SAFETY: `buf` is READ_CHUNK bytes owned here; the descriptor is non-blocking.
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                sink.bytes(None, stream, &buf[..n as usize]);
                continue;
            }
            break;
        }
        if stopping {
            break;
        }
    }
    // SAFETY: the read end is this reader's.
    unsafe { libc::close(fd) };
}

#[cfg(test)]
mod tests;
