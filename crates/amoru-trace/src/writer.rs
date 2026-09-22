//! The trace writer: the bounded channel, the writer thread, the in-memory chunks, the
//! overflow file and the final Arrow IPC file (04 d.1, e.1, e.2, f.1, f.3).

use std::collections::VecDeque;
use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use amoru_kernel::{AmoruError, RunId, StageId, TraceRecord, TraceSink, TraceTail};
use arrow::array::{
    ArrayBuilder, Float32Builder, Int64Builder, ListBuilder, RecordBatch, StringBuilder,
    UInt8Builder, UInt16Builder, UInt64Builder,
};
use arrow::datatypes::{DataType, SchemaRef};
use arrow::ipc::writer::{FileWriter, StreamWriter};
use crossbeam::channel::{Receiver, RecvTimeoutError, Sender, bounded};

use crate::view::{ChunkSet, TraceView, record_at};
use crate::{CHUNK_ROWS, DRAIN_INTERVAL_MS, Result, lock, run_id_hex};

/// What the facade gives the writer at start (d.1). The three tunables are the preamble's
/// `trace.path`, `trace.channel_capacity` and `trace.memory_limit` rows (i).
#[derive(Clone, Debug)]
pub struct TraceConfig {
    /// The final Arrow IPC file; `None` keeps the trace in memory plus the overflow file.
    pub path: Option<PathBuf>,
    /// Where the overflow file goes (e.2).
    pub staging_dir: PathBuf,
    /// `trace.channel_capacity`: records buffered before the writer.
    pub channel_capacity: usize,
    /// `trace.memory_limit`: in-memory chunk bytes before overflow to disk (TR-I4).
    pub memory_limit: u64,
    /// The run's identity, minted or read by the facade (12 f.1, f.7).
    pub run_id: RunId,
}

/// What travels down the channel. `Record` carries the record by value, so the record path
/// allocates nothing beyond the channel slot it is written into (l, anti-patterns).
///
/// The size difference between the variants is deliberate: boxing the record to even the
/// variants out would be an allocation per `record` call, which l forbids, and the markers are
/// one per flush and one per run.
#[allow(clippy::large_enum_variant)]
enum Msg {
    Record(TraceRecord),
    /// A flush marker: the writer answers when every record queued before it is in a chunk.
    Flush(Sender<Result<()>>),
    /// Finalise and exit; `finish` joins the thread afterwards.
    Finish,
}

/// Per-column Arrow builders for one chunk (l: builders per column, never row-to-batch
/// conversion of a `Vec<TraceRecord>`).
struct Builders {
    schema: SchemaRef,
    rows: usize,
    seq: UInt64Builder,
    stage: UInt16Builder,
    worker: UInt16Builder,
    instance: UInt16Builder,
    t_start_ns: UInt64Builder,
    t_end_ns: UInt64Builder,
    rows_in: UInt64Builder,
    bytes_in: UInt64Builder,
    rows_out: UInt64Builder,
    bytes_out: UInt64Builder,
    tier_in: UInt8Builder,
    tier_out: UInt8Builder,
    feat_mean_string_len: Float32Builder,
    feat_null_ratio: Float32Builder,
    feat_column_bytes: ListBuilder<UInt64Builder>,
    knob_morsel_target: UInt64Builder,
    knob_active_workers: UInt16Builder,
    knob_read_ahead: UInt16Builder,
    mem_anon_before: UInt64Builder,
    mem_anon_peak: UInt64Builder,
    dev_mem_peak: UInt64Builder,
    cpu_time_us: UInt64Builder,
    throttled_delta_us: UInt64Builder,
    q_bytes_before: ListBuilder<UInt64Builder>,
    q_bytes_after: ListBuilder<UInt64Builder>,
    staging_bytes_delta: Int64Builder,
    placement_miss_wait_us: UInt64Builder,
    state_bytes: UInt64Builder,
    sizer: UInt8Builder,
    outcome: UInt8Builder,
    error: StringBuilder,
}

fn list_builder() -> ListBuilder<UInt64Builder> {
    // Sized for one value per record; a record whose lists are longer grows the buffer
    // geometrically, and a record whose lists are empty does not carry the capacity of one
    // that is not (a chunk's memory is what TR-I4 bounds).
    ListBuilder::new(UInt64Builder::with_capacity(CHUNK_ROWS)).with_field(Arc::new(
        arrow::datatypes::Field::new("item", DataType::UInt64, false),
    ))
}

fn u64b() -> UInt64Builder {
    UInt64Builder::with_capacity(CHUNK_ROWS)
}
fn u16b() -> UInt16Builder {
    UInt16Builder::with_capacity(CHUNK_ROWS)
}
fn u8b() -> UInt8Builder {
    UInt8Builder::with_capacity(CHUNK_ROWS)
}

