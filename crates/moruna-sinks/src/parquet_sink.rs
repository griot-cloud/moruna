//! `ParquetSink` (08 f.1, e.2, f.5, f.7, f.8).
//!
//! The encoder writes straight into one arena buffer per open file through the `std::io::Write`
//! implementation below, and the reactor writes the finished file from a view over that buffer.
//! That encode is the one CPU copy G-I2 allows a sink, and it is the only one: no `Vec<u8>` of
//! payload size exists anywhere in this file.

use std::collections::BTreeSet;
use std::io::Write;
use std::sync::{Arc, Mutex};

use moruna_kernel::arrow::datatypes::SchemaRef;
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{
    Allocator, BoxFuture, Buffer, Completion, Encoded, MorunaError, Payload, PayloadKind,
    PayloadSpec, Reactor, Result, RunId, Seq, Sink, SinkSummary, SourceSchema, TierPref,
};
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;

use crate::checkpoint::{CommittedFile, Ledger};
use crate::commit::{ObjectPrefix, TMP_SUFFIX};
use crate::file_buf::{FileBuf, FileBuffers, TOO_LARGE, differing_field, hex, rows_bytes};
use crate::stats::SinkStats;
use crate::{Phase, host_tier, part_name, require_host};

/// The kind this sink writes into its checkpoint (e.5).
const KIND: &str = "parquet";

/// The marker `finish` writes once every file is committed (e.2).
const SUCCESS: &str = "_SUCCESS";

/// Where a Parquet sink writes and how big its pieces are.
pub struct ParquetSinkConfig {
    /// Prefix; files are created under it. A `file://` URL or a bare path is a local
    /// directory, which is the only kind of destination `resume` can list (f.8).
    pub url: String,
    /// Row group target, measured as encoded bytes.
    pub row_group_bytes: u64,
    /// File roll size.
    pub file_bytes: u64,
    /// Page compression.
    pub compression: Compression,
    /// Escape hatch; overrides the two fields above.
    pub writer_props: Option<WriterProperties>,
}

impl Default for ParquetSinkConfig {
    /// The defaults of preamble section 5: 128 MiB row groups, 1 GiB files, Zstd level 3.
    fn default() -> ParquetSinkConfig {
        ParquetSinkConfig {
            url: String::new(),
            row_group_bytes: 128 << 20,
            file_bytes: 1 << 30,
            compression: Compression::ZSTD(ZstdLevel::default()),
            writer_props: None,
        }
    }
}

/// The `std::io::Write` the Parquet writer encodes into: one arena buffer, no heap staging.
struct ArenaWrite {
    shared: Arc<Mutex<FileBuf>>,
}

