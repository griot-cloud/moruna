//! `VortexSink` (08 f.11, e.7; SO-O1's other half, built on 2026-09-29).
//!
//! The Vortex writer encodes (repartitions into row blocks, builds zone maps, compresses with
//! its cascading encodings) straight into one arena buffer per open file through the
//! `VortexWrite` implementation below, and the reactor writes the finished file from a view over
//! that buffer, exactly as `ParquetSink` does (f.1): the file buffers, the roll rule, the commit
//! and the checkpoint are the Parquet sink's. The encode is the one CPU copy G-I2 allows a sink.
//!
//! The Vortex writer is a future that is not `Send`, and a sink is shared between the threads
//! the scheduler's sink drive runs on, so the writer lives on one encoder thread per sink and
//! the sink talks to it over a channel (f.11). Every call waits for its answer, so the thread
//! adds no concurrency and holds nothing the sink does not know about.

use std::collections::BTreeSet;
use std::future::{Future, ready};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use moruna_kernel::arrow::datatypes::SchemaRef;
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{
    Allocator, BoxFuture, Buffer, Completion, MorunaError, Payload, PayloadKind, PayloadSpec,
    Reactor, Result, RunId, Seq, Sink, SinkSummary, SourceSchema, TierPref,
};
use vortex::VortexSessionDefault;
use vortex::arrow::ArrowSessionExt;
use vortex::file::{WriteOptionsSessionExt, Writer};
use vortex::io::runtime::BlockingRuntime;
use vortex::io::runtime::current::CurrentThreadRuntime;
use vortex::io::session::RuntimeSessionExt;
use vortex::io::{IoBuf, VortexWrite};
use vortex::session::VortexSession;

use crate::checkpoint::{CommittedFile, Ledger};
use crate::commit::{ObjectPrefix, TMP_SUFFIX};
use crate::file_buf::{FileBuf, FileBuffers, TOO_LARGE, differing_field, hex, rows_bytes};
use crate::stats::SinkStats;
use crate::{Phase, host_tier, part_name, require_host};

/// The kind this sink writes into its checkpoint (e.5).
const KIND: &str = "vortex";

/// The marker `finish` writes once every file is committed (e.2).
const SUCCESS: &str = "_SUCCESS";

/// The file extension, and the one `VortexSource` lists a prefix for.
const EXTENSION: &str = "vortex";

/// The metadata segment every file carries the run id in (e.7).
const RUN_ID_KEY: &str = "moruna.run_id";

/// Where a Vortex sink writes and how big its files are (d.1).
#[derive(Clone, Debug)]
pub struct VortexSinkConfig {
    /// Prefix; files `part-00000.vortex` onward are created under it. A `file://` URL or a bare
    /// path is a local directory, which is the only kind of destination `resume` can list.
    pub url: String,
    /// File roll size: an upper bound on a file, not a reservation (f.1).
    pub file_bytes: u64,
}

impl Default for VortexSinkConfig {
    /// `sink.file_bytes` of preamble section 5: 1 GiB.
    fn default() -> VortexSinkConfig {
        VortexSinkConfig {
            url: String::new(),
            file_bytes: 1 << 30,
        }
    }
}

/// The `VortexWrite` the Vortex writer encodes into: one arena buffer, no heap staging.
struct ArenaWrite {
    shared: Arc<Mutex<FileBuf>>,
}

impl VortexWrite for ArenaWrite {
    fn write_all<B: IoBuf>(
        &mut self,
        buffer: B,
    ) -> impl Future<Output = std::io::Result<B>> + Send {
        let mut file = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        let appended = file.append(buffer.as_slice());
        ready(appended.map(|()| buffer))
    }

    fn flush(&mut self) -> impl Future<Output = std::io::Result<()>> + Send {
        ready(Ok(()))
    }

    fn shutdown(&mut self) -> impl Future<Output = std::io::Result<()>> + Send {
        ready(Ok(()))
    }
}