impl Builders {
    fn new(schema: SchemaRef) -> Builders {
        Builders {
            schema,
            rows: 0,
            seq: u64b(),
            stage: u16b(),
            worker: u16b(),
            instance: u16b(),
            t_start_ns: u64b(),
            t_end_ns: u64b(),
            rows_in: u64b(),
            bytes_in: u64b(),
            rows_out: u64b(),
            bytes_out: u64b(),
            tier_in: u8b(),
            tier_out: u8b(),
            feat_mean_string_len: Float32Builder::with_capacity(CHUNK_ROWS),
            feat_null_ratio: Float32Builder::with_capacity(CHUNK_ROWS),
            feat_column_bytes: list_builder(),
            knob_morsel_target: u64b(),
            knob_active_workers: u16b(),
            knob_read_ahead: u16b(),
            mem_anon_before: u64b(),
            mem_anon_peak: u64b(),
            dev_mem_peak: u64b(),
            cpu_time_us: u64b(),
            throttled_delta_us: u64b(),
            q_bytes_before: list_builder(),
            q_bytes_after: list_builder(),
            staging_bytes_delta: Int64Builder::with_capacity(CHUNK_ROWS),
            placement_miss_wait_us: u64b(),
            state_bytes: u64b(),
            sizer: u8b(),
            outcome: u8b(),
            error: StringBuilder::new(),
        }
    }

    /// Append one record, column by column. No allocation of record size beyond the
    /// builders' amortised growth.
    fn append(&mut self, r: TraceRecord) {
        self.seq.append_value(r.seq);
        self.stage.append_value(r.stage);
        self.worker.append_value(r.worker);
        self.instance.append_value(r.instance);
        self.t_start_ns.append_value(r.t_start_ns);
        self.t_end_ns.append_value(r.t_end_ns);
        self.rows_in.append_value(r.rows_in);
        self.bytes_in.append_value(r.bytes_in);
        self.rows_out.append_value(r.rows_out);
        self.bytes_out.append_value(r.bytes_out);
        self.tier_in.append_value(r.tier_in);
        self.tier_out.append_value(r.tier_out);
        self.feat_mean_string_len
            .append_value(r.feat_mean_string_len);
        self.feat_null_ratio.append_value(r.feat_null_ratio);
        self.feat_column_bytes
            .values()
            .append_slice(&r.feat_column_bytes);
        self.feat_column_bytes.append(true);
        self.knob_morsel_target.append_value(r.knob_morsel_target);
        self.knob_active_workers.append_value(r.knob_active_workers);
        self.knob_read_ahead.append_value(r.knob_read_ahead);
        self.mem_anon_before.append_value(r.mem_anon_before);
        self.mem_anon_peak.append_value(r.mem_anon_peak);
        self.dev_mem_peak.append_value(r.dev_mem_peak);
        self.cpu_time_us.append_value(r.cpu_time_us);
        self.throttled_delta_us.append_value(r.throttled_delta_us);
        self.q_bytes_before.values().append_slice(&r.q_bytes_before);
        self.q_bytes_before.append(true);
        self.q_bytes_after.values().append_slice(&r.q_bytes_after);
        self.q_bytes_after.append(true);
        self.staging_bytes_delta.append_value(r.staging_bytes_delta);
        self.placement_miss_wait_us
            .append_value(r.placement_miss_wait_us);
        self.state_bytes.append_value(r.state_bytes);
        self.sizer.append_value(r.sizer);
        self.outcome.append_value(r.outcome.code());
        self.error.append_option(r.error.as_deref());
        self.rows += 1;
    }

    /// The columns in schema order, either reset (`finish`) or cloned (`finish_cloned`).
    fn columns(&mut self, reset: bool) -> Vec<arrow::array::ArrayRef> {
        macro_rules! col {
            ($f:ident) => {
                if reset {
                    ArrayBuilder::finish(&mut self.$f)
                } else {
                    ArrayBuilder::finish_cloned(&self.$f)
                }
            };
        }
        vec![
            col!(seq),
            col!(stage),
            col!(worker),
            col!(instance),
            col!(t_start_ns),
            col!(t_end_ns),
            col!(rows_in),
            col!(bytes_in),
            col!(rows_out),
            col!(bytes_out),
            col!(tier_in),
            col!(tier_out),
            col!(feat_mean_string_len),
            col!(feat_null_ratio),
            col!(feat_column_bytes),
            col!(knob_morsel_target),
            col!(knob_active_workers),
            col!(knob_read_ahead),
            col!(mem_anon_before),
            col!(mem_anon_peak),
            col!(dev_mem_peak),
            col!(cpu_time_us),
            col!(throttled_delta_us),
            col!(q_bytes_before),
            col!(q_bytes_after),
            col!(staging_bytes_delta),
            col!(placement_miss_wait_us),
            col!(state_bytes),
            col!(sizer),
            col!(outcome),
            col!(error),
        ]
    }

    /// Finalise the chunk and reset the builders.
    fn finish(&mut self) -> Result<RecordBatch> {
        let cols = self.columns(true);
        self.rows = 0;
        RecordBatch::try_new(Arc::clone(&self.schema), cols).map_err(arrow_err("chunk"))
    }

