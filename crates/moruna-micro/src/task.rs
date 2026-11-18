//! An `image` stage on the Moruna kernel (SDD 30 f.5 to f.8): the image's command as a task
//! of the data plane, with SDD 19's contract, the same as on a Linux node.
//!
//! The stage's task is a process the Linux personality starts with `run_plane` (SDD 28 d.3)
//! on its own connection to the personality, whose closing ends it: one input slot, one
//! output slot, its port, and a log ring for its text. An image entry that declares
//! `abi: moruna` names a program of the kernel's own user target instead: the engine checks
//! its ELF against the declaration and starts it itself, a process of the kernel with its plane
//! attached, never through the personality, and the task says its exit code on its rings.
//!
//! A registered function (item 3.6) is a task of either kind, by its file's content: a native
//! program, which the kernel loads from the function's memory object where it lies
//! ([`ImageTask::with_native_function`]), or a Moruna image archive, which the image filesystem
//! attaches in place as the task's root ([`ImageTask::with_root`]; `run_plane`'s `root`,
//! `moruna.linux` 4.0) so the personality runs its command on its own files.
//!
//! Its batches cross in one of two ways:
//!
//! - **ring mode** (`architecture/data-os.md` §5), for an image that declares
//!   `MORUNA_PLANE=rings` (the Python shim's layer does): the task maps its input region and
//!   its rings object once, the engine lays each batch out in the region and posts it on the
//!   input ring, and the task posts its record on the output ring (several when its answer is
//!   bigger than a window), then releases the batch to say it is done. No call per batch moves
//!   a batch, and the kernel and the personality copy nothing ([`crate::rings`]);
//! - **descriptors 0 and 1**, for an unmodified image: the personality turns the slots'
//!   records into an Arrow IPC stream and back, which copies, as any `read` and `write` do.
//!
//! Each `apply` lays the batch out as a page-aligned record (the one copy of each buffer the
//! engine makes, counted as the stage's boundary copy, until the batch is decoded straight
//! into the task's region), and waits for the one record the task puts back, which the plane
//! checks before the engine sees it (DP1). The engine decodes that record in place and hands
//! it on as it lies, with no landing copy, while the task has a window left to write its next
//! record in. `finish` ends the input and requires the task to end its stream and exit 0.
//!
//! What ends the stage, each with its cause, as on Linux: the task exiting or faulting before
//! it answered (with the last lines of its standard error), a record the check refused (the
//! kernel ends the task with the check's sentence), a record more than it was sent, a port
//! call that fails, and the run's cancel, which closes the connection and so ends the task.
//!
//! The task's process is charged to an account the stage carves out of the engine's own
//! (SDD 30 f.11, SDD 20 f.18): the stage's ceiling, within half of what the engine has left,
//! as a Linux node puts its child in a cgroup of that ceiling. The kernel holds the task to
//! it, a task it ends for want of room is named with its account, the account's limit is
//! what the stage holds outside the engine's process for the run's sampler, and its peak is
//! the task's true footprint, said when the task ends.

use std::collections::VecDeque;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, OnceLock};

use linux_protocol::{Bytes, Ended, Plane as LinuxPlane, how, linux};
use moruna_adapters::{UNKNOWN_SEQ, UNKNOWN_STAGE};
use moruna_kernel::arrow::buffer::Buffer as ArrowBuffer;
use moruna_kernel::ports::Answer as PortAnswer;
use moruna_kernel::{
    Allocator, CancelToken, Fingerprint, InitCtx, Kernel, KernelHints, KernelKind, KernelState,
    MorunaError, OutputSink, Payload, PayloadKind, PayloadSpec, Result, RunPorts, SourceSchema,
    StageId, StagePorts, Stream as OutStream, TierPref,
};
use moruna_plane::kernel::Kernel as Calls;
use moruna_plane::ringed::{Region, RingShape};
use moruna_plane::{BatchId, CallId, DataPlane, Event, PlaneError, RingPlane, Shape, TaskId};
use moruna_svc::{
    Account, Channel, ClientCore, Encoder, Error as KernelError, Rights, Signals, Usage, Wire, map,
};

use crate::functions::Function;
use crate::layout::{self, PAGE};
use crate::ring::{LogRing, Stream};
use crate::rings::{self, Released};

const SECONDS: u64 = 1_000_000_000;
/// How long the personality may take to say hello on a task's connection.
const HELLO_SECONDS: u64 = 120;
/// How often a stage waiting on its task looks at the run's cancel and at the personality's
/// answer: an answer of the task's own arrives at once, whatever this is.
const LOOK_NS: u64 = 250_000_000;
/// How long the personality may take to answer for a task the plane has seen end.
const END_SECONDS: u64 = 60;
/// A task's log ring.
const RING_BYTES: u64 = 64 * 1024;
// The kernel's largest memory object, which a task's region and rings object are sized under.
const _: () = assert!(layout::OBJECT_MAX == moruna_abi::limits::MEMORY_MAX_BYTES);

/// The last lines of standard error a failure quotes, as on Linux.
pub const TAIL_LINES: usize = 20;
/// A stage's plane holds a record of its input and a window for its output; its arena is
/// bounded by the stage's ceiling and by this.
const PLANE_BUDGET_MAX: u64 = 1 << 30;

fn kernel_error(msg: String) -> MorunaError {
    MorunaError::Kernel {
        stage: UNKNOWN_STAGE,
        seq: UNKNOWN_SEQ,
        msg,
        features: None,
    }
}

