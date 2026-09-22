//! `ArrowIpcSink` (08 f.2, e.3).
//!
//! The record batches are the page-aligned records of contracts e.7, produced by
//! `amoru_kernel::ipc::encode_framing` and by nothing else, so a segment body and an IPC file
//! record batch are byte-identical for the same batch and the two cannot drift. Each body is
//! written from the payload's own arena buffer at a page-aligned offset, so the sink copies no
//! payload bytes at all (SI-I4).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use amoru_kernel::arrow::datatypes::SchemaRef;
use amoru_kernel::arrow::ipc::writer::FileWriter;
use amoru_kernel::arrow::record_batch::RecordBatch;
use amoru_kernel::{
    Allocator, AmoruError, BoxFuture, Buffer, BufferView, Completion, Payload, PayloadKind,
    PayloadSpec, Reactor, Result, RunId, Seq, Sink, SinkSummary, SourceSchema, TierPref, ipc,
};

use crate::checkpoint::{CommittedFile, Ledger};
use crate::commit::{LocalDir, TMP_SUFFIX};
use crate::stats::SinkStats;
use crate::{Phase, host_tier, part_name, require_host, round_up};

/// `ARROW1` and the two padding bytes that start every Arrow IPC file.
const MAGIC: [u8; 8] = [b'A', b'R', b'R', b'O', b'W', b'1', 0, 0];

/// The kind this sink writes into its checkpoint (e.5).
const KIND: &str = "ipc";

/// One flatbuffer `Block`: an i64 offset, an i32 metadata length, padding, an i64 body length.
const BLOCK_BYTES: usize = 24;

/// Where an Arrow IPC sink writes and how big its files are.
pub struct ArrowIpcSinkConfig {
    /// Directory the `part-NNNNN.arrow` files are created in.
    pub path: PathBuf,
    /// File roll size.
    pub file_bytes: u64,
}

impl Default for ArrowIpcSinkConfig {
    /// The default of preamble section 5: 1 GiB files.
    fn default() -> ArrowIpcSinkConfig {
        ArrowIpcSinkConfig {
            path: PathBuf::new(),
            file_bytes: 1 << 30,
        }
    }
}

/// One output file while it is being written.
struct FileState {
    name: String,
    tmp: PathBuf,
    cursor: u64,
    blocks: Vec<(i64, i32, i64)>,
    seqs: Vec<Seq>,
    rows: u64,
    inflight: u64,
    closing: bool,
}

struct Inner {
    phase: Phase,
    schema: Option<SchemaRef>,
    /// The length of the schema message, which every record's framing repeats and which fixes
    /// where a later record's `base` sits relative to the byte it is written at (f.2).
    schema_len: Option<u64>,
    files: BTreeMap<u64, FileState>,
    current: Option<u64>,
    next_file_id: u64,
    ledger: Ledger,
    stats: SinkStats,
}

/// A sink that writes Arrow IPC files whose buffers are page-aligned (e.3).
pub struct ArrowIpcSink {
    cfg: ArrowIpcSinkConfig,
    dir: LocalDir,
    reactor: Arc<dyn Reactor>,
    alloc: Arc<dyn Allocator>,
    run_id: RunId,
    inner: Mutex<Inner>,
}

/// What one record's submission left for the caller to await.
struct Record {
    file_id: u64,
    completions: Vec<Completion<()>>,
    /// The framing buffer and the payload stay alive until the last completion resolves; the
    /// views the reactor holds read their bytes (SI-I1).
    framing: Arc<Buffer>,
    /// Buffers the IPC encoder made rather than the payload's own, staged in the arena.
    extra: Vec<Arc<Buffer>>,
    payload: Payload,
}

/// A file whose footer is with the reactor and which is not yet committed.
struct Pending {
    entry: CommittedFile,
    seqs: Vec<Seq>,
    completion: Completion<()>,
    footer: Arc<Buffer>,
}

