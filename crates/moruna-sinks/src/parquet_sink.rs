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
    Allocator, BoxFuture, Buffer, Completion, MorunaError, Payload, PayloadKind, PayloadSpec,
    Reactor, Result, RunId, Seq, Sink, SinkSummary, SourceSchema, TierPref,
};
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;

use crate::checkpoint::{CommittedFile, Ledger};
use crate::commit::{ObjectPrefix, TMP_SUFFIX};
use crate::stats::SinkStats;
use crate::{Phase, host_tier, part_name, require_host};

/// Room above the roll size for the footer the writer appends at close (f.1).
const FOOTER_HEADROOM: u64 = 1 << 20;

/// The smallest file buffer worth asking for: the arena's own smallest class (02 e.2).
const GRANULE: u64 = 64 << 10;

/// The size class 02 e.2 will charge for `n` bytes: a power of two, no smaller than the 64 KiB
/// granule. The arena charges the class and not the request (AR-I5), so a sink that asks for
/// anything else pays for the difference and needs the whole class free at once.
fn class_ceil(n: u64) -> u64 {
    n.max(GRANULE).next_power_of_two()
}

/// The footer's share of a `want` byte class (f.1): `FOOTER_HEADROOM`, or half the class when
/// that is smaller, because a small file's footer is small and a buffer that is all footer can
/// hold no row group. A file with a `want / 2` roll size can hold at most one row group per
/// `row_group_bytes`, so half the class is ample for its footer.
fn footer_for(want: u64) -> u64 {
    FOOTER_HEADROOM.min(want.max(2) / 2)
}

/// The message a buffer overflow carries out of the encoder, so `write` can turn a
/// `std::io::Error` back into the `Sink` error section h names.
const TOO_LARGE: &str = "morsel too large to encode";

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

/// One open output file's arena buffer and how much of it the encoder has used.
struct FileBuf {
    buf: Option<Buffer>,
    used: usize,
}

/// The `std::io::Write` the Parquet writer encodes into: one arena buffer, no heap staging.
struct ArenaWrite {
    shared: Arc<Mutex<FileBuf>>,
}