    /// The rows appended since the last `finish`, without resetting (f.3: `tail` sees the
    /// builder's partial chunk).
    fn partial(&mut self) -> Result<Option<RecordBatch>> {
        if self.rows == 0 {
            return Ok(None);
        }
        let cols = self.columns(false);
        RecordBatch::try_new(Arc::clone(&self.schema), cols)
            .map(Some)
            .map_err(arrow_err("partial"))
    }
}

fn arrow_err(op: &'static str) -> impl Fn(arrow::error::ArrowError) -> AmoruError {
    move |e| AmoruError::Io {
        op: "trace",
        target: op.to_string(),
        msg: e.to_string(),
    }
}

fn io_err(op: &'static str, target: &std::path::Path) -> impl Fn(std::io::Error) -> AmoruError {
    let target = target.display().to_string();
    move |e| AmoruError::Io {
        op,
        target: target.clone(),
        msg: e.to_string(),
    }
}

/// The in-memory chunks and what has already left them (TR-I4).
struct ChunkStore {
    /// Finalised chunks, oldest first.
    chunks: VecDeque<Arc<RecordBatch>>,
    /// The chunk currently being appended to the overflow file: still in this process's
    /// memory and not yet counted as overflow, so a reader sees it exactly once.
    spilling: Option<Arc<RecordBatch>>,
    /// Bytes the chunks hold (`get_array_memory_size`).
    bytes: u64,
    /// Chunks already in the overflow file.
    overflow_chunks: usize,
    /// Rows already in the overflow file.
    overflow_rows: u64,
    /// Every row the writer has put into a chunk.
    rows_total: u64,
}

impl ChunkStore {
    fn new() -> ChunkStore {
        ChunkStore {
            chunks: VecDeque::new(),
            spilling: None,
            bytes: 0,
            overflow_chunks: 0,
            overflow_rows: 0,
            rows_total: 0,
        }
    }
}

/// What the writer thread and its callers share.
pub(crate) struct Shared {
    builders: Mutex<Builders>,
    chunks: Mutex<ChunkStore>,
    late_records: AtomicU64,
    overflow_failed: AtomicBool,
    finished: AtomicBool,
    pub(crate) overflow_path: PathBuf,
    pub(crate) final_path: Option<PathBuf>,
    pub(crate) schema: SchemaRef,
    memory_limit: u64,
}

/// The trace writer (d.1). Held by the facade as `Arc<TraceWriter>`, by the scheduler as
/// `Arc<dyn TraceSink>` and by the controller as `Arc<dyn TraceTail>`.
pub struct TraceWriter {
    tx: Sender<Msg>,
    /// A second handle on the channel, so `finish` can count anything that raced the
    /// finish flag into it after the writer thread stopped reading (e.1).
    rx: Receiver<Msg>,
    shared: Arc<Shared>,
    handle: Mutex<Option<std::thread::JoinHandle<Result<()>>>>,
    outcome: Mutex<Option<Result<()>>>,
    run_id: RunId,
}

impl TraceWriter {
    /// Start the writer: check the schema (TR-I6), open the final file when a path was
    /// given, and spawn the writer thread.
    pub fn start(cfg: TraceConfig) -> Result<Arc<TraceWriter>> {
        Self::start_with_schema(cfg, TraceRecord::arrow_schema())
    }