fn plane_error(name: &str, what: &str, e: PlaneError) -> MorunaError {
    kernel_error(format!(
        "image `{name}`: {what}: the data plane refused: {e:?}"
    ))
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The personality's listener, granted to the engine under `linux` (SDD 30 d.3): every stage
/// connects to it once for its task.
pub(crate) fn personality() -> std::result::Result<&'static Channel, String> {
    static LISTENER: OnceLock<std::result::Result<Channel, String>> = OnceLock::new();
    LISTENER
        .get_or_init(|| {
            crate::start::take("linux").map(Channel).ok_or_else(|| {
                format!(
                    "the engine was granted no `linux` listener of the personality, which starts \
                     an image's tasks (it holds {:?})",
                    crate::start::slots()
                )
            })
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// The most bytes of a program the kernel loads (`process_map_image`).
const NATIVE_MAX: u64 = 64 << 20;
/// The bytes of a program's head read to tell a native program from a Linux one: its ELF
/// header and its program headers.
const ELF_HEAD: usize = 64 + 64 * 56;

/// The process resource the engine is granted under `process`: a stage whose image declares
/// `abi: moruna` makes its task's process from it.
fn process_resource() -> std::result::Result<&'static moruna_svc::Handle, String> {
    static RESOURCE: OnceLock<std::result::Result<moruna_svc::Handle, String>> = OnceLock::new();
    RESOURCE
        .get_or_init(|| {
            crate::start::take("process").ok_or_else(|| {
                format!(
                    "the engine was granted no `process` resource, which starts an image's \
                     native program (it holds {:?})",
                    crate::start::slots()
                )
            })
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// The program at `path` in the image (the engine's own view of it, the image filesystem its
/// tasks run on), checked against the ABI its entry declares: its bytes for a native program
/// (`abi: moruna`), which must be of the kernel's user target ([`layout::native_elf`]); for a
/// Linux one (the default), a refusal only when it is such a native program.
pub fn program(path: &str, native: bool) -> std::result::Result<Option<Vec<u8>>, String> {
    use std::io::Read;
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("reading the program `{path}`: {e}"))?;
    if !native {
        let mut head = Vec::new();
        file.take(ELF_HEAD as u64)
            .read_to_end(&mut head)
            .map_err(|e| format!("reading the program `{path}`: {e}"))?;
        return match layout::native_elf(&head) {
            Ok(()) => Err(format!(
                "`{path}` is a program of the Moruna kernel's user target, and the entry \
                 declares a Linux program (the default): declare `abi: moruna`"
            )),
            Err(_) => Ok(None),
        };
    }
    let len = file
        .metadata()
        .map_err(|e| format!("reading the program `{path}`: {e}"))?
        .len();
    if len > NATIVE_MAX {
        return Err(format!(
            "`{path}` is {len} bytes, and the kernel loads a program of at most {}",
            NATIVE_MAX
        ));
    }
    let mut elf = Vec::new();
    file.read_to_end(&mut elf)
        .map_err(|e| format!("reading the program `{path}`: {e}"))?;
    layout::native_elf(&elf).map_err(|why| {
        format!(
            "the entry declares `abi: moruna`, and `{path}` is not a program of the Moruna \
             kernel's user target: {why}"
        )
    })?;
    Ok(Some(elf))
}

/// Loads `elf` into `process`: a memory object of its bytes, which the kernel parses and maps
/// (`process_map_image`). Its entry point.
/// A ring task's plane, opened: the plane, the task's id in it, and its port.
type OpenedPlane = std::result::Result<(RingPlane<Calls>, TaskId, Channel), PlaneError>;

/// A ring task's plane of `budget` bytes ([`layout::ring_sizes`]), the task placed on `cpu`:
/// each task on a CPU of its own, since the kernel keeps a thread where it first ran, and
/// tasks started while the others slept all went to one.
fn open_ring_plane(budget: u64, cpu: Option<u32>) -> OpenedPlane {
    let mut plane = RingPlane::new(Calls, budget);
    let sizes = layout::ring_sizes(budget);
    let (task, port) = plane.open_ring(RingShape {
        region: Region::New(sizes.region),
        windows: sizes.windows,
        window_bytes: sizes.window,
    })?;
    plane.place(task, cpu)?;
    Ok((plane, task, port))
}

/// The native program a stage starts itself: an image's ELF, read from its file (`abi:
/// moruna`), or a registered function's memory object, which the kernel reads where it lies.
#[derive(Clone)]
enum Native {
    Elf(Arc<[u8]>),
    Function(Arc<Function>),
}

impl Native {
    fn bytes(&self) -> usize {
        match self {
            Native::Elf(elf) => elf.len(),
            Native::Function(f) => f.bytes().len(),
        }
    }

    /// Loads it into `process`; its entry point.
    fn load(&self, process: &moruna_svc::Process) -> std::result::Result<u64, KernelError> {
        match self {
            Native::Elf(elf) => load_native(process, elf),
            // No copy of the engine's: the kernel reads the function's own object.
            Native::Function(f) => process.map_image(f.memory()),
        }
    }
}

/// A registered image function's root (item 3.6): its Moruna image archive's memory object and
/// its layers' spans, which the image filesystem attaches in place afresh for each task.
pub struct Root {
    function: Arc<Function>,
    layers: Vec<imagefs_protocol::Attached>,
}

impl Root {
    /// The archive `function`, whose layer files lie at `layers`: each its offset and length in
    /// pages, and its header's seal, lowest first (`moruna_image::archive::Archive`'s).
    pub fn new(
        function: Arc<Function>,
        layers: impl IntoIterator<Item = (u64, u64, [u8; 32])>,
    ) -> Root {
        Root {
            function,
            layers: layers
                .into_iter()
                .map(|(offset, pages, seal)| imagefs_protocol::Attached {
                    offset,
                    pages,
                    seal: imagefs_protocol::Bytes(seal.to_vec()),
                })
                .collect(),
        }
    }

    /// A task's root: an unconnected connection to the archive, attached.
    fn attach(&self) -> std::result::Result<Channel, String> {
        let image = moruna_svc::Memory(
            self.function
                .handle(Rights::READ | Rights::MAP | Rights::TRANSFER)?,
        );
        crate::filesystem::attach(image, self.layers.clone())
            .map_err(|why| format!("the function `{}`: {why}", self.function.entry.name))
    }
}

fn load_native(process: &moruna_svc::Process, elf: &[u8]) -> std::result::Result<u64, KernelError> {
    let bytes = layout::pages(elf.len() as u64).ok_or(KernelError::OutOfRange)?;
    let memory = moruna_svc::Memory::create(bytes)?;
    {
        let mut mapping = memory.map(0, bytes, map::READ | map::WRITE)?;
        mapping.shared_region()?[..elf.len()].copy_from_slice(elf);
    }
    process.map_image(&memory.0)
}

/// The account a stage's task is charged to, while the task lives: what the stage holds
/// outside the engine's process (SB-I9) is its whole limit, which the engine's account holds
/// for it.
#[derive(Default)]
pub struct TaskMemory {
    account: Mutex<Option<(Account, u64)>>,
    /// Sticky uncertainty is a typed accounting failure, never a zero reservation.
    failure: Mutex<Option<String>>,
}

impl TaskMemory {
    /// The task's account's figures; `None` before the task starts.
    pub fn usage(&self) -> Option<std::result::Result<Usage, KernelError>> {
        lock(&self.account)
            .as_ref()
            .map(|(account, _)| account.usage())
    }
}

impl moruna_kernel::OutsideMemory for TaskMemory {
    fn in_use(&self) -> moruna_kernel::Result<u64> {
        // A carved limit is held until the account is released. Reading its
        // current usage is fallible, but cannot make that known reservation zero.
        if let Some(why) = lock(&self.failure).as_ref() {
            return Err(kernel_error(format!(
                "task account cleanup uncertain: {why}"
            )));
        }
        // The plane's objects, records handed on as they lie included, are charged to the
        // engine's own account, which the run samples: they are not outside it.
        Ok(lock(&self.account).as_ref().map_or(0, |(_, limit)| *limit))
    }
}

/// The memory every process of a stage holds outside the engine's process.
pub struct TaskMemories(Vec<Arc<TaskMemory>>);

impl moruna_kernel::OutsideMemory for TaskMemories {
    fn in_use(&self) -> moruna_kernel::Result<u64> {
        let mut sum = 0u64;
        for m in &self.0 {
            sum = sum.saturating_add(m.in_use()?);
        }
        Ok(sum)
    }
}

impl Drop for TaskMemory {
    fn drop(&mut self) {
        // Uncertified original ownership cannot be released by a Rust receiver drop.
        // The supervisor must retire the owning engine process to close this quarantine.
        if let Some((account, _)) = lock(&self.account).take() {
            core::mem::forget(account);
        }
    }
}

/// An image's command as a stage on the kernel.
pub struct ImageTask {
    name: String,
    fingerprint: Fingerprint,
    /// The program the task runs: the command's first word as the image's `PATH` found it.
    path: String,
    command: Vec<String>,
    env: Vec<String>,
    /// The stage's ports, in name order: a task's port `n` is the `n`th (SDD 30 f.7).
    port_names: Vec<String>,
    ports: OnceLock<Arc<StagePorts>>,
    /// The run's cancel, which a stage waiting on its task looks at.
    cancel: OnceLock<CancelToken>,
    /// The stage's ceiling, which bounds its plane's arena and its task's account.
    ceiling: Mutex<Option<u64>>,
    /// Each process's account, once it has started: one a process the stage may run
    /// (`moruna_adapters::instances::count`, system design 3E).
    memory: Vec<Arc<TaskMemory>>,
    /// The memory all of them hold outside the engine's process.
    outside: Arc<TaskMemories>,
    /// The job's CPUs, one process each (system design 3E).
    cpus: usize,
    /// The processes the run's memory funds, at most `memory.len()` ([`ImageTask::fund`]): the
    /// instances the stage runs.
    funded: std::sync::atomic::AtomicUsize,
    /// Native PEX controls reserve each reply explicitly; ordinary image behavior is unchanged.
    managed_windows: bool,
    /// Source path is only resolved through the actual filesystem capability at start.
    assets_path: Option<std::path::PathBuf>,
    /// The image declares the ring protocol (`MORUNA_PLANE=rings`).
    rings: bool,
    /// The native program the stage starts itself: its ELF (`abi: moruna`), or a registered
    /// function's memory object.
    native: Option<Native>,
    /// A registered image function's archive, each task's root.
    root: Option<Arc<Root>>,
    /// What the stage's tasks share: how many hold a batch, their answers' schema, and what
    /// each did, for the run's report.
    busy: Arc<Busy>,
    /// Ring mode: every task's plane, made ahead as the first task starts.
    ahead: layout::Ahead<OpenedPlane>,
}

/// Where a ring stage's workers' time inside the stage goes, in nanoseconds: cutting batches
/// into pieces, laying the pieces out in the tasks' regions, waiting for room there or on the
/// rings, waiting for the tasks' answers, taking them (decoding in place, landing, joining),
/// and polling for answers already come. The rest of a worker's time is the scheduler's
/// (decode, encode, idle: the run's report says it worker by worker). Always on: two reads of
/// the kernel's clock a step.
#[derive(Debug, Default)]
struct Feeder {
    cut: std::sync::atomic::AtomicU64,
    layout: std::sync::atomic::AtomicU64,
    room: std::sync::atomic::AtomicU64,
    wait: std::sync::atomic::AtomicU64,
    take: std::sync::atomic::AtomicU64,
    poll: std::sync::atomic::AtomicU64,
}

/// The places of [`TaskState::timed`], as [`Feeder`]'s fields.
const CUT: usize = 0;
const LAYOUT: usize = 1;
const ROOM: usize = 2;
const WAIT: usize = 3;
const TAKE: usize = 4;
const POLL: usize = 5;

/// What a ring stage's tasks share.
#[derive(Debug, Default)]
struct Busy {
    /// Tasks holding a batch now, and the most at once.
    now: std::sync::atomic::AtomicUsize,
    peak: std::sync::atomic::AtomicUsize,
    /// The schema of the stage's answers, from the first any task gave: the stage waits for
    /// one answer, not one a task, before it stands batches of no rows in for answers.
    schema: Mutex<Option<moruna_kernel::arrow::datatypes::SchemaRef>>,
    /// Where the stage's workers' time inside the stage went, summed over its tasks.
    feeder: Feeder,
    /// The run's cancel, which a task's waits look at: shared, so a task started before its
    /// stage was bound to the run (`init_instances`) still sees it.
    cancel: OnceLock<CancelToken>,
    /// What each task did, by instance, summed over the tasks an instance had.
    figures: Mutex<Vec<Option<layout::TaskFigures>>>,
}

impl Busy {
    fn start(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        let now = self.now.fetch_add(1, Relaxed) + 1;
        self.peak.fetch_max(now, Relaxed);
    }

    fn stop(&self) {
        self.now.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

impl std::fmt::Debug for ImageTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageTask")
            .field("name", &self.name)
            .field("path", &self.path)
            .field("command", &self.command)
            .finish()
    }
}

impl ImageTask {
    /// The stage `name` runs the program at `path` (absolute, in the image) with `command` as
    /// its arguments and `env` as its whole environment, and may call `port_names`: one process
    /// per CPU of the job's `cpus`, at most the ceiling `env` declares (system design 3E,
    /// `moruna_adapters::instances`), until the run funds fewer ([`ImageTask::fund`]).
    pub fn new(
        name: &str,
        identity: &str,
        path: String,
        command: Vec<String>,
        env: Vec<String>,
        mut port_names: Vec<String>,
        cpus: usize,
    ) -> std::result::Result<ImageTask, String> {
        if !path.starts_with('/') {
            return Err(format!(
                "the program `{path}` is not an absolute path in the image"
            ));
        }
        if command.is_empty() {
            return Err("the command is empty".to_string());
        }
        if command
            .iter()
            .chain([&path])
            .chain(env.iter())
            .any(|s| s.as_bytes().contains(&0))
        {
            return Err("the command or the environment holds a NUL byte".to_string());
        }
        personality()?;
        let rings = layout::declares_rings(&env);
        let declared = moruna_adapters::instances::declared(&env)?;
        let processes = moruna_adapters::instances::count(cpus, declared);
        port_names.sort();
        port_names.dedup();
        Ok(ImageTask {
            name: name.to_string(),
            fingerprint: Fingerprint::compute("moruna-micro::ImageTask", identity.as_bytes()),
            path,
            command,
            env,
            port_names,
            ports: OnceLock::new(),
            cancel: OnceLock::new(),
            ceiling: Mutex::new(None),
            outside: Arc::new(TaskMemories(Vec::new())),
            memory: Vec::new(),
            cpus,
            funded: std::sync::atomic::AtomicUsize::new(0),
            managed_windows: false,
            assets_path: None,
            rings,
            native: None,
            root: None,
            busy: Arc::new(Busy::default()),
            ahead: layout::Ahead::default(),
        }
        .with_processes(processes))
    }

    /// The stage runs at most `processes` processes: an account and the figures of each.
    fn with_processes(mut self, processes: usize) -> Self {
        let processes = processes.max(1);
        self.memory = (0..processes)
            .map(|_| Arc::new(TaskMemory::default()))
            .collect();
        self.outside = Arc::new(TaskMemories(self.memory.clone()));
        self.funded = std::sync::atomic::AtomicUsize::new(processes);
        self.busy = Arc::new(Busy {
            figures: Mutex::new(vec![None; processes]),
            ..Busy::default()
        });
        self
    }

    /// A native program declares its ceiling in its ELF note as well (`.note.moruna`, system
    /// design 3E, 5A, 5B): the stage runs one process per CPU of the job, at most the least of
    /// the two ceilings. A note that is not one is refused, naming the stage.
    fn with_note(self, elf: &[u8]) -> std::result::Result<Self, String> {
        let ceiling = moruna_adapters::instances::ceiling(&self.env, Some(elf))
            .map_err(|why| format!("the program of `{}`: {why}", self.name))?;
        let processes = moruna_adapters::instances::count(self.cpus, ceiling);
        Ok(self.with_processes(processes))
    }

    /// The run's memory funds `instances` of the stage's processes (system design 3E, PM
    /// 2026-11-18): the stage runs that many, at least one and at most those it may run.
    pub fn fund(&self, instances: usize) {
        self.funded.store(
            instances.clamp(1, self.memory.len().max(1)),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// The processes the stage runs: those the run funds.
    pub fn instances(&self) -> usize {
        self.funded
            .load(std::sync::atomic::Ordering::Relaxed)
            .clamp(1, self.memory.len().max(1))
    }

    /// The stage's name, as the run's notes say it.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Request a real READ-only source-directory capability for this task.
    /// No pathname aliases a directory or grants access without export_directory RPC.
    pub fn with_assets(mut self, path: &std::path::Path) -> std::result::Result<Self, String> {
        let path = std::path::absolute(path)
            .map_err(|error| format!("actual asset source path: {error}"))?;
        if path.to_str().is_none() {
            return Err("native asset source path is not UTF-8".into());
        }
        self.assets_path = Some(path);
        Ok(self)
    }

    /// The job's entry says its command makes the data plane's calls itself (`plane: true`):
    /// on the kernel that is the ring protocol, as an image whose own environment declares
    /// `MORUNA_PLANE=rings`.
    pub fn with_plane(mut self, plane: bool) -> Self {
        self.rings |= plane;
        self
    }

    /// The entry declares `abi: moruna`: the command is a native program of the kernel's user
    /// target, `elf` ([`program`] checked it), which the stage starts itself on the rings, at
    /// most the ceiling its ELF note declares.
    pub fn with_native(mut self, elf: Vec<u8>) -> std::result::Result<Self, String> {
        let elf: Arc<[u8]> = Arc::from(elf);
        self.native = Some(Native::Elf(Arc::clone(&elf)));
        self.rings = true;
        self.with_note(&elf)
    }

    /// The stage is a registered function whose file is a native program of the kernel's user
    /// target (item 3.6): the kernel loads it from the function's memory object, and the stage
    /// starts it itself on the rings, as [`ImageTask::with_native`]'s, at most the ceiling its
    /// ELF note declares.
    pub fn with_native_function(
        mut self,
        function: Arc<Function>,
    ) -> std::result::Result<Self, String> {
        self.native = Some(Native::Function(Arc::clone(&function)));
        self.rings = true;
        self.with_note(function.bytes())
    }

    /// The stage is a registered image function (item 3.6): each of its tasks runs on `root`,
    /// the function's archive attached in place, instead of the boot image.
    pub fn with_root(mut self, root: Root) -> Self {
        self.root = Some(Arc::new(root));
        self
    }

    /// Use per-command actual reply windows, without pregranting the next window.
    /// This changes no syscall or plane protocol; ordinary image tasks keep their flow.
    pub fn managed_windows(mut self) -> Self {
        self.managed_windows = true;
        self
    }

    /// Exchange one real checked record with an explicitly bounded reply window.
    /// Caller must use managed_windows before exchanging nonzero reply capacities.
    pub fn exchange(
        &self,
        state: &mut dyn KernelState,
        input: Payload,
        reply_bytes: u64,
    ) -> Result<Payload> {
        if reply_bytes != 0 && !self.managed_windows {
            return Err(kernel_error(
                "explicit replies require managed task windows".into(),
            ));
        }
        let st = state
            .as_any_mut()
            .downcast_mut::<TaskState>()
            .ok_or_else(|| kernel_error("image: a state this kernel did not make".into()))?;
        if !self.memory.iter().any(|m| Arc::ptr_eq(&st.memory, m)) {
            return Err(kernel_error(
                "image: state belongs to another actual task owner".into(),
            ));
        }
        let Payload::Table(batch, _) = input else {
            return Err(MorunaError::Plan(
                "an image takes a table; its batches cross the data plane as records".into(),
            ));
        };
        st.reap();
        if reply_bytes != 0 && st.rings {
            return Err(kernel_error(
                "explicit reply windows are a task of the slots; a ring task's windows are its plane's"
                    .into(),
            ));
        }
        if reply_bytes != 0 {
            if st.window.is_some() {
                return Err(kernel_error("explicit reply window already held".into()));
            }
            let ceiling = lock(&self.ceiling)
                .ok_or_else(|| kernel_error("reply has no bound run ceiling".into()))?;
            let bytes = layout::pages(reply_bytes)
                .ok_or_else(|| kernel_error("reply window overflow".into()))?;
            if bytes == 0 || bytes > ceiling.min(PLANE_BUDGET_MAX) {
                return Err(kernel_error(
                    "reply window exceeds actual stage plane budget".into(),
                ));
            }
            st.plane
                .grant(st.task, 0, bytes)
                .map_err(|e| plane_error(&self.name, "granting explicit reply window", e))?;
            st.window = Some(bytes);
        }
        if st.rings {
            return st.pipelined(&batch, self.ports.get(), &self.port_names);
        }
        // Explicit output admission is complete BEFORE the child can read this request.
        let (len, _) = st.offer(&batch, self.ports.get(), &self.port_names)?;
        drop(batch);
        if reply_bytes == 0 && !st.rings {
            st.grant(len)?;
        }
        st.sent += 1;
        match st.wait(self.ports.get(), &self.port_names)? {
            Seen::Put(out) => {
                st.received += 1;
                let out = st.output(out)?;
                // The next record's room, before the task asks for it.
                if !self.managed_windows && !st.rings {
                    st.grant(len)?;
                }
                Ok(out)
            }
            Seen::Ended => {
                let cause = format!(
                    "it ended after answering {} of the {} batches it was sent",
                    st.received, st.sent
                );
                Err(st.failed(&cause))
            }
            Seen::Released(_) => Err(kernel_error(format!(
                "image `{}`: a release where a record was waited for",
                self.name
            ))),
        }
    }

    /// What the stage's tasks did, for the run's report (ring mode): each task's batches, CPU
    /// and account, and the most that held a batch at once. `None` for a stage on descriptors.
    pub fn note(&self) -> Option<String> {
        self.rings.then(|| {
            let f = &self.busy.feeder;
            let s = |t: &std::sync::atomic::AtomicU64| {
                t.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1e9
            };
            format!(
                "{}; its workers in the stage: cutting {:.2} s, laying out {:.2} s, waiting for room {:.2} s, waiting for answers {:.2} s, taking answers {:.2} s, polling {:.2} s",
                layout::stage_note(
                    &self.name,
                    &lock(&self.busy.figures)[..self.instances()],
                    self.busy.peak.load(std::sync::atomic::Ordering::Relaxed),
                ),
                s(&f.cut),
                s(&f.layout),
                s(&f.room),
                s(&f.wait),
                s(&f.take),
                s(&f.poll)
            )
        })
    }

    /// The CPU the stage places its task `instance` on (ring mode): one task to a CPU.
    fn cpu_of(&self, instance: usize) -> Option<u32> {
        self.rings.then(|| {
            layout::task_cpu(
                instance,
                crate::start::kernel().map_or(1, |kernel| kernel.cpus),
            )
        })
    }

    /// The memory the stage holds outside the engine's process: its task's account.
    pub fn outside(&self) -> Arc<dyn moruna_kernel::OutsideMemory> {
        Arc::clone(&self.outside) as Arc<dyn moruna_kernel::OutsideMemory>
    }

    /// The stage's ports, once it is placed (MH 4.10).
    pub fn bind_ports(&self, ports: Arc<StagePorts>) {
        let _ = self.ports.set(ports);
    }

    /// The run starts: its cancel and its waits, for the stage's ports and its own wait.
    pub fn bind_run_ports(&self, run: &RunPorts) {
        if let Some(ports) = self.ports.get() {
            ports.bind_run(run.clone());
        }
        let _ = self.cancel.set(run.cancel.clone());
        let _ = self.busy.cancel.set(run.cancel.clone());
    }

    /// The run's ceiling, which bounds the stage's plane (SDD 30 f.6).
    pub fn bind_run(&self, ceiling: u64) {
        *lock(&self.ceiling) = Some(ceiling);
    }

    fn start(&self, ctx: &InitCtx) -> Result<TaskState> {
        let name = &self.name;
        let memory = self.memory.get(ctx.instance).ok_or_else(|| {
            kernel_error(format!(
                "image `{name}`: instance {} of a stage of {} processes",
                ctx.instance,
                self.memory.len()
            ))
        })?;
        if lock(&memory.account).is_some() || lock(&memory.failure).is_some() {
            return Err(kernel_error(format!(
                "image `{name}`: previous task account has no certified retirement"
            )));
        }
        let ceiling = lock(&self.ceiling).ok_or_else(|| {
            kernel_error(format!(
                "image `{name}`: the stage has no ceiling; the run binds it before the task starts"
            ))
        })?;
        // A native task's plane comes out of its share at its real size, beside its account.
        let budget = if self.native.is_some() {
            layout::native_split(ceiling).0
        } else {
            ceiling.min(PLANE_BUDGET_MAX)
        };
        let (plane, task, port) = if self.rings {
            // Every task's plane is made ahead, together, as the first task starts: the kernel
            // zeroes each and the host faults its pages in on CPUs of their own, not one task
            // after another. A task restarted later makes its own now.
            let instances = self.instances();
            let cpus: Vec<Option<u32>> = (0..instances).map(|i| self.cpu_of(i)).collect();
            self.ahead.start(instances, move |i| {
                open_ring_plane(budget, cpus.get(i).copied().flatten())
            });
            let opened = match self.ahead.take(ctx.instance) {
                Some(opened) => opened,
                None => open_ring_plane(budget, self.cpu_of(ctx.instance)),
            };
            let (plane, task, port) =
                opened.map_err(|e| plane_error(name, "opening the task's plane", e))?;
            (plane, task, Some(port))
        } else {
            let mut plane = RingPlane::new(Calls, budget);
            let (task, port) = plane
                .open(Shape {
                    inputs: 1,
                    outputs: 1,
                    port: !self.port_names.is_empty(),
                })
                .map_err(|e| plane_error(name, "opening the task's plane", e))?;
            (plane, task, port)
        };
        let slots: Vec<moruna_svc::Handle> = plane
            .slots(task)
            .map_err(|e| plane_error(name, "its slots", e))?
            .iter()
            .map(|s| {
                s.0.duplicate(Rights::WRITE | Rights::TRANSFER | Rights::WAIT)
            })
            .collect::<std::result::Result<_, _>>()
            .map_err(|e| kernel_error(format!("image `{name}`: its slots: {}", e.name())))?;
        let [input, output]: [moruna_svc::Handle; 2] = slots.try_into().map_err(|_| {
            kernel_error(format!(
                "image `{name}`: a plane of one input and one output has other slots"
            ))
        })?;
        let ring = LogRing::new(RING_BYTES)
            .map_err(|e| kernel_error(format!("image `{name}`: its log ring: {e}")))?;
        if let Some(native) = &self.native {
            drop((input, output));
            return self.start_native(ctx, memory, plane, task, port, ring, ceiling, native);
        }
        let listener = personality().map_err(|e| kernel_error(format!("image `{name}`: {e}")))?;
        let channel = moruna_svc::connect(listener).map_err(|e| {
            kernel_error(format!(
                "image `{name}`: connecting to the personality: {e:?}"
            ))
        })?;
        let mut client = ClientCore::connect(
            channel,
            linux::ID,
            moruna_svc::deadline_in(HELLO_SECONDS * SECONDS),
        )
        .map_err(|e| kernel_error(format!("image `{name}`: hello to the personality: {e:?}")))?;
        // The 2.0 handshake rejects the old unaccounted 1.x wire layouts before
        // any run_plane request is sent. Every 2.0 method is available at minor 0.
        let (account, kept, limit) = self.carve(ceiling, false)?;
        let mut body = Encoder::new();
        let plane_arg = LinuxPlane {
            root: self
                .root
                .as_deref()
                .map(Root::attach)
                .transpose()
                .map_err(|why| kernel_error(format!("image `{name}`: {why}")))?,
            assets: self
                .assets_path
                .as_deref()
                .map(crate::filesystem::export_directory)
                .transpose()
                .map_err(kernel_error)?,
            input: Some(input),
            output: Some(output),
            port,
            log: ring
                .give()
                .map_err(|e| kernel_error(format!("image `{name}`: {e}")))?,
            log_bytes: ring.bytes(),
            account: account.0,
        };
        let encoded = (|| {
            Bytes(self.path.as_bytes().to_vec()).encode(&mut body)?;
            self.command
                .iter()
                .map(|a| Bytes(a.as_bytes().to_vec()))
                .collect::<Vec<Bytes>>()
                .encode(&mut body)?;
            self.env
                .iter()
                .map(|e| Bytes(e.as_bytes().to_vec()))
                .collect::<Vec<Bytes>>()
                .encode(&mut body)?;
            plane_arg.encode(&mut body)
        })();
        encoded
            .map_err(|e| kernel_error(format!("image `{name}`: the run_plane request: {e:?}")))?;
        let run = client
            .send(2, body)
            .map_err(|e| kernel_error(format!("image `{name}`: run_plane: {e:?}")))?;
        // Publish the retained account only after request construction and transfer
        // succeeded. Failed startup drops every local handle and returns its limit.
        *lock(&memory.account) = Some((kept, limit));
        tracing::info!(target: "micro", image = %name, command = ?self.command, rings = self.rings, "image task asked of the personality");
        Ok(self.state(ctx, memory, plane, task, ring, Some((client, run)), None))
    }

    /// The task's account, carved out of the engine's (SDD 30 f.11): the account, a read-only
    /// handle to it the stage keeps, and its limit.
    fn carve(&self, ceiling: u64, native: bool) -> Result<(Account, Account, u64)> {
        let name = &self.name;
        let room = Usage::own()
            .map_err(|e| {
                kernel_error(format!(
                    "image `{name}`: the engine's account: {}",
                    e.name()
                ))
            })?
            .room();
        let limit = if native {
            layout::native_task_limit(ceiling, room)
        } else {
            layout::task_limit(ceiling, room)
        };
        let limit = limit.ok_or_else(|| {
            kernel_error(format!(
                "image `{name}`: the engine's account has {room} bytes left, too few for a task's \
                 account of at least {} bytes beside the engine's own half",
                layout::TASK_MIN
            ))
        })?;
        let account_name = layout::account_name(name);
        let account = Account::carve(limit, &account_name).map_err(|e| {
            kernel_error(format!(
                "image `{name}`: an account of {limit} bytes for its task: {}",
                e.name()
            ))
        })?;
        let kept = account
            .0
            .duplicate(Rights::READ)
            .map(Account)
            .map_err(|e| {
                kernel_error(format!("image `{name}`: its task's account: {}", e.name()))
            })?;
        Ok((account, kept, limit))
    }

    /// Starts the image's native program itself (`abi: moruna`): a process of the kernel made
    /// from the engine's process resource and charged to the task's account, the ELF loaded
    /// by the kernel, the plane attached (its region, its rings object and its port), the log
    /// ring given under `log` and the stage's environment under `env` (item 5.5). The
    /// personality has no part in it.
    #[allow(clippy::too_many_arguments)]
    fn start_native(
        &self,
        ctx: &InitCtx,
        memory: &Arc<TaskMemory>,
        plane: RingPlane<Calls>,
        task: TaskId,
        port: Option<Channel>,
        ring: LogRing,
        ceiling: u64,
        native: &Native,
    ) -> Result<TaskState> {
        let name = &self.name;
        let fail = |what: &str, e: KernelError| {
            kernel_error(format!("image `{name}`: {what}: {}", e.name()))
        };
        let resource =
            process_resource().map_err(|e| kernel_error(format!("image `{name}`: {e}")))?;
        let (account, kept, limit) = self.carve(ceiling, true)?;
        let process =
            moruna_svc::Process::create_in(resource, &layout::account_name(name), &account.0)
                .map_err(|e| fail("making its process", e))?;
        let entry = native
            .load(&process)
            .map_err(|e| fail("loading its program", e))?;
        let slots = plane
            .ring_slots(task)
            .map_err(|e| plane_error(name, "its slots", e))?;
        moruna_sys::plane::attach(&process, &slots, port)
            .map_err(|(e, _)| fail("attaching its plane", e))?;
        let log = ring
            .give()
            .map_err(|e| kernel_error(format!("image `{name}`: {e}")))?;
        // Its environment, as an image task's process gets its own (item 5.5): the slot `env`,
        // which std's start reads and the C library's `environ` is built from.
        let env = moruna_sys::env::give(&self.env).map_err(|e| fail("its environment", e))?;
        process
            .start_with(
                entry,
                &mut [(Some(log), "log"), (Some(env), moruna_sys::env::SLOT)],
            )
            .map_err(|e| fail("starting its process", e))?;
        drop(account);
        *lock(&memory.account) = Some((kept, limit));
        tracing::info!(target: "micro", image = %name, command = ?self.command, bytes = native.bytes(), "native task started");
        Ok(self.state(ctx, memory, plane, task, ring, None, Some(process)))
    }

    /// A started task's state.
    #[allow(clippy::too_many_arguments)]
    fn state(
        &self,
        ctx: &InitCtx,
        memory: &Arc<TaskMemory>,
        plane: RingPlane<Calls>,
        task: TaskId,
        ring: LogRing,
        personality: Option<(ClientCore, u32)>,
        process: Option<moruna_svc::Process>,
    ) -> TaskState {
        let (client, run) = match personality {
            Some((client, run)) => (Some(client), run),
            None => (None, 0),
        };
        TaskState {
            memory: Arc::clone(memory),
            name: self.name.clone(),
            plane,
            task,
            client,
            run,
            process,
            ended: None,
            terminal_confirmed: false,
            ring,
            out: moruna_kernel::output::current(),
            tail: VecDeque::new(),
            alloc: Arc::clone(&ctx.alloc),
            sent: 0,
            received: 0,
            answered: 0,
            joined: 0,
            window: None,
            rings: self.rings,
            released: Released::default(),
            held: 0,
            copied: 0,
            kept: 0,
            landed: 0,
            stage: self.ports.get().map_or(UNKNOWN_STAGE, |p| p.stage()),
            cancel: self.cancel.get().cloned(),
            in_flight: VecDeque::new(),
            queued: VecDeque::new(),
            applying: false,
            ready: VecDeque::new(),
            taking: None,
            counted: false,
            early: VecDeque::new(),
            schema: None,
            busy: Arc::clone(&self.busy),
            deferred: 0,
            instance: ctx.instance,
            cpu: self.cpu_of(ctx.instance),
            recorded: false,
            timed: [0; 6],
        }
    }
}

/// One stage's task, the engine's side.
pub struct TaskState {
    /// Its account, which the stage reads its peak from.
    memory: Arc<TaskMemory>,
    name: String,
    plane: RingPlane<Calls>,
    task: TaskId,
    /// The task's own connection to the personality: closing it ends the task.
    client: Option<ClientCore>,
    /// The `run_plane` request, answered when the task has ended.
    run: u32,
    /// The task's process, for a native program the stage started itself: the engine waits
    /// for its end, and ends it when the stage does.
    process: Option<moruna_svc::Process>,
    ended: Option<std::result::Result<Ended, String>>,
    terminal_confirmed: bool,
    ring: LogRing,
    out: Option<(Arc<dyn OutputSink>, StageId)>,
    tail: VecDeque<String>,
    alloc: Arc<dyn Allocator>,
    sent: u64,
    /// Records the task put, and batches it said it was done with.
    received: u64,
    answered: u64,
    /// Batches answered with several records, joined into one batch.
    joined: u64,
    /// The bytes of the window the task holds for its next record.
    window: Option<u64>,
    /// The task speaks the ring protocol.
    rings: bool,
    /// Records handed on as they lie whose payloads downstream has dropped.
    released: Released,
    /// Records handed on as they lie and not yet given back.
    held: u64,
    /// Body bytes the engine copied laying batches out for the task.
    copied: u64,
    /// Records handed on as they lie, and records copied into the run's arena instead.
    kept: u64,
    landed: u64,
    stage: StageId,
    cancel: Option<CancelToken>,
    /// Ring mode: the batches offered and not yet answered, oldest first.
    in_flight: VecDeque<BatchId>,
    /// Ring mode: pieces given to the stage and not yet posted to the task, oldest first: a
    /// morsel's pieces past what the task holds in flight wait here, not the worker.
    queued: VecDeque<moruna_kernel::arrow::record_batch::RecordBatch>,
    /// Ring mode: a unit's batch is cut and queued, and the unit has not returned. A unit the
    /// run's arena refused (`MorunaError::Alloc`) is applied again with the same batch (RB5,
    /// the scheduler's `run_task`), and every other failure retires the state, so a unit that
    /// finds this set goes on from where the refusal stopped it: its batch is not cut and
    /// posted twice (item 3.1: rows came out twice, or not at all).
    applying: bool,
    /// Ring mode: answers the unit under way has taken and not yet given, oldest first: kept
    /// across a refused unit, given with the unit that completes.
    ready: VecDeque<Payload>,
    /// Ring mode: the answer being taken when the run's arena refused one of its records.
    taking: Option<Taking>,
    /// Whether this task counts in the stage's busy tasks now.
    counted: bool,
    /// What the task put or released while the engine laid out its next batch.
    early: VecDeque<Seen>,
    /// The schema of the task's answers, from its first: a batch of no rows stands for an
    /// answer still to come.
    schema: Option<moruna_kernel::arrow::datatypes::SchemaRef>,
    /// The stage's gauge of busy tasks.
    busy: Arc<Busy>,
    /// Answers the stage gave as batches of no rows, the answers coming later.
    deferred: u64,
    /// Which of the stage's tasks it is, and the CPU it was placed on.
    instance: usize,
    cpu: Option<u32>,
    /// Its figures are in the stage's ([`TaskState::record`]).
    recorded: bool,
    /// This task's share of the stage's [`Feeder`] times, in nanoseconds, added at its end.
    timed: [u64; 6],
}

impl Drop for TaskState {
    fn drop(&mut self) {
        // A task retired before its stage's end is still in the report.
        self.record();
        // Read an already-available ORIGINAL terminal reply, never manufacture one
        // from a close or a transient zero while a start request may still be queued.
        let _ = self.answer(0);
        // A native task ends with its stage: killed if it has not ended, and waited for.
        if let Some(process) = self.process.as_ref()
            && !self.terminal_confirmed
        {
            let _ = process.kill();
            let deadline = moruna_svc::now().saturating_add(END_SECONDS * SECONDS);
            self.terminal_confirmed =
                moruna_svc::wait_one(&process.0, Signals::TERMINATED, deadline)
                    .is_ok_and(|s| s.intersects(Signals::TERMINATED));
        }
        drop(self.client.take());
        // Release our original plane/record owners before asking whether physical
        // child charges retired. No alias, fake reset or fabricated completion.
        let original = core::mem::replace(&mut self.plane, RingPlane::new(Calls, 0));
        drop(original);
        let result = if self.terminal_confirmed {
            // A native task's process handle goes first: its process is charged to the account.
            let memory = &self.memory;
            layout::retire(
                self.process.take(),
                || {
                    lock(&memory.account)
                        .as_ref()
                        .map(|(account, _)| account.usage())
                        .transpose()
                        .map(|usage| usage.map(|u| (u.used, u.reserved)))
                        .map_err(|e| format!("actual Account read: {}", e.name()))
                },
                std::time::Duration::from_secs(5),
            )
        } else {
            Err("owner closed without an actual terminal acknowledgement".into())
        };
        if result.is_ok() {
            lock(&self.memory.account).take();
        }
        if let Err(why) = result {
            *lock(&self.memory.failure) = Some(why.clone());
            tracing::error!(target:"micro",image=%self.name,cause=%why,"task cleanup uncertain; actual Account observer retained");
        }
    }
}

impl KernelState for TaskState {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

/// What a task's wait saw.
enum Seen {
    Put(BatchId),
    /// The task released this batch: it is done with it (ring mode).
    Released(BatchId),
    Ended,
}

/// An answer being taken: the batch it answers, its records taken so far (each decoded as it
/// came, so its window cycles for the next part), the record the run's arena refused, to take
/// first when the unit is applied again, and whether the task has released the batch.
struct Taking {
    offered: BatchId,
    parts: Vec<Payload>,
    refused: Option<BatchId>,
    released: bool,
}

/// A mapping of a batch the engine holds, owning the pages an Arrow buffer points into.
struct Mapped(#[allow(dead_code)] moruna_svc::Mapping<'static>);

impl TaskState {
    /// Whether the run is cancelled: as the task started, or as its stage was bound since.
    fn cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .or_else(|| self.busy.cancel.get())
            .is_some_and(CancelToken::is_cancelled)
    }

    /// This task in the stage's busy tasks, or out of them, once each way.
    fn count_busy(&mut self, busy: bool) {
        if busy != self.counted {
            if busy {
                self.busy.start();
            } else {
                self.busy.stop();
            }
            self.counted = busy;
        }
    }

    /// Adds what this task did to its instance's figures in the stage's, once.
    fn record(&mut self) {
        if std::mem::replace(&mut self.recorded, true) {
            return;
        }
        let (peak, limit) = match self.memory.usage() {
            Some(Ok(u)) => (u.peak, u.limit),
            _ => (0, 0),
        };
        if let Some(slot) = lock(&self.busy.figures).get_mut(self.instance) {
            let f = slot.get_or_insert_with(layout::TaskFigures::default);
            f.states += 1;
            f.batches += self.sent;
            f.records += self.received;
            f.deferred += self.deferred;
            f.cpu = self.cpu;
            f.peak = f.peak.max(peak);
            let feeder = &self.busy.feeder;
            for (at, ns) in [
                &feeder.cut,
                &feeder.layout,
                &feeder.room,
                &feeder.wait,
                &feeder.take,
                &feeder.poll,
            ]
            .into_iter()
            .zip(self.timed)
            {
                at.fetch_add(ns, std::sync::atomic::Ordering::Relaxed);
            }
            f.limit = limit;
        }
    }

    /// Takes the task's lines out of its ring: to the stage's output (MH 4.3 `log`), and its
    /// standard error to the tail a failure quotes.
    fn drain_log(&mut self) {
        let lines = match self.ring.drain() {
            Ok(lines) => lines,
            Err(why) => {
                tracing::error!(target: "micro", image = %self.name, %why, "the task's log ring");
                return;
            }
        };
        for (stream, body) in lines {
            let out = match stream {
                Stream::Stdout => OutStream::Stdout,
                Stream::Stderr => OutStream::Stderr,
            };
            if stream == Stream::Stderr {
                if self.tail.len() == TAIL_LINES {
                    self.tail.pop_front();
                }
                self.tail
                    .push_back(String::from_utf8_lossy(&body).into_owned());
            }
            let line = layout::line(&body);
            match &self.out {
                Some((sink, stage)) => sink.bytes(Some(*stage), out, &line),
                None => {
                    use std::io::Write;
                    let _ = std::io::stderr().write_all(&line);
                }
            }
        }
    }

    /// The personality's answer for the task, waiting up to `wait` nanoseconds (zero: only
    /// if it has come).
    fn answer(&mut self, wait: u64) -> Option<&std::result::Result<Ended, String>> {
        if let Some(process) = self.process.as_ref() {
            // A native task: its process's end, and the exit code it said on its rings.
            if self.ended.is_none() {
                let deadline = moruna_svc::now().saturating_add(wait.max(1));
                let ended = moruna_svc::wait_one(&process.0, Signals::TERMINATED, deadline)
                    .is_ok_and(|s| s.intersects(Signals::TERMINATED));
                if ended {
                    self.terminal_confirmed = true;
                    let said = self.plane.exit_code(self.task);
                    self.ended = Some(Ok(Ended {
                        how: if said.is_some() {
                            how::EXITED
                        } else {
                            how::FAULTED
                        },
                        code: said.unwrap_or(0),
                        ..Ended::default()
                    }));
                }
            }
            return self.ended.as_ref();
        }
        if self.ended.is_none() {
            let deadline = moruna_svc::now().saturating_add(wait.max(1));
            let Some(client) = self.client.as_mut() else {
                return self.ended.as_ref();
            };
            match client.reply(self.run, deadline) {
                Ok(mut frame) => {
                    let mut d = frame.decoder();
                    let ended = Ended::decode(&mut d).and_then(|e| d.finish().map(|()| e));
                    self.terminal_confirmed = ended.is_ok();
                    self.ended =
                        Some(ended.map_err(|e| {
                            format!("the personality's answer does not decode: {e:?}")
                        }));
                }
                Err(moruna_svc::CallError::TimedOut) => {}
                Err(moruna_svc::CallError::Remote(code)) => {
                    self.terminal_confirmed =
                        <linux::Error as moruna_svc::ProtocolError>::from_code(code).is_some();
                    let why = match <linux::Error as moruna_svc::ProtocolError>::from_code(code) {
                        Some(linux::Error::NotFound) => "the command is not in the image".to_string(),
                        Some(linux::Error::NotExecutable) => {
                            "the command is not a program the personality loads (the console says why)"
                                .to_string()
                        }
                        Some(e) => format!("the personality refused to start it: {e:?}"),
                        None => format!("the personality refused to start it with code {code}"),
                    };
                    self.ended = Some(Err(why));
                }
                Err(e) => self.ended = Some(Err(format!("the personality's answer: {e:?}"))),
            }
        }
        self.ended.as_ref()
    }

    /// What the task's account says of it: its peak against its limit.
    fn footprint(&self) -> String {
        match self.memory.usage() {
            Some(Ok(u)) => format!(
                "its account {} peaked at {} of {} bytes",
                u.name(),
                u.peak,
                u.limit
            ),
            Some(Err(e)) => format!("its account could not be read: {}", e.name()),
            None => String::from("it had no account"),
        }
    }

    /// The stage's failure, with how the task ended and its last lines.
    fn failed(&mut self, cause: &str) -> MorunaError {
        self.drain_log();
        let name = self.name.clone();
        let footprint = self.footprint();
        let native = self.process.is_some();
        let head = match self.answer(END_SECONDS * SECONDS) {
            Some(Ok(e)) => match e.how {
                how::EXITED => format!("image `{name}`: {cause}; exited with status {}", e.code),
                how::SIGNALED => format!("image `{name}`: {cause}; was ended by signal {}", e.code),
                _ if native => format!(
                    "image `{name}`: {cause}; it ended without saying its exit code on its rings \
                     (`Rings::exit`): it faulted or was killed, which the kernel's console line \
                     says, or returned without saying ({footprint})"
                ),
                _ => format!(
                    "image `{name}` was ended by the kernel ({cause}; {footprint}; the kernel's \
                     console line says why)"
                ),
            },
            Some(Err(why)) => format!("image `{name}`: {cause}; {why}"),
            None => format!("image `{name}`: {cause}"),
        };
        self.drain_log();
        let lines: Vec<String> = self.tail.iter().cloned().collect();
        kernel_error(with_stderr(head, &lines))
    }

    /// Grants the window for the task's next record, once none is held.
    fn grant(&mut self, input: u64) -> Result<()> {
        if self.window.is_some() {
            return Ok(());
        }
        let bytes = layout::window_for(input);
        self.plane
            .grant(self.task, 0, bytes)
            .map_err(|e| plane_error(&self.name, "granting the task's window", e))?;
        self.window = Some(bytes);
        Ok(())
    }

    /// Gives the plane back every record whose payload downstream has dropped.
    fn reap(&mut self) {
        for batch in self.released.take() {
            self.held = self.held.saturating_sub(1);
            if let Err(e) = self.plane.drop_batch(batch) {
                tracing::error!(target: "micro", image = %self.name, ?e, "giving a record back to the plane");
            }
        }
    }

    /// Lays `batch` out as a record where the task reads it and offers it: in ring mode, in
    /// the task's region, posted on its input ring; otherwise in an arena object pushed on
    /// its input slot. The record's length, and the batch it is.
    fn offer(
        &mut self,
        batch: &moruna_kernel::arrow::record_batch::RecordBatch,
        ports: Option<&Arc<StagePorts>>,
        names: &[String],
    ) -> Result<(u64, BatchId)> {
        let (started, room) = (moruna_svc::now(), self.timed[ROOM]);
        let offered = self.offer_timed(batch, ports, names);
        let spent = moruna_svc::now().saturating_sub(started);
        self.timed[LAYOUT] += spent.saturating_sub(self.timed[ROOM] - room);
        offered
    }

    fn offer_timed(
        &mut self,
        batch: &moruna_kernel::arrow::record_batch::RecordBatch,
        ports: Option<&Arc<StagePorts>>,
        names: &[String],
    ) -> Result<(u64, BatchId)> {
        let (framing, placed) =
            moruna_kernel::ipc::encode_framing(batch, PAGE as usize, 0, self.alloc.as_ref())?;
        let pieces: Vec<(u64, u64)> = placed
            .iter()
            .map(|(at, b)| (*at as u64, b.len() as u64))
            .collect();
        let len = layout::record_len(framing.len() as u64, &pieces)
            .ok_or_else(|| kernel_error(format!("image `{}`: a record past u64", self.name)))?;
        let body: u64 = pieces.iter().map(|(_, n)| n).sum();
        let id = if self.rings {
            let region = self
                .plane
                .region_of(self.task)
                .ok_or_else(|| kernel_error(format!("image `{}`: no input region", self.name)))?;
            let id = loop {
                let laid = self.plane.load_into(region, len, |view, at| {
                    rings::write_record(view, at, len, &framing, &placed).map(|_| ())
                });
                match laid {
                    Ok((id, _)) => break id,
                    // The region holds the task's unreleased batches: wait for a release.
                    Err(PlaneError::Budget { asked, free }) => {
                        if !self.step(ports, names)? {
                            return Err(kernel_error(format!(
                                "image `{}`: a record of {asked} bytes does not fit its input \
                                 region, which has {free} bytes free and nothing to release",
                                self.name
                            )));
                        }
                    }
                    Err(e) => return Err(plane_error(&self.name, "laying the batch out", e)),
                }
            };
            loop {
                match self.plane.offer(self.task, 0, id) {
                    Ok(()) => break,
                    Err(PlaneError::Full) => {
                        if !self.step(ports, names)? {
                            return Err(kernel_error(format!(
                                "image `{}`: its input ring stays full",
                                self.name
                            )));
                        }
                    }
                    Err(e) => return Err(plane_error(&self.name, "posting the batch", e)),
                }
            }
            id
        } else {
            let (id, _) = self
                .plane
                .plane
                .load_with(len, |_, memory| {
                    write_record(memory, len, &framing, &placed)
                })
                .map_err(|e| plane_error(&self.name, "laying the batch out", e))?;
            self.plane
                .offer(self.task, 0, id)
                .map_err(|e| plane_error(&self.name, "offering the batch", e))?;
            id
        };
        // The engine's one copy at the boundary, until the batch is decoded straight into
        // the task's region.
        self.copied += body;
        self.alloc.note_boundary_copy(body);
        // The task's share keeps it until the task releases it or ends.
        self.plane
            .drop_batch(id)
            .map_err(|e| plane_error(&self.name, "the engine's hold", e))?;
        Ok((len, id))
    }

    /// One step of the plane while the engine waits for room: whether anything came.
    fn step(&mut self, ports: Option<&Arc<StagePorts>>, names: &[String]) -> Result<bool> {
        let started = moruna_svc::now();
        let stepped = self.step_timed(ports, names);
        self.timed[ROOM] += moruna_svc::now().saturating_sub(started);
        stepped
    }

    fn step_timed(&mut self, ports: Option<&Arc<StagePorts>>, names: &[String]) -> Result<bool> {
        let deadline = moruna_svc::now().saturating_add(LOOK_NS);
        let event = self
            .plane
            .next(deadline)
            .map_err(|e| plane_error(&self.name, "waiting on the task", e))?;
        self.drain_log();
        match event {
            None => {
                if self.cancelled() {
                    return Err(MorunaError::Cancelled);
                }
                Ok(false)
            }
            Some(Event::Call {
                call,
                port,
                request,
            }) => {
                self.call(ports, names, call, port, request)?;
                Ok(true)
            }
            Some(Event::Refused { reason, .. }) => Err(self.failed(&format!(
                "its record on its output was refused by the engine's check: {reason}"
            ))),
            Some(Event::Ended { .. }) => {
                Err(self.failed("it ended while the engine offered a batch"))
            }
            // An answer to a batch in flight, taken in its turn.
            Some(Event::Put { batch, .. }) if !self.in_flight.is_empty() => {
                self.window = None;
                self.early.push_back(Seen::Put(batch));
                Ok(true)
            }
            Some(Event::Put { batch, .. }) => {
                let _ = self.plane.drop_batch(batch);
                Err(self.failed("it put a record before it was offered a batch"))
            }
            Some(Event::Released { batch, .. }) if !self.in_flight.is_empty() => {
                self.early.push_back(Seen::Released(batch));
                Ok(true)
            }
            Some(Event::Released { .. } | Event::Freed { .. }) => Ok(true),
        }
    }

    /// The record the task put, decoded in place and handed on as it lies; or copied into
    /// the run's arena when the task would otherwise run out of windows to write in.
    fn output(&mut self, out: BatchId) -> Result<Payload> {
        let tier = if self.alloc.is_pinned() {
            moruna_kernel::Tier::PinnedHost
        } else {
            moruna_kernel::Tier::Host
        };
        let windows = if self.rings {
            layout::ring_sizes(self.plane.residency().budget).windows
        } else {
            // A task of the slots is granted a fresh window each time: a few held downstream
            // cost only the budget.
            4
        };
        if !layout::keep_in_place(self.held, windows) {
            return self.land(out);
        }
        let payload = if let Some((view, offset, len)) = self.plane.batch_view(out) {
            let ptr = moruna_plane::ringed::View::base(&*view) + offset as usize;
            // SAFETY: the record lies at `offset` of the engine's mapping `view`, which the
            // payload keeps; it was checked after the task's write access was taken back,
            // and the plane gives its window to the task again only when the payload is
            // dropped (`reap`), so nothing writes it meanwhile (DP-I4).
            #[allow(unsafe_code)]
            unsafe {
                rings::payload_over(view, ptr, len, out, &self.released, tier)
            }
        } else {
            let (memory, offset, len) = self
                .plane
                .plane
                .batch(out)
                .map_err(|e| plane_error(&self.name, "the record put", e))?;
            let first = offset & !(PAGE - 1);
            let within = (offset - first) as usize;
            let span = layout::pages(offset + len).unwrap_or(u64::MAX) - first;
            let mapping = memory.map_detached(first, span, map::READ).map_err(|e| {
                kernel_error(format!(
                    "image `{}`: mapping its record: {}",
                    self.name,
                    e.name()
                ))
            })?;
            let ptr = mapping.addr() as usize + within;
            // SAFETY: the record lies inside `mapping`, which the payload keeps; nothing
            // writes a batch the engine holds (DP-I4), and its hold is dropped only when the
            // payload is (`reap`).
            #[allow(unsafe_code)]
            unsafe {
                rings::payload_over(Mapped(mapping), ptr, len, out, &self.released, tier)
            }
        };
        match payload {
            Ok(p) => {
                self.held += 1;
                self.kept += 1;
                Ok(p)
            }
            Err(e) => {
                // The hold went back with the failed buffer.
                self.reap();
                Err(e)
            }
        }
    }

    /// The record the task put, decoded in place and copied into the run's arena, its
    /// window given back at once.
    fn land(&mut self, out: BatchId) -> Result<Payload> {
        let (memory, offset, len) = self
            .plane
            .plane
            .batch(out)
            .map_err(|e| plane_error(&self.name, "the record put", e))?;
        let first = offset & !(PAGE - 1);
        let within = (offset - first) as usize;
        let span = layout::pages(offset + len).unwrap_or(u64::MAX) - first;
        let mapping = memory.map_detached(first, span, map::READ).map_err(|e| {
            kernel_error(format!(
                "image `{}`: mapping its record: {}",
                self.name,
                e.name()
            ))
        })?;
        let start = mapping.bytes().as_ptr();
        let owner = Arc::new(Mapped(mapping));
        let ptr = NonNull::new(start.wrapping_add(within) as *mut u8)
            .ok_or_else(|| kernel_error("a record mapped at address 0".to_string()))?;
        // SAFETY: `ptr` and `len` lie inside the mapping, which `owner` keeps for as long as
        // the buffer lives; nothing writes the batch while the engine holds it (DP-I4), and
        // the engine's hold is dropped below only after the landing copy, when the buffer
        // and every array over it are gone.
        #[allow(unsafe_code)]
        let buffer = unsafe { ArrowBuffer::from_custom_allocation(ptr, len as usize, owner) };
        // Only once it has landed is the record given back: one the run's arena refused stays
        // the engine's, taken first when the unit is applied again (`Taking::refused`).
        let landed = moruna_kernel::ipc::decode(buffer, PAGE as usize)
            .and_then(|b| moruna_adapters::land::land_table(b, &self.alloc))?;
        self.plane
            .drop_batch(out)
            .map_err(|e| plane_error(&self.name, "dropping the record put", e))?;
        self.landed += 1;
        Ok(landed.0)
    }

    /// Answers a port call through the stage's ports.
    fn call(
        &mut self,
        ports: Option<&Arc<StagePorts>>,
        names: &[String],
        call: CallId,
        port: u64,
        request: Vec<u8>,
    ) -> Result<()> {
        let Some(ports) = ports else {
            return Err(kernel_error(format!(
                "image `{}` called port {port}, and its stage lists no ports",
                self.name
            )));
        };
        let Some(name) = layout::port_name(names, port) else {
            let _ = self.plane.answer(call, Err(KernelError::BadHandle));
            return Err(kernel_error(format!(
                "image `{}` called port {port}; its entry lists {} ({}), numbered from 1 in name order",
                self.name,
                names.len(),
                names.join(", ")
            )));
        };
        let answer = ports
            .call(name, request)
            .map_err(|f| f.into_error(ports.stage()))?;
        let reply = match &answer {
            PortAnswer::Ok(bytes) => Ok(&bytes[..]),
            PortAnswer::Error(said) => {
                let line = format!("port `{name}` refused the call: {said}\n");
                if let Some((sink, stage)) = &self.out {
                    sink.bytes(Some(*stage), OutStream::Stderr, line.as_bytes());
                }
                Err(KernelError::AccessDenied)
            }
            PortAnswer::Lost(why) => {
                return Err(kernel_error(format!(
                    "image `{}`: port `{name}`: {why}",
                    self.name
                )));
            }
        };
        match self.plane.answer(call, reply) {
            Ok(()) | Err(PlaneError::Closed) => Ok(()),
            Err(e) => Err(plane_error(&self.name, "answering a port call", e)),
        }
    }

    /// [`Self::wait_any`] past the task's releases: a record, or the end.
    fn wait(&mut self, ports: Option<&Arc<StagePorts>>, names: &[String]) -> Result<Seen> {
        loop {
            match self.wait_any(ports, names)? {
                Seen::Released(_) => {}
                seen => return Ok(seen),
            }
        }
    }

    /// The task's answer to the batch `t` names in ring mode: every record it puts until it
    /// releases the batch, which says it is done. One record is handed on as it lies; several
    /// (an answer bigger than a window) are joined into one batch. The first is kept in place
    /// when it may be, and each after it lands at once, so its window goes back for the next.
    /// A record the run's arena refused stays in `t`, which the caller keeps for the unit
    /// applied again: nothing taken is lost and nothing is taken twice.
    fn answers(
        &mut self,
        t: &mut Taking,
        ports: Option<&Arc<StagePorts>>,
        names: &[String],
    ) -> Result<Payload> {
        let (started, waited) = (moruna_svc::now(), self.timed[WAIT]);
        let answer = self.answers_timed(t, ports, names);
        let spent = moruna_svc::now().saturating_sub(started);
        self.timed[TAKE] += spent.saturating_sub(self.timed[WAIT] - waited);
        answer
    }

    fn answers_timed(
        &mut self,
        t: &mut Taking,
        ports: Option<&Arc<StagePorts>>,
        names: &[String],
    ) -> Result<Payload> {
        if let Some(out) = t.refused.take() {
            self.part(t, out)?;
        }
        while !t.released {
            match self.wait_any(ports, names)? {
                Seen::Put(out) => {
                    self.received += 1;
                    self.part(t, out)?;
                }
                Seen::Released(b) if b == t.offered => t.released = true,
                Seen::Released(_) => {}
                Seen::Ended => {
                    let cause = format!(
                        "it ended after answering {} of the {} batches it was sent",
                        self.answered, self.sent
                    );
                    return Err(self.failed(&cause));
                }
            }
        }
        let answer = match t.parts.len() {
            0 => {
                return Err(self.failed(
                    "it said a batch was done without a record for it: a task answers every \
                     batch, with a record of no rows when it keeps none",
                ));
            }
            1 => t
                .parts
                .pop()
                .ok_or_else(|| kernel_error(format!("image `{}`: no part", self.name)))?,
            _ => {
                let joined = self.join(&t.parts)?;
                // The parts' windows go back once the joined batch has landed.
                t.parts.clear();
                self.reap();
                self.joined += 1;
                joined
            }
        };
        self.answered += 1;
        Ok(answer)
    }

    /// Takes record `out` as the next part of the answer `t`: the first decoded in place when
    /// it may be, each after it landed. A record the run's arena refused is kept in `t`.
    fn part(&mut self, t: &mut Taking, out: BatchId) -> Result<()> {
        let part = if t.parts.is_empty() {
            self.output(out)
        } else {
            self.land(out)
        };
        match part {
            Ok(part) => {
                t.parts.push(part);
                Ok(())
            }
            Err(e) => {
                t.refused = Some(out);
                Err(e)
            }
        }
    }

    /// Ring mode: `batch` goes to the task in pieces of at most its plane's piece, queued and
    /// posted as the task has room ([`layout::IN_FLIGHT`] in flight), and the stage answers
    /// with every answer it has taken meanwhile, joined, the oldest first. It waits for an
    /// answer only for the task's first (whose schema stands for the rest) and to make room for
    /// pieces still queued, since it returns with none queued ([`layout::QUEUED_MAX`]); with
    /// no answer come it answers with a batch of no rows, and the answers come with a later
    /// batch, or at the stage's end. A morsel of [`layout::IN_FLIGHT`] pieces is posted at
    /// once, so one engine worker keeps every task of the stage busy.
    ///
    /// A unit the run's arena refused is applied again with the same batch (RB5): it goes on
    /// from where it stopped ([`TaskState::applying`]), with the answers it had taken
    /// ([`TaskState::ready`]) and the one it was taking ([`TaskState::taking`]).
    fn pipelined(
        &mut self,
        batch: &moruna_kernel::arrow::record_batch::RecordBatch,
        ports: Option<&Arc<StagePorts>>,
        names: &[String],
    ) -> Result<Payload> {
        let piece = layout::ring_sizes(self.plane.residency().budget).piece;
        if self.schema.is_none() {
            self.schema = lock(&self.busy.schema).clone();
        }
        if !self.applying {
            let started = moruna_svc::now();
            let pieces = self.pieces(batch, piece)?;
            self.timed[CUT] += moruna_svc::now().saturating_sub(started);
            self.queued.extend(pieces);
            self.applying = true;
        }
        if self.taking.is_some() {
            let answer = self.take_oldest(ports, names)?;
            self.ready.push_back(answer);
        }
        self.feed(ports, names)?;
        if self.schema.is_none() && self.ready.is_empty() {
            let answer = self.take_oldest(ports, names)?;
            self.ready.push_back(answer);
            self.feed(ports, names)?;
        }
        loop {
            // The answers that have come, without waiting for any: each makes room for one
            // the task has queued.
            self.gather(ports, names)?;
            let mut came = false;
            while let Some(&oldest) = self.in_flight.front()
                && self
                    .early
                    .iter()
                    .any(|seen| matches!(seen, Seen::Released(b) if *b == oldest))
            {
                let answer = self.take_oldest(ports, names)?;
                self.ready.push_back(answer);
                came = true;
            }
            self.feed(ports, names)?;
            // Nothing stays queued past this unit: a queued piece holds its morsel's memory,
            // and under memory pressure no unit may come to post it ([`layout::QUEUED_MAX`]).
            if layout::holds_worker(self.queued.len()) {
                let answer = self.take_oldest(ports, names)?;
                self.ready.push_back(answer);
                self.feed(ports, names)?;
            } else if !came {
                break;
            }
        }
        debug_assert!(self.queued.is_empty(), "a unit left pieces queued");
        let answer = match self.ready.len() {
            0 => {
                let schema = self.schema.clone().ok_or_else(|| {
                    kernel_error(format!("image `{}`: no answer's schema yet", self.name))
                })?;
                self.deferred += 1;
                Payload::Table(
                    moruna_kernel::arrow::record_batch::RecordBatch::new_empty(schema),
                    moruna_kernel::Tier::Host,
                )
            }
            1 => self
                .ready
                .pop_front()
                .ok_or_else(|| kernel_error(format!("image `{}`: no answer", self.name)))?,
            _ => self.join_ready()?,
        };
        self.applying = false;
        Ok(answer)
    }

    /// The answers taken and not given, joined; kept when the run's arena refuses the join.
    fn join_ready(&mut self) -> Result<Payload> {
        let ready: Vec<Payload> = std::mem::take(&mut self.ready).into();
        match self.join(&ready) {
            Ok(joined) => {
                // Their windows go back once the joined batch has landed.
                drop(ready);
                self.reap();
                Ok(joined)
            }
            Err(e) => {
                self.ready = ready.into();
                Err(e)
            }
        }
    }

    /// Posts the task as many of its queued pieces, oldest first, as it has room for
    /// ([`layout::to_post`]). A piece whose layout failed (the run's arena refused its
    /// framing) goes back to the front of the queue, for the unit applied again.
    fn feed(&mut self, ports: Option<&Arc<StagePorts>>, names: &[String]) -> Result<()> {
        for _ in 0..layout::to_post(self.in_flight.len(), self.queued.len()) {
            let Some(part) = self.queued.pop_front() else {
                break;
            };
            let offered = match self.offer(&part, ports, names) {
                Ok((_, offered)) => offered,
                Err(e) => {
                    self.queued.push_front(part);
                    return Err(e);
                }
            };
            self.sent += 1;
            self.count_busy(true);
            self.in_flight.push_back(offered);
        }
        Ok(())
    }

    /// The answer to the oldest batch in flight, waited for; or the one a refusal interrupted.
    fn take_oldest(
        &mut self,
        ports: Option<&Arc<StagePorts>>,
        names: &[String],
    ) -> Result<Payload> {
        let mut taking = match self.taking.take() {
            Some(taking) => taking,
            None => Taking {
                offered: self.in_flight.pop_front().ok_or_else(|| {
                    kernel_error(format!("image `{}`: no batch in flight", self.name))
                })?,
                parts: Vec::new(),
                refused: None,
                released: false,
            },
        };
        let answer = match self.answers(&mut taking, ports, names) {
            Ok(answer) => answer,
            Err(e) => {
                // Taken on from here when the unit is applied again.
                self.taking = Some(taking);
                return Err(e);
            }
        };
        self.count_busy(!self.in_flight.is_empty());
        if let (None, Payload::Table(batch, _)) = (&self.schema, &answer) {
            let schema = batch.schema();
            lock(&self.busy.schema).get_or_insert_with(|| Arc::clone(&schema));
            self.schema = Some(schema);
        }
        Ok(answer)
    }

    /// `batch` as the records the engine lays out for the task: itself when its record fits a
    /// piece, else slices of its rows that each do ([`layout::cut`], cut again where a slice's
    /// record is still past a piece). A slice is a view of the batch's buffers: nothing is
    /// copied until the record is laid out in the region.
    fn pieces(
        &self,
        batch: &moruna_kernel::arrow::record_batch::RecordBatch,
        piece: u64,
    ) -> Result<Vec<moruna_kernel::arrow::record_batch::RecordBatch>> {
        let len = self.record_len(batch)?;
        if len <= piece || batch.num_rows() <= 1 {
            return Ok(vec![batch.clone()]);
        }
        let mut out = Vec::new();
        for (at, rows) in layout::cut(batch.num_rows(), len, piece) {
            let part = batch.slice(at, rows);
            if rows <= 1 || self.record_len(&part)? <= piece {
                out.push(part);
            } else {
                out.extend(self.pieces(&part, piece)?);
            }
        }
        Ok(out)
    }

    /// The bytes of `batch`'s record as the engine lays it out.
    fn record_len(&self, batch: &moruna_kernel::arrow::record_batch::RecordBatch) -> Result<u64> {
        let (framing, placed) =
            moruna_kernel::ipc::encode_framing(batch, PAGE as usize, 0, self.alloc.as_ref())?;
        let pieces: Vec<(u64, u64)> = placed
            .iter()
            .map(|(at, b)| (*at as u64, b.len() as u64))
            .collect();
        layout::record_len(framing.len() as u64, &pieces)
            .ok_or_else(|| kernel_error(format!("image `{}`: a record past u64", self.name)))
    }

    /// Takes what the task has done by now, without waiting: its records and releases go to
    /// [`Self::early`], in order, and its port calls are served.
    fn gather(&mut self, ports: Option<&Arc<StagePorts>>, names: &[String]) -> Result<()> {
        let started = moruna_svc::now();
        let gathered = self.gather_timed(ports, names);
        self.timed[POLL] += moruna_svc::now().saturating_sub(started);
        gathered
    }

    fn gather_timed(&mut self, ports: Option<&Arc<StagePorts>>, names: &[String]) -> Result<()> {
        loop {
            // A poll: it looks, and never spins or sleeps.
            let event = self
                .plane
                .next(0)
                .map_err(|e| plane_error(&self.name, "looking at the task", e))?;
            match event {
                None => return Ok(()),
                Some(Event::Put { batch, .. }) => {
                    self.window = None;
                    self.early.push_back(Seen::Put(batch));
                }
                Some(Event::Released { batch, .. }) => self.early.push_back(Seen::Released(batch)),
                Some(Event::Ended { .. }) => {
                    self.early.push_back(Seen::Ended);
                    return Ok(());
                }
                Some(Event::Call {
                    call,
                    port,
                    request,
                }) => self.call(ports, names, call, port, request)?,
                Some(Event::Refused { reason, .. }) => {
                    return Err(self.failed(&format!(
                        "its record on its output was refused by the engine's check: {reason}"
                    )));
                }
                Some(Event::Freed { .. }) => {}
            }
        }
    }

    /// Several records answering one batch, joined into one batch in the run's arena: the one
    /// answer copied twice, once to join and once to land, each copy counted. The parts stay
    /// the caller's, who drops them (their windows going back) once this has landed, and keeps
    /// them when the run's arena refused it.
    fn join(&mut self, parts: &[Payload]) -> Result<Payload> {
        let mut batches = Vec::with_capacity(parts.len());
        for p in parts {
            match p {
                Payload::Table(b, _) => batches.push(b.clone()),
                Payload::Tensor(..) => {
                    return Err(kernel_error(format!(
                        "image `{}`: a record that is not a table",
                        self.name
                    )));
                }
            }
        }
        let schema = batches[0].schema();
        let joined =
            moruna_kernel::arrow::compute::concat_batches(&schema, &batches).map_err(|e| {
                kernel_error(format!(
                    "image `{}`: joining the {} records of one batch: {e}",
                    self.name,
                    batches.len()
                ))
            })?;
        drop(batches);
        self.alloc
            .note_boundary_copy(joined.get_array_memory_size() as u64);
        Ok(moruna_adapters::land::land_table(joined, &self.alloc)?.0)
    }

    /// Steps the plane until the task puts a record, releases a batch or ends, serving its
    /// port calls, looking at the run's cancel and at how the task ended meanwhile.
    fn wait_any(&mut self, ports: Option<&Arc<StagePorts>>, names: &[String]) -> Result<Seen> {
        let started = moruna_svc::now();
        let seen = self.wait_any_timed(ports, names);
        self.timed[WAIT] += moruna_svc::now().saturating_sub(started);
        seen
    }

    fn wait_any_timed(
        &mut self,
        ports: Option<&Arc<StagePorts>>,
        names: &[String],
    ) -> Result<Seen> {
        if let Some(seen) = self.early.pop_front() {
            return Ok(seen);
        }
        let mut notes = layout::WaitNotes::begin(moruna_svc::now());
        loop {
            self.reap();
            let deadline = moruna_svc::now().saturating_add(LOOK_NS);
            let event = self
                .plane
                .next(deadline)
                .map_err(|e| plane_error(&self.name, "waiting on the task", e))?;
            self.drain_log();
            match event {
                Some(Event::Put { batch, .. }) => {
                    self.window = None;
                    return Ok(Seen::Put(batch));
                }
                Some(Event::Refused { reason, .. }) => {
                    return Err(self.failed(&format!(
                        "its record on its output was refused by the engine's check: {reason}"
                    )));
                }
                Some(Event::Call {
                    call,
                    port,
                    request,
                }) => self.call(ports, names, call, port, request)?,
                Some(Event::Ended { .. }) => return Ok(Seen::Ended),
                Some(Event::Released { batch, .. }) => return Ok(Seen::Released(batch)),
                Some(Event::Freed { .. }) => {}
                None => {
                    if self.cancelled() {
                        return Err(MorunaError::Cancelled);
                    }
                    match self.answer(0) {
                        Some(Err(why)) => {
                            let why = why.clone();
                            return Err(kernel_error(format!("image `{}`: {why}", self.name)));
                        }
                        Some(Ok(ended)) if ended.how != how::EXITED || ended.code != 0 => {
                            // This is the ORIGINAL decoded Linux terminal reply,
                            // not inferred EOF from a closed slot. A failed child
                            // cannot answer another morsel; preserve its status
                            // and drained stderr instead of waiting forever for
                            // an independent plane-drain notification. Successful
                            // termination still waits for the real plane EOF.
                            return Err(self.failed("the task reported unsuccessful termination"));
                        }
                        _ => {}
                    }
                    if let Some(waited) = notes.due(moruna_svc::now()) {
                        self.say_waiting(waited);
                    }
                }
            }
        }
    }

    /// One line, every [`layout::WAIT_NOTE_NS`] of a wait with no answer come: the stage, the
    /// task, what the stage holds of it, and each of its windows as the plane has it. To the
    /// engine's log and the stage's output, which the run's events carry.
    fn say_waiting(&mut self, waited: u64) {
        let windows = self.plane.window_states(self.task);
        let line = layout::wait_note(
            &self.name,
            self.instance,
            waited,
            (self.in_flight.len(), self.queued.len(), self.held),
            windows.as_deref(),
        );
        tracing::warn!(target: "micro", stage = self.stage, "{line}");
        if let Some((sink, stage)) = &self.out {
            sink.bytes(
                Some(*stage),
                OutStream::Stderr,
                &layout::line(line.as_bytes()),
            );
        }
    }
}

/// Writes a record into `memory`: its framing at 0, each body buffer at its offset, the gaps
/// zero, through one writable mapping of the record's pages.
fn write_record(
    memory: &moruna_svc::Memory,
    len: u64,
    framing: &[u8],
    placed: &[(usize, ArrowBuffer)],
) -> std::result::Result<(), KernelError> {
    let span = layout::pages(len).ok_or(KernelError::OutOfRange)?;
    let mut mapping = memory.map(0, span, map::READ | map::WRITE)?;
    let region = mapping.shared_region()?;
    let len = len as usize;
    region[..framing.len()].copy_from_slice(framing);
    let mut end = framing.len();
    for (at, buffer) in placed {
        let bytes = buffer.as_slice();
        if *at < end || at + bytes.len() > len {
            return Err(KernelError::InvalidArgs);
        }
        region[end..*at].fill(0);
        region[*at..at + bytes.len()].copy_from_slice(bytes);
        end = at + bytes.len();
    }
    region[end..len].fill(0);
    Ok(())
}

fn with_stderr(head: String, lines: &[String]) -> String {
    if lines.is_empty() {
        return format!("{head}; it wrote nothing to its standard error");
    }
    let n = lines.len().min(TAIL_LINES);
    let quoted: Vec<String> = lines[lines.len() - n..]
        .iter()
        .map(|l| format!("  {l}"))
        .collect();
    format!(
        "{head}; the last {n} lines of its standard error:\n{}",
        quoted.join("\n")
    )
}

impl Kernel for ImageTask {
    fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    /// One task process per instance: one per CPU of the job, at most the `MORUNA_PROCESSES`
    /// the image declares, as many as the run's memory funds (system design 3E).
    fn kind(&self) -> KernelKind {
        KernelKind::Stateful {
            max_instances: core::num::NonZeroUsize::new(self.instances())
                .unwrap_or(core::num::NonZeroUsize::MIN),
        }
    }

    fn hints(&self) -> KernelHints {
        KernelHints::default()
    }

    fn state_uses_arena(&self) -> bool {
        false
    }

    /// A ring stage's tasks keep batches in flight of their own, so its instances take its
    /// batches in turn and one worker keeps every task busy.
    fn deals_instances(&self) -> bool {
        self.rings
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    /// The input's schema, as on Linux: the task's own is its first record's.
    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        self.accepts().check(input)?;
        Ok(input.clone())
    }

    fn init(&self, ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        Ok(Box::new(self.start(ctx)?))
    }

    /// SDD 19 f.6 on the kernel: one record to the task and one back, in lockstep; in ring
    /// mode in pieces, with at most [`layout::IN_FLIGHT`] in flight, the answers coming with later
    /// batches ([`TaskState::pipelined`]).
    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        self.exchange(state, input, 0)
    }

    /// SDD 19 f.6, f.7 on the kernel: the input ends; the task puts nothing more, ends its
    /// stream and exits 0. It gives the answers still in flight (ring mode), joined into one
    /// batch, and nothing else: every batch an image writes answers one it was sent.
    fn finish(&self, state: &mut dyn KernelState) -> Result<Option<Payload>> {
        (|| -> Result<Option<Payload>> {
            let st = state
                .as_any_mut()
                .downcast_mut::<TaskState>()
                .ok_or_else(|| kernel_error("image: a state this kernel did not make".into()))?;
            if !self.memory.iter().any(|m| Arc::ptr_eq(&st.memory, m)) {
                return Err(kernel_error(
                    "image: state belongs to another actual task owner".into(),
                ));
            }
            // The answers a unit took and did not give first, then every piece the task has
            // queued, as it has room, and every answer back.
            loop {
                st.feed(self.ports.get(), &self.port_names)?;
                if st.in_flight.is_empty() && st.taking.is_none() {
                    break;
                }
                let answer = st.take_oldest(self.ports.get(), &self.port_names)?;
                st.ready.push_back(answer);
            }
            let rest = match st.ready.len() {
                0 => None,
                1 => st.ready.pop_front(),
                _ => Some(st.join_ready()?),
            };
            st.plane
                .finish(st.task, 0)
                .map_err(|e| plane_error(&st.name, "ending the task's input", e))?;
            match st.wait(self.ports.get(), &self.port_names)? {
                Seen::Put(out) => {
                    let _ = st.plane.drop_batch(out);
                    let cause = if st.rings {
                        format!(
                            "it put a record after it said the last of its {} batches was done",
                            st.sent
                        )
                    } else {
                        format!(
                            "its output holds more record batches than the {} sent",
                            st.sent
                        )
                    };
                    Err(st.failed(&cause))
                }
                Seen::Released(_) => Err(st.failed("it released a batch it was not sent")),
                Seen::Ended => match st.answer(END_SECONDS * SECONDS) {
                    Some(Ok(e)) if e.how == how::EXITED && e.code == 0 => {
                        let (served, refused) = (e.served, e.refused.clone());
                        st.drain_log();
                        let footprint = st.footprint();
                        let c = st.plane.counters();
                        tracing::info!(
                            target: "micro",
                            image = %st.name,
                            served,
                            refused = ?refused,
                            stage = st.stage,
                            %footprint,
                            rings = st.rings,
                            native = st.process.is_some(),
                            batches = st.sent,
                            records = st.received,
                            joined = st.joined,
                            copied = st.copied,
                            kept = st.kept,
                            landed = st.landed,
                            revokes = c.revokes,
                            regrants = c.regrants,
                            generations = c.generations,
                            waits = c.waits,
                            sleeps = c.sleeps,
                            bells = c.bells,
                            zeroed = st.plane.plane.zeroed(),
                            deferred = st.deferred,
                            cpu = ?st.cpu,
                            busy_peak = self.busy.peak.load(std::sync::atomic::Ordering::Relaxed),
                            "image task exited 0"
                        );
                        st.record();
                        Ok(rest)
                    }
                    _ => Err(st.failed("it ended")),
                },
            }
        })()
    }
}
