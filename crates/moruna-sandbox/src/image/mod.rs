//! An OCI image's command as a kernel (SDD 19): confined processes started from an unpacked
//! image, their batches crossing the data plane (SDD 29, KA2).
//!
//! A stage runs one process for each CPU of the job unless its image declares fewer (system
//! design 3E, the `stage` module): each is an instance of the stage, in a cgroup of its own
//! under the stage's, with an equal share of the stage's ceiling. A function's memory is
//! measured, never assumed (3A): a stage the run has no measurement of warms up on its first
//! instance before its others start (the `warm` module), and an instance is given at most what
//! its share holds by the measured line, a larger batch cut into slices of its rows whose
//! answers are joined in order.
//!
//! Each instance is a task of a plane of its own (`moruna_plane_linux::LinuxPlane`), one input
//! slot and one output slot. Each `apply` writes the batch once, as a record, into a memfd that
//! the engine seals and checks, offers it on the input, grants a window of the instance's share
//! on the output, and takes the record the task puts back, checked after the task lost write
//! access to it, decoded in place and landed in the run's arena. A plane is made for the stage's
//! ceiling as the instance starts, and a window is the smaller of the instance's share and that
//! ceiling: a share grows past what the plane was made with when a warm-up funds fewer
//! instances than started, and a window past the plane's budget is refused.
//!
//! The command reads and writes its batches one of two ways. Unmodified (the default), it
//! reads one Arrow IPC stream on its standard input and writes one on its standard output (SDD
//! 19 f.6), and the engine's bridge ([`stdio`]) carries both onto the plane: the schema, then
//! one record batch for each `apply`, then the end-of-stream marker at `finish`; it writes the
//! schema, then exactly one record batch for each it read, in order, and when its input ends
//! it ends its stream and exits with status 0. A run that applied no batch ends the input
//! without a byte. With `plane`, the command makes the plane's three calls itself on its
//! descriptor 3 (`moruna_plane_linux::task`), the same contract with no stream and no copy: a
//! record per batch read, its input's end, its exit. Either way a record that fails the check
//! ends the stage with the check's reason.

mod process;
mod spawn;
mod stage;
mod stdio;
mod warm;

pub use process::{ProcessEnd, Started, start_process};

use std::collections::BTreeMap;
use std::ffi::CString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use moruna_adapters::{UNKNOWN_SEQ, UNKNOWN_STAGE, instances};
use moruna_image::{Image, Launch};
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::declare::Declared;
use moruna_kernel::{
    Allocator, Fingerprint, InitCtx, Kernel, KernelHints, KernelKind, KernelState, MorunaError,
    OutsideMemory, Payload, PayloadKind, PayloadSpec, Result, Sampler, SourceSchema, TierPref,
};
use moruna_plane::{DataPlane, Event, PlaneError, Shape, TaskId};
use moruna_plane_linux::LinuxPlane;
use seccompiler::BpfProgram;

use crate::STOP_GRACE;
use crate::cgroup::StageCgroup;
use crate::output::{TAIL_LINES, Tail, follow_stderr};
use spawn::{Plan, Streams, spawn};
use stage::{Busy, Place, Stage};
use stdio::Bridge;

/// The size of an image's `/tmp` (SDD 19 f.5 step 4): the stage has no scratch setting yet.
pub const TMP_BYTES: u64 = 64 << 20;
/// How long an image has to exit once its input has ended (SDD 19 f.7).
pub const EXIT_WAIT: Duration = Duration::from_secs(60);
/// How long the end of a child's standard error is waited for once the child is gone.
const STDERR_WAIT: Duration = Duration::from_millis(500);
/// The page a record's buffers start on.
const PAGE: usize = 4096;
/// The input slot and the output slot of a stage's plane.
const INPUT: u32 = 0;
const OUTPUT: u32 = 0;
/// Names this process's stage cgroups apart, across kernels.
static STAGES: AtomicU64 = AtomicU64::new(0);

fn kernel_error(msg: String) -> MorunaError {
    MorunaError::Kernel {
        stage: UNKNOWN_STAGE,
        seq: UNKNOWN_SEQ,
        msg,
        features: None,
    }
}

fn config(msg: String) -> MorunaError {
    MorunaError::Config { name: "image", msg }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What a run's ceiling is computed from (SDD 17 f.7, which an image's stage follows).
#[derive(Clone)]
struct Binding {
    sampler: Arc<dyn Sampler>,
    ceiling: u64,
}

/// The sampler's view of a stage's processes (SDD 19 IM-I7): what its instances hold, summed;
/// zero until one exists.
#[derive(Default)]
struct ImageMemory {
    /// The stage, while an instance of it lives.
    stage: Mutex<Weak<Stage>>,
}

impl ImageMemory {
    fn stage(&self) -> Option<Arc<Stage>> {
        lock(&self.stage).upgrade()
    }
}

impl OutsideMemory for ImageMemory {
    fn in_use(&self) -> Result<u64> {
        match self.stage() {
            None => Ok(0),
            Some(stage) => stage.memory_current().map_err(memory_unread),
        }
    }
}

/// A `memory.current` that could not be read: an error, never zero.
fn memory_unread(e: std::io::Error) -> MorunaError {
    MorunaError::Io {
        op: "read",
        target: "memory.current".into(),
        msg: e.to_string(),
    }
}

/// How an image's process ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// By `exit` with this status.
    Code(i32),
    /// By this signal.
    Signal(i32),
    /// Reaped by something else: its status is not known.
    Lost,
}

fn cstring(text: &str, what: &str) -> Result<CString> {
    CString::new(text).map_err(|_| config(format!("{what} holds a NUL byte")))
}

/// What a run funds a stage's instances from, and what bounds the batch one of them takes
/// (`ImageKernel::bind_room`; system design 3A, 3E).
#[derive(Clone, Debug)]
struct Room {
    /// The bytes the run has for them past its arena.
    bytes: u64,
    /// The largest batch the run may give an instance, in bytes.
    morsel: u64,
    /// The least morsel the run plans, in bytes.
    least: u64,
    /// What the function holds for each byte of a batch while its memory is not measured: what
    /// it declares, or the run's default.
    factor: f64,
    /// The function's measured memory, when the run has it: what the job came with, else what
    /// the run's store holds.
    line: Option<moruna_cost::Memory>,
}

