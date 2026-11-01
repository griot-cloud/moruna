//! Required historical logical rows, bridged into the sole governed artifact publisher.
//! Kernels enqueue exact boundary batches; the bounded IO worker writes complete Arrow streams.
//! Logs, spill, final fused output and source re-execution are never historical substitutes.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use moruna_kernel::arrow::{ipc::writer::StreamWriter, record_batch::RecordBatch};
use moruna_kernel::{
    Allocator, CancelToken, Kernel, LogicalRows, LogicalTaskRows, MorunaError, SourceSchema, Tier,
};
use moruna_storage::{
    Access, Governed, Metadata, Object,
    artifact::{Boundary, Capture, Manifest, Request},
};

/// One orchestrator-authorized logical output. Credentials remain shared, never copied or logged.
#[derive(Clone)]
pub struct Output {
    /// Explicit identity, policy and artifact-byte reservation.
    pub request: Request,
    /// Governed manifest destination.
    pub target: Object,
    /// Per-run live authority, checked by storage on every operation.
    pub access: Arc<Access>,
}

/// Explicit required-capture plan; no capture is inferred from any other runtime feature.
pub struct Historical {
    /// The sole governed storage authority, also used by historical readers.
    pub store: Arc<Governed>,
    /// Requested outputs, with separate identities and byte reservations.
    pub outputs: Vec<Output>,
    /// Every executable stage's logical tasks in fused order, including unrequested members.
    pub bindings: Vec<Vec<LogicalTaskRows>>,
    /// Maximum retained input bytes waiting for publication.
    pub queue_bytes: u64,
    /// Maximum queued batches, independently bounding empty-batch/metadata overhead.
    pub queue_batches: usize,
    /// Charged maximum capacity of one self-contained Arrow stream.
    pub encoding_bytes: usize,
    /// Charged manifest serialization limit.
    pub metadata_bytes: usize,
    /// Upper bound on a single storage operation.
    pub operation_timeout: Duration,
    /// Upper bound on initial publication and final drain/refusal.
    pub drain_timeout: Duration,
    facts: Mutex<Vec<Publication>>,
    claimed: AtomicBool,
}

/// The actual publication fact; only a successful completion supplies a complete version.
#[derive(Clone, Debug)]
pub struct Publication {
    /// The artifact publisher's own state and exact admitted chunk/row/byte counts.
    pub manifest: Manifest,
    /// Its immutable completed manifest version, absent on refusal/incompletion.
    pub complete: Option<Metadata>,
    /// Last successfully persisted manifest pointer, including incomplete/refused states.
    pub latest: Option<Metadata>,
}

fn failure(reason: impl Into<String>) -> MorunaError {
    MorunaError::Io {
        op: "historical rows",
        target: "required logical outputs".into(),
        msg: reason.into(),
    }
}