impl Write for ArenaWrite {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let mut file = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        file.append(data)?;
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The file the sink is encoding into right now.
struct Current {
    name: String,
    writer: ArrowWriter<ArenaWrite>,
    shared: Arc<Mutex<FileBuf>>,
    seqs: Vec<Seq>,
    rows: u64,
}

/// One file an encode can write into, and the byte count it rolls at (f.1). The sink keeps as
/// many lanes as encodes have run at once (RB1, `architecture/worker-io.md`): a worker takes a
/// free lane or opens one, so encodes never contend for one writer and there is no setting for
/// how many there are. Lock order: a lane, then `Inner`, never the other way.
#[derive(Default)]
struct Lane {
    current: Option<Current>,
    roll_bytes: u64,
}

/// A rolled file whose bytes are with the reactor and not yet committed.
struct Pending {
    entry: CommittedFile,
    seqs: Vec<Seq>,
    completion: Completion<()>,
    /// Kept alive until the completion resolves; the view the reactor holds reads these bytes.
    buf: Arc<Buffer>,
}

struct Inner {
    phase: Phase,
    /// The schema the sink was opened with: the chain's, which for an opaque kernel is only
    /// the source's (f.1). Used when the run writes nothing, and never for the writer.
    declared: Option<SchemaRef>,
    /// The output schema, adopted from the first payload that arrives (f.1).
    schema: Option<SchemaRef>,
    ledger: Ledger,
    stats: SinkStats,
}

/// A sink that writes Arrow batches as Parquet files under a prefix (e.2).
pub struct ParquetSink {
    cfg: ParquetSinkConfig,
    props: WriterProperties,
    dest: ObjectPrefix,
    reactor: Arc<dyn Reactor>,
    alloc: Arc<dyn Allocator>,
    run_id: RunId,
    inner: Mutex<Inner>,
    /// The files encodes write into, one per encode that has run at once (RB1).
    lanes: Mutex<Vec<Arc<Mutex<Lane>>>>,
    /// The file buffers, the set taken at `open` and the ones kept after each write (f.1, f.12).
    buffers: FileBuffers,
    /// The most lanes the sink opens: one per worker that may encode at once (f.12). The
    /// facade sets it from the run's workers; a sink built on its own encodes into two.
    max_lanes: usize,
    /// The file target of one lane: the set's buffer size, settled at `open`.
    lane_bytes: std::sync::atomic::AtomicU64,
    /// Where a worker that finds every lane busy waits next, round robin.
    next_lane: std::sync::atomic::AtomicUsize,
}

impl ParquetSink {
    /// Build a sink over `cfg`. The reactor writes the files; the allocator provides the
    /// encoder's output buffer, one per open file.
    pub fn new(
        cfg: ParquetSinkConfig,
        reactor: Arc<dyn Reactor>,
        alloc: Arc<dyn Allocator>,
    ) -> Result<ParquetSink> {
        let dest = ObjectPrefix::parse(&cfg.url)?;
        let props = match &cfg.writer_props {
            Some(props) => props.clone(),
            None => WriterProperties::builder()
                .set_compression(cfg.compression)
                .build(),
        };
        let buffers = FileBuffers::new(Arc::clone(&alloc));
        Ok(ParquetSink {
            cfg,
            props,
            dest,
            reactor,
            alloc,
            run_id: RunId([0; 16]),
            inner: Mutex::new(Inner {
                phase: Phase::Created,
                declared: None,
                schema: None,
                ledger: Ledger::new(KIND),
                stats: SinkStats::default(),
            }),
            lanes: Mutex::new(Vec::new()),
            buffers,
            max_lanes: 2,
            lane_bytes: std::sync::atomic::AtomicU64::new(0),
            next_lane: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// Encode into at most `lanes` files at once: the run's workers (f.12). The sink's buffers
    /// are the same bytes whatever this is, divided between the lanes.
    pub fn with_lanes(mut self, lanes: usize) -> ParquetSink {
        self.max_lanes = lanes.max(1);
        self
    }

    /// Name the run whose id goes into every file's footer metadata (e.2). Unset, the footer
    /// carries the nil run id: `ParquetSinkConfig` has no field for a run id (d.1).
    pub fn with_run_id(mut self, run_id: RunId) -> ParquetSink {
        self.run_id = run_id;
        self
    }

    /// The counters of section j.
    pub fn stats(&self) -> SinkStats {
        self.lock().stats.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Every lane the sink has opened so far.
    fn lanes(&self) -> Vec<Arc<Mutex<Lane>>> {
        self.lanes.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Allocate the next file's buffer and its writer in `lane`, and settle the byte count the
    /// file will roll at. `min_bytes` is 0 for an ordinary file and twice a single oversized
    /// morsel's bytes when one is on its way in (section h).
    ///
    /// f.1: the buffer is not `file_bytes` reserved in advance. The sink asks for
    /// `file_bytes + footer`, halves on `Alloc` down to `row_group_bytes + footer`, and rolls
    /// at whatever it got, so `sink.file_bytes` is an upper bound on the file and the arena's
    /// free space is the other. A 1 GiB default that reserved a gigabyte could not open inside
    /// any budget below about 1.2 GiB, which is most of them. With several lanes the file
    /// target is shared between them, so the lanes together ask for what one file would.
    fn open_file(&self, lane: &mut Lane, min_bytes: u64) -> Result<()> {
        let (schema, index) = {
            let mut inner = self.lock();
            let Some(schema) = inner.schema.clone() else {
                return Err(MorunaError::Sink("the sink has no schema".into()));
            };
            (schema, inner.ledger.take_index())
        };
        // f.12: a lane's file is the set's buffer, so every lane together holds what the sink's
        // two buffers held; a morsel larger than that still gets the buffer it needs (h).
        let file_bytes = match self.lane_bytes.load(std::sync::atomic::Ordering::SeqCst) {
            0 => self.cfg.file_bytes,
            lane => lane,
        };
        let (buf, roll_bytes) =
            self.buffers
                .alloc_file_buf(min_bytes, self.cfg.row_group_bytes, file_bytes)?;
        lane.roll_bytes = roll_bytes;
        self.lock().stats.roll_bytes = roll_bytes;
        let shared = Arc::new(Mutex::new(FileBuf {
            buf: Some(buf),
            used: 0,
        }));
        let writer = ArrowWriter::try_new(
            ArenaWrite {
                shared: Arc::clone(&shared),
            },
            schema,
            Some(self.props.clone()),
        )
        .map_err(|e| MorunaError::Sink(format!("parquet writer: {e}")))?;
        lane.current = Some(Current {
            name: part_name(index, "parquet"),
            writer,
            shared,
            seqs: Vec::new(),
            rows: 0,
        });
        Ok(())
    }

    /// Close `lane`'s open file and hand its bytes to the reactor (f.1, f.5). The lane opens
    /// its next file when the next encode reaches it.
    fn roll(&self, lane: &mut Lane) -> Result<Pending> {
        let Some(current) = lane.current.take() else {
            return Err(MorunaError::Sink("the sink has no open file".into()));
        };
        let Current {
            name,
            mut writer,
            shared,
            seqs,
            rows,
        } = current;
        let seq_min = seqs.iter().min().copied().unwrap_or(0);
        let seq_max = seqs.iter().max().copied().unwrap_or(0);
        writer.append_key_value_metadata(KeyValue::new(
            "moruna.run_id".to_string(),
            hex(&self.run_id),
        ));
        if !seqs.is_empty() {
            writer.append_key_value_metadata(KeyValue::new(
                "moruna.seq_min".to_string(),
                seq_min.to_string(),
            ));
            writer.append_key_value_metadata(KeyValue::new(
                "moruna.seq_max".to_string(),
                seq_max.to_string(),
            ));
        }
        writer.close().map_err(|e| encode_error(&e.to_string()))?;
        let (buf, used) = {
            let mut file = shared.lock().unwrap_or_else(|e| e.into_inner());
            (file.buf.take(), file.used)
        };
        let Some(buf) = buf else {
            return Err(MorunaError::Sink("the output buffer is gone".into()));
        };
        let buf = Arc::new(buf);
        let view = buf.view().slice(0, used);
        let completion = self.dest.put(&*self.reactor, &name, view);
        self.lock().stats.rolls += 1;
        tracing::info!(target: "sink.roll", file = %name, bytes = used, "rolled a parquet file");
        Ok(Pending {
            entry: CommittedFile {
                name,
                seq_min,
                seq_max,
                rows,
                bytes: used as u64,
            },
            seqs,
            completion,
            buf,
        })
    }

    /// Record a file whose completion resolved (f.5, f.7).
    fn commit(&self, entry: CommittedFile, seqs: &[Seq]) {
        let mut inner = self.lock();
        tracing::info!(target: "sink.commit", file = %entry.name, "committed a parquet file");
        inner.stats.files_committed += 1;
        inner.ledger.commit(entry, seqs);
    }

    /// Move to `Failed`; the files being written are released, the committed files stay (h).
    fn fail(&self, error: &MorunaError) {
        {
            let mut inner = self.lock();
            if !matches!(inner.phase, Phase::Failed(_)) {
                inner.phase = Phase::Failed(error.to_string());
            }
        }
        for lane in self.lanes() {
            lane.lock().unwrap_or_else(|e| e.into_inner()).current = None;
        }
    }

    /// Await a rolled file's outcome and record it (f.5, f.7).
    fn settle(&self, pending: Pending) -> Result<()> {
        let Pending {
            entry,
            seqs,
            completion,
            buf,
        } = pending;
        let outcome = completion.wait();
        self.settle_outcome(entry, &seqs, buf, outcome)
    }

    /// Record a rolled file whose completion resolved with `outcome` (f.5, f.7).
    fn settle_outcome(
        &self,
        entry: CommittedFile,
        seqs: &[Seq],
        buf: Arc<Buffer>,
        outcome: Result<()>,
    ) -> Result<()> {
        self.buffers.keep_spare(buf);
        match outcome {
            Ok(()) => {
                self.commit(entry, seqs);
                Ok(())
            }
            Err(e) => {
                self.fail(&e);
                Err(e)
            }
        }
    }

    /// The CPU half of a write (RB1): validate the morsel, take a free lane (or open one),
    /// roll its file if this morsel would overflow it, and encode. Returns the rolled file, if
    /// there was one, for `put` to await.
    fn encode_morsel(&self, seq: Seq, payload: Payload) -> Result<Option<Pending>> {
        require_host(&payload)?;
        let rows = payload.rows();
        let Payload::Table(batch, _) = payload else {
            return Err(MorunaError::Sink(
                "the parquet sink accepts a table payload".into(),
            ));
        };
        let bytes = rows_bytes(&batch);
        {
            let mut inner = self.lock();
            inner.phase.require_open()?;
            // f.1: the first payload settles the output schema; every later one is checked
            // against it, so a kernel that appends a column is written as it is and a kernel
            // that drifts between morsels is still an error.
            if inner.schema.is_none() {
                inner.schema = Some(batch.schema());
            } else {
                self.check_schema(&inner, &batch)?;
            }
        }
        let lanes = self.lanes();
        let mut taken = None;
        for lane in &lanes {
            if let Ok(guard) = lane.try_lock() {
                taken = Some(guard);
                break;
            }
        }
        let fresh;
        let mut lane = match taken {
            Some(guard) => guard,
            None if lanes.len() < self.max_lanes => {
                fresh = Arc::new(Mutex::new(Lane::default()));
                self.lanes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(Arc::clone(&fresh));
                fresh.lock().unwrap_or_else(|e| e.into_inner())
            }
            // Every lane is busy and there are as many as workers may encode: wait for one,
            // round robin, which is no longer than one encode.
            None => {
                let at = self
                    .next_lane
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    % lanes.len();
                lanes[at].lock().unwrap_or_else(|e| e.into_inner())
            }
        };
        // Validation is done: from here a failure is the encoder's or the store's, and it moves
        // the sink to `Failed` (e.1), where `finish` reports it and the committed files stand.
        let out = self.encode_checked(&mut lane, seq, batch, rows, bytes);
        if let Err(e) = &out {
            lane.current = None;
            let mut inner = self.lock();
            inner.phase = Phase::Failed(e.to_string());
        }
        out
    }

    fn encode_checked(
        &self,
        lane: &mut Lane,
        seq: Seq,
        batch: RecordBatch,
        rows: u64,
        bytes: u64,
    ) -> Result<Option<Pending>> {
        let mut pending = None;
        match lane.current.as_ref() {
            None => self.open_file(lane, 0)?,
            Some(current) => {
                let so_far = current.writer.bytes_written() as u64
                    + current.writer.in_progress_size() as u64;
                if !current.seqs.is_empty() && so_far + bytes > lane.roll_bytes {
                    // A morsel larger than a file gets a file of its own, sized for it (h); one
                    // that fits an ordinary file gets an ordinary file. Asking every roll for
                    // twice the morsel doubled the buffer's class whenever morsels were a large
                    // share of it, and at the smallest budget that was the allocation the arena
                    // could not give (F8.8).
                    let need = if bytes > lane.roll_bytes {
                        bytes.saturating_mul(2)
                    } else {
                        0
                    };
                    pending = Some(self.roll(lane)?);
                    self.open_file(lane, need)?;
                }
            }
        }
        let roll_bytes = lane.roll_bytes;
        let Some(current) = lane.current.as_mut() else {
            return Err(MorunaError::Sink("the sink has no open file".into()));
        };
        current
            .writer
            .write(&batch)
            .map_err(|e| encode_error(&e.to_string()))?;
        // The payload's arena bytes go back to the arena here, before the reactor has written
        // anything: the encoder has consumed them and the sink keeps no second copy (SI-I1).
        drop(batch);
        current.rows += rows;
        current.seqs.push(seq);
        // The footer shares the buffer, so a row group may not be allowed to grow past the roll
        // size: `row_group_bytes` is a target, and `roll_bytes` is what the arena agreed to.
        if current.writer.in_progress_size() as u64 >= self.cfg.row_group_bytes.min(roll_bytes) {
            current
                .writer
                .flush()
                .map_err(|e| encode_error(&e.to_string()))?;
        }
        {
            let mut inner = self.lock();
            inner.stats.writes += 1;
            inner.stats.encode_bytes += bytes;
        }
        tracing::trace!(target: "sink.encode", seq, bytes, "encoded a morsel");
        // The encode is the symmetric exception to G-I2 and the arena is where it is counted.
        self.alloc.note_payload_copy(bytes);
        Ok(pending)
    }

    /// Roll every lane whose file holds a morsel, and settle each file (f.1, f.12).
    /// Close every lane's open file and write it. The lanes close at once, one thread each: a
    /// close encodes and compresses the file's last row group, and done one lane after another
    /// on the drive that called `finish` it was a tail the whole run waited on, a quarter of a
    /// one-second job's wall on eight lanes (RB1). The lanes are the run's workers, which are idle
    /// by the time a run finishes, so the threads take no CPU the run was not given.
    fn roll_all(&self) -> Result<()> {
        let lanes = self.lanes();
        let roll_one = |lane: &Arc<Mutex<Lane>>| -> Result<Option<Pending>> {
            let mut lane = lane.lock().unwrap_or_else(|e| e.into_inner());
            if lane
                .current
                .as_ref()
                .is_some_and(|file| !file.seqs.is_empty())
            {
                self.roll(&mut lane).map(Some).inspect_err(|_| {
                    lane.current = None;
                })
            } else {
                // An empty file is never written: its buffer goes back to the arena.
                lane.current = None;
                Ok(None)
            }
        };
        let open = lanes
            .iter()
            .filter(|lane| {
                lane.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .current
                    .as_ref()
                    .is_some_and(|file| !file.seqs.is_empty())
            })
            .count();
        let rolled: Vec<Result<Option<Pending>>> = if open <= 1 {
            lanes.iter().map(roll_one).collect()
        } else {
            std::thread::scope(|scope| {
                let handles: Vec<_> = lanes
                    .iter()
                    .map(|lane| scope.spawn(move || roll_one(lane)))
                    .collect();
                handles
                    .into_iter()
                    .map(|handle| {
                        handle.join().unwrap_or_else(|_| {
                            Err(MorunaError::Sink("closing a parquet file panicked".into()))
                        })
                    })
                    .collect()
            })
        };
        let mut pendings = Vec::new();
        for outcome in rolled {
            match outcome {
                Ok(Some(pending)) => pendings.push(pending),
                Ok(None) => {}
                Err(error) => {
                    self.fail(&error);
                    return Err(error);
                }
            }
        }
        for pending in pendings {
            self.settle(pending)?;
        }
        Ok(())
    }

    fn check_schema(&self, inner: &Inner, batch: &RecordBatch) -> Result<()> {
        let Some(schema) = inner.schema.as_ref() else {
            return Err(MorunaError::Sink("the sink has no schema".into()));
        };
        let batch_schema = batch.schema();
        if schema.fields() == batch_schema.fields() {
            return Ok(());
        }
        Err(MorunaError::Sink(differing_field(schema, &batch_schema)))
    }

    /// An empty object under the prefix, which is the marker downstream readers look for (e.2).
    fn write_success(&self) -> Result<()> {
        let buf = Arc::new(self.alloc.alloc(1, host_tier(&*self.alloc))?);
        let view = buf.view().slice(0, 0);
        self.dest.put(&*self.reactor, SUCCESS, view).wait()
    }

    /// Remove whatever an aborted run left under a local destination (h).
    fn clear_tmps(&self) {
        if let Ok(listed) = self.dest.list() {
            for name in listed.iter().filter(|n| n.ends_with(TMP_SUFFIX)) {
                let _ = self.dest.remove(name);
            }
        }
    }
}
fn encode_error(msg: &str) -> MorunaError {
    if msg.contains(TOO_LARGE) {
        MorunaError::Sink(msg.to_string())
    } else {
        MorunaError::Sink(format!("parquet encode: {msg}"))
    }
}

impl Sink for ParquetSink {
    fn open(&mut self, schema: &SourceSchema) -> Result<()> {
        let SourceSchema::Table(schema) = schema else {
            return Err(MorunaError::Sink(
                "the parquet sink accepts a table schema".into(),
            ));
        };
        let mut inner = self.lock();
        inner.phase.require_created()?;
        // f.1: the schema `open` is given is the chain's, and a kernel the chain cannot see
        // through (a Python kernel, 05 d.1) reports its input schema as its output. The real
        // output schema is the first payload's, so the writer is built there and this one is
        // kept only for a run that writes nothing.
        inner.declared = Some(Arc::clone(schema));
        // The file buffers, now, while the arena holds nothing else: the pair's bytes, one
        // buffer per lane (f.12).
        let lane = self
            .buffers
            .alloc_set(self.cfg.file_bytes, self.max_lanes)?;
        self.lane_bytes
            .store(lane, std::sync::atomic::Ordering::SeqCst);
        inner.phase = Phase::Open;
        Ok(())
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    fn encodes_on_workers(&self) -> bool {
        true
    }

    fn encode(&self, seq: Seq, payload: Payload) -> Result<Encoded> {
        Ok(Encoded(Box::new(self.encode_morsel(seq, payload)?)))
    }

    fn put(&self, encoded: Encoded) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let pending = encoded
                .0
                .downcast::<Option<Pending>>()
                .map_err(|_| MorunaError::Sink("put was given another sink's encode".into()))?;
            let Some(pending) = *pending else {
                return Ok(());
            };
            let pending = Box::new(pending);
            let Pending {
                entry,
                seqs,
                completion,
                buf,
            } = *pending;
            let outcome = completion.await;
            self.settle_outcome(entry, &seqs, buf, outcome)
        })
    }

    fn write(&self, seq: Seq, payload: Payload) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let encoded = self.encode(seq, payload)?;
            self.put(encoded).await
        })
    }