impl Room {
    /// What each instance needs at the least. A measured function's line at the least morsel,
    /// so every CPU of the job runs an instance and the piece shrinks to fit the share first
    /// (system design 3A: memory bounds the unit of work, never the workers, until the least
    /// piece); an unmeasured one's largest batch times its factor (`instances::need`).
    fn need(&self) -> u64 {
        match &self.line {
            Some(line) => {
                let account = line.account(self.least as f64);
                if account.is_finite() && account >= 0.0 {
                    (account.ceil() as u64).max(stage::LEAST)
                } else {
                    u64::MAX
                }
            }
            None => instances::need(self.morsel, self.factor, stage::LEAST),
        }
    }

    /// The largest batch, in bytes, an instance whose cgroup holds `share` bytes takes: by a
    /// measured function's line, piece <= (share - fixed) / per-byte (`Memory::piece_within`);
    /// for an unmeasured one, the share over its factor. At least one byte; `u64::MAX` for a
    /// line that does not grow.
    fn piece_within(&self, share: u64) -> u64 {
        let piece = match &self.line {
            Some(line) => line.piece_within(share as f64),
            None if self.factor.is_finite() && self.factor > 0.0 => share as f64 / self.factor,
            None => share as f64,
        };
        if piece.is_finite() {
            (piece as u64).max(1)
        } else {
            u64::MAX
        }
    }

    /// The note's words for what bounds an instance's batch at `share`.
    fn bounded(&self, share: u64) -> String {
        let piece = self.piece_within(share);
        match &self.line {
            Some(line) => format!(
                "measured: batches of at most {piece} bytes, what an instance's share of {share} \
                 bytes holds by its function's line ({:.0} bytes plus {:.2} a byte, margin \
                 {:.2}); each instance funded for {} bytes, the line at the least morsel of {} \
                 bytes",
                line.fixed_bytes,
                line.per_input_byte,
                line.margin(),
                self.need(),
                self.least,
            ),
            None => format!(
                "unmeasured: bounded by the share, batches of at most {piece} bytes, an \
                 instance's share of {share} bytes over {:.2}; each instance funded for {} bytes, \
                 the largest batch of {} bytes times {:.2}",
                self.factor,
                self.need(),
                self.morsel,
                self.factor,
            ),
        }
    }
}

/// An image's command as a kernel.
pub struct ImageKernel {
    name: String,
    fingerprint: Fingerprint,
    rootfs: PathBuf,
    cgroup: PathBuf,
    /// The single image-config reader shared with the native engine.
    launch: Launch,
    filters: Vec<BpfProgram>,
    /// The command makes the plane's calls on descriptor 3 rather than reading and writing
    /// streams.
    plane: bool,
    binding: Mutex<Option<Binding>>,
    memory: Arc<ImageMemory>,
    /// The ports the task calls (its needs, MH 4.10), once the build has made them.
    ports: Mutex<Option<Arc<moruna_kernel::StagePorts>>>,
    /// The job's CPUs as the stage's build has them (`budget.cpu`, or the CPUs it finds),
    /// which stand until the run is bound and for a run that says no quota.
    cpus: usize,
    /// The ceiling the image declares (`MORUNA_PROCESSES`), when it declares one.
    declared: Option<usize>,
    /// The instances the rule asks for (system design 3E): one for each CPU of the job, at
    /// most the ceiling the image declares.
    asked: AtomicUsize,
    /// The instances the run's memory funds of them, once the run is bound; 0 before.
    funded: AtomicUsize,
    /// The job's CPUs as the run says them once it is bound, which the instances share.
    job_cpus: AtomicUsize,
    /// What the run funds the stage's instances from, when it says ([`ImageKernel::bind_room`]).
    room: Mutex<Option<Room>>,
    /// Each instance's share of the stage's ceiling as last written, or as the run's binding
    /// expects it before the stage exists; 0 before the run is bound.
    share: AtomicU64,
    /// While the stage warms up on its first instance (system design 3A): only that one is
    /// given work, one piece at a time, and its readings size the others (`warm`).
    warming: AtomicBool,
    /// The warm-up's readings and what it found.
    warm: Mutex<warm::Warm>,
    /// Told the bytes of the run's room for the stage its instances do not need, once a
    /// warm-up has funded them: the run's arena takes them.
    on_sized: OnceLock<Box<dyn Fn(u64) + Send + Sync>>,
    /// Held by the instance that makes the stage's cgroup, so one does.
    starting: Mutex<()>,
    /// What the instances did, for the run's report.
    busy: Arc<Busy>,
}

impl std::fmt::Debug for ImageKernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageKernel")
            .field("name", &self.name)
            .field("command", &self.launch.command)
            .finish()
    }
}