/// A request to the encoder thread; each carries the channel its answer goes back on.
enum Command {
    /// Start a file of this schema over this buffer.
    Open {
        schema: SchemaRef,
        write: ArenaWrite,
        run_id: String,
        reply: SyncSender<Result<()>>,
    },
    /// Encode one batch into the open file.
    Push {
        batch: RecordBatch,
        reply: SyncSender<Result<()>>,
    },
    /// Flush the open file and write its footer.
    Close { reply: SyncSender<Result<()>> },
}

/// The thread that owns the Vortex writer (module comment).
struct Encoder {
    commands: Option<SyncSender<Command>>,
    thread: Option<JoinHandle<()>>,
}

impl Encoder {
    fn start() -> Result<Encoder> {
        let (commands, incoming) = sync_channel::<Command>(0);
        let thread = std::thread::Builder::new()
            .name("moruna-vortex-sink".into())
            .spawn(move || encode_loop(incoming))
            .map_err(|e| MorunaError::Sink(format!("the vortex encoder thread: {e}")))?;
        Ok(Encoder {
            commands: Some(commands),
            thread: Some(thread),
        })
    }

    /// Send a command and wait for its answer.
    fn call<T>(&self, command: impl FnOnce(SyncSender<Result<T>>) -> Command) -> Result<T> {
        let gone = || MorunaError::Sink("the vortex encoder thread has stopped".into());
        let Some(commands) = self.commands.as_ref() else {
            return Err(gone());
        };
        let (reply, answer) = sync_channel(1);
        commands.send(command(reply)).map_err(|_| gone())?;
        answer.recv().map_err(|_| gone())?
    }
}