struct Shared {
    queued: AtomicU64,
    error: Mutex<Option<String>>,
    finishing: Mutex<Option<bool>>,
    deadline: Mutex<Option<Instant>>,
    cancel: CancelToken,
}
impl Shared {
    fn fail(&self, reason: String) {
        self.error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert(reason);
        self.cancel.cancel();
    }
    fn timeout(&self, limit: Duration) -> Duration {
        self.deadline
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map_or(limit, |at| {
                limit.min(at.saturating_duration_since(Instant::now()))
            })
    }
}
struct Charge {
    bytes: u64,
    shared: Arc<Shared>,
}
impl Drop for Charge {
    fn drop(&mut self) {
        self.shared.queued.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}
struct Batch {
    index: usize,
    rows: RecordBatch,
    _charge: Charge,
}
enum Message {
    Batch(Batch),
    Snapshot {
        sequence: u64,
        reply: mpsc::SyncSender<Result<Vec<Metadata>, String>>,
    },
}
/// FIFO capture-prefix snapshot, invoked only while the actual producers are quiescent.
#[derive(Clone)]
pub(crate) struct Checkpoint {
    tx: mpsc::SyncSender<Message>,
    shared: Arc<Shared>,
    timeout: Duration,
    metadata_bytes: usize,
}
impl Checkpoint {
    pub(crate) fn preparation(
        &self,
        alloc: Arc<dyn Allocator>,
    ) -> moruna_scheduler::ManifestPreparation {
        let checkpoint = self.clone();
        Arc::new(move || {
            // Admission happens before quiescence. No allocator fallback while paused.
            let buffer = alloc.alloc(checkpoint.metadata_bytes, Tier::Host)?;
            let buffer = Mutex::new(Some(buffer));
            let checkpoint = checkpoint.clone();
            Ok(Arc::new(move |path: &std::path::Path| {
                use std::io::{Read, Write};
                let mut buffer = buffer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                    .ok_or_else(|| failure("capture checkpoint admission consumed"))?;
                let mut input =
                    std::fs::File::open(path).map_err(|error| failure(error.to_string()))?;
                let size = usize::try_from(
                    input
                        .metadata()
                        .map_err(|error| failure(error.to_string()))?
                        .len(),
                )
                .map_err(|_| failure("capture checkpoint metadata length"))?;
                if size > buffer.len() {
                    return Err(failure("capture checkpoint metadata ceiling"));
                }
                input
                    .read_exact(&mut buffer.as_mut()[..size])
                    .map_err(|error| failure(error.to_string()))?;
                let mut value: serde_json::Value = serde_json::from_slice(&buffer.as_ref()[..size])
                    .map_err(|_| failure("capture checkpoint manifest"))?;
                let sequence = value
                    .get("written_ns")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or_else(|| failure("capture checkpoint sequence"))?;
                let prefixes = checkpoint.snapshot(sequence)?;
                value
                    .as_object_mut()
                    .ok_or_else(|| failure("capture checkpoint object"))?
                    .insert(
                        crate::portable::HISTORY_FIELD.into(),
                        serde_json::to_value(prefixes)
                            .map_err(|_| failure("capture prefix metadata"))?,
                    );
                let mut cursor = std::io::Cursor::new(buffer.as_mut());
                serde_json::to_writer(&mut cursor, &value)
                    .map_err(|_| failure("capture checkpoint encoded metadata ceiling"))?;
                let used = cursor.position() as usize;
                let temporary = path.with_extension("history-pending");
                let mut output = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temporary)
                    .map_err(|error| failure(error.to_string()))?;
                let written = (|| {
                    output.write_all(&buffer.as_ref()[..used])?;
                    output.sync_all()?;
                    std::fs::rename(&temporary, path)?;
                    std::fs::File::open(
                        path.parent()
                            .ok_or_else(|| std::io::Error::other("checkpoint parent"))?,
                    )?
                    .sync_all()
                })();
                if written.is_err() {
                    let _ = std::fs::remove_file(&temporary);
                }
                written.map_err(|error| failure(error.to_string()))
            }) as moruna_scheduler::ManifestHook)
        })
    }
    pub(crate) fn snapshot(&self, sequence: u64) -> moruna_kernel::Result<Vec<Metadata>> {
        let (reply, result) = mpsc::sync_channel(1);
        let deadline = Instant::now() + self.timeout;
        let mut command = Message::Snapshot { sequence, reply };
        loop {
            match self.tx.try_send(command) {
                Ok(()) => break,
                Err(mpsc::TrySendError::Full(returned)) if Instant::now() < deadline => {
                    command = returned;
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(_) => {
                    return Err(failure(
                        "checkpoint history queue unavailable or drain deadline elapsed",
                    ));
                }
            }
        }
        result
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| {
                self.shared
                    .fail("checkpoint history drain/snapshot deadline elapsed".into());
                failure("checkpoint history drain/snapshot deadline elapsed")
            })?
            .map_err(failure)
    }
}
struct Tap {
    tx: mpsc::SyncSender<Message>,
    identities: HashMap<(String, String), usize>,
    shared: Arc<Shared>,
    budget: u64,
}
impl LogicalRows for Tap {
    fn append(&self, task: &str, port: &str, batch: &RecordBatch) -> moruna_kernel::Result<()> {
        if self
            .shared
            .finishing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
        {
            return Err(failure("capture has ended"));
        }
        if let Some(reason) = self
            .shared
            .error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Err(failure(reason));
        }
        let index = *self
            .identities
            .get(&(task.to_owned(), port.to_owned()))
            .ok_or_else(|| failure("unrequested logical output boundary"))?;
        let bytes = batch.get_array_memory_size() as u64;
        if self
            .shared
            .queued
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                held.checked_add(bytes).filter(|next| *next <= self.budget)
            })
            .is_err()
        {
            let error = moruna_storage::Error::CaptureCapacity {
                need: bytes,
                budget: self.budget,
            };
            self.shared.fail(error.to_string());
            return Err(failure(error.to_string()));
        }
        let message = Batch {
            index,
            rows: batch.clone(),
            _charge: Charge {
                bytes,
                shared: self.shared.clone(),
            },
        };
        self.tx.try_send(Message::Batch(message)).map_err(|_| {
            self.shared
                .fail("required historical row queue is full or unavailable".into());
            failure("required historical row queue is full or unavailable")
        })
    }
}