impl ImageKernel {
    /// The kernel of `image`'s command, or `command` where the settings replace it, with the
    /// settings' `env` added to the image's, in stages whose cgroups are made under `cgroup`;
    /// with `plane`, the command makes the plane's calls itself. Its stage runs one instance
    /// for each of the job's CPUs unless its environment declares fewer (`MORUNA_PROCESSES`,
    /// system design 3E): `cpus` of them, as the stage's build has them, until the run is
    /// bound and says its own ([`ImageKernel::bind_run`]).
    pub fn new(
        name: &str,
        image: &Image,
        command: Option<Vec<String>>,
        env: &BTreeMap<String, String>,
        cgroup: PathBuf,
        plane: bool,
        cpus: usize,
    ) -> Result<ImageKernel> {
        let launch = moruna_image::Launch::new(name, &image.config, command, env)
            .map_err(|e| config(e.to_string()))?;
        let declared = instances::declared(&launch.env_entries())
            .map_err(|why| config(format!("{name}: {why}")))?;
        for at in launch.candidates() {
            cstring(&at, "the command")?;
        }
        let rootfs = image
            .rootfs
            .canonicalize()
            .map_err(|e| config(format!("{}: {e}", image.rootfs.display())))?;
        let filters = crate::seccomp::image_filters().map_err(config)?;
        let identity = serde_json::json!({
            "image": image.digest.to_string(), "command": &launch.command, "env": &launch.env,
            "working_dir": &launch.working_dir, "user": image.config.user, "plane": plane,
        });
        Ok(ImageKernel {
            name: name.to_string(),
            fingerprint: Fingerprint::compute(
                "moruna-sandbox::ImageKernel",
                identity.to_string().as_bytes(),
            ),
            rootfs,
            cgroup,
            launch,
            filters,
            plane,
            binding: Mutex::new(None),
            memory: Arc::new(ImageMemory::default()),
            ports: Mutex::new(None),
            cpus,
            declared,
            asked: AtomicUsize::new(instances::count(cpus, declared)),
            funded: AtomicUsize::new(0),
            job_cpus: AtomicUsize::new(cpus.max(1)),
            room: Mutex::new(None),
            share: AtomicU64::new(0),
            warming: AtomicBool::new(false),
            warm: Mutex::new(warm::Warm::default()),
            on_sized: OnceLock::new(),
            starting: Mutex::new(()),
            busy: Arc::new(Busy::default()),
        })
    }

    /// The instances the stage runs: those the rule asks for, one for each CPU of the job and
    /// at most what the image declares, until the run is bound; from then on as many of them
    /// as the run's memory funds ([`ImageKernel::bind_run`]).
    pub fn instances(&self) -> usize {
        match self.funded.load(Ordering::Relaxed) {
            0 => self.asked.load(Ordering::Relaxed),
            funded => funded,
        }
    }

    /// What the stage's instances did, for the run's report: how many there were, the most
    /// that held a batch at once, the batches each answered, and whether memory funded fewer
    /// than the rule asked for.
    pub fn note(&self) -> String {
        let asked = self.asked.load(Ordering::Relaxed);
        let mut note = self.busy.note(&self.name, self.instances(), asked);
        if let Some(said) = self.warm_said() {
            note.push_str(&format!("; {said}"));
        }
        if let Some(room) = lock(&self.room).as_ref() {
            note.push_str(&format!(
                "; {}, of {} bytes past the arena; each told {} of the job's {} CPUs",
                room.bounded(self.share.load(Ordering::Relaxed)),
                room.bytes,
                self.cpu_share(),
                self.job_cpus.load(Ordering::Relaxed),
            ));
        }
        note
    }

    /// Each instance's share of the job's CPUs (`instances::cpu_share`).
    fn cpu_share(&self) -> usize {
        instances::cpu_share(self.job_cpus.load(Ordering::Relaxed), self.instances())
    }

    /// The environment of an instance's process: the image's and the settings', with its share
    /// of the job's CPUs (`MORUNA_CPUS`, and the thread pools the environment does not set:
    /// `instances::with_cpu_share`), and `extra` when given.
    fn instance_env(&self, extra: Option<&str>) -> Vec<String> {
        let mut env = instances::with_cpu_share(&self.launch.env_entries(), self.cpu_share());
        env.extend(extra.map(str::to_string));
        env
    }

    /// What the run funds the stage's instances from (system design 3A, 3E): `room`, the bytes
    /// it has for them past its arena. The run says it before it binds the stage
    /// ([`ImageKernel::bind_run`], which funds them).
    ///
    /// - A function whose memory is measured (`line`, what the job came with or the run's store
    ///   holds) has each instance funded at its line at the `least` morsel, so every CPU of the
    ///   job runs one where that fits, and its batches are cut to what an instance's share
    ///   holds by the line.
    /// - An unmeasured one has each instance funded at the largest batch the run may give it,
    ///   `morsel` bytes, times `factor` (what it declares it holds a byte, or the run's
    ///   default), and its batches are bounded by the share over that factor: with no warm-up
    ///   and no restart here, a batch past its cgroup would end the instance.
    ///
    /// A job that ran in one process runs in as many as that funds, and never fails for the
    /// count.
    pub fn bind_room(
        &self,
        room: u64,
        morsel: u64,
        least: u64,
        factor: f64,
        line: Option<moruna_cost::Memory>,
    ) {
        *lock(&self.room) = Some(Room {
            bytes: room,
            morsel,
            least,
            factor,
            line,
        });
    }

    /// The largest batch one instance takes at `share` bytes, by what the run said of its
    /// function ([`ImageKernel::bind_room`]); `None` when it said nothing, no share is known
    /// yet, or the function's line does not grow.
    fn piece_cap(&self, share: u64) -> Option<u64> {
        if share == 0 {
            return None;
        }
        lock(&self.room)
            .as_ref()
            .map(|room| room.piece_within(share))
            .filter(|&piece| piece < u64::MAX)
    }

    /// The ports the task calls (MH 4.10): served on the port socket of every process the
    /// stage starts (`crate::ports`).
    pub fn bind_ports(&self, ports: Arc<moruna_kernel::StagePorts>) {
        *lock(&self.ports) = Some(ports);
    }

