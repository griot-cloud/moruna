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
use crate::file_buf::{
    FileBuf, FileBuffers, TOO_LARGE, differing_field, footer_for, hex, rows_bytes,
};
use crate::lane_heap::{self, Bounds};
use crate::stats::SinkStats;
use crate::{Phase, host_tier, part_name, require_host};

/// The kind this sink writes into its checkpoint (e.5).
const KIND: &str = "parquet";

/// The marker `finish` writes once every file is committed (e.2).
const SUCCESS: &str = "_SUCCESS";

/// What a run grants a Parquet sink once it knows the sink's schema (system design 3A, item
/// 6.3d): its writers' heap above the arena, every lane together, and its two file buffers'
/// bytes in the arena. The run claims a grant when it builds the sink, before the schema is
/// known, sets it after it has validated its graph and capped its arena, and the sink reads it
/// at `open`. Clones share the grant.
#[derive(Clone, Debug, Default)]
pub struct WriterGrant(Arc<Granted>);

#[derive(Debug, Default)]
struct Granted {
    set: std::sync::atomic::AtomicBool,
    writers: std::sync::atomic::AtomicU64,
    file_buffers: std::sync::atomic::AtomicU64,
}

impl WriterGrant {
    /// A grant not set yet.
    pub fn new() -> WriterGrant {
        WriterGrant::default()
    }

    /// Grant `writers` bytes of heap to the writers, every lane together (`None` keeps
    /// parquet's own limits), and `file_buffers` bytes of the arena to the sink's two file
    /// buffers.
    pub fn grant(&self, writers: Option<u64>, file_buffers: u64) {
        use std::sync::atomic::Ordering::SeqCst;
        self.0.writers.store(writers.unwrap_or(0), SeqCst);
        self.0.file_buffers.store(file_buffers, SeqCst);
        self.0.set.store(true, SeqCst);
    }

    /// The writers' heap granted, `None` before the grant or when it keeps parquet's limits.
    pub fn writers(&self) -> Option<u64> {
        use std::sync::atomic::Ordering::SeqCst;
        let writers = self.0.writers.load(SeqCst);
        (self.0.set.load(SeqCst) && writers > 0).then_some(writers)
    }

    /// The file buffers' bytes granted, `None` before the grant.
    pub fn file_buffers(&self) -> Option<u64> {
        use std::sync::atomic::Ordering::SeqCst;
        self.0
            .set
            .load(SeqCst)
            .then(|| self.0.file_buffers.load(SeqCst))
    }
}

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
    /// The decoded bytes written into the file, and how many of them were in row groups
    /// already flushed (6.3d): what the lane's pages are counted by.
    decoded: u64,
    flushed_decoded: u64,
}