    fn finish(&mut self) -> Result<SinkSummary> {
        let failed = {
            let mut inner = self.lock();
            let failed = match &inner.phase {
                Phase::Created => return Err(MorunaError::Sink("the sink is not open".into())),
                Phase::Finished => return Err(MorunaError::Sink("finish ran twice".into())),
                Phase::Failed(msg) => Some(format!(
                    "{msg}; committed files: {}",
                    inner.ledger.names().join(", ")
                )),
                Phase::Open => None,
            };
            if failed.is_some() {
                inner.phase = Phase::Finished;
            }
            failed
        };
        if let Some(msg) = failed {
            for lane in self.lanes() {
                lane.lock().unwrap_or_else(|e| e.into_inner()).current = None;
            }
            // The reactor aborted the multipart upload already (06 f.4); a local destination
            // may still hold temporary files, which go here.
            self.clear_tmps();
            return Err(MorunaError::Sink(msg));
        }
        // A run that wrote nothing never adopted a schema, so the one `open` was given is what
        // the empty file carries (h).
        let wrote_nothing = {
            let inner = self.lock();
            inner.ledger.committed().is_empty()
        } && self.lanes().iter().all(|lane| {
            lane.lock()
                .unwrap_or_else(|e| e.into_inner())
                .current
                .as_ref()
                .is_none_or(|file| file.seqs.is_empty())
        });
        if wrote_nothing {
            {
                let mut inner = self.lock();
                inner.schema = inner.declared.clone();
            }
            let mut lane = Lane::default();
            self.open_file(&mut lane, 0)?;
            let pending = self.roll(&mut lane)?;
            self.settle(pending)?;
        } else {
            self.roll_all()?;
        }
        self.write_success()?;
        let mut inner = self.lock();
        inner.phase = Phase::Finished;
        let (rows, bytes) = inner.ledger.totals();
        Ok(SinkSummary {
            rows,
            bytes,
            files: inner.ledger.names(),
            write: None,
            commit: None,
        })
    }