    /// The run's sampler and its starting ceiling, which the stage ceiling is computed from
    /// (SDD 17 f.7). A process is not started before this.
    ///
    /// The run's CPUs and its room are known from here on, and nothing has counted the
    /// stage's instances yet, so this is where they are counted:
    /// - the job's CPUs are the run's CPU quota, rounded up, which is the figure the engine
    ///   starts its workers from, so the stage has an instance for each of them (a sampler
    ///   that says no quota leaves the CPUs the stage's build had);
    /// - memory bounds them: as many of those the rule asks for as the room funds, each at
    ///   its need, when the run said them ([`ImageKernel::bind_room`]); otherwise, and while
    ///   the stage warms up on its first instance ([`ImageKernel::warm_up`], whose measurement
    ///   then funds them), as many as the stage's ceiling funds at the least an instance needs
    ///   (`stage::funded`). A stage of one instance has nothing to warm up for.
    ///
    /// A stage whose instances are already running keeps the instances it has.
    pub fn bind_run(&self, sampler: Arc<dyn Sampler>, ceiling: u64) {
        *lock(&self.binding) = Some(Binding { sampler, ceiling });
        if self.memory.stage().is_some() {
            return;
        }
        // A sample that fails here fails the stage when its first instance starts.
        if let Ok((ceiling, quota)) = self.stage_limits(0) {
            let cpus = instances::cpus(Some(quota), self.cpus);
            let asked = instances::count(cpus, self.declared);
            if asked <= 1 {
                self.warming.store(false, Ordering::Relaxed);
            }
            let (room, need) = match lock(&self.room).as_ref() {
                Some(room) if !self.warming() => (room.bytes, room.need()),
                _ => (ceiling, stage::LEAST),
            };
            let funded = stage::funded(room, asked, need);
            self.job_cpus.store(cpus, Ordering::Relaxed);
            self.asked.store(asked, Ordering::Relaxed);
            self.funded.store(funded, Ordering::Relaxed);
            // The share each instance's cgroup will hold, until the stage writes its own.
            self.share
                .store(stage::share(ceiling, funded), Ordering::Relaxed);
        }
    }

    /// The memory the stage's processes hold, for the run's sampler (SDD 19 IM-I7).
    pub fn outside(&self) -> Arc<dyn OutsideMemory> {
        Arc::clone(&self.memory) as Arc<dyn OutsideMemory>
    }

    /// The limits moved (MH 4.4): the stage ceiling is recomputed and written now, to the
    /// stage and to every instance.
    pub fn limits_changed(&self) {
        if let Some(stage) = self.memory.stage()
            && let Err(e) = self.write_limits(&stage)
        {
            tracing::warn!(target: "sandbox", "an image's stage ceiling could not be written: {e}");
        }
    }

    /// `(stage ceiling, cpu quota)` from a fresh sample (SDD 17 f.7).
    fn stage_limits(&self, own: u64) -> Result<(u64, f64)> {
        let binding = lock(&self.binding).clone().ok_or_else(|| {
            config(format!(
                "{}: the stage has no ceiling; the run binds it before the process starts",
                self.name
            ))
        })?;
        let sample = binding.sampler.sample()?;
        let ceiling = if sample.ceiling_bytes > 0 {
            sample.ceiling_bytes
        } else {
            binding.ceiling
        };
        let rest = sample.anon_bytes.saturating_sub(own);
        Ok((ceiling.saturating_sub(rest), sample.cpu_limit))
    }

    /// The stage's `memory.max` and `cpu.max` from a fresh sample, and every instance's share
    /// of the ceiling, written when they moved; the share.
    fn write_limits(&self, stage: &Stage) -> Result<u64> {
        // What the stage's instances hold is the stage's own: the rest of the run is what the
        // sample holds beside it.
        let own = stage.memory_current().unwrap_or(0);
        let (ceiling, cores) = self.stage_limits(own)?;
        let share = stage
            .write_limits(ceiling, cores)
            .map_err(|e| config(e.to_string()))?;
        self.share.store(share, Ordering::Relaxed);
        Ok(share)
    }

    /// The stage's cgroup, with its ceiling and the run's CPU quota: made by the first instance
    /// to start and kept while one lives.
    fn stage(&self) -> Result<Arc<Stage>> {
        let _one = lock(&self.starting);
        if let Some(stage) = self.memory.stage() {
            return Ok(stage);
        }
        let (ceiling, cores) = self.stage_limits(0)?;
        if ceiling == 0 {
            return Err(config(format!(
                "{}: the stage ceiling is 0: the rest of the run holds the whole ceiling",
                self.name
            )));
        }
        let n = STAGES.fetch_add(1, Ordering::Relaxed);
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let name = format!("image-{}-{at}-{n}", std::process::id());
        let stage = Arc::new(
            Stage::create(&self.cgroup, &name, ceiling, cores, self.instances())
                .map_err(|e| config(format!("the stage's cgroup: {e}")))?,
        );
        *lock(&self.memory.stage) = Arc::downgrade(&stage);
        self.share.store(stage.share(), Ordering::Relaxed);
        Ok(stage)
    }