// A single logical task's returned payload is its exact boundary. This wrapper must never
// represent multiple fused tasks: those require the executable's native intermediate taps.
struct FinalRows {
    kernel: Arc<dyn Kernel>,
    task: LogicalTaskRows,
    rows: Arc<dyn LogicalRows>,
}
impl FinalRows {
    fn table_schema(schema: SourceSchema) -> moruna_kernel::Result<SourceSchema> {
        if matches!(&schema, SourceSchema::Table(_)) {
            Ok(schema)
        } else {
            Err(failure("required logical output has non-table schema"))
        }
    }
    fn capture(
        &self,
        output: moruna_kernel::Payload,
    ) -> moruna_kernel::Result<moruna_kernel::Payload> {
        let moruna_kernel::Payload::Table(batch, _) = &output else {
            return Err(failure(
                "required logical output returned non-table payload",
            ));
        };
        for port in &self.task.ports {
            self.rows.append(&self.task.task, port, batch)?;
        }
        Ok(output)
    }
}
impl Kernel for FinalRows {
    fn fingerprint(&self) -> moruna_kernel::Fingerprint {
        self.kernel.fingerprint()
    }
    fn kind(&self) -> moruna_kernel::KernelKind {
        self.kernel.kind()
    }
    fn hints(&self) -> moruna_kernel::KernelHints {
        self.kernel.hints()
    }
    fn declared(&self) -> moruna_kernel::Declared {
        self.kernel.declared()
    }
    fn accepts(&self) -> moruna_kernel::PayloadSpec {
        self.kernel.accepts()
    }
    fn output_schema(&self, input: &SourceSchema) -> moruna_kernel::Result<SourceSchema> {
        Self::table_schema(self.kernel.output_schema(input)?)
    }
    fn output_schema_of(&self, inputs: &[SourceSchema]) -> moruna_kernel::Result<SourceSchema> {
        Self::table_schema(self.kernel.output_schema_of(inputs)?)
    }
    fn init(
        &self,
        ctx: &moruna_kernel::InitCtx,
    ) -> moruna_kernel::Result<Box<dyn moruna_kernel::KernelState>> {
        self.kernel.init(ctx)
    }
    fn restore(
        &self,
        ctx: &moruna_kernel::InitCtx,
        state: &[u8],
    ) -> moruna_kernel::Result<Box<dyn moruna_kernel::KernelState>> {
        self.kernel.restore(ctx, state)
    }
    fn apply(
        &self,
        state: &mut dyn moruna_kernel::KernelState,
        input: moruna_kernel::Payload,
    ) -> moruna_kernel::Result<moruna_kernel::Payload> {
        self.capture(self.kernel.apply(state, input)?)
    }
    fn apply_port(
        &self,
        state: &mut dyn moruna_kernel::KernelState,
        port: u16,
        input: moruna_kernel::Payload,
    ) -> moruna_kernel::Result<moruna_kernel::Payload> {
        self.capture(self.kernel.apply_port(state, port, input)?)
    }
    fn finish(&self, state: &mut dyn moruna_kernel::KernelState) -> moruna_kernel::Result<()> {
        self.kernel.finish(state)
    }
    fn max_inputs(&self) -> u16 {
        self.kernel.max_inputs()
    }
    fn waiting_state(&self, instance: usize) -> Option<Vec<u8>> {
        self.kernel.waiting_state(instance)
    }
}