    fn committed_seq(&self) -> Option<Seq> {
        self.lock().ledger.committed_seq()
    }

    fn skip(&self, seq: Seq) {
        self.lock().ledger.skip(seq);
    }

    fn checkpoint_flush(&self) -> Result<()> {
        {
            let inner = self.lock();
            if matches!(inner.phase, Phase::Finished) {
                return Ok(());
            }
            inner.phase.require_open()?;
        }
        self.roll_all()
    }

    fn checkpoint(&self) -> Result<Option<Vec<u8>>> {
        Ok(Some(self.lock().ledger.to_bytes()?))
    }

    fn resume(
        &mut self,
        schema: &SourceSchema,
        state: &[u8],
        committed_seq: Option<Seq>,
    ) -> Result<()> {
        let SourceSchema::Table(schema) = schema else {
            return Err(MorunaError::Sink(
                "the parquet sink accepts a table schema".into(),
            ));
        };
        let mut inner = self.lock();
        inner.phase.require_created()?;
        let doc = Ledger::parse(KIND, state)?;
        let (ledger, above) = Ledger::restore(KIND, doc, committed_seq)?;
        let keep: BTreeSet<String> = ledger.committed().iter().map(|f| f.name.clone()).collect();
        let mut removed = 0u64;
        for name in self.dest.list()? {
            if name == SUCCESS || keep.contains(&name) {
                continue;
            }
            self.dest.remove(&name)?;
            removed += 1;
        }
        for file in &above {
            self.dest.remove(&file.name)?;
        }
        inner.ledger = ledger;
        inner.stats.resumed_files_removed = removed;
        // As in `open` (f.1): the writer waits for the first payload's schema, which on a
        // resumed run is the same one the interrupted run wrote.
        inner.declared = Some(Arc::clone(schema));
        inner.phase = Phase::Open;
        Ok(())
    }
}