impl Drop for Encoder {
    /// Close the channel and wait for the thread, so the file buffer it holds is back with the
    /// arena when the sink is gone.
    fn drop(&mut self) {
        drop(self.commands.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn encode_loop(incoming: Receiver<Command>) {
    let runtime = CurrentThreadRuntime::new();
    let session = VortexSession::default().with_handle(runtime.handle());
    let mut writer: Option<(Writer<'static>, SchemaRef)> = None;
    while let Ok(command) = incoming.recv() {
        match command {
            Command::Open {
                schema,
                write,
                run_id,
                reply,
            } => {
                let opened = session
                    .arrow()
                    .from_arrow_schema(&schema)
                    .map_err(|e| encode_error(&e.to_string()))
                    .map(|dtype| {
                        session
                            .write_options()
                            .with_metadata_segment(RUN_ID_KEY, run_id.into_bytes())
                            .writer(write, dtype)
                    });
                let _ = reply.send(opened.map(|w| {
                    writer = Some((w, schema));
                }));
            }
            Command::Push { batch, reply } => {
                let outcome = match writer.as_mut() {
                    None => Err(MorunaError::Sink("the sink has no open file".into())),
                    Some((w, schema)) => session
                        .arrow()
                        .from_arrow_record_batch(batch, schema)
                        .and_then(|array| runtime.block_on(w.push(array)))
                        .map_err(|e| encode_error(&e.to_string())),
                };
                let _ = reply.send(outcome);
            }
            Command::Close { reply } => {
                let outcome = match writer.take() {
                    None => Err(MorunaError::Sink("the sink has no open file".into())),
                    Some((w, _)) => runtime
                        .block_on(w.finish())
                        .map(|_| ())
                        .map_err(|e| encode_error(&e.to_string())),
                };
                let _ = reply.send(outcome);
            }
        }
    }
}

/// The file the sink is encoding into right now.
struct Current {
    name: String,
    shared: Arc<Mutex<FileBuf>>,
    seqs: Vec<Seq>,
    rows: u64,
    /// The Arrow bytes pushed into the file so far: the bound on what closing it writes, since
    /// the writer's compressed size is known only at close (f.11).
    pushed: u64,
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
    /// The schema the sink was opened with, used when the run writes nothing (f.1).
    declared: Option<SchemaRef>,
    /// The output schema, adopted from the first payload that arrives (f.1).
    schema: Option<SchemaRef>,
    current: Option<Current>,
    ledger: Ledger,
    stats: SinkStats,
    /// The byte count the open file rolls at (f.1).
    roll_bytes: u64,
}

/// A sink that writes Arrow batches as Vortex files under a prefix (d.1, f.11).
pub struct VortexSink {
    cfg: VortexSinkConfig,
    dest: ObjectPrefix,
    reactor: Arc<dyn Reactor>,
    alloc: Arc<dyn Allocator>,
    run_id: RunId,
    inner: Mutex<Inner>,
    buffers: FileBuffers,
    encoder: Encoder,
}

impl VortexSink {
    /// Build a sink over `cfg`. The reactor writes the files; the allocator provides the
    /// encoder's output buffer, one per open file.
    pub fn new(
        cfg: VortexSinkConfig,
        reactor: Arc<dyn Reactor>,
        alloc: Arc<dyn Allocator>,
    ) -> Result<VortexSink> {
        let dest = ObjectPrefix::parse(&cfg.url)?;
        let buffers = FileBuffers::new(Arc::clone(&alloc));
        Ok(VortexSink {
            cfg,
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
            buffers,
            encoder: Encoder::start()?,
        })
    }

    /// Name the run whose id goes into every file's `moruna.run_id` metadata segment (e.7).
    pub fn with_run_id(mut self, run_id: RunId) -> VortexSink {
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

    /// Allocate the next file's buffer, start the writer over it, and settle the byte count
    /// the file will roll at (f.1): `min_bytes` is 0 for an ordinary file and twice a single
    /// oversized morsel's bytes when one is on its way in (h).
    fn open_file(&self, inner: &mut Inner, min_bytes: u64) -> Result<()> {
        let index = inner.ledger.take_index();
        self.open_named(inner, min_bytes, part_name(index, EXTENSION))
    }

    /// Give back the open file, which holds nothing yet, and open it again under the same name
    /// with a buffer of at least `min_bytes` (h).
    fn reopen_larger(&self, inner: &mut Inner, min_bytes: u64) -> Result<()> {
        let Some(current) = inner.current.take() else {
            return Err(MorunaError::Sink("the sink has no open file".into()));
        };
        self.encoder.call(|reply| Command::Close { reply })?;
        let buf = current
            .shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .buf
            .take();
        if let Some(buf) = buf {
            self.buffers.keep_spare(Arc::new(buf));
        }
        self.open_named(inner, min_bytes, current.name)
    }

    fn open_named(&self, inner: &mut Inner, min_bytes: u64, name: String) -> Result<()> {
        let Some(schema) = inner.schema.clone() else {
            return Err(MorunaError::Sink("the sink has no schema".into()));
        };
        // Vortex has no row group: the writer cuts its own row blocks (e.7), so the file buffer
        // is floored by the granule and the morsel in hand only.
        let (buf, roll_bytes) = self
            .buffers
            .alloc_file_buf(min_bytes, 0, self.cfg.file_bytes)?;
        inner.roll_bytes = roll_bytes;
        inner.stats.roll_bytes = roll_bytes;
        let shared = Arc::new(Mutex::new(FileBuf {
            buf: Some(buf),
            used: 0,
        }));
        let write = ArenaWrite {
            shared: Arc::clone(&shared),
        };
        let run_id = hex(&self.run_id);
        self.encoder.call(|reply| Command::Open {
            schema,
            write,
            run_id,
            reply,
        })?;
        inner.current = Some(Current {
            name,
            shared,
            seqs: Vec::new(),
            rows: 0,
            pushed: 0,
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
            shared,
            seqs,
            rows,
            ..
        } = current;
        let seq_min = seqs.iter().min().copied().unwrap_or(0);
        let seq_max = seqs.iter().max().copied().unwrap_or(0);
        self.encoder.call(|reply| Command::Close { reply })?;
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
        tracing::info!(target: "sink.roll", file = %name, bytes = used, "rolled a vortex file");
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
        tracing::info!(target: "sink.commit", file = %entry.name, "committed a vortex file");
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
    /// it. Returns the rolled file, if there was one, to await outside.
    fn encode(&self, inner: &mut Inner, seq: Seq, payload: Payload) -> Result<Option<Pending>> {
        inner.phase.require_open()?;
        require_host(&payload)?;
        let rows = payload.rows();
        let Payload::Table(batch, _) = payload else {
            return Err(MorunaError::Sink(
                "the vortex sink accepts a table payload".into(),
            ));
        };
        let bytes = rows_bytes(&batch);
        let first = inner.schema.is_none();
        if first {
            inner.schema = Some(batch.schema());
        } else {
            let Some(schema) = inner.schema.as_ref() else {
                return Err(MorunaError::Sink("the sink has no schema".into()));
            };
            let found = batch.schema();
            if schema.fields() != found.fields() {
                return Err(MorunaError::Sink(differing_field(schema, &found)));
            }
        }
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
            Some(current) => (current.pushed, !current.seqs.is_empty()),
            None => return Err(MorunaError::Sink("the sink has no open file".into())),
        };
        if !started && bytes > inner.roll_bytes {
            // Nothing is in the open file yet, so it is given back and reopened under the same
            // name with a buffer sized for this morsel (h).
            self.reopen_larger(inner, bytes.saturating_mul(2))?;
        }
        let pending = if started && so_far + bytes > inner.roll_bytes {
            let need = if bytes > inner.roll_bytes {
                bytes.saturating_mul(2)
            } else {
                0
            };
            Some(self.roll(inner, need, true)?)
        } else {
            None
        };
        self.encoder.call(|reply| Command::Push { batch, reply })?;
        let Some(current) = inner.current.as_mut() else {
            return Err(MorunaError::Sink("the sink has no open file".into()));
        };
        current.pushed += bytes;
        current.rows += rows;
        current.seqs.push(seq);
        inner.stats.writes += 1;
        inner.stats.encode_bytes += bytes;
        tracing::trace!(target: "sink.encode", seq, bytes, "encoded a morsel");
        // The encode is the symmetric exception to G-I2 and the arena is where it is counted.
        self.alloc.note_payload_copy(bytes);
        Ok(pending)
    }

    /// An empty object under the prefix, which is the marker downstream readers look for.
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
        MorunaError::Sink(format!("vortex encode: {msg}"))
    }
}

impl Sink for VortexSink {
    fn open(&mut self, schema: &SourceSchema) -> Result<()> {
        let SourceSchema::Table(schema) = schema else {
            return Err(MorunaError::Sink(
                "the vortex sink accepts a table schema".into(),
            ));
        };
        let mut inner = self.lock();
        inner.phase.require_created()?;
        inner.declared = Some(Arc::clone(schema));
        // The file buffers, now, while the arena holds nothing else.
        self.buffers.alloc_pair(self.cfg.file_bytes)?;
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
            let Some(Pending {
                entry,
                seqs,
                completion,
                buf,
            }) = pending
            else {
                return Ok(());
            };
            let outcome = completion.await;
            self.buffers.keep_spare(buf);
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
            self.clear_tmps();
            return Err(MorunaError::Sink(msg));
        }
        let Pending {
            entry,
            seqs,
            completion,
            buf,
        } = {
            let mut inner = self.lock();
            // A run that wrote nothing never adopted a schema, so the one `open` was given is
            // what the empty file carries (h).
            if inner.current.is_none() {
                inner.schema = inner.declared.clone();
                self.open_file(&mut inner, 0)?;
            }
            self.roll(&mut inner, 0, false)?
        };
        let outcome = completion.wait();
        self.buffers.keep_spare(buf);
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
            snapshot: None,
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
                "the vortex sink accepts a table schema".into(),
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
        inner.declared = Some(Arc::clone(schema));
        inner.phase = Phase::Open;
        Ok(())
    }
}