impl Historical {
    /// Create an explicit plan. No authority or budget is supplied by a default.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<Governed>,
        outputs: Vec<Output>,
        bindings: Vec<Vec<LogicalTaskRows>>,
        queue_bytes: u64,
        queue_batches: usize,
        encoding_bytes: usize,
        metadata_bytes: usize,
        operation_timeout: Duration,
        drain_timeout: Duration,
    ) -> moruna_kernel::Result<Arc<Self>> {
        if outputs.is_empty()
            || outputs.iter().any(|output| !output.request.required)
            || queue_bytes == 0
            || queue_batches == 0
            || encoding_bytes == 0
            || metadata_bytes == 0
            || operation_timeout.is_zero()
            || drain_timeout.is_zero()
        {
            return Err(failure(
                "required capture needs explicit outputs and positive queue, encoding, metadata and time budgets",
            ));
        }
        Ok(Arc::new(Self {
            store,
            outputs,
            bindings,
            queue_bytes,
            queue_batches,
            encoding_bytes,
            metadata_bytes,
            operation_timeout,
            drain_timeout,
            facts: Mutex::new(Vec::new()),
            claimed: AtomicBool::new(false),
        }))
    }
    /// Resolve requested ports against the checked job and executable plan, without changing
    /// fusion. Source/sink/opaque boundaries that cannot expose logical rows are refused.
    pub fn bindings_for_job(
        job: &moruna_job::Job,
        parts: &crate::job::Parts,
        outputs: &[Output],
    ) -> moruna_kernel::Result<Vec<Vec<LogicalTaskRows>>> {
        for output in outputs {
            let id = &output.request.identity;
            let task = job
                .tasks
                .iter()
                .find(|task| task.id == id.task)
                .ok_or_else(|| failure("historical request names an absent logical task"))?;
            if task.role != moruna_job::Role::Kernel || !task.outputs.contains(&id.port) {
                return Err(failure(
                    "requested logical output has no supported executable row boundary",
                ));
            }
        }
        parts
            .stages
            .iter()
            .map(|members| {
                members
                    .iter()
                    .map(|&at| {
                        let task = job
                            .tasks
                            .get(at)
                            .ok_or_else(|| failure("plan names an absent logical task"))?;
                        Ok(LogicalTaskRows {
                            task: task.id.clone(),
                            ports: task
                                .outputs
                                .iter()
                                .filter(|port| {
                                    outputs.iter().any(|output| {
                                        output.request.identity.task == task.id
                                            && output.request.identity.port == **port
                                    })
                                })
                                .cloned()
                                .collect(),
                        })
                    })
                    .collect()
            })
            .collect()
    }

    /// Current facts from the sole publisher, including honest partial/refused state.
    pub fn publications(&self) -> Vec<Publication> {
        self.facts.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    /// Prepare every real logical boundary and publish incomplete manifests before execution.
    pub(crate) fn start(
        self: &Arc<Self>,
        kernels: &mut [Arc<dyn Kernel>],
        alloc: Arc<dyn Allocator>,
        cancel: CancelToken,
        run_id: &str,
        resume: Option<&std::path::Path>,
    ) -> moruna_kernel::Result<HistoryRun> {
        let prefixes = resume
            .map(|path| checkpoint_prefixes(path, self.metadata_bytes))
            .transpose()?;
        if prefixes.as_ref().is_some_and(|prefixes| {
            prefixes
                .as_ref()
                .is_none_or(|prefixes| prefixes.len() != self.outputs.len())
        }) {
            return Err(failure(
                "required capture resume needs every exact checkpoint prefix",
            ));
        }
        let prefixes = prefixes.flatten();
        if kernels.len() != self.bindings.len() || self.claimed.swap(true, Ordering::AcqRel) {
            return Err(failure(
                "capture bindings must match executable stages and a fresh attempt",
            ));
        }
        if self
            .outputs
            .iter()
            .any(|output| output.request.identity.run != run_id || output.access.run != run_id)
        {
            return Err(failure(
                "historical request/authority must identify this actual run",
            ));
        }
        let mut identities = HashMap::new();
        for (index, output) in self.outputs.iter().enumerate() {
            let id = &output.request.identity;
            if identities
                .insert((id.task.clone(), id.port.clone()), index)
                .is_some()
            {
                return Err(failure("logical output requested twice"));
            }
        }
        let mut found = vec![false; self.outputs.len()];
        for stage in &self.bindings {
            for task in stage {
                for port in &task.ports {
                    let index = *identities
                        .get(&(task.task.clone(), port.clone()))
                        .ok_or_else(|| failure("logical binding has no authorized request"))?;
                    if found[index] {
                        return Err(failure("logical boundary bound twice"));
                    }
                    found[index] = true;
                }
            }
        }
        if found.contains(&false) {
            return Err(failure(
                "a requested logical output has no executable boundary",
            ));
        }
        let shared = Arc::new(Shared {
            queued: AtomicU64::new(0),
            error: Mutex::new(None),
            finishing: Mutex::new(None),
            deadline: Mutex::new(Some(Instant::now() + self.drain_timeout)),
            cancel,
        });
        let (tx, rx) = mpsc::sync_channel(self.queue_batches);
        let checkpoint = Checkpoint {
            tx: tx.clone(),
            shared: shared.clone(),
            timeout: self.drain_timeout,
            metadata_bytes: self.metadata_bytes,
        };
        let tap: Arc<dyn LogicalRows> = Arc::new(Tap {
            tx,
            identities,
            shared: shared.clone(),
            budget: self.queue_bytes,
        });
        // Opaque capture refuses before any publisher thread, kernel init or executable launch.
        let mut bound = Vec::with_capacity(kernels.len());
        for (kernel, tasks) in kernels.iter().zip(&self.bindings) {
            if tasks.iter().any(|task| !task.ports.is_empty()) {
                if let [task] = tasks.as_slice() {
                    if task.ports.len() != 1 {
                        return Err(failure(
                            "single-payload kernel cannot expose multiple distinct output ports",
                        ));
                    }
                    bound.push(Arc::new(FinalRows {
                        kernel: kernel.clone(),
                        task: task.clone(),
                        rows: tap.clone(),
                    }) as Arc<dyn Kernel>);
                } else {
                    bound.push(kernel.with_logical_rows(tasks, tap.clone())?);
                }
            } else {
                bound.push(kernel.clone());
            }
        }
        // Reserve real charged encoding capacity before publisher preparation/launch.
        // The queue retains already-owned Arrow buffers under its separate explicit pin cap.
        let encoding = alloc.alloc(self.encoding_bytes, Tier::Host)?;
        let mut captures = Vec::with_capacity(self.outputs.len());
        for output in &self.outputs {
            captures.push(
                Capture::prepare(
                    output.request.clone(),
                    Boundary::LogicalRows,
                    output.target.clone(),
                    self.store.clone(),
                    alloc.clone(),
                    self.metadata_bytes,
                )
                .map_err(|e| failure(e.to_string()))?,
            );
        }
        let (ready_tx, ready_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let plan = self.clone();
        let state = shared.clone();
        let thread = std::thread::Builder::new()
            .name("historical-rows".into())
            .spawn(move || {
                let result = worker(
                    &plan,
                    captures,
                    prefixes,
                    rx,
                    alloc,
                    encoding,
                    (&state, ready_tx),
                );
                if let Err(reason) = &result {
                    state.fail(reason.clone());
                }
                let _ = done_tx.send(result);
            })
            .map_err(|e| failure(format!("cannot start capture IO worker: {e}")))?;
        let run = HistoryRun {
            checkpoint,
            shared,
            done: done_rx,
            thread: Some(thread),
            timeout: self.drain_timeout,
            finished: false,
        };
        match ready_rx.recv_timeout(self.drain_timeout) {
            Ok(Ok(())) => {}
            Ok(Err(reason)) => {
                run.shared.fail(reason.clone());
                return Err(failure(reason));
            }
            Err(_) => {
                run.shared
                    .fail("initial historical publication timed out".into());
                return Err(failure("initial historical publication timed out"));
            }
        }
        run.shared
            .deadline
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        kernels.clone_from_slice(&bound);
        Ok(run)
    }
}

/// Read only a bounded native manifest extension; storage verifies every prefix and chunk
/// before continuation. Absence is distinct from an empty valid list.
pub(crate) fn checkpoint_prefixes(
    path: &std::path::Path,
    limit: usize,
) -> moruna_kernel::Result<Option<Vec<Metadata>>> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|error| failure(error.to_string()))?;
    let mut bytes = Vec::new();
    file.take((limit as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| failure(error.to_string()))?;
    if bytes.len() > limit {
        return Err(failure("capture checkpoint metadata ceiling"));
    }
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| failure("capture checkpoint manifest"))?;
    value
        .get(crate::portable::HISTORY_FIELD)
        .cloned()
        .map(|value| {
            serde_json::from_value(value)
                .map_err(|_| failure("capture checkpoint prefixes malformed"))
        })
        .transpose()
}