impl ArrowIpcSink {
    /// Build a sink over `cfg`. The reactor writes every byte; the allocator provides the
    /// framing and footer buffers, which hold no payload bytes.
    pub fn new(
        cfg: ArrowIpcSinkConfig,
        reactor: Arc<dyn Reactor>,
        alloc: Arc<dyn Allocator>,
    ) -> Result<ArrowIpcSink> {
        let dir = LocalDir::new(&cfg.path)?;
        Ok(ArrowIpcSink {
            cfg,
            dir,
            reactor,
            alloc,
            run_id: RunId([0; 16]),
            inner: Mutex::new(Inner {
                phase: Phase::Created,
                schema: None,
                schema_len: None,
                files: BTreeMap::new(),
                current: None,
                next_file_id: 0,
                ledger: Ledger::new(KIND),
                stats: SinkStats::default(),
            }),
        })
    }

    /// Name the run whose id goes into every file's footer metadata (e.3). Unset, the footer
    /// carries the nil run id: `ArrowIpcSinkConfig` has no field for a run id (d.1).
    pub fn with_run_id(mut self, run_id: RunId) -> ArrowIpcSink {
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

    /// Start the next file: the magic and its padding, written and waited for, so that every
    /// later offset in the file is one this sink chose (f.2).
    fn open_file(&self, inner: &mut Inner) -> Result<()> {
        let index = inner.ledger.take_index();
        let name = part_name(index, "arrow");
        let tmp = self.dir.tmp_of(&name);
        let mut buf = self.alloc.alloc(MAGIC.len(), host_tier(&*self.alloc))?;
        buf[..MAGIC.len()].copy_from_slice(&MAGIC);
        let buf = Arc::new(buf);
        let view = buf.view().slice(0, MAGIC.len());
        self.reactor.write_file(&tmp, 0, view).wait()?;
        let id = inner.next_file_id;
        inner.next_file_id += 1;
        inner.files.insert(
            id,
            FileState {
                name,
                tmp,
                cursor: MAGIC.len() as u64,
                blocks: Vec::new(),
                seqs: Vec::new(),
                rows: 0,
                inflight: 0,
                closing: false,
            },
        );
        inner.current = Some(id);
        Ok(())
    }

    /// Lay one record out and submit its writes (f.2).
    fn submit(&self, inner: &mut Inner, seq: Seq, payload: Payload) -> Result<Record> {
        inner.phase.require_open()?;
        require_host(&payload)?;
        let rows = payload.rows();
        let tier = payload.tier();
        let Payload::Table(batch, _) = &payload else {
            return Err(AmoruError::Sink(
                "the arrow ipc sink accepts a table payload".into(),
            ));
        };
        // Cloning a `RecordBatch` bumps the reference count of every column; it copies no
        // payload bytes, and the payload itself stays whole until its writes resolve.
        let batch = batch.clone();
        self.check_schema(inner, &batch)?;
        let out = self.submit_checked(inner, seq, &batch, tier, rows, payload);
        if let Err(e) = &out {
            inner.phase = Phase::Failed(e.to_string());
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn submit_checked(
        &self,
        inner: &mut Inner,
        seq: Seq,
        batch: &RecordBatch,
        tier: amoru_kernel::Tier,
        rows: u64,
        payload: Payload,
    ) -> Result<Record> {
        let page = self.alloc.page_bytes() as u64;
        let Some(file_id) = inner.current else {
            return Err(AmoruError::Sink("the sink has no open file".into()));
        };
        let Some(file) = inner.files.get(&file_id) else {
            return Err(AmoruError::Sink("the sink has no open file".into()));
        };
        let base = round_up(file.cursor, page);
        let first = file.blocks.is_empty();
        let tmp = file.tmp.clone();

        let (framing, bodies) = ipc::encode_framing(batch, page as usize, base, &*self.alloc)?;
        let lengths = MessageLengths::of(&framing)?;
        match inner.schema_len {
            Some(known) if known != lengths.schema => {
                return Err(AmoruError::Sink(format!(
                    "schema drift: the schema message is {} bytes, the sink opened with {known}",
                    lengths.schema
                )));
            }
            Some(_) => {}
            None => inner.schema_len = Some(lengths.schema),
        }

        let framing = Arc::new(framing);
        let mut completions = Vec::with_capacity(bodies.len() + 1);
        // The first record of a file carries the file's one Schema message; every later record
        // writes only its RecordBatch message, at `base + schema_len`, which is where the body
        // offsets `encode_framing` computed assume it sits (f.2, e.3).
        let framing_view = framing.view();
        if first {
            completions.push(self.reactor.write_file(
                &tmp,
                base,
                framing_view.slice(0, lengths.total() as usize),
            ));
        } else {
            completions.push(self.reactor.write_file(
                &tmp,
                base + lengths.schema,
                framing_view.slice(lengths.schema as usize, lengths.record as usize),
            ));
        }

        let own = own_buffers(batch);
        let mut extra: Vec<Arc<Buffer>> = Vec::new();
        let mut copied = 0u64;
        let body_base = base + lengths.total();
        let mut end = body_base;
        for (offset, body) in &bodies {
            if body.is_empty() {
                // A zero-length buffer entry has nothing to write and nothing to keep alive.
                continue;
            }
            let view = match BufferView::of_arrow(body, &*self.alloc) {
                // The payload's own buffer, written straight out of the arena: no copy (SI-I4).
                Ok(view) => view,
                Err(_) => {
                    // A buffer the IPC encoder made rather than one of the payload's, such as
                    // the all-ones validity bitmap it emits for a column with no nulls. It is
                    // not arena memory, so it is staged in a small arena buffer of its own.
                    // A payload buffer that is somehow not the arena's is a copy of payload
                    // bytes, and is counted as one rather than passed off as metadata.
                    let mut buf = self.alloc.alloc(body.len(), host_tier(&*self.alloc))?;
                    buf[..body.len()].copy_from_slice(body.as_slice());
                    if own.contains(&(body.as_ptr() as usize)) {
                        self.alloc.note_payload_copy(body.len() as u64);
                        copied += body.len() as u64;
                    }
                    let buf = Arc::new(buf);
                    let view = buf.view().slice(0, body.len());
                    extra.push(buf);
                    view
                }
            };
            completions.push(self.reactor.write_file(&tmp, *offset as u64, view));
            end = end.max(*offset as u64 + body.len() as u64);
        }
        let _ = tier;

        let block = (
            (base + lengths.schema) as i64,
            lengths.record as i32,
            (end - body_base) as i64,
        );
        let Some(file) = inner.files.get_mut(&file_id) else {
            return Err(AmoruError::Sink("the sink has no open file".into()));
        };
        file.blocks.push(block);
        file.seqs.push(seq);
        file.rows += rows;
        file.cursor = end;
        file.inflight += 1;
        let roll = file.cursor >= self.cfg.file_bytes;
        if roll {
            file.closing = true;
            inner.stats.rolls += 1;
            tracing::info!(target: "sink.roll", file = %file.name, bytes = file.cursor, "rolled an arrow ipc file");
            self.open_file(inner)?;
        }
        inner.stats.writes += 1;
        inner.stats.encode_bytes += copied;
        tracing::trace!(target: "sink.encode", seq, bytes = copied, "laid out an ipc record");
        Ok(Record {
            file_id,
            completions,
            framing,
            extra,
            payload,
        })
    }

    /// One record's writes have resolved: drop the payload, and close the file if this was the
    /// last record it was waiting on.
    fn record_done(&self, file_id: u64) -> Result<Option<Pending>> {
        let mut inner = self.lock();
        let Some(file) = inner.files.get_mut(&file_id) else {
            return Ok(None);
        };
        file.inflight -= 1;
        if !file.closing || file.inflight > 0 {
            return Ok(None);
        }
        self.close_file(&mut inner, file_id).map(Some)
    }

    /// Write the footer of a file whose records are all on disk (f.2, e.3).
    fn close_file(&self, inner: &mut Inner, file_id: u64) -> Result<Pending> {
        let Some(file) = inner.files.remove(&file_id) else {
            return Err(AmoruError::Sink("the sink has no such file".into()));
        };
        if inner.current == Some(file_id) {
            inner.current = None;
        }
        let Some(schema) = inner.schema.clone() else {
            return Err(AmoruError::Sink("the sink has no schema".into()));
        };
        let seq_min = file.seqs.iter().min().copied().unwrap_or(0);
        let seq_max = file.seqs.iter().max().copied().unwrap_or(0);
        let tail = build_footer(
            &schema,
            &file.blocks,
            &self.run_id,
            (!file.seqs.is_empty()).then_some((seq_min, seq_max)),
        )?;
        let at = round_up(file.cursor, 8);
        let mut buf = self.alloc.alloc(tail.len(), host_tier(&*self.alloc))?;
        buf[..tail.len()].copy_from_slice(&tail);
        let buf = Arc::new(buf);
        let view = buf.view().slice(0, tail.len());
        let completion = self.reactor.write_file(&file.tmp, at, view);
        Ok(Pending {
            entry: CommittedFile {
                name: file.name,
                seq_min,
                seq_max,
                rows: file.rows,
                bytes: at + tail.len() as u64,
            },
            seqs: file.seqs,
            completion,
            footer: buf,
        })
    }

    /// Make a written file visible under its final name and record it (f.5, f.7).
    fn commit(&self, entry: CommittedFile, seqs: &[Seq]) -> Result<()> {
        self.dir.commit(&entry.name)?;
        let mut inner = self.lock();
        tracing::info!(target: "sink.commit", file = %entry.name, "committed an arrow ipc file");
        inner.stats.files_committed += 1;
        inner.ledger.commit(entry, seqs);
        Ok(())
    }

    fn fail(&self, error: &AmoruError) {
        let mut inner = self.lock();
        if !matches!(inner.phase, Phase::Failed(_)) {
            inner.phase = Phase::Failed(error.to_string());
        }
    }

    fn check_schema(&self, inner: &Inner, batch: &RecordBatch) -> Result<()> {
        let Some(schema) = inner.schema.as_ref() else {
            return Err(AmoruError::Sink("the sink has no schema".into()));
        };
        if schema.fields() == batch.schema().fields() {
            return Ok(());
        }
        Err(AmoruError::Sink(format!(
            "schema drift: the batch schema {:?} is not the schema the sink opened with",
            batch.schema().fields()
        )))
    }
}

/// Every buffer the batch's own arrays hold, by address, so a body the IPC encoder made can be
/// told from one of the payload's (SI-I4).
fn own_buffers(batch: &RecordBatch) -> BTreeSet<usize> {
    fn walk(data: &amoru_kernel::arrow::array::ArrayData, out: &mut BTreeSet<usize>) {
        if let Some(nulls) = data.nulls() {
            out.insert(nulls.buffer().as_ptr() as usize);
        }
        for buffer in data.buffers() {
            out.insert(buffer.as_ptr() as usize);
        }
        for child in data.child_data() {
            walk(child, out);
        }
    }
    let mut out = BTreeSet::new();
    for column in batch.columns() {
        walk(&column.to_data(), &mut out);
    }
    out
}

/// The two message headers inside one framing buffer (contracts e.7).
struct MessageLengths {
    schema: u64,
    record: u64,
}

impl MessageLengths {
    fn of(framing: &Buffer) -> Result<MessageLengths> {
        let bytes: &[u8] = framing;
        let schema = message_len(bytes, 0)?;
        let record = message_len(bytes, schema as usize)?;
        Ok(MessageLengths { schema, record })
    }

    fn total(&self) -> u64 {
        self.schema + self.record
    }
}

fn message_len(bytes: &[u8], at: usize) -> Result<u64> {
    let Some(header) = bytes.get(at..at + 8) else {
        return Err(AmoruError::Sink(
            "the framing is shorter than a header".into(),
        ));
    };
    if header[0..4] != [0xff; 4] {
        return Err(AmoruError::Sink(
            "the continuation marker is missing".into(),
        ));
    }
    let len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as u64;
    Ok(8 + len)
}

/// The footer, its length and the trailing magic, as the Arrow IPC file format wants them.
///
/// The flatbuffer builders that produce a `Footer` are `arrow::ipc`'s, but they take a
/// `flatbuffers::FlatBufferBuilder`, which `arrow` does not re-export and which is not in the
/// preamble's dependency table. So the footer is produced by `arrow`'s own `FileWriter` over a
/// template file with one empty record batch per block, and the blocks in it are then rewritten
/// to this file's offsets in place, the same way `amoru_kernel::ipc` rewrites buffer entries.
fn build_footer(
    schema: &SchemaRef,
    blocks: &[(i64, i32, i64)],
    run_id: &RunId,
    seq_range: Option<(Seq, Seq)>,
) -> Result<Vec<u8>> {
    let bad = |e: String| AmoruError::Sink(format!("arrow ipc footer: {e}"));
    let mut template: Vec<u8> = Vec::new();
    let mut writer = FileWriter::try_new(&mut template, schema).map_err(|e| bad(e.to_string()))?;
    writer.write_metadata("amoru.run_id", hex(run_id));
    if let Some((seq_min, seq_max)) = seq_range {
        writer.write_metadata("amoru.seq_min", seq_min.to_string());
        writer.write_metadata("amoru.seq_max", seq_max.to_string());
    }
    let empty = RecordBatch::new_empty(Arc::clone(schema));
    for _ in blocks {
        writer.write(&empty).map_err(|e| bad(e.to_string()))?;
    }
    writer.finish().map_err(|e| bad(e.to_string()))?;
    drop(writer);

    if template.len() < 10 {
        return Err(bad("the template file has no footer".into()));
    }
    let end = template.len() - 10;
    let len_bytes = [
        template[end],
        template[end + 1],
        template[end + 2],
        template[end + 3],
    ];
    let footer_len = u32::from_le_bytes(len_bytes) as usize;
    if footer_len > end {
        return Err(bad("the template footer is longer than the file".into()));
    }
    let mut footer = template[end - footer_len..end].to_vec();
    rewrite_blocks(&mut footer, blocks)?;

    let mut tail = Vec::with_capacity(footer.len() + 10);
    tail.extend_from_slice(&footer);
    tail.extend_from_slice(&(footer.len() as u32).to_le_bytes());
    tail.extend_from_slice(&MAGIC[..6]);
    Ok(tail)
}

/// Overwrite the footer's record batch blocks with this file's offsets and lengths.
fn rewrite_blocks(footer: &mut [u8], blocks: &[(i64, i32, i64)]) -> Result<()> {
    let bad = |e: &str| AmoruError::Sink(format!("arrow ipc footer: {e}"));
    if blocks.is_empty() {
        return Ok(());
    }
    let start = {
        let parsed = amoru_kernel::arrow::ipc::root_as_footer(footer)
            .map_err(|e| bad(&format!("the template footer does not parse: {e}")))?;
        let vector = parsed
            .recordBatches()
            .ok_or_else(|| bad("the template footer has no record batches"))?;
        let raw = vector.bytes();
        if raw.len() != blocks.len() * BLOCK_BYTES {
            return Err(bad("the template footer has the wrong number of blocks"));
        }
        let base = footer.as_ptr() as usize;
        let at = raw.as_ptr() as usize;
        if at < base || at + raw.len() > base + footer.len() {
            return Err(bad("the block vector is outside the footer"));
        }
        at - base
    };
    for (i, (offset, meta, body)) in blocks.iter().enumerate() {
        let block = amoru_kernel::arrow::ipc::Block::new(*offset, *meta, *body);
        let at = start + i * BLOCK_BYTES;
        footer[at..at + BLOCK_BYTES].copy_from_slice(&block.0);
    }
    Ok(())
}

fn hex(run_id: &RunId) -> String {
    use std::fmt::Write as _;
    run_id.0.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

impl Sink for ArrowIpcSink {
    fn open(&mut self, schema: &SourceSchema) -> Result<()> {
        let SourceSchema::Table(schema) = schema else {
            return Err(AmoruError::Sink(
                "the arrow ipc sink accepts a table schema".into(),
            ));
        };
        let mut inner = self.lock();
        inner.phase.require_created()?;
        inner.schema = Some(Arc::clone(schema));
        self.open_file(&mut inner)?;
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
            let record = {
                let mut inner = self.lock();
                self.submit(&mut inner, seq, payload)?
            };
            let Record {
                file_id,
                completions,
                framing,
                extra,
                payload,
            } = record;
            let mut outcome = Ok(());
            for completion in completions {
                if let Err(e) = completion.await
                    && outcome.is_ok()
                {
                    outcome = Err(e);
                }
            }
            // Every view over the payload's buffers and over the framing has been read; the
            // arena gets both back here, which is what SI-I1 promises for this sink.
            drop(framing);
            drop(extra);
            drop(payload);
            if let Err(e) = outcome {
                self.fail(&e);
                let mut inner = self.lock();
                if let Some(file) = inner.files.get_mut(&file_id) {
                    file.inflight -= 1;
                }
                return Err(e);
            }
            let pending = self.record_done(file_id)?;
            let Some(pending) = pending else {
                return Ok(());
            };
            let Pending {
                entry,
                seqs,
                completion,
                footer,
            } = pending;
            let outcome = completion.await;
            drop(footer);
            match outcome {
                Ok(()) => self.commit(entry, &seqs),
                Err(e) => {
                    self.fail(&e);
                    Err(e)
                }
            }
        })
    }

    fn finish(&mut self) -> Result<SinkSummary> {
        let (failed, last) = {
            let mut inner = self.lock();
            match &inner.phase {
                Phase::Created => return Err(AmoruError::Sink("the sink is not open".into())),
                Phase::Finished => return Err(AmoruError::Sink("finish ran twice".into())),
                Phase::Failed(msg) => {
                    let msg = format!(
                        "{msg}; committed files: {}",
                        inner.ledger.names().join(", ")
                    );
                    inner.files.clear();
                    inner.current = None;
                    inner.phase = Phase::Finished;
                    (Some(msg), None)
                }
                Phase::Open => {
                    let Some(file_id) = inner.current else {
                        return Err(AmoruError::Sink("the sink has no open file".into()));
                    };
                    let pending = self.close_file(&mut inner, file_id)?;
                    (None, Some(pending))
                }
            }
        };
        if let Some(msg) = failed {
            let _ = self.dir.remove_tmps();
            return Err(AmoruError::Sink(msg));
        }
        let Some(pending) = last else {
            return Err(AmoruError::Sink("the sink has no open file".into()));
        };
        let Pending {
            entry,
            seqs,
            completion,
            footer,
        } = pending;
        let outcome = completion.wait();
        drop(footer);
        match outcome {
            Ok(()) => self.commit(entry, &seqs)?,
            Err(e) => {
                self.fail(&e);
                return Err(e);
            }
        }
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
                "the arrow ipc sink accepts a table schema".into(),
            ));
        };
        let mut inner = self.lock();
        inner.phase.require_created()?;
        let doc = Ledger::parse(KIND, state)?;
        let (ledger, above) = Ledger::restore(KIND, doc, committed_seq)?;
        let keep: BTreeSet<String> = ledger.committed().iter().map(|f| f.name.clone()).collect();
        let mut removed = 0u64;
        for name in self.dir.list()? {
            if keep.contains(&name) {
                continue;
            }
            if name.ends_with(TMP_SUFFIX) || name.starts_with("part-") {
                self.dir.remove(&name)?;
                removed += 1;
            }
        }
        for file in &above {
            self.dir.remove(&file.name)?;
        }
        inner.ledger = ledger;
        inner.stats.resumed_files_removed = removed;
        inner.schema = Some(Arc::clone(schema));
        self.open_file(&mut inner)?;
        inner.phase = Phase::Open;
        Ok(())
    }
}