    /// `start`, with the schema injected so a test can alter one field type and see the
    /// writer refuse (TR-T6). The only caller outside tests is `start`.
    fn start_with_schema(cfg: TraceConfig, schema: SchemaRef) -> Result<Arc<TraceWriter>> {
        check_schema(&schema)?;
        let capacity = cfg.channel_capacity.max(1);
        let hex = run_id_hex(cfg.run_id);
        let overflow_path = cfg.staging_dir.join(format!("trace-overflow-{hex}.arrow"));
        let final_writer = match &cfg.path {
            Some(p) => {
                if let Some(dir) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
                    std::fs::create_dir_all(dir).map_err(io_err("trace_dir", dir))?;
                }
                let file = File::create(p).map_err(io_err("trace_create", p))?;
                Some(FileWriter::try_new(file, &schema).map_err(arrow_err("final file"))?)
            }
            None => None,
        };
        let shared = Arc::new(Shared {
            builders: Mutex::new(Builders::new(Arc::clone(&schema))),
            chunks: Mutex::new(ChunkStore::new()),
            late_records: AtomicU64::new(0),
            overflow_failed: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            overflow_path,
            final_path: cfg.path.clone(),
            schema,
            memory_limit: cfg.memory_limit,
        });
        let (tx, rx) = bounded::<Msg>(capacity);
        let rx_keep = rx.clone();
        let thread_shared = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("amoru-trace".to_string())
            .spawn(move || run_writer(thread_shared, rx, final_writer))
            .map_err(|e| AmoruError::Config {
                name: "trace.writer",
                msg: format!("cannot spawn the trace writer thread: {e}"),
            })?;
        Ok(Arc::new(TraceWriter {
            tx,
            rx: rx_keep,
            shared,
            handle: Mutex::new(Some(handle)),
            outcome: Mutex::new(None),
            run_id: cfg.run_id,
        }))
    }

    /// Everything recorded so far: the in-memory chunks, the builder's partial chunk and
    /// the overflow file (d.1).
    pub fn snapshot(&self) -> TraceView {
        let set = self.chunk_set();
        TraceView::from_parts(
            set,
            Arc::clone(&self.shared),
            self.shared.overflow_failed.load(Ordering::Relaxed),
            self.shared.late_records.load(Ordering::Relaxed),
            false,
        )
    }

    /// TR-I5: flush, footer, join. Idempotent; the facade calls it once after the scheduler
    /// returned, and every later call returns a view over the same trace.
    pub fn finish(&self) -> Result<TraceView> {
        let handle = lock(&self.handle).take();
        if let Some(handle) = handle {
            self.shared.finished.store(true, Ordering::SeqCst);
            // The writer thread may already be gone (a panic that the drain loop ended);
            // a send failure is not itself an error, the join result is what counts.
            let _ = self.tx.send(Msg::Finish);
            let joined = match handle.join() {
                Ok(r) => r,
                Err(_) => Err(AmoruError::Io {
                    op: "trace",
                    target: "writer thread".to_string(),
                    msg: "the trace writer thread panicked".to_string(),
                }),
            };
            // Anything that raced the finish flag into the channel is late, never lost
            // silently: it is counted and reported (e.1, h).
            let mut late = 0u64;
            while let Ok(msg) = self.rx.try_recv() {
                if let Msg::Record(_) = msg {
                    late += 1;
                }
            }
            if late > 0 {
                self.shared.late_records.fetch_add(late, Ordering::Relaxed);
            }
            *lock(&self.outcome) = Some(joined);
        }
        let stored = lock(&self.outcome);
        let view = self.final_view();
        match stored.as_ref() {
            Some(Err(e)) => Err(AmoruError::Io {
                op: "trace",
                target: self
                    .shared
                    .final_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(in memory)".to_string()),
                msg: e.to_string(),
            }),
            _ => Ok(view),
        }
    }

    /// The run's identity, as the report prints it (b).
    pub fn run_id(&self) -> RunId {
        self.run_id
    }

    /// Bytes the in-memory chunks hold right now. TR-I4 bounds this by
    /// `trace.memory_limit`; it is what TR-T4 samples, so the invariant is proved by the
    /// quantity the writer controls and not only by the process's resident set.
    pub fn memory_bytes(&self) -> u64 {
        lock(&self.shared.chunks).bytes
    }

    fn final_view(&self) -> TraceView {
        let set = self.chunk_set();
        TraceView::from_parts(
            set,
            Arc::clone(&self.shared),
            self.shared.overflow_failed.load(Ordering::Relaxed),
            self.shared.late_records.load(Ordering::Relaxed),
            true,
        )
    }

    /// The consistent snapshot both `snapshot` and `tail` walk: the builders lock is taken
    /// before the chunks lock, the same order the writer publishes in, so no record is seen
    /// twice or missed (g).
    fn chunk_set(&self) -> ChunkSet {
        let mut builders = lock(&self.shared.builders);
        let partial = builders.partial().ok().flatten().map(Arc::new);
        let store = lock(&self.shared.chunks);
        let mut memory: Vec<Arc<RecordBatch>> = Vec::with_capacity(store.chunks.len() + 2);
        if let Some(s) = &store.spilling {
            memory.push(Arc::clone(s));
        }
        memory.extend(store.chunks.iter().map(Arc::clone));
        let partial_rows = partial.as_ref().map_or(0, |b| b.num_rows() as u64);
        if let Some(p) = partial {
            memory.push(p);
        }
        ChunkSet {
            memory,
            overflow_chunks: store.overflow_chunks,
            rows: store.rows_total + partial_rows,
        }
    }
}

impl core::fmt::Debug for TraceWriter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let store = lock(&self.shared.chunks);
        f.debug_struct("TraceWriter")
            .field("run_id", &run_id_hex(self.run_id))
            .field("records", &store.rows_total)
            .field("memory_bytes", &store.bytes)
            .field("overflow_chunks", &store.overflow_chunks)
            .field(
                "overflow_failed",
                &self.shared.overflow_failed.load(Ordering::Relaxed),
            )
            .field(
                "late_records",
                &self.shared.late_records.load(Ordering::Relaxed),
            )
            .field("finished", &self.shared.finished.load(Ordering::SeqCst))
            .finish()
    }
}