fn io<T>(
    runtime: &tokio::runtime::Runtime,
    shared: &Shared,
    timeout: Duration,
    future: impl core::future::Future<Output = moruna_storage::Result<T>>,
) -> Result<T, String> {
    runtime
        .block_on(async { tokio::time::timeout(shared.timeout(timeout), future).await })
        .map_err(|_| "historical storage operation timed out".to_owned())?
        .map_err(|error| error.to_string())
}
fn worker(
    plan: &Historical,
    mut captures: Vec<Capture>,
    prefixes: Option<Vec<Metadata>>,
    rx: mpsc::Receiver<Message>,
    alloc: Arc<dyn Allocator>,
    encoding: moruna_kernel::Buffer,
    startup: (&Shared, mpsc::Sender<Result<(), String>>),
) -> Result<(), String> {
    let (shared, ready) = startup;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let begun = captures
        .iter_mut()
        .zip(&plan.outputs)
        .enumerate()
        .try_for_each(|(index, (capture, output))| match &prefixes {
            Some(prefixes) => io(
                &runtime,
                shared,
                plan.operation_timeout,
                capture.resume(&output.access, &prefixes[index]),
            ),
            None => io(
                &runtime,
                shared,
                plan.operation_timeout,
                capture.begin(&output.access),
            ),
        });
    update_facts(plan, &captures, &[]);
    let _ = ready.send(begun.clone());
    let mut error = begun.err();
    let mut encoding = Arc::new(encoding);
    while error.is_none() {
        if let Some(reason) = shared
            .error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            error = Some(reason);
            break;
        }
        if *shared.finishing.lock().unwrap_or_else(|e| e.into_inner()) == Some(false) {
            error = Some("run or required logical output coverage did not complete".into());
            break;
        }
        match rx.recv_timeout(Duration::from_millis(10)) {
            Ok(Message::Snapshot { sequence, reply }) => {
                let result = captures
                    .iter_mut()
                    .zip(&plan.outputs)
                    .enumerate()
                    .map(|(index, (capture, output))| {
                        let mut target = output.target.clone();
                        target.key = format!("{}.checkpoints/{sequence}/{index}", target.key);
                        io(
                            &runtime,
                            shared,
                            plan.operation_timeout,
                            capture.snapshot(&output.access, &target),
                        )
                    })
                    .collect::<Result<Vec<_>, _>>();
                if let Err(reason) = &result {
                    shared.fail(reason.clone());
                    error = Some(reason.clone());
                }
                let _ = reply.send(result);
            }
            Ok(Message::Batch(message)) => {
                let output = &plan.outputs[message.index];
                let capture = &mut captures[message.index];
                if Arc::get_mut(&mut encoding).is_none() {
                    // A backend retaining an immutable view cannot be overwritten. Any replacement
                    // must obtain new charged capacity; no uncharged heap fallback exists.
                    encoding = match alloc.alloc(plan.encoding_bytes, Tier::Host) {
                        Ok(buffer) => Arc::new(buffer),
                        Err(reason) => {
                            shared.fail(reason.to_string());
                            error = Some(reason.to_string());
                            break;
                        }
                    };
                }
                let encoded = encode_into(
                    &message.rows,
                    Arc::get_mut(&mut encoding).expect("exclusive encoding buffer"),
                );
                let appended = encoded.and_then(|used| {
                    let schema = SourceSchema::Table(message.rows.schema())
                        .hash()
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>();
                    io(
                        &runtime,
                        shared,
                        plan.operation_timeout,
                        capture.append(
                            &output.access,
                            schema,
                            message.rows.num_rows() as u64,
                            encoding.view().slice(0, used),
                        ),
                    )
                });
                if let Err(reason) = appended {
                    shared.fail(reason.clone());
                    error = Some(reason);
                }
                update_facts(plan, &captures, &[]);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if shared
                    .finishing
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_some()
                {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                error = Some("capture producer disappeared".into());
            }
        }
    }
    // A failed finish may arrive while recv_timeout is asleep. Its timeout branch
    // only observes that finishing was requested; it is not a completion grant.
    // Require the actual positive disposition before publishing any complete fact.
    if error.is_none() && *shared.finishing.lock().unwrap_or_else(|e| e.into_inner()) != Some(true)
    {
        error = Some("run or required logical output coverage did not complete".into());
    }
    let mut complete = Vec::with_capacity(captures.len());
    for (capture, output) in captures.iter_mut().zip(&plan.outputs) {
        if error.is_none() {
            match io(
                &runtime,
                shared,
                plan.operation_timeout,
                capture.complete(&output.access),
            ) {
                Ok(metadata) => complete.push(Some(metadata)),
                Err(reason) => {
                    error = Some(reason);
                    complete.push(None);
                }
            }
        } else {
            let _ = io(
                &runtime,
                shared,
                plan.operation_timeout,
                capture.refuse(&output.access, "required capture or run did not complete"),
            );
            complete.push(None);
        }
    }
    update_facts(plan, &captures, &complete);
    error.map_or(Ok(()), Err)
}
fn update_facts(plan: &Historical, captures: &[Capture], complete: &[Option<Metadata>]) {
    *plan.facts.lock().unwrap_or_else(|e| e.into_inner()) = captures
        .iter()
        .enumerate()
        .map(|(index, capture)| Publication {
            manifest: capture.manifest().clone(),
            complete: complete.get(index).cloned().flatten(),
            latest: capture.current_metadata().cloned(),
        })
        .collect();
}
fn encode_into(batch: &RecordBatch, buffer: &mut moruna_kernel::Buffer) -> Result<usize, String> {
    let mut cursor = std::io::Cursor::new(buffer.as_mut());
    {
        let mut writer = StreamWriter::try_new(&mut cursor, batch.schema().as_ref())
            .map_err(|_| "artifact schema exceeds encoding budget")?;
        writer
            .write(batch)
            .map_err(|_| "artifact batch exceeds encoding budget")?;
        writer
            .finish()
            .map_err(|_| "artifact stream completion exceeds encoding budget")?;
    }
    Ok(cursor.position() as usize)
}