    /// SDD 19 f.5: the cgroup, then the process. The cgroup is the instance's own, under the
    /// stage's, with its share of the stage's ceiling.
    fn start(&self, ctx: &InitCtx) -> Result<ImageState> {
        // The instance's cgroup goes with its place, whichever way this ends.
        let place = self
            .stage()?
            .enter(ctx.instance)
            .map_err(|e| config(format!("{}: the instance's cgroup: {e}", self.name)))?;
        let ceiling = place.stage().share();
        let ports = lock(&self.ports).clone().filter(|p| !p.is_empty());
        let socket = port_socket(&self.name, ports.is_some(), self.plane)?;
        let plan = plan_of(
            &self.launch,
            &self.rootfs,
            &self.filters,
            place.cgroup().dir(),
            self.instance_env(socket.as_ref().map(|(_, _, env)| env.as_str())),
        )?;
        // The instance's plane: one input, one output, no port. Its budget is a record of the
        // input and a window of the output, each at most the stage's ceiling: the instance's
        // share is at most that, and grows when a warm-up funds fewer instances than started.
        let window_max = place.stage().ceiling();
        let mut plane = LinuxPlane::new(window_max.saturating_mul(2));
        let (task, end) = plane
            .open(Shape {
                inputs: 1,
                outputs: 1,
                port: false,
            })
            .map_err(|e| config(format!("{}: the stage's plane: {e:?}", self.name)))?;
        let (streams, bridged) = if self.plane {
            (Streams::Plane(end), None)
        } else {
            (Streams::Stdio, Some(end))
        };
        let (task_end, runner_end) = match socket {
            Some((task, runner, _)) => (Some(task), Some(runner)),
            None => (None, None),
        };
        let started = spawn(&plan, streams, task_end).map_err(|e| {
            config(format!(
                "{}: the process could not be started: {e}",
                self.name
            ))
        })?;
        let tail = Arc::new(Tail::default());
        // What the process writes on its standard error is its stage's (MH 4.3 `log`), read until
        // it is gone.
        follow_stderr(
            started.stderr,
            moruna_kernel::output::current(),
            &self.name,
            Arc::clone(&tail),
        );
        if let (Some(runner), Some(ports)) = (runner_end, ports) {
            crate::ports::serve_on_thread(runner, ports, self.name.clone())
                .map_err(|e| config(format!("{}: the port socket's thread: {e}", self.name)))?;
        }
        tracing::info!(target: "sandbox", pid = started.pid, image = %self.name, "image process started");
        let child = Child {
            pid: started.pid,
            tail,
            cgroup: Arc::clone(place.cgroup()),
            exit: None,
            name: self.name.clone(),
        };
        let bridge = match (bridged, started.stdin, started.stdout) {
            (Some(end), Some(stdin), Some(stdout)) => Some(
                Bridge::start(end, stdin, stdout)
                    .map_err(|e| config(format!("{}: {e}", self.name)))?,
            ),
            (None, None, None) => {
                // The command is the task: a refused record ends it through its pidfd.
                let pidfd = moruna_plane_linux::sys::pidfd(started.pid as u32)
                    .map_err(|e| config(format!("{}: pidfd_open: {e}", self.name)))?;
                plane
                    .attach(task, pidfd)
                    .map_err(|e| config(format!("{}: the stage's plane: {e:?}", self.name)))?;
                None
            }
            _ => {
                return Err(config(format!(
                    "{}: the process was started with streams its interface does not have",
                    self.name
                )));
            }
        };
        Ok(ImageState {
            child,
            bridge,
            plane,
            task,
            granted: false,
            sent: 0,
            received: 0,
            alloc: Arc::clone(&ctx.alloc),
            ceiling,
            window_max,
            index: ctx.instance,
            busy: Arc::clone(&self.busy),
            place,
        })
    }
}

/// The port socket of a process that calls ports (MH 4.10): the task's end, which the command
/// gets at its port descriptor, Moruna's end, which `crate::ports` serves, and the environment
/// entry that names the descriptor.
fn port_socket(
    name: &str,
    calls: bool,
    plane: bool,
) -> Result<Option<(std::os::fd::OwnedFd, std::os::unix::net::UnixStream, String)>> {
    if !calls {
        return Ok(None);
    }
    let (task, runner) = std::os::unix::net::UnixStream::pair()
        .map_err(|e| config(format!("{name}: the port socket: {e}")))?;
    let env = format!(
        "{}=fd:{}",
        crate::ports::SOCKET_ENV,
        spawn::port_descriptor(plane)
    );
    Ok(Some((std::os::fd::OwnedFd::from(task), runner, env)))
}

/// What the child of an image needs, made before the fork: the image's root, its identity,
/// its command and `env`, its whole environment, its filters and its cgroup.
fn plan_of(
    launch: &Launch,
    rootfs: &std::path::Path,
    filters: &[BpfProgram],
    cgroup: &std::path::Path,
    env: Vec<String>,
) -> Result<Plan> {
    Ok(Plan {
        cgroup: cgroup.to_path_buf(),
        rootfs: rootfs.to_path_buf(),
        uid: launch.uid,
        gid: launch.gid,
        working_dir: cstring(&launch.working_dir, "the working directory")?,
        candidates: launch
            .candidates()
            .iter()
            .map(|c| cstring(c, "the command"))
            .collect::<Result<_>>()?,
        argv: launch
            .command
            .iter()
            .map(|a| cstring(a, "the command"))
            .collect::<Result<_>>()?,
        envp: env
            .iter()
            .map(|e| cstring(e, "the environment"))
            .collect::<Result<_>>()?,
        tmp_bytes: TMP_BYTES,
        filters: filters.to_vec(),
    })
}

/// One stage's process, Moruna's side.
struct Child {
    pid: libc::pid_t,
    tail: Arc<Tail>,
    cgroup: Arc<StageCgroup>,
    exit: Option<Exit>,
    name: String,
}