impl TraceSink for TraceWriter {
    /// TR-I1. A bounded push; the caller blocks only for the channel slot and nothing is
    /// dropped. After `finish` the writer is gone, so the record is counted as late (e.1).
    fn record(&self, r: TraceRecord) {
        if self.shared.finished.load(Ordering::SeqCst) {
            self.shared.late_records.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(target: "trace.late_record", seq = r.seq, stage = r.stage);
            return;
        }
        if self.tx.send(Msg::Record(r)).is_err() {
            self.shared.late_records.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(target: "trace.late_record", "the trace writer is gone");
        }
    }

    /// A rendezvous with the writer thread: every record queued before this call is in a
    /// chunk when it returns, and in the final file when a path was given (b, e.1).
    fn flush(&self) -> Result<()> {
        if self.shared.finished.load(Ordering::SeqCst) {
            return Ok(());
        }
        let (tx, rx) = bounded::<Result<()>>(1);
        if self.tx.send(Msg::Flush(tx)).is_err() {
            return Ok(());
        }
        match rx.recv() {
            Ok(r) => r,
            // The writer thread died without answering; `finish` reports the reason.
            Err(_) => Ok(()),
        }
    }
}

impl TraceTail for TraceWriter {
    /// f.3: the last `n` records of `stage`, oldest first, from the in-memory chunks and
    /// the builder's partial chunk. The overflow file is not read: the controller's window
    /// is recent by definition, and a `tail` larger than memory holds returns fewer records.
    fn tail(&self, stage: StageId, n: usize) -> Vec<TraceRecord> {
        if n == 0 {
            return Vec::new();
        }
        let set = self.chunk_set();
        let mut out: Vec<TraceRecord> = Vec::new();
        for batch in set.memory.iter().rev() {
            let stages = match batch
                .column(1)
                .as_any()
                .downcast_ref::<arrow::array::UInt16Array>()
            {
                Some(a) => a,
                None => continue,
            };
            for row in (0..batch.num_rows()).rev() {
                if stages.value(row) != stage {
                    continue;
                }
                if let Some(rec) = record_at(batch, row) {
                    out.push(rec);
                }
                if out.len() == n {
                    out.reverse();
                    return out;
                }
            }
        }
        out.reverse();
        out
    }
}

/// TR-I6. The schema the writer will build must be the schema the contracts pinned: every
/// field, in order, with the type `TraceRecord::SCHEMA_FIELDS` names. Comparing the
/// rendered field list to `SCHEMA_FIELDS` is the same assertion as comparing the digests,
/// because `TraceRecord::SCHEMA_HASH` is the digest of that string and `schema_hash()`
/// recomputes it; both are checked here, so a drift on either side is a `Config` error at
/// start and never a silent divergence.
fn check_schema(schema: &SchemaRef) -> Result<()> {
    if TraceRecord::schema_hash() != TraceRecord::SCHEMA_HASH {
        return Err(AmoruError::Config {
            name: "trace.schema",
            msg: "amoru_kernel::TraceRecord::SCHEMA_HASH is not the digest of SCHEMA_FIELDS"
                .to_string(),
        });
    }
    let rendered = render_fields(schema)?;
    if rendered != TraceRecord::SCHEMA_FIELDS {
        return Err(AmoruError::Config {
            name: "trace.schema",
            msg: format!(
                "the trace schema does not match TraceRecord::SCHEMA_HASH: built \"{rendered}\", pinned \"{}\"",
                TraceRecord::SCHEMA_FIELDS
            ),
        });
    }
    Ok(())
}

/// The canonical `"name:type,..."` rendering of an Arrow schema (contracts e.5).
fn render_fields(schema: &SchemaRef) -> Result<String> {
    let mut out = String::with_capacity(TraceRecord::SCHEMA_FIELDS.len());
    for (i, field) in schema.fields().iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(field.name());
        out.push(':');
        out.push_str(
            token_of(field.data_type()).ok_or_else(|| AmoruError::Config {
                name: "trace.schema",
                msg: format!(
                    "field {} has type {:?}, which the trace schema does not define",
                    field.name(),
                    field.data_type()
                ),
            })?,
        );
    }
    Ok(out)
}

fn token_of(t: &DataType) -> Option<&'static str> {
    Some(match t {
        DataType::UInt64 => "u64",
        DataType::UInt16 => "u16",
        DataType::UInt8 => "u8",
        DataType::Int64 => "i64",
        DataType::Float32 => "f32",
        DataType::Utf8 => "string",
        DataType::List(inner) if matches!(inner.data_type(), DataType::UInt64) => "list<u64>",
        _ => return None,
    })
}

/// The writer thread. A panic inside it must not block a worker for ever (h, failures), so
/// the body is caught and the channel is drained into `late_records` afterwards.
fn run_writer(
    shared: Arc<Shared>,
    rx: Receiver<Msg>,
    final_writer: Option<FileWriter<File>>,
) -> Result<()> {
    let drain = Arc::clone(&shared);
    let rx_for_drain = rx.clone();
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        writer_loop(shared, rx, final_writer)
    }));
    match caught {
        Ok(r) => r,
        Err(_) => Err(drain_after_failure(&drain, &rx_for_drain)),
    }
}

