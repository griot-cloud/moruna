//! `ParquetSink` (08 f.1, e.2, f.5, f.7, f.8).
//!
//! The encoder writes straight into one arena buffer per open file through the `std::io::Write`
//! implementation below, and the reactor writes the finished file from a view over that buffer.
//! That encode is the one CPU copy G-I2 allows a sink, and it is the only one: no `Vec<u8>` of
//! payload size exists anywhere in this file.

use std::collections::BTreeSet;
use std::io::Write;
use std::sync::{Arc, Mutex};

use amoru_kernel::arrow::datatypes::SchemaRef;
use amoru_kernel::arrow::record_batch::RecordBatch;
use amoru_kernel::{
    Allocator, AmoruError, BoxFuture, Buffer, Completion, Payload, PayloadKind, PayloadSpec,
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

/// Room above `file_bytes` for the footer the writer appends at close (f.1).
const FOOTER_HEADROOM: u64 = 1 << 20;

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
    schema: Option<SchemaRef>,
    current: Option<Current>,
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
                schema: None,
                current: None,
                ledger: Ledger::new(KIND),
                stats: SinkStats::default(),
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

    /// Allocate the next file's buffer and its writer. `min_bytes` is `file_bytes`, or twice a
    /// single oversized morsel's bytes when one is on its way in (section h).
    fn open_file(&self, inner: &mut Inner, min_bytes: u64) -> Result<()> {
        let Some(schema) = inner.schema.clone() else {
            return Err(AmoruError::Sink("the sink has no schema".into()));
        };
        let capacity = min_bytes.max(self.cfg.file_bytes) + FOOTER_HEADROOM;
        let Ok(capacity) = usize::try_from(capacity) else {
            return Err(AmoruError::Sink(format!(
                "a file buffer of {capacity} bytes does not fit this platform"
            )));
        };
        let buf = self.alloc.alloc(capacity, host_tier(&*self.alloc))?;
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
        .map_err(|e| AmoruError::Sink(format!("parquet writer: {e}")))?;
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
            return Err(AmoruError::Sink("the sink has no open file".into()));
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
            "amoru.run_id".to_string(),
            hex(&self.run_id),
        ));
        if !seqs.is_empty() {
            writer.append_key_value_metadata(KeyValue::new(
                "amoru.seq_min".to_string(),
                seq_min.to_string(),
            ));
            writer.append_key_value_metadata(KeyValue::new(
                "amoru.seq_max".to_string(),
                seq_max.to_string(),
            ));
        }
        writer.close().map_err(|e| encode_error(&e.to_string()))?;
        let (buf, used) = {
            let mut file = shared.lock().unwrap_or_else(|e| e.into_inner());
            (file.buf.take(), file.used)
        };
        let Some(buf) = buf else {
            return Err(AmoruError::Sink("the output buffer is gone".into()));
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
    fn fail(&self, error: &AmoruError) {
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
            return Err(AmoruError::Sink(
                "the parquet sink accepts a table payload".into(),
            ));
        };
        self.check_schema(inner, &batch)?;
        // Validation is done: from here a failure is the encoder's or the store's, and it moves
        // the sink to `Failed` (e.1), where `finish` reports it and the committed files stand.
        let out = self.encode_checked(inner, seq, batch, rows, bytes);
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
            None => return Err(AmoruError::Sink("the sink has no open file".into())),
        };
        let pending = if started && so_far + bytes > self.cfg.file_bytes {
            // A morsel larger than `file_bytes` gets a file of its own, sized for it (h).
            Some(self.roll(inner, bytes.saturating_mul(2), true)?)
        } else {
            None
        };

        let Some(current) = inner.current.as_mut() else {
            return Err(AmoruError::Sink("the sink has no open file".into()));
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
        if current.writer.in_progress_size() as u64 >= self.cfg.row_group_bytes {
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
            return Err(AmoruError::Sink("the sink has no schema".into()));
        };
        let batch_schema = batch.schema();
        if schema.fields() == batch_schema.fields() {
            return Ok(());
        }
        Err(AmoruError::Sink(differing_field(schema, &batch_schema)))
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

fn encode_error(msg: &str) -> AmoruError {
    if msg.contains(TOO_LARGE) {
        AmoruError::Sink(msg.to_string())
    } else {
        AmoruError::Sink(format!("parquet encode: {msg}"))
    }
}

/// Name the first field that differs, which is what the scheduler's diagnostic needs (h).
fn differing_field(expected: &SchemaRef, found: &SchemaRef) -> String {
    for (i, field) in expected.fields().iter().enumerate() {
        match found.fields().get(i) {
            Some(other) if other == field => {}
            Some(other) => {
                return format!(
                    "schema drift: field {} is {:?}, the sink opened with {:?}",
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
            return Err(AmoruError::Sink(
                "the parquet sink accepts a table schema".into(),
            ));
        };
        let mut inner = self.lock();
        inner.phase.require_created()?;
        inner.schema = Some(Arc::clone(schema));
        self.open_file(&mut inner, self.cfg.file_bytes)?;
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
                Phase::Created => return Err(AmoruError::Sink("the sink is not open".into())),
                Phase::Finished => return Err(AmoruError::Sink("finish ran twice".into())),
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
            return Err(AmoruError::Sink(msg));
        }
        let pending = {
            let mut inner = self.lock();
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
            return Err(AmoruError::Sink(
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
        inner.schema = Some(Arc::clone(schema));
        self.open_file(&mut inner, self.cfg.file_bytes)?;
        inner.phase = Phase::Open;
        Ok(())
    }
}