#[cfg(test)]
fn encode(
    batch: &RecordBatch,
    capacity: usize,
    alloc: &dyn Allocator,
) -> Result<moruna_kernel::Buffer, String> {
    let mut buffer = alloc
        .alloc(capacity, Tier::Host)
        .map_err(|error| error.to_string())?;
    let len = encode_into(batch, &mut buffer)?;
    let (used, remainder) = buffer.split_at(len);
    drop(remainder);
    Ok(used)
}

/// Run-owned finalizer; errors/cancellation never label a partial output complete.
pub(crate) struct HistoryRun {
    checkpoint: Checkpoint,
    shared: Arc<Shared>,
    done: mpsc::Receiver<Result<(), String>>,
    thread: Option<JoinHandle<()>>,
    timeout: Duration,
    finished: bool,
}
impl HistoryRun {
    pub(crate) fn checkpoint(&self) -> Checkpoint {
        self.checkpoint.clone()
    }
    pub(crate) fn finish(mut self, succeeded: bool) -> moruna_kernel::Result<()> {
        self.end(succeeded)
    }
    fn end(&mut self, succeeded: bool) -> moruna_kernel::Result<()> {
        *self
            .shared
            .deadline
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Instant::now() + self.timeout);
        *self
            .shared
            .finishing
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(succeeded);
        self.finished = true;
        let result = self.done.recv_timeout(self.timeout).map_err(|_| {
            failure("historical publication did not drain within its explicit budget")
        })?;
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| failure("historical publisher panicked"))?;
        }
        result.map_err(failure)
    }
}
impl Drop for HistoryRun {
    fn drop(&mut self) {
        if !self.finished {
            self.shared.cancel.cancel();
            let _ = self.end(false);
        }
    }
}