/// h, failures: the writer thread has failed, so no record can reach a chunk any more. The
/// channel must still be answered, or a worker blocks on a full one for ever; every record
/// that arrives from here on is counted in `late_records` rather than silently lost, and the
/// facade terminates the run with the diagnostic this returns.
fn drain_after_failure(shared: &Arc<Shared>, rx: &Receiver<Msg>) -> AmoruError {
    shared.overflow_failed.store(true, Ordering::Relaxed);
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Record(_) => {
                shared.late_records.fetch_add(1, Ordering::Relaxed);
            }
            Msg::Flush(tx) => {
                let _ = tx.send(Ok(()));
            }
            Msg::Finish => break,
        }
    }
    AmoruError::Io {
        op: "trace",
        target: "writer thread".to_string(),
        msg: "the trace writer thread panicked; records after it are counted as late".to_string(),
    }
}

struct WriterState {
    shared: Arc<Shared>,
    final_writer: Option<FileWriter<File>>,
    overflow: Option<StreamWriter<File>>,
    failure: Option<AmoruError>,
}

fn writer_loop(
    shared: Arc<Shared>,
    rx: Receiver<Msg>,
    final_writer: Option<FileWriter<File>>,
) -> Result<()> {
    let mut st = WriterState {
        shared,
        final_writer,
        overflow: None,
        failure: None,
    };
    let interval = core::time::Duration::from_millis(DRAIN_INTERVAL_MS);
    loop {
        let first = match rx.recv_timeout(interval) {
            Ok(m) => m,
            Err(RecvTimeoutError::Timeout) => {
                // f.1: finalise what is in the builders every 100 ms so `snapshot` and the
                // report do not wait for a chunk to fill.
                st.finalise_chunk(false);
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        // Append up to a chunk's worth under one builders lock; a marker ends the batch so
        // the two locks are never nested in the other order (g).
        let mut marker: Option<Msg> = None;
        {
            let mut b = lock(&st.shared.builders);
            let mut taken = 0usize;
            let mut next = Some(first);
            while let Some(msg) = next.take() {
                match msg {
                    Msg::Record(r) => {
                        b.append(r);
                        taken += 1;
                    }
                    other => {
                        marker = Some(other);
                        break;
                    }
                }
                if taken >= CHUNK_ROWS {
                    break;
                }
                next = rx.try_recv().ok();
            }
        }
        st.finalise_chunk(true);
        match marker {
            Some(Msg::Flush(tx)) => {
                st.finalise_chunk(false);
                let _ = tx.send(st.take_failure());
            }
            Some(Msg::Finish) => break,
            _ => {}
        }
    }
    st.finalise_chunk(false);
    st.close()
}

impl WriterState {
    /// Move the builders' rows into a chunk. `only_when_full` is the normal path (a chunk is
    /// 4,096 records); a flush, a 100 ms tick and `finish` finalise a partial chunk.
    fn finalise_chunk(&mut self, only_when_full: bool) {
        let built = {
            // The builders lock is held across the spill and the publication, so a reader,
            // which takes it first (g), never sees a chunk twice or misses one.
            let shared = Arc::clone(&self.shared);
            let mut b = lock(&shared.builders);
            if b.rows == 0 || (only_when_full && b.rows < CHUNK_ROWS) {
                return;
            }
            match b.finish() {
                Ok(batch) => {
                    let batch = Arc::new(batch);
                    let bytes = batch.get_array_memory_size() as u64;
                    // Make room before the chunk joins the in-memory set, so the set never
                    // exceeds the limit (TR-I4) rather than exceeding it and recovering.
                    self.spill_to_fit(bytes);
                    let mut store = lock(&shared.chunks);
                    store.bytes += bytes;
                    store.rows_total += batch.num_rows() as u64;
                    store.chunks.push_back(Arc::clone(&batch));
                    Ok(batch)
                }
                Err(e) => Err(e),
            }
        };
        let batch = match built {
            Ok(batch) => batch,
            Err(e) => {
                self.note(e);
                return;
            }
        };
        if let Some(w) = self.final_writer.as_mut()
            && let Err(e) = w.write(&batch)
        {
            self.note(arrow_err("final file")(e));
        }
    }

    /// TR-I4: in-memory chunks never exceed `trace.memory_limit`; the oldest go to the
    /// overflow file and are freed. A failure to write the overflow keeps them in memory
    /// and sets `overflow_failed` (h); nothing is dropped.
    fn spill_to_fit(&mut self, incoming: u64) {
        loop {
            let victim = {
                let mut store = lock(&self.shared.chunks);
                if store.bytes + incoming <= self.shared.memory_limit || store.chunks.is_empty() {
                    return;
                }
                match store.chunks.pop_front() {
                    Some(b) => {
                        // The victim leaves the accounted set at once and is visible in
                        // `spilling` until the overflow file has it, so a reader sees it
                        // exactly once.
                        store.bytes = store.bytes.saturating_sub(b.get_array_memory_size() as u64);
                        store.spilling = Some(Arc::clone(&b));
                        b
                    }
                    None => return,
                }
            };
            match self.append_overflow(&victim) {
                Ok(()) => {
                    let mut store = lock(&self.shared.chunks);
                    store.spilling = None;
                    store.overflow_chunks += 1;
                    store.overflow_rows += victim.num_rows() as u64;
                    tracing::debug!(
                        target: "trace.overflow",
                        chunks = store.overflow_chunks,
                        rows = store.overflow_rows
                    );
                }
                Err(e) => {
                    let mut store = lock(&self.shared.chunks);
                    if let Some(b) = store.spilling.take() {
                        store.bytes += b.get_array_memory_size() as u64;
                        store.chunks.push_front(b);
                    }
                    drop(store);
                    // h, failures: a chunk that cannot reach the overflow file stays in
                    // memory and the flag is raised. It is not a `finish` error: the trace
                    // is complete, only larger than the limit, and the report says so.
                    self.shared.overflow_failed.store(true, Ordering::Relaxed);
                    tracing::warn!(target: "trace.overflow", error = %e, "the overflow file could not be written; chunks stay in memory");
                    return;
                }
            }
        }
    }

    fn append_overflow(&mut self, batch: &RecordBatch) -> Result<()> {
        if self.overflow.is_none() {
            let path = &self.shared.overflow_path;
            if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir).map_err(io_err("overflow_dir", dir))?;
            }
            let file = File::create(path).map_err(io_err("overflow_create", path))?;
            self.overflow = Some(
                StreamWriter::try_new(file, &self.shared.schema).map_err(arrow_err("overflow"))?,
            );
        }
        let w = match self.overflow.as_mut() {
            Some(w) => w,
            None => return Ok(()),
        };
        w.write(batch).map_err(arrow_err("overflow"))?;
        // A reader of the overflow file must see complete messages while the run goes on.
        w.flush().map_err(arrow_err("overflow"))
    }

    fn note(&mut self, e: AmoruError) {
        tracing::warn!(target: "trace.error", error = %e);
        if self.failure.is_none() {
            self.failure = Some(e);
        }
    }

    fn take_failure(&mut self) -> Result<()> {
        match self.failure.take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// The footer on the final file, the overflow stream closed, and the overflow file
    /// removed once the final file holds the whole trace (e.2).
    fn close(&mut self) -> Result<()> {
        if let Some(mut w) = self.overflow.take()
            && let Err(e) = w.finish()
        {
            self.note(arrow_err("overflow")(e));
        }
        let wrote_final = self.final_writer.is_some();
        if let Some(mut w) = self.final_writer.take()
            && let Err(e) = w.finish()
        {
            self.note(arrow_err("final file")(e));
        }
        if wrote_final && self.failure.is_none() && self.shared.overflow_path.exists() {
            // The final file is the whole trace; the overflow copy is not needed.
            let _ = std::fs::remove_file(&self.shared.overflow_path);
            let mut store = lock(&self.shared.chunks);
            store.overflow_chunks = 0;
        }
        let rows = lock(&self.shared.chunks).rows_total;
        tracing::info!(
            target: "trace.finished",
            records = rows,
            path = ?self.shared.final_path
        );
        self.take_failure()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::datatypes::{Field, Schema};

    fn cfg(dir: &std::path::Path) -> TraceConfig {
        TraceConfig {
            path: None,
            staging_dir: dir.to_path_buf(),
            channel_capacity: 256,
            memory_limit: 8 * 1024 * 1024,
            run_id: RunId([7u8; 16]),
        }
    }

    /// TR-T6. The writer refuses to start when the schema it would build is not the one
    /// `TraceRecord::SCHEMA_HASH` pins: here a test-only schema alters one field's type.
    /// Proves TR-I6.
    #[test]
    fn tr_t6_schema_hash() {
        let dir = std::env::temp_dir().join("amoru-trace-t6");
        let _ = std::fs::create_dir_all(&dir);
        // The real schema starts.
        let w = TraceWriter::start(cfg(&dir)).expect("the pinned schema starts");
        w.finish().expect("finish");

        // One field type altered: UInt64 where the schema pins UInt16.
        let mut fields: Vec<Field> = TraceRecord::arrow_schema()
            .fields()
            .iter()
            .map(|f| f.as_ref().clone())
            .collect();
        fields[1] = Field::new("stage", DataType::UInt64, false);
        let drifted: SchemaRef = Arc::new(Schema::new(fields));
        let err = TraceWriter::start_with_schema(cfg(&dir), drifted)
            .expect_err("a drifted schema must not start");
        match err {
            AmoruError::Config { name, msg } => {
                assert_eq!(name, "trace.schema");
                assert!(msg.contains("stage:u64"), "{msg}");
            }
            other => panic!("expected Config, got {other:?}"),
        }

        // A field renamed is caught too.
        let mut fields: Vec<Field> = TraceRecord::arrow_schema()
            .fields()
            .iter()
            .map(|f| f.as_ref().clone())
            .collect();
        fields[0] = Field::new("sequence", DataType::UInt64, false);
        let renamed: SchemaRef = Arc::new(Schema::new(fields));
        assert!(TraceWriter::start_with_schema(cfg(&dir), renamed).is_err());

        // A field type outside the trace schema's vocabulary is named in the error.
        let mut fields: Vec<Field> = TraceRecord::arrow_schema()
            .fields()
            .iter()
            .map(|f| f.as_ref().clone())
            .collect();
        fields[5] = Field::new("t_end_ns", DataType::Boolean, false);
        let alien: SchemaRef = Arc::new(Schema::new(fields));
        let err = TraceWriter::start_with_schema(cfg(&dir), alien)
            .expect_err("an alien type must not start");
        assert!(format!("{err}").contains("t_end_ns"), "{err}");
    }

    /// h, failures: once the writer thread has failed, the channel is still answered, so no
    /// worker blocks on a full one; every record that arrives is counted in `late_records`
    /// and the diagnostic the facade terminates the run with names the trace.
    #[test]
    fn a_failed_writer_drains_the_channel_and_counts_what_it_cannot_write() {
        let schema = TraceRecord::arrow_schema();
        let shared = Arc::new(Shared {
            builders: Mutex::new(Builders::new(Arc::clone(&schema))),
            chunks: Mutex::new(ChunkStore::new()),
            late_records: AtomicU64::new(0),
            overflow_failed: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            overflow_path: std::env::temp_dir().join("amoru-trace-drain.arrow"),
            final_path: None,
            schema,
            memory_limit: 1024,
        });
        let (tx, rx) = bounded::<Msg>(8);

        let drained = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || drain_after_failure(&shared, &rx))
        };

        for seq in 0..5 {
            tx.send(Msg::Record(TraceRecord {
                seq,
                ..empty_record()
            }))
            .expect("the drain loop keeps the channel moving");
        }
        let (ftx, frx) = bounded::<Result<()>>(1);
        tx.send(Msg::Flush(ftx)).expect("send a flush marker");
        assert!(
            frx.recv()
                .expect("a flush is answered, never left waiting")
                .is_ok(),
            "a flush after the failure returns rather than blocking a worker"
        );
        tx.send(Msg::Finish).expect("send finish");

        let err = drained.join().expect("the drain loop returns a diagnostic");
        assert!(format!("{err}").contains("trace"), "{err}");
        assert_eq!(shared.late_records.load(Ordering::Relaxed), 5);
        assert!(shared.overflow_failed.load(Ordering::Relaxed));
    }

    /// The run identity the report prints comes from the configuration, unchanged (b).
    #[test]
    fn the_writer_carries_the_run_id() {
        let dir = std::env::temp_dir().join("amoru-trace-runid");
        let _ = std::fs::create_dir_all(&dir);
        let mut c = cfg(&dir);
        c.run_id = RunId([0x0f; 16]);
        let w = TraceWriter::start(c).expect("start");
        assert_eq!(w.run_id().0, [0x0f; 16]);
        assert_eq!(crate::run_id_hex(w.run_id()), "0f".repeat(16));
        w.finish().expect("finish");
    }

    fn empty_record() -> TraceRecord {
        TraceRecord {
            seq: 0,
            stage: 0,
            worker: 0,
            instance: u16::MAX,
            t_start_ns: 0,
            t_end_ns: 0,
            rows_in: 0,
            bytes_in: 0,
            rows_out: 0,
            bytes_out: 0,
            tier_in: 2,
            tier_out: 2,
            feat_mean_string_len: 0.0,
            feat_null_ratio: 0.0,
            feat_column_bytes: Vec::new(),
            knob_morsel_target: 0,
            knob_active_workers: 1,
            knob_read_ahead: 0,
            mem_anon_before: 0,
            mem_anon_peak: 0,
            dev_mem_peak: 0,
            cpu_time_us: 0,
            throttled_delta_us: 0,
            q_bytes_before: Vec::new(),
            q_bytes_after: Vec::new(),
            staging_bytes_delta: 0,
            placement_miss_wait_us: 0,
            state_bytes: 0,
            sizer: 0,
            outcome: amoru_kernel::Outcome::Ok,
            error: None,
        }
    }

    /// The canonical rendering of the pinned schema is exactly `SCHEMA_FIELDS`, which is
    /// what makes the string comparison in `check_schema` equal to comparing the digests.
    #[test]
    fn tr_t6_render_matches_contract() {
        let rendered = render_fields(&TraceRecord::arrow_schema()).expect("render");
        assert_eq!(rendered, TraceRecord::SCHEMA_FIELDS);
        assert_eq!(TraceRecord::schema_hash(), TraceRecord::SCHEMA_HASH);
    }
}