impl Child {
    /// How the process ended, waiting up to `wait` for it to; `None` while it lives.
    fn reap(&mut self, wait: Duration) -> Option<Exit> {
        if self.exit.is_some() {
            return self.exit;
        }
        let deadline = Instant::now() + wait;
        loop {
            let mut status = 0;
            // SAFETY: waiting without blocking for the child this process made, into a local.
            let r = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
            if r == self.pid {
                self.exit = Some(if libc::WIFSIGNALED(status) {
                    Exit::Signal(libc::WTERMSIG(status))
                } else {
                    Exit::Code(libc::WEXITSTATUS(status))
                });
                return self.exit;
            }
            if r == -1 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                self.exit = Some(Exit::Lost);
                return self.exit;
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// End the process and everything it started.
    fn kill(&mut self) {
        // SAFETY: a signal to the child this process made.
        unsafe { libc::kill(self.pid, libc::SIGKILL) };
        self.cgroup.kill();
        self.reap(STOP_GRACE);
    }

    /// The failure of a call or of the end of input, in words: preserve its actual cause
    /// alongside any observed process exit. `ended` says the cause is the process being gone (a
    /// closed pipe), so it is waited for; a protocol failure waits only a moment, since a
    /// process that writes what is not Arrow is usually alive.
    fn failed(&mut self, ended: bool, cause: String) -> MorunaError {
        let wait = if ended {
            STOP_GRACE
        } else {
            Duration::from_millis(100)
        };
        let by_itself = self.reap(wait);
        if by_itself.is_none() {
            self.kill();
        }
        let lines = self.tail.lines(STDERR_WAIT);
        let head = failure_head(&self.name, &cause, self.exit, self.cgroup.oom_kills() > 0);
        kernel_error(with_stderr(head, &lines))
    }
}

impl Child {
    /// Whether the process exited with status 0 within `wait`; else what happened, with the end
    /// of its standard error. A process still there at the end of `wait` is killed.
    fn ended_well(&mut self, wait: Duration) -> std::result::Result<(), String> {
        match self.reap(wait) {
            Some(Exit::Code(0)) => Ok(()),
            Some(exit) => {
                let lines = self.tail.lines(STDERR_WAIT);
                let oom = self.cgroup.oom_kills() > 0;
                Err(with_stderr(describe(exit, oom, &self.name), &lines))
            }
            None => {
                self.kill();
                let lines = self.tail.lines(STDERR_WAIT);
                Err(with_stderr(
                    format!(
                        "image `{}` did not exit within {} seconds of the end of its input",
                        self.name,
                        wait.as_secs()
                    ),
                    &lines,
                ))
            }
        }
    }
}

impl Drop for Child {
    /// A process goes when its owner does, whatever the path.
    fn drop(&mut self) {
        if self.exit.is_none() {
            self.kill();
        }
    }
}

/// A protocol refusal remains the cause even when the process has already exited
/// successfully or was killed during cleanup. Exit evidence must never erase it.
fn failure_head(name: &str, cause: &str, exit: Option<Exit>, oom: bool) -> String {
    match exit {
        Some(exit) => format!("{}; {cause}", describe(exit, oom, name)),
        None => format!("image `{name}`: {cause}"),
    }
}

/// How a process ended, in words (SDD 19 f.7).
pub fn describe(exit: Exit, oom: bool, name: &str) -> String {
    match exit {
        Exit::Code(code) => format!("image `{name}` exited with status {code}"),
        Exit::Signal(libc::SIGSYS) => format!(
            "image `{name}` made a system call it is not allowed (the process was killed by \
             SIGSYS)"
        ),
        Exit::Signal(libc::SIGKILL) if oom => format!(
            "image `{name}` was killed for using more memory than its stage's limit (SIGKILL by \
             the cgroup)"
        ),
        Exit::Signal(signal) => {
            format!(
                "image `{name}` was killed by {} ({signal})",
                signal_name(signal)
            )
        }
        Exit::Lost => format!("image `{name}` ended and its status is not known"),
    }
}

fn signal_name(signal: i32) -> &'static str {
    match signal {
        libc::SIGKILL => "SIGKILL",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGILL => "SIGILL",
        libc::SIGTERM => "SIGTERM",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGSYS => "SIGSYS",
        _ => "a signal",
    }
}

/// `head` and the last lines of the process's standard error.
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

/// The state of one instance of a stage: its process, its plane and the bridge of its streams,
/// and the arena outputs land in. The process goes first when it is dropped, then the bridge,
/// then the plane, and last its place in the stage: its cgroup, and with the stage's last
/// instance the stage's.
pub struct ImageState {
    child: Child,
    bridge: Option<Bridge>,
    plane: LinuxPlane,
    task: TaskId,
    /// A window is granted on the output and not yet put.
    granted: bool,
    sent: u64,
    received: u64,
    alloc: Arc<dyn Allocator>,
    /// The instance's share of the stage's ceiling: the most its record and its window are.
    ceiling: u64,
    /// The stage's ceiling when the instance started, half its plane's budget: the most a
    /// window is, whatever its share grows to.
    window_max: u64,
    /// Which of the stage's instances this is.
    index: usize,
    busy: Arc<Busy>,
    place: Place,
}

impl ImageState {
    /// The process's pid.
    pub fn pid(&self) -> u32 {
        self.child.pid as u32
    }

    /// The instance's cgroup's directory, under the stage's.
    pub fn cgroup(&self) -> PathBuf {
        self.child.cgroup.dir().to_path_buf()
    }

    /// The stage's cgroup's directory, which carries the stage's ceiling.
    pub fn stage_cgroup(&self) -> PathBuf {
        self.place.stage().dir().to_path_buf()
    }

    /// What the plane holds now (SDD 29 d.4).
    pub fn residency(&self) -> moruna_plane::Residency {
        self.plane.residency()
    }

    /// The stage's error for what the plane said: the process ended or refused, its words
    /// quoted.
    fn failed(&mut self, ended: bool, cause: String) -> MorunaError {
        let cause = match self.bridge.as_ref().and_then(Bridge::fault) {
            Some(fault) => fault,
            None => cause,
        };
        self.child.failed(ended, cause)
    }

    /// One batch to the process and its answer back, in lockstep, across the plane: its record
    /// written and offered, a window granted for the answer, and the record the task puts back
    /// taken and landed.
    fn exchange(&mut self, batch: RecordBatch) -> Result<Payload> {
        let record = self.load(&batch)?;
        drop(batch);
        let offered = self.plane.offer(self.task, INPUT, record);
        // The engine's hold ends now: the record is freed once the task has released it.
        let _ = self.plane.drop_batch(record);
        if let Err(e) = offered {
            let words = plane_words(&e, &self.plane);
            return Err(self.failed(true, format!("its input could not be offered: {words}")));
        }
        self.sent += 1;
        self.grant()?;
        match self.next()? {
            Event::Put { batch, .. } => {
                self.granted = false;
                self.received += 1;
                self.busy.answered(self.index);
                self.take(batch)
            }
            Event::Refused { reason, .. } => Err(self.failed(
                false,
                format!("its output batch was refused by the engine's check: {reason}"),
            )),
            Event::Ended { .. } => {
                let cause = format!(
                    "its output ended after {} batches, for {} sent",
                    self.received, self.sent
                );
                Err(self.failed(true, cause))
            }
            other => Err(self.failed(false, format!("its plane said {other:?}"))),
        }
    }