/// One file an encode can write into, and the byte count it rolls at (f.1). The sink keeps as
/// many lanes as encodes have run at once (RB1, `architecture/worker-io.md`): a worker takes a
/// free lane or opens one, so encodes never contend for one writer and there is no setting for
/// how many there are. Lock order: a lane, then `Inner`, never the other way.
#[derive(Default)]
struct Lane {
    current: Option<Current>,
    roll_bytes: u64,
    /// The writer limits of the lane's heap allowance (6.3d), when the sink has one.
    bounds: Option<Bounds>,
    /// The Rust heap the lane's writer holds: every call into it counts its thread's heap here
    /// (6.3d), where the program's allocator counts the heap (`moruna_kernel::held`).
    heap: moruna_kernel::Held,
    /// The most the writer's heap grew in one slice of the in-progress row group, its first
    /// slice aside (which makes the row group's column writers).
    jump: u64,
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
    /// The heap the run gives the sink's writers, every lane together (system design 3A,
    /// 6.3d); `None` keeps parquet's own limits.
    writer_share: Option<u64>,
    /// The run's grant, read at `open` (6.3d): it sets the writers' heap and the file buffers.
    grant: Option<WriterGrant>,
    /// One lane's part of it, settled at `open` with the lanes it holds.
    lane_heap: Option<u64>,
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
            writer_share: None,
            grant: None,
            lane_heap: None,
        })
    }

    /// Encode into at most `lanes` files at once: the run's workers (f.12). The sink's buffers
    /// are the same bytes whatever this is, divided between the lanes.
    pub fn with_lanes(mut self, lanes: usize) -> ParquetSink {
        self.max_lanes = lanes.max(1);
        self
    }

    /// Keep the sink's writers within `bytes` of heap, every lane together (system design 3A,
    /// item 6.3d): their share of the room above the arena. At `open` the share is divided
    /// among as many lanes, up to [`ParquetSink::with_lanes`], as each hold their columns'
    /// codecs and the writer's least heap ([`crate::writer_lanes`]); a schema one lane cannot
    /// hold is refused. Each lane's page, dictionary and write limits are derived from its part
    /// and the schema's leaf columns, its row group is flushed before what it holds reaches its
    /// part less a margin, and its file rolls before the file's page index outgrows its share
    /// ([`crate::lane_heap`]). Only row group, page, encoding and file boundaries move. `None`,
    /// the default, keeps parquet's own limits.
    pub fn with_writer_heap(mut self, bytes: Option<u64>) -> ParquetSink {
        self.writer_share = bytes.filter(|&bytes| bytes > 0);
        self
    }

    /// Take the writers' heap and the file buffers from `grant` at `open` (system design 3A,
    /// item 6.3d): the run sets it once it knows the sink's schema. The writers' heap is as
    /// [`ParquetSink::with_writer_heap`] would set it, and the file buffers bound
    /// `ParquetSinkConfig::file_bytes` at half of them. A grant not set by `open` changes
    /// nothing.
    pub fn with_grant(mut self, grant: WriterGrant) -> ParquetSink {
        self.grant = Some(grant);
        self
    }

    /// The codec the writer compresses a column with, unless a column of `writer_props` says
    /// otherwise.
    fn compression(&self) -> Compression {
        self.props
            .compression(&parquet::schema::types::ColumnPath::new(Vec::new()))
    }

    /// Divide the writers' share among the lanes `schema`'s columns let it hold (6.3d), before
    /// the file buffers are divided among the same lanes; refuse a schema one lane cannot hold.
    fn plan_lanes(&mut self, schema: &SchemaRef) -> Result<()> {
        if let Some(share) = self.writer_share {
            let leaves = lane_heap::leaves_of(schema)?;
            let planned = crate::writer_lanes(share, self.max_lanes, &leaves, self.compression())
                .map_err(|least| lane_refusal(leaves.count, least, share))?;
            self.max_lanes = planned.lanes;
            self.lane_heap = Some(planned.lane_bytes);
        }
        self.lock().stats.lanes = self.max_lanes as u64;
        Ok(())
    }

    /// The writer's properties for `schema`, and the limits of the lane's allowance when the
    /// sink has one (6.3d): parquet's limits, or the sink's `writer_props`, lowered to what the
    /// allowance holds for the schema's leaf columns. A payload whose schema has more columns
    /// than one lane holds is refused, as `open` refuses one.
    fn writer_props(&self, schema: &SchemaRef) -> Result<(WriterProperties, Option<Bounds>)> {
        let Some(allowance) = self.lane_heap else {
            return Ok((self.props.clone(), None));
        };
        let leaves = lane_heap::leaves_of(schema)?;
        let compression = self.compression();
        let least = crate::least_lane_bytes(&leaves, compression);
        if least > allowance {
            return Err(lane_refusal(leaves.count, least, allowance));
        }
        self.lock().stats.writer_heap_measured = moruna_kernel::held::heap_counting();
        let props = &self.props;
        // A bounded lane writes the claim's page, or parquet's when that is smaller (6.3d).
        let page = props
            .data_page_size_limit()
            .min(usize::try_from(crate::CLAIM_PAGE_BYTES).unwrap_or(usize::MAX));
        let mut bounds = lane_heap::bounds(
            allowance,
            &leaves,
            compression,
            props.data_page_row_count_limit(),
            page,
            props.dictionary_page_size_limit(),
            props.write_batch_size(),
        );
        let mut builder = props
            .clone()
            .into_builder()
            .set_data_page_row_count_limit(bounds.page_rows)
            .set_write_batch_size(bounds.write_batch)
            .set_data_page_size_limit(bounds.page_bytes)
            .set_dictionary_page_size_limit(bounds.dictionary_bytes);
        // A fixed-width column whose dictionary's empty hash index the lane cannot hold is
        // written plain; its dictionary otherwise keeps parquet's limit, bounded by the flush.
        for path in std::mem::take(&mut bounds.plain) {
            builder = builder.set_column_dictionary_enabled(path, false);
        }
        Ok((builder.build(), Some(bounds)))
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
    /// file will roll at. `min_bytes` is 0 for an ordinary file and a single oversized morsel's
    /// bytes when one is on its way in (section h); the footer is added to either.
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
        let (file, roll_bytes) =
            self.buffers
                .alloc_file_buf(min_bytes, self.cfg.row_group_bytes, file_bytes)?;
        lane.roll_bytes = roll_bytes;
        self.lock().stats.roll_bytes = roll_bytes;
        let (props, bounds) = self.writer_props(&schema)?;
        lane.bounds = bounds;
        let shared = Arc::new(Mutex::new(file));
        let writer = {
            let _counted = lane.heap.enter_heap();
            ArrowWriter::try_new(
                ArenaWrite {
                    shared: Arc::clone(&shared),
                },
                schema,
                Some(props),
            )
        }
        .map_err(|e| MorunaError::Sink(format!("parquet writer: {e}")))?;
        lane.current = Some(Current {
            name: part_name(index, "parquet"),
            writer,
            shared,
            seqs: Vec::new(),
            rows: 0,
            decoded: 0,
            flushed_decoded: 0,
        });
        lane.jump = 0;
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
            ..
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
        {
            // 6.3d: closing frees what the writer holds; that is counted out of the lane's heap.
            let _counted = lane.heap.enter_heap();
            writer.close().map_err(|e| encode_error(&e.to_string()))?;
        }
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
        let busy;
        let mut lane = match taken {
            Some(guard) => guard,
            None => {
                // The count and the push under one lock (6.3d): two encoders that find every
                // lane busy at once open one lane between them, never more than the sink's
                // lanes. Before, both could see room for one and push two, and a third lane held
                // a writer's heap beyond the writers' share (si_lane_heap's lane cap, under load).
                let opened = {
                    let mut all = self.lanes.lock().unwrap_or_else(|e| e.into_inner());
                    (all.len() < self.max_lanes).then(|| {
                        let lane = Arc::new(Mutex::new(Lane::default()));
                        all.push(Arc::clone(&lane));
                        lane
                    })
                };
                match opened {
                    Some(lane) => {
                        fresh = lane;
                        fresh.lock().unwrap_or_else(|e| e.into_inner())
                    }
                    // Every lane is busy and there are as many as workers may encode: wait for
                    // one, round robin, which is no longer than one encode.
                    None => {
                        busy = self.lanes();
                        let at = self
                            .next_lane
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                            % busy.len();
                        busy[at].lock().unwrap_or_else(|e| e.into_inner())
                    }
                }
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
                // 6.3d: a file whose page index would pass its share of the lane's heap with this
                // morsel rolls too, and the page index the footer will carry counts toward the
                // file's bytes: small pages make a large one.
                let index = lane.bounds.as_ref().map_or(0, |b| {
                    let row_groups = current.writer.flushed_row_groups().len() as u64 + 1;
                    b.index(current.rows + rows, current.decoded + bytes, row_groups)
                });
                let index_full = lane.bounds.as_ref().is_some_and(|b| index > b.index_share);
                if !current.seqs.is_empty()
                    && (so_far + bytes + index > lane.roll_bytes || index_full)
                {
                    // A morsel larger than a file gets a file of its own, sized for it (h); one
                    // that fits an ordinary file gets an ordinary file. Asking every roll for
                    // twice the morsel doubled the buffer's class whenever morsels were a large
                    // share of it, and at the smallest budget that was the allocation the arena
                    // could not give (F8.8). The file of its own is the morsel and its footer
                    // (`alloc_file_buf` adds the footer), not twice the morsel: its encoded bytes
                    // are no more than its decoded ones, and twice took a function's output a
                    // little past the lane's roll size (Enrich adds columns) into the next class,
                    // 256 MiB the arena did not have in one piece, and ended the run (2026-11-14).
                    let need = if bytes > lane.roll_bytes { bytes } else { 0 };
                    pending = Some(self.roll(lane)?);
                    self.open_file(lane, need)?;
                }
            }
        }
        // Stream C (2026-11-17): the buffer grows with the file. Room for the morsel and the
        // footer is made before it is encoded; when the arena has none, a file that holds
        // morsels rolls at what it holds and the morsel opens the next, so a tight arena writes
        // smaller files rather than refusing the run.
        if let Some(current) = lane.current.as_ref() {
            let so_far =
                current.writer.bytes_written() as u64 + current.writer.in_progress_size() as u64;
            let need = so_far.saturating_add(bytes);
            let roomy = current
                .shared
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .reserve(need.saturating_add(footer_for(need)));
            if !roomy && !current.seqs.is_empty() && pending.is_none() {
                let need = if bytes > lane.roll_bytes { bytes } else { 0 };
                pending = Some(self.roll(lane)?);
                self.open_file(lane, need)?;
            }
        }
        let roll_bytes = lane.roll_bytes;
        let bounds = lane.bounds.as_ref();
        let heap = &lane.heap;
        let jump = &mut lane.jump;
        // 6.3d: the writer's heap is measured where the program's allocator counts it, and
        // estimated from parquet's own figure where it does not.
        let measured = moruna_kernel::held::heap_counting();
        let Some(current) = lane.current.as_mut() else {
            return Err(MorunaError::Sink("the sink has no open file".into()));
        };
        // The footer shares the buffer, so a row group may not be allowed to grow past the roll
        // size: `row_group_bytes` is a target, and `roll_bytes` is what the arena agreed to.
        let target = self.cfg.row_group_bytes.min(roll_bytes);
        // 6.3d: with a heap allowance the morsel is written in slices (zero copy) of no more
        // rows than the lane's room under its flush point holds, or than the transients kept
        // above it hold where that is more, so a morsel that fits the room is written whole;
        // the row group is flushed once the heap the writer's calls hold, and twice the most
        // one slice grew it by, reach the flush point.
        let all = batch.num_rows();
        let mut at = 0;
        let mut slices = 0;
        while at < all {
            let take = bounds
                .map_or(all, |b| {
                    let room = b.room(
                        measured.then(|| (heap.held(), *jump)),
                        if measured {
                            0
                        } else {
                            current.writer.memory_size() as u64
                        },
                    );
                    b.slice(all, bytes, room)
                })
                .clamp(1, all - at);
            slices += 1;
            let first = current.writer.in_progress_rows() == 0;
            let before = heap.held();
            {
                let _counted = heap.enter_heap();
                current
                    .writer
                    .write(&batch.slice(at, take))
                    .map_err(|e| encode_error(&e.to_string()))?;
            }
            if !first {
                *jump = (*jump).max(heap.held().saturating_sub(before));
            }
            at += take;
            current.decoded += bytes * take as u64 / all as u64;
            let writer = &current.writer;
            if writer.in_progress_size() as u64 >= target
                || bounds.is_some_and(|b| {
                    b.flush(
                        measured.then(|| (heap.held(), *jump)),
                        writer.memory_size() as u64,
                    )
                })
            {
                let _counted = heap.enter_heap();
                current
                    .writer
                    .flush()
                    .map_err(|e| encode_error(&e.to_string()))?;
                current.flushed_decoded = current.decoded;
                *jump = 0;
            }
        }
        // The payload's arena bytes go back to the arena here, before the reactor has written
        // anything: the encoder has consumed them and the sink keeps no second copy (SI-I1).
        drop(batch);
        current.rows += rows;
        current.seqs.push(seq);
        {
            let mut inner = self.lock();
            inner.stats.writes += 1;
            inner.stats.slices += slices;
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

/// A schema whose columns one lane's writer heap cannot hold (6.3d). The facade refuses such a
/// run before it starts with the least budget that holds it; this is the sink's own refusal,
/// for a sink run without the facade or a payload wider than the schema it was opened with.
fn lane_refusal(leaves: usize, least: u64, given: u64) -> MorunaError {
    MorunaError::Config {
        name: "budget.host",
        msg: format!(
            "a Parquet writer of {leaves} leaf columns needs at least {least} bytes of heap for \
             one lane, its columns' codecs at the smallest page and its own least heap, and is \
             given {given}: give the run a larger budget"
        ),
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
        self.lock().phase.require_created()?;
        // 6.3d: the run's grant, set once it knew this schema, gives the writers' heap and the
        // file buffers.
        if let Some(grant) = &self.grant {
            if let Some(writers) = grant.writers() {
                self.writer_share = Some(writers);
            }
            if let Some(file_buffers) = grant.file_buffers() {
                self.cfg.file_bytes = self.cfg.file_bytes.min(file_buffers / 2);
            }
        }
        // 6.3d: the writers' share is divided among the lanes the schema's columns let it
        // hold, before the file buffers are divided among the same lanes.
        self.plan_lanes(schema)?;
        let mut inner = self.lock();
        // f.1: the schema `open` is given is the chain's, and a kernel the chain cannot see
        // through (a Python kernel, 05 d.1) reports its input schema as its output. The real
        // output schema is the first payload's, so the writer is built there and this one is
        // kept only for a run that writes nothing.
        inner.declared = Some(Arc::clone(schema));
        // A fresh run's output is the prefix's whole content (f.1, 2026-11-14).
        inner.stats.earlier_files_removed = self.dest.clear_earlier_output("parquet");
        // Each lane's share of the pair's bytes (f.12). No buffer is made here (stream C,
        // 2026-11-17): a lane makes its own when it opens its first file, and it grows with the
        // file, so a sink that writes little holds little and a lane that never opens a file
        // holds nothing.
        let lane = self.buffers.lane_cap(self.cfg.file_bytes, self.max_lanes);
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
            commit: None,
        })
    }

    fn committed_seq(&self) -> Option<Seq> {
        self.lock().ledger.committed_seq()
    }

    fn lagging(&self) -> bool {
        self.lock().ledger.lagging()
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
        self.lock().phase.require_created()?;
        // 6.3d: as at `open`.
        self.plan_lanes(schema)?;
        let mut inner = self.lock();
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