/// Authorized native historical read port shared by CLI/SDK/MCP adapters.
/// It delegates all version, hash, scope and encoding checks to the sole storage reader.
pub struct ReadPort {
    reader: moruna_storage::artifact::Reader,
    access: Arc<Access>,
    alloc: Arc<dyn Allocator>,
}
impl ReadPort {
    /// Explicit live governed authority and charged read allocator; no default permission.
    pub fn new(
        store: Arc<Governed>,
        access: Arc<Access>,
        alloc: Arc<dyn Allocator>,
        metadata_bytes: usize,
    ) -> moruna_storage::Result<Self> {
        let reader = moruna_storage::artifact::Reader::new(store, alloc.clone(), metadata_bytes)?;
        Ok(Self {
            reader,
            access,
            alloc,
        })
    }
    /// Read the exact pinned historical manifest; never rerun a task or query present data.
    pub async fn manifest(&self, pinned: &Metadata) -> moruna_storage::Result<Manifest> {
        self.reader.manifest(&self.access, pinned).await
    }
    /// Fetch one complete pinned Arrow stream, admitted by the caller's explicit byte budget.
    pub async fn chunk(
        &self,
        manifest: &Manifest,
        index: usize,
        byte_budget: u64,
    ) -> moruna_storage::Result<moruna_kernel::Buffer> {
        let chunk = manifest
            .chunks
            .get(index)
            .ok_or(moruna_storage::Error::Invalid("artifact chunk index"))?;
        if chunk.metadata.bytes > byte_budget {
            return Err(moruna_storage::Error::CaptureCapacity {
                need: chunk.metadata.bytes,
                budget: byte_budget,
            });
        }
        let len = usize::try_from(chunk.metadata.bytes)
            .map_err(|_| moruna_storage::Error::Invalid("artifact chunk length"))?;
        let destination = self
            .alloc
            .alloc(len, Tier::Host)
            .map_err(|_| moruna_storage::Error::Unavailable("historical read arena"))?;
        self.reader
            .chunk(&self.access, manifest, index, destination)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moruna_kernel::arrow::{
        array::Int64Array,
        datatypes::{DataType, Field, Schema},
        ipc::reader::StreamReader,
    };
    use moruna_testkit::FakeAllocator;
    fn batch() -> RecordBatch {
        RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)])),
            vec![Arc::new(Int64Array::from(vec![1, 2, 3]))],
        )
        .unwrap()
    }
    struct IdentityKernel;
    impl Kernel for IdentityKernel {
        fn fingerprint(&self) -> moruna_kernel::Fingerprint {
            moruna_kernel::Fingerprint::compute("history-single-task-test", b"")
        }
        fn kind(&self) -> moruna_kernel::KernelKind {
            moruna_kernel::KernelKind::Stateless
        }
        fn accepts(&self) -> moruna_kernel::PayloadSpec {
            moruna_kernel::PayloadSpec {
                kind: moruna_kernel::PayloadKind::Table,
                tier: moruna_kernel::TierPref::Host,
            }
        }
        fn output_schema(&self, input: &SourceSchema) -> moruna_kernel::Result<SourceSchema> {
            Ok(input.clone())
        }
        fn init(
            &self,
            _: &moruna_kernel::InitCtx,
        ) -> moruna_kernel::Result<Box<dyn moruna_kernel::KernelState>> {
            Ok(Box::new(moruna_kernel::NoState))
        }
        fn apply(
            &self,
            _: &mut dyn moruna_kernel::KernelState,
            input: moruna_kernel::Payload,
        ) -> moruna_kernel::Result<moruna_kernel::Payload> {
            Ok(input)
        }
    }
    struct Collected(Mutex<Vec<(String, String, RecordBatch)>>);
    impl LogicalRows for Collected {
        fn append(&self, task: &str, port: &str, batch: &RecordBatch) -> moruna_kernel::Result<()> {
            self.0
                .lock()
                .unwrap()
                .push((task.into(), port.into(), batch.clone()));
            Ok(())
        }
    }
    #[test]
    fn single_task_without_native_tap_retains_actual_returned_rows_and_identity() {
        let original: Arc<dyn Kernel> = Arc::new(IdentityKernel);
        let rows = Arc::new(Collected(Mutex::new(Vec::new())));
        let bound = FinalRows {
            kernel: original.clone(),
            task: LogicalTaskRows {
                task: "real-task".into(),
                ports: vec!["out".into()],
            },
            rows: rows.clone(),
        };
        assert_eq!(bound.fingerprint(), original.fingerprint());
        let expected = batch();
        let mut state = moruna_kernel::NoState;
        let actual = bound
            .apply_port(
                &mut state,
                0,
                moruna_kernel::Payload::Table(expected.clone(), Tier::Host),
            )
            .unwrap();
        let moruna_kernel::Payload::Table(actual, _) = actual else {
            panic!("table result");
        };
        assert_eq!(actual, expected);
        let collected = rows.0.lock().unwrap();
        assert_eq!(
            collected.as_slice(),
            &[("real-task".into(), "out".into(), expected)]
        );
        assert!(
            FinalRows::table_schema(SourceSchema::Tensor {
                dtype: moruna_kernel::DType::I64,
                shape: vec![1]
            })
            .is_err()
        );
    }
    #[test]
    fn checkpoint_fifo_preserves_acknowledged_prefix_through_real_publisher_resume() {
        use moruna_storage::{Action, Authorizer, Credential, Operation, Role, local::FileStore};
        struct Verifier;
        impl Authorizer for Verifier {
            fn verify<'a>(
                &'a self,
                access: &'a Access,
                object: &'a Object,
                _: Action,
                _: Option<&'a moruna_storage::Version>,
            ) -> Operation<'a, ()> {
                Box::pin(async move {
                    if access.workspace == "history-fixture"
                        && object.workspace == access.workspace
                        && access.credential.application() == Some(&b"TEST-ONLY-HISTORY"[..])
                    {
                        Ok(())
                    } else {
                        Err(moruna_storage::Error::Denied)
                    }
                })
            }
        }
        struct Scratch(std::path::PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                std::fs::remove_dir_all(&self.0).unwrap();
            }
        }
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce).unwrap();
        let run = moruna_kernel::RunId(nonce).to_hex();
        let root = Scratch(std::env::temp_dir().join(format!("moruna-history-prefix-{run}")));
        std::fs::create_dir(&root.0).unwrap();
        let backend = FileStore::open(
            root.0.join("home"),
            "history-fixture".into(),
            "home".into(),
            Role::FormatProof,
        )
        .unwrap();
        let store = Arc::new(Governed::new(Arc::new(backend), Arc::new(Verifier)).unwrap());
        let access = Arc::new(Access {
            workspace: "history-fixture".into(),
            run: run.clone(),
            credential: Credential::new(b"TEST-ONLY-HISTORY".to_vec()).unwrap(),
        });
        let target = Object {
            workspace: access.workspace.clone(),
            bucket: "home".into(),
            key: "rows".into(),
        };
        let output = Output {
            request: Request {
                identity: moruna_storage::artifact::Identity {
                    workspace: access.workspace.clone(),
                    run: run.clone(),
                    attempt: "original-attempt".into(),
                    task: "task".into(),
                    port: "out".into(),
                },
                required: true,
                byte_budget: 65536,
                policy: "test-only-explicit-grant".into(),
            },
            target,
            access: access.clone(),
        };
        let binding = vec![vec![LogicalTaskRows {
            task: "task".into(),
            ports: vec!["out".into()],
        }]];
        let make = || {
            Historical::new(
                store.clone(),
                vec![output.clone()],
                binding.clone(),
                65536,
                2,
                65536,
                65536,
                Duration::from_secs(2),
                Duration::from_secs(5),
            )
            .unwrap()
        };
        let alloc: Arc<dyn Allocator> = Arc::new(FakeAllocator::new());
        let first = make();
        let mut kernels: Vec<Arc<dyn Kernel>> = vec![Arc::new(IdentityKernel)];
        let running = first
            .start(&mut kernels, alloc.clone(), CancelToken::new(), &run, None)
            .unwrap();
        kernels[0]
            .apply(
                &mut moruna_kernel::NoState,
                moruna_kernel::Payload::Table(batch(), Tier::Host),
            )
            .unwrap();
        // The command follows the row in the real FIFO; its receipt cannot precede append ack.
        let prefixes = running.checkpoint().snapshot(1).unwrap();
        let checkpoint = root.0.join("manifest.json");
        std::fs::write(
            &checkpoint,
            serde_json::to_vec(&serde_json::json!({(crate::portable::HISTORY_FIELD): prefixes}))
                .unwrap(),
        )
        .unwrap();
        assert!(
            running.finish(false).is_err(),
            "interrupted attempt never completes"
        );
        let interrupted = first.publications();
        assert_eq!(interrupted.len(), 1);
        assert!(
            interrupted[0].complete.is_none(),
            "no interrupted completion fact"
        );
        let resumed = make();
        let mut kernels: Vec<Arc<dyn Kernel>> = vec![Arc::new(IdentityKernel)];
        let running = resumed
            .start(
                &mut kernels,
                alloc.clone(),
                CancelToken::new(),
                &run,
                Some(&checkpoint),
            )
            .unwrap();
        kernels[0]
            .apply(
                &mut moruna_kernel::NoState,
                moruna_kernel::Payload::Table(batch(), Tier::Host),
            )
            .unwrap();
        running.finish(true).unwrap();
        let facts = resumed.publications();
        assert_eq!(facts[0].manifest.rows, 6);
        assert_eq!(facts[0].manifest.chunks.len(), 2);
        assert_eq!(facts[0].manifest.identity.attempt, "original-attempt");
        let reader = ReadPort::new(store, access, alloc, 65536).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let manifest = reader
                .manifest(facts[0].complete.as_ref().unwrap())
                .await
                .unwrap();
            for index in 0..2 {
                let encoded = reader.chunk(&manifest, index, 65536).await.unwrap();
                let mut stream =
                    StreamReader::try_new(std::io::Cursor::new(encoded.as_ref()), None).unwrap();
                assert_eq!(stream.next().unwrap().unwrap(), batch());
                assert!(stream.next().is_none());
            }
        });
    }
    #[test]
    fn every_artifact_chunk_is_a_complete_charged_arrow_stream_or_refuses_encoding() {
        let rows = batch();
        let alloc = FakeAllocator::new();
        let encoded = encode(&rows, 4096, &alloc).expect("complete stream");
        let mut read = StreamReader::try_new(std::io::Cursor::new(encoded.as_ref()), None).unwrap();
        assert_eq!(read.next().unwrap().unwrap(), rows);
        assert!(
            read.next().is_none(),
            "one full chunk, not a stream fragment"
        );
        assert!(
            encode(&rows, 8, &alloc).is_err(),
            "an encoding ceiling never truncates a published chunk"
        );
    }
    #[test]
    fn required_queue_refusal_cancels_and_returns_its_unadmitted_pin_budget() {
        let rows = batch();
        let bytes = rows.get_array_memory_size() as u64;
        let cancel = CancelToken::new();
        let shared = Arc::new(Shared {
            queued: AtomicU64::new(0),
            error: Mutex::new(None),
            finishing: Mutex::new(None),
            deadline: Mutex::new(None),
            cancel: cancel.clone(),
        });
        let (tx, rx) = mpsc::sync_channel(1);
        let tap = Tap {
            tx,
            identities: HashMap::from([(("task".into(), "out".into()), 0)]),
            shared: shared.clone(),
            budget: bytes * 2,
        };
        tap.append("task", "out", &rows).unwrap();
        assert_eq!(shared.queued.load(Ordering::Acquire), bytes);
        assert!(
            tap.append("task", "out", &rows).is_err(),
            "full row queue is an explicit run failure"
        );
        assert!(cancel.is_cancelled());
        assert_eq!(
            shared.queued.load(Ordering::Acquire),
            bytes,
            "failed enqueue returned its charge"
        );
        drop(rx);
        assert_eq!(
            shared.queued.load(Ordering::Acquire),
            0,
            "queued pins released on refusal cleanup"
        );
    }
}