    /// `batch` as a record, written once into a memfd of the plane and checked there: the
    /// framing, then each buffer at its page.
    fn load(&mut self, batch: &RecordBatch) -> Result<moruna_plane::BatchId> {
        let (framing, buffers) = moruna_kernel::ipc::encode_framing(batch, PAGE, 0, &*self.alloc)?;
        let end = buffers
            .iter()
            .map(|(at, b)| at + b.len())
            .chain(std::iter::once(framing.len()))
            .max()
            .unwrap_or(PAGE);
        // The record ends where its body does, padded to 8 as the body length is.
        let len = end.div_ceil(8) * 8;
        let loaded = self.plane.load_with(len as u64, |window| {
            window[..framing.len()].copy_from_slice(&framing);
            for (at, b) in &buffers {
                window[*at..*at + b.len()].copy_from_slice(b.as_slice());
            }
            Ok(())
        });
        match loaded {
            Ok((id, _)) => Ok(id),
            Err(PlaneError::Budget { asked, free }) => Err(kernel_error(format!(
                "image `{}`: its input batch is {asked} bytes as a record, more than the {free} \
                 its plane has left (twice the stage's ceiling, {}, when it started)",
                self.child.name, self.window_max
            ))),
            Err(e) => Err(kernel_error(format!(
                "image `{}`: its input batch could not be put on its plane: {}",
                self.child.name,
                plane_words(&e, &self.plane)
            ))),
        }
    }

    /// A window of the ceiling on the output, unless one is there.
    fn grant(&mut self) -> Result<()> {
        if self.granted {
            return Ok(());
        }
        // At most the stage's ceiling as the plane was made for it, which a share that grew
        // since may pass.
        let window = self.ceiling.min(self.window_max);
        match self.plane.grant(self.task, OUTPUT, window) {
            Ok(()) => {
                self.granted = true;
                Ok(())
            }
            Err(e) => Err(kernel_error(format!(
                "image `{}`: its output's window of {window} bytes: {}",
                self.child.name,
                plane_words(&e, &self.plane)
            ))),
        }
    }

    /// The next event of the stage's plane that is a put, a refusal or the task's end.
    fn next(&mut self) -> Result<Event> {
        loop {
            match self.plane.next(u64::MAX) {
                Ok(Some(Event::Released { .. } | Event::Freed { .. })) => {}
                // The stage's plane has no port, so a call never reaches here; one that did is
                // refused as a plane without a port refuses it.
                Ok(Some(Event::Call { call, .. })) => {
                    let _ = self.plane.answer(call, Err(moruna_abi::Error::BadHandle));
                }
                Ok(Some(e)) => return Ok(e),
                Ok(None) => {
                    return Err(self.failed(true, "its plane has nothing left to wait on".into()));
                }
                Err(e) => {
                    let words = plane_words(&e, &self.plane);
                    return Err(self.failed(false, format!("its plane failed: {words}")));
                }
            }
        }
    }

    /// The record the task put, decoded in place over the sealed memfd and landed in the
    /// run's arena; the plane's batch dropped.
    fn take(&mut self, batch: moruna_plane::BatchId) -> Result<Payload> {
        let read = read_batch(&mut self.plane, batch);
        let landed = read.and_then(|b| moruna_adapters::land::land_table(b, &self.alloc));
        self.plane
            .drop_batch(batch)
            .map_err(|e| kernel_error(format!("image `{}`: {e:?}", self.child.name)))?;
        Ok(landed?.0)
    }
}

/// The bytes of `batch` an instance is given: its record's framing and the buffers of its rows,
/// without the gaps that put each buffer on its page. A slice is measured by its own rows, which
/// the run's measure of a batch (its buffers' whole allocations) is not.
fn laid_bytes(batch: &RecordBatch, alloc: &dyn Allocator) -> Result<u64> {
    let (framing, buffers) = moruna_kernel::ipc::encode_framing(batch, PAGE, 0, alloc)?;
    Ok(buffers
        .iter()
        .map(|(_, b)| b.len() as u64)
        .fold(framing.len() as u64, u64::saturating_add))
}

/// `batch` as the batches one instance is given when the largest it takes is `cap` bytes
/// (system design 3A: a morsel fits the instance it goes to): itself when nothing bounds it, it
/// is no larger or it has one row; else slices of its rows that each are ([`stage::cut`], cut
/// again where a slice of uneven rows is still past it), in order. A slice is a view of the
/// batch's buffers: nothing is copied until its record is written.
fn pieces(
    batch: &RecordBatch,
    cap: Option<u64>,
    alloc: &dyn Allocator,
) -> Result<Vec<RecordBatch>> {
    let Some(cap) = cap else {
        return Ok(vec![batch.clone()]);
    };
    let len = laid_bytes(batch, alloc)?;
    if len <= cap || batch.num_rows() <= 1 {
        return Ok(vec![batch.clone()]);
    }
    let mut out = Vec::new();
    for (at, rows) in stage::cut(batch.num_rows(), len, cap) {
        out.extend(pieces(&batch.slice(at, rows), Some(cap), alloc)?);
    }
    Ok(out)
}

/// The answers of image `name` to the slices of one batch, joined into one batch in the run's
/// arena, in the order the slices were cut: the answer copied twice, once to join and once to
/// land, each copy counted, as an image task on the Moruna kernel joins its pieces.
fn join(name: &str, answers: Vec<Payload>, alloc: &Arc<dyn Allocator>) -> Result<Payload> {
    let mut batches = Vec::with_capacity(answers.len());
    for answer in &answers {
        match answer {
            Payload::Table(batch, _) => batches.push(batch.clone()),
            Payload::Tensor(..) => {
                return Err(kernel_error(format!(
                    "image `{name}`: an answer that is not a table"
                )));
            }
        }
    }
    drop(answers);
    let schema = batches
        .first()
        .map(RecordBatch::schema)
        .ok_or_else(|| kernel_error(format!("image `{name}`: no answer to join")))?;
    let joined = moruna_kernel::arrow::compute::concat_batches(&schema, &batches).map_err(|e| {
        kernel_error(format!(
            "image `{name}`: joining its answers to the {} slices of one batch: {e}",
            batches.len()
        ))
    })?;
    drop(batches);
    alloc.note_boundary_copy(joined.get_array_memory_size() as u64);
    Ok(moruna_adapters::land::land_table(joined, alloc)?.0)
}

/// `batch` decoded over a read-only mapping of its memfd: the arrays point into the record's
/// pages, which the mapping keeps while they live.
fn read_batch(plane: &mut LinuxPlane, batch: moruna_plane::BatchId) -> Result<RecordBatch> {
    let map = plane
        .map(batch)
        .map_err(|e| kernel_error(format!("the output's record: {}", plane_words(&e, plane))))?;
    let len = map.len();
    let at = core::ptr::NonNull::new(map.addr() as *mut u8)
        .ok_or_else(|| kernel_error("the output's record is mapped at 0".into()))?;
    // SAFETY: `at` is valid for `len` bytes for as long as the mapping lives, and the buffer
    // holds the mapping; the memfd is sealed, so no one writes the bytes.
    let buffer = unsafe {
        moruna_kernel::arrow::buffer::Buffer::from_custom_allocation(at, len, Arc::new(map))
    };
    moruna_kernel::ipc::decode(buffer, PAGE)
}

/// What the plane said, in words.
fn plane_words(e: &PlaneError, plane: &LinuxPlane) -> String {
    match (e, plane.last_error()) {
        (PlaneError::Kernel(k), Some(said)) => format!("{} ({said})", k.name()),
        (PlaneError::Kernel(k), None) => k.name().to_string(),
        (other, _) => format!("{other:?}"),
    }
}

impl KernelState for ImageState {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    /// The process's memory, which is the instance's footprint.
    fn footprint(&self) -> Result<Option<u64>> {
        self.child
            .cgroup
            .memory_current()
            .map(Some)
            .map_err(memory_unread)
    }
}

impl Kernel for ImageKernel {
    fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    /// One process for each instance ([`ImageKernel::instances`]).
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