impl Write for ArenaWrite {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let mut file = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        let FileBuf { buf, used } = &mut *file;
        let Some(buf) = buf.as_mut() else {
            return Err(std::io::Error::other("the output buffer is gone"));
        };
        let end = *used + data.len();
        if end > buf.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                format!("{TOO_LARGE}: {end} bytes into {} bytes", buf.len()),
            ));
        }
        buf[*used..end].copy_from_slice(data);
        *used = end;
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
    current: Option<Current>,
    ledger: Ledger,
    stats: SinkStats,
    /// The byte count the open file rolls at (f.1): `file_bytes`, or the smaller buffer the
    /// arena could serve.
    roll_bytes: u64,
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
                current: None,
                ledger: Ledger::new(KIND),
                stats: SinkStats::default(),
                roll_bytes: 0,
            }),
        })
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

    /// The file buffer and the roll size it implies (f.1). A morsel that needs more than
    /// `file_bytes` gets the buffer it needs or nothing; an ordinary file takes the largest
    /// buffer the arena will give between `file_bytes` and the class its row group needs.
    ///
    /// Every request is a whole size class and the footer headroom lives *inside* it (f.1).
    /// Asking for `row_group_bytes + footer` asked for 129 MiB at the default row group, and
    /// 02 e.2 serves that out of the 256 MiB class, which has to be free all at once: the
    /// sink reserved twice what it wanted and no 512 MiB budget could open it (PM, 2026-09-23).
    fn alloc_file_buf(&self, min_bytes: u64) -> Result<(Buffer, u64)> {
        // `row_group_bytes` is a flush threshold and an upper bound, so the footer may come out
        // of its class; `min_bytes` is a morsel that has to fit, so its class is sized above the
        // footer as well as above the morsel.
        let soft = class_ceil(self.cfg.row_group_bytes.max(GRANULE));
        let hard = match min_bytes {
            0 => 0,
            need => class_ceil(need.saturating_add(footer_for(need))),
        };
        let floor = soft.max(hard);
        let mut want = class_ceil(self.cfg.file_bytes).max(floor);
        loop {
            match usize::try_from(want) {
                Ok(capacity) => match self.alloc.alloc(capacity, host_tier(&*self.alloc)) {
                    // The footer is spent out of the class, so the file rolls below it.
                    Ok(buf) => return Ok((buf, want - footer_for(want))),
                    // Below the floor the error stands: a sink that cannot hold one row group
                    // cannot encode one.
                    Err(e) if want <= floor => return Err(e),
                    Err(_) => {}
                },
                Err(_) if want <= floor => {
                    return Err(MorunaError::Sink(format!(
                        "a file buffer of {want} bytes does not fit this platform"
                    )));
                }
                Err(_) => {}
            }
            want = (want / 2).max(floor);
        }
    }

    /// Allocate the next file's buffer and its writer, and settle the byte count the file will
    /// roll at. `min_bytes` is 0 for an ordinary file and twice a single oversized morsel's
    /// bytes when one is on its way in (section h).
    ///
    /// f.1: the buffer is not `file_bytes` reserved in advance. The sink asks for
    /// `file_bytes + footer`, halves on `Alloc` down to `row_group_bytes + footer`, and rolls
    /// at whatever it got, so `sink.file_bytes` is an upper bound on the file and the arena's
    /// free space is the other. A 1 GiB default that reserved a gigabyte could not open inside
    /// any budget below about 1.2 GiB, which is most of them.
    fn open_file(&self, inner: &mut Inner, min_bytes: u64) -> Result<()> {
        let Some(schema) = inner.schema.clone() else {
            return Err(MorunaError::Sink("the sink has no schema".into()));
        };
        let (buf, roll_bytes) = self.alloc_file_buf(min_bytes)?;
        inner.roll_bytes = roll_bytes;
        inner.stats.roll_bytes = roll_bytes;
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
        let index = inner.ledger.take_index();
        inner.current = Some(Current {
            name: part_name(index, "parquet"),
            writer,
            shared,
            seqs: Vec::new(),
            rows: 0,
        });
        Ok(())
    }

    /// Close the open file, hand its bytes to the reactor, and open the next one (f.1, f.5).
    fn roll(&self, inner: &mut Inner, next_min_bytes: u64, reopen: bool) -> Result<Pending> {
        let Some(current) = inner.current.take() else {
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
        inner.stats.rolls += 1;
        tracing::info!(target: "sink.roll", file = %name, bytes = used, "rolled a parquet file");
        if reopen {
            self.open_file(inner, next_min_bytes)?;
        }
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

    /// Move to `Failed`; the file being written is released, the committed files stay (h).
    fn fail(&self, error: &MorunaError) {
        let mut inner = self.lock();
        if !matches!(inner.phase, Phase::Failed(_)) {
            inner.phase = Phase::Failed(error.to_string());
        }
        inner.current = None;
    }

    /// Prepare one write under the lock: roll if this morsel would overflow the file, encode
    /// it, release its bytes. Returns the rolled file, if there was one, to await outside.
    fn encode(&self, inner: &mut Inner, seq: Seq, payload: Payload) -> Result<Option<Pending>> {
        inner.phase.require_open()?;
        require_host(&payload)?;
        let rows = payload.rows();
        let bytes = payload.bytes();
        let Payload::Table(batch, _) = payload else {
            return Err(MorunaError::Sink(
                "the parquet sink accepts a table payload".into(),
            ));
        };
        // f.1: the first payload settles the output schema; every later one is checked
        // against it, so a kernel that appends a column is written as it is and a kernel that
        // drifts between morsels is still an error.
        let first = inner.schema.is_none();
        if first {
            inner.schema = Some(batch.schema());
        } else {
            self.check_schema(inner, &batch)?;
        }
        // Validation is done: from here a failure is the encoder's or the store's, and it moves
        // the sink to `Failed` (e.1), where `finish` reports it and the committed files stand.
        let out = if first {
            match self.open_file(inner, 0) {
                Ok(()) => self.encode_checked(inner, seq, batch, rows, bytes),
                Err(e) => Err(e),
            }
        } else {
            self.encode_checked(inner, seq, batch, rows, bytes)
        };
        if let Err(e) = &out {
            inner.phase = Phase::Failed(e.to_string());
            inner.current = None;
        }
        out
    }

    fn encode_checked(
        &self,
        inner: &mut Inner,
        seq: Seq,
        batch: RecordBatch,
        rows: u64,
        bytes: u64,
    ) -> Result<Option<Pending>> {
        let (so_far, started) = match inner.current.as_ref() {
            Some(current) => (
                current.writer.bytes_written() as u64 + current.writer.in_progress_size() as u64,
                !current.seqs.is_empty(),
            ),
            None => return Err(MorunaError::Sink("the sink has no open file".into())),
        };
        let pending = if started && so_far + bytes > inner.roll_bytes {
            // A morsel larger than `file_bytes` gets a file of its own, sized for it (h).
            Some(self.roll(inner, bytes.saturating_mul(2), true)?)
        } else {
            None
        };

        let Some(current) = inner.current.as_mut() else {
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
        if current.writer.in_progress_size() as u64
            >= self.cfg.row_group_bytes.min(inner.roll_bytes)
        {
            current
                .writer
                .flush()
                .map_err(|e| encode_error(&e.to_string()))?;
        }
        inner.stats.writes += 1;
        inner.stats.encode_bytes += bytes;
        tracing::trace!(target: "sink.encode", seq, bytes, "encoded a morsel");
        // The encode is the symmetric exception to G-I2 and the arena is where it is counted.
        self.alloc.note_payload_copy(bytes);
        Ok(pending)
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

fn hex(run_id: &RunId) -> String {
    use std::fmt::Write as _;
    run_id.0.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

fn encode_error(msg: &str) -> MorunaError {
    if msg.contains(TOO_LARGE) {
        MorunaError::Sink(msg.to_string())
    } else {
        MorunaError::Sink(format!("parquet encode: {msg}"))
    }
}

/// Name the first field that differs, which is what the scheduler's diagnostic needs (h).
fn differing_field(expected: &SchemaRef, found: &SchemaRef) -> String {
    for (i, field) in expected.fields().iter().enumerate() {
        match found.fields().get(i) {
            Some(other) if other == field => {}
            Some(other) => {
                return format!(
                    "schema drift: field {} is {:?}, the first morsel had {:?}",
                    field.name(),
                    other.data_type(),
                    field.data_type()
                );
            }
            None => return format!("schema drift: field {} is missing", field.name()),
        }
    }
    match found.fields().get(expected.fields().len()) {
        Some(extra) => format!("schema drift: field {} is not in the schema", extra.name()),
        None => "schema drift: the schemas differ in metadata".to_string(),
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
        inner.phase = Phase::Open;
        Ok(())
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    fn write(&self, seq: Seq, payload: Payload) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let pending = {
                let mut inner = self.lock();
                self.encode(&mut inner, seq, payload)?
            };
            let Some(pending) = pending else {
                return Ok(());
            };
            let Pending {
                entry,
                seqs,
                completion,
                buf,
            } = pending;
            let outcome = completion.await;
            drop(buf);
            match outcome {
                Ok(()) => {
                    self.commit(entry, &seqs);
                    Ok(())
                }
                Err(e) => {
                    self.fail(&e);
                    Err(e)
                }
            }
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
                inner.current = None;
                inner.phase = Phase::Finished;
            }
            failed
        };
        if let Some(msg) = failed {
            // The reactor aborted the multipart upload already (06 f.4); a local destination
            // may still hold temporary files, which go here.
            self.clear_tmps();
            return Err(MorunaError::Sink(msg));
        }
        let pending = {
            let mut inner = self.lock();
            // A run that wrote nothing never adopted a schema, so the one `open` was given is
            // what the empty file carries (h).
            if inner.current.is_none() {
                inner.schema = inner.declared.clone();
                self.open_file(&mut inner, 0)?;
            }
            self.roll(&mut inner, 0, false)?
        };
        let Pending {
            entry,
            seqs,
            completion,
            buf,
        } = pending;
        let outcome = completion.wait();
        drop(buf);
        match outcome {
            Ok(()) => self.commit(entry, &seqs),
            Err(e) => {
                self.fail(&e);
                return Err(e);
            }
        }
        self.write_success()?;
        let mut inner = self.lock();
        inner.phase = Phase::Finished;
        let (rows, bytes) = inner.ledger.totals();
        Ok(SinkSummary {
            rows,
            bytes,
            files: inner.ledger.names(),
        })
    }

    fn committed_seq(&self) -> Option<Seq> {
        self.lock().ledger.committed_seq()
    }

    fn skip(&self, seq: Seq) {
        self.lock().ledger.skip(seq);
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