    fn declared(&self) -> Declared {
        Declared::default()
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    /// The input's schema, as for a Python kernel in process (05 d.1): the process's own is its
    /// first reply's, which the sink adopts.
    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        self.accepts().check(input)?;
        Ok(input.clone())
    }

    fn init(&self, ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        Ok(Box::new(self.start(ctx)?))
    }

    /// Its first instance alone while the stage warms up on it, then those the warm-up's
    /// measurement funded (system design 3A): the others start when their first work comes.
    fn instances_ready(&self) -> usize {
        if self.warming() { 1 } else { self.instances() }
    }

    /// The largest morsel, in input bytes, one instance takes inside its cgroup's share (system
    /// design 3A, item 6.3f): by its function's measured line, piece <= (share - fixed) /
    /// per-byte; for an unmeasured function, the share over its factor. The share is the one
    /// last written, so a ceiling that moved moves it. `None` until the run says what bounds it
    /// ([`ImageKernel::bind_room`]).
    fn morsel_bytes_max(&self) -> Option<u64> {
        self.piece_cap(self.share.load(Ordering::Relaxed))
    }

    /// SDD 19 f.6: one batch to the process and one back, in lockstep, across the plane. A
    /// batch past what an instance takes ([`Kernel::morsel_bytes_max`], at the share just
    /// written) is cut into slices of its rows, each given and answered in turn, and the
    /// answers joined in order: the stage's target bounds only what the source reads for its
    /// first consumer, and a batch past an instance's cgroup would end it.
    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        let st = state
            .as_any_mut()
            .downcast_mut::<ImageState>()
            .ok_or_else(|| kernel_error("image: a state this kernel did not make".into()))?;
        let Payload::Table(batch, _) = input else {
            return Err(MorunaError::Plan(
                "an image takes a table; its batches cross the data plane as records".into(),
            ));
        };
        st.ceiling = self.write_limits(st.place.stage())?;
        // The instance holds the batch from here until its answers are taken, or it fails.
        let _holding = st.busy.hold();
        // The stage's first instance measures its function on its first pieces while the stage
        // warms up (system design 3A).
        if st.index == 0 && self.warming() {
            return self.warm(st, batch);
        }
        let slices = pieces(&batch, self.piece_cap(st.ceiling), &*st.alloc)?;
        drop(batch);
        if slices.len() > 1 {
            st.busy.cut(slices.len());
        }
        let mut answers = Vec::with_capacity(slices.len());
        for slice in slices {
            answers.push(st.exchange(slice)?);
        }
        match answers.len() {
            1 => answers
                .pop()
                .ok_or_else(|| kernel_error(format!("image `{}`: no answer", self.name))),
            _ => join(&self.name, answers, &st.alloc),
        }
    }

    /// SDD 19 f.6, f.7: the input ends; the process may then write one more batch (what its
    /// function's `finish` gives), which goes downstream, and its output must end after that
    /// and the process exit with status 0.
    fn finish(&self, state: &mut dyn KernelState) -> Result<Option<Payload>> {
        let st = state
            .as_any_mut()
            .downcast_mut::<ImageState>()
            .ok_or_else(|| kernel_error("image: a state this kernel did not make".into()))?;
        if let Err(e) = st.plane.finish(st.task, INPUT) {
            let words = plane_words(&e, &st.plane);
            return Err(st.failed(true, format!("its input could not be ended: {words}")));
        }
        let mut last: Option<Payload> = None;
        loop {
            // A window is there for a batch it may write, so that one more is named.
            st.grant()?;
            match st.next()? {
                Event::Ended { .. } => break,
                Event::Put { batch, .. } if last.is_none() => {
                    st.granted = false;
                    last = Some(st.take(batch)?);
                }
                Event::Put { batch, .. } => {
                    let _ = st.plane.drop_batch(batch);
                    let cause = format!(
                        "its output holds more record batches than the {} sent and one at its end",
                        st.sent
                    );
                    return Err(st.failed(false, cause));
                }
                Event::Refused { reason, .. } => {
                    return Err(st.failed(
                        false,
                        format!("its output was refused by the engine's check: {reason}"),
                    ));
                }
                other => return Err(st.failed(false, format!("its plane said {other:?}"))),
            }
        }
        if let Some(fault) = st.bridge.as_ref().and_then(Bridge::fault) {
            return Err(st.child.failed(false, fault));
        }
        st.child.ended_well(EXIT_WAIT).map_err(kernel_error)?;
        Ok(last)
    }
}

#[cfg(test)]
mod tests;
