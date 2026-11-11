//! An OCI image's command as a kernel (SDD 19): one confined process per stage, started from an
//! unpacked image, its batches crossing the data plane (SDD 29, KA2).
//!
//! The stage is a task of a plane of its own (`moruna_plane_linux::LinuxPlane`), one input slot
//! and one output slot. Each `apply` writes the batch once, as a record, into a memfd that the
//! engine seals and checks, offers it on the input, grants a window of the stage's ceiling on
//! the output, and takes the record the task puts back, checked after the task lost write access
//! to it, decoded in place and landed in the run's arena.
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
mod stdio;

pub use process::{ProcessEnd, Started, start_process};

use std::collections::BTreeMap;
use std::ffi::CString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use moruna_adapters::{UNKNOWN_SEQ, UNKNOWN_STAGE};
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

/// The sampler's view of a stage's process (SDD 19 IM-I7): zero until it exists.
#[derive(Default)]
struct ImageMemory {
    cgroup: Mutex<Option<Arc<StageCgroup>>>,
}

impl OutsideMemory for ImageMemory {
    fn in_use(&self) -> Result<u64> {
        match lock(&self.cgroup).as_ref() {
            None => Ok(0),
            Some(cgroup) => cgroup.memory_current().map_err(|e| MorunaError::Io {
                op: "read",
                target: "memory.current".into(),
                msg: e.to_string(),
            }),
        }
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
    /// with `plane`, the command makes the plane's calls itself.
    pub fn new(
        name: &str,
        image: &Image,
        command: Option<Vec<String>>,
        env: &BTreeMap<String, String>,
        cgroup: PathBuf,
        plane: bool,
    ) -> Result<ImageKernel> {
        let launch = moruna_image::Launch::new(name, &image.config, command, env)
            .map_err(|e| config(e.to_string()))?;
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
        })
    }

    /// The ports the task calls (MH 4.10): served on the port socket of every process the
    /// stage starts (`crate::ports`).
    pub fn bind_ports(&self, ports: Arc<moruna_kernel::StagePorts>) {
        *lock(&self.ports) = Some(ports);
    }

    /// The run's sampler and its starting ceiling, which the stage ceiling is computed from
    /// (SDD 17 f.7). A process is not started before this.
    pub fn bind_run(&self, sampler: Arc<dyn Sampler>, ceiling: u64) {
        *lock(&self.binding) = Some(Binding { sampler, ceiling });
    }

    /// The process's memory, for the run's sampler (SDD 19 IM-I7).
    pub fn outside(&self) -> Arc<dyn OutsideMemory> {
        Arc::clone(&self.memory) as Arc<dyn OutsideMemory>
    }

    /// The limits moved (MH 4.4): the stage ceiling is recomputed and written now.
    pub fn limits_changed(&self) {
        let cgroup = lock(&self.memory.cgroup).clone();
        if let Some(cgroup) = cgroup
            && let Err(e) = self.write_limits(&cgroup, cgroup.memory_current().unwrap_or(0))
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

    /// The stage's `memory.max` and `cpu.max`; the ceiling written.
    fn write_limits(&self, cgroup: &StageCgroup, own: u64) -> Result<u64> {
        let (ceiling, cores) = self.stage_limits(own)?;
        cgroup
            .set_memory_max(ceiling)
            .map_err(|e| config(e.to_string()))?;
        cgroup
            .set_cpu_max(cores)
            .map_err(|e| config(e.to_string()))?;
        Ok(ceiling)
    }

    /// SDD 19 f.5: the cgroup, then the process.
    fn start(&self, ctx: &InitCtx) -> Result<ImageState> {
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
        let cgroup_name = format!("image-{}-{at}-{n}", std::process::id());
        let cgroup = Arc::new(
            StageCgroup::create(&self.cgroup, &cgroup_name, ceiling, cores)
                .map_err(|e| config(format!("the stage's cgroup: {e}")))?,
        );
        let ports = lock(&self.ports).clone().filter(|p| !p.is_empty());
        let socket = port_socket(&self.name, ports.is_some(), self.plane)?;
        let plan = plan_of(
            &self.launch,
            &self.rootfs,
            &self.filters,
            cgroup.dir(),
            socket.as_ref().map(|(_, _, env)| env.as_str()),
        )?;
        // The stage's plane: one input, one output, no port. Its budget is a record of the
        // input and a window of the output, each at most the stage's ceiling.
        let mut plane = LinuxPlane::new(ceiling.saturating_mul(2));
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
        *lock(&self.memory.cgroup) = Some(Arc::clone(&cgroup));
        if let (Some(runner), Some(ports)) = (runner_end, ports) {
            crate::ports::serve_on_thread(runner, ports, self.name.clone())
                .map_err(|e| config(format!("{}: the port socket's thread: {e}", self.name)))?;
        }
        tracing::info!(target: "sandbox", pid = started.pid, image = %self.name, "image process started");
        let child = Child {
            pid: started.pid,
            tail,
            cgroup,
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
            memory: Arc::clone(&self.memory),
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
/// its command and its environment (with `extra`, when given), its filters and its cgroup.
fn plan_of(
    launch: &Launch,
    rootfs: &std::path::Path,
    filters: &[BpfProgram],
    cgroup: &std::path::Path,
    extra: Option<&str>,
) -> Result<Plan> {
    let mut env = launch.env_entries();
    env.extend(extra.map(str::to_string));
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

/// A stage's state: its process, its plane and the bridge of its streams, and the arena
/// outputs land in. The process goes first when it is dropped, then the bridge, then the plane.
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
    ceiling: u64,
    memory: Arc<ImageMemory>,
}

impl ImageState {
    /// The process's pid.
    pub fn pid(&self) -> u32 {
        self.child.pid as u32
    }

    /// The stage cgroup's directory.
    pub fn cgroup(&self) -> PathBuf {
        self.child.cgroup.dir().to_path_buf()
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
                 its stage's plane has left (twice the stage's ceiling of {})",
                self.child.name, self.ceiling
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
        match self.plane.grant(self.task, OUTPUT, self.ceiling) {
            Ok(()) => {
                self.granted = true;
                Ok(())
            }
            Err(e) => Err(kernel_error(format!(
                "image `{}`: its output's window of {} bytes: {}",
                self.child.name,
                self.ceiling,
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

impl Drop for ImageState {
    fn drop(&mut self) {
        let mut held = lock(&self.memory.cgroup);
        if held
            .as_ref()
            .is_some_and(|c| Arc::ptr_eq(c, &self.child.cgroup))
        {
            *held = None;
        }
    }
}

impl KernelState for ImageState {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    /// The process's memory, which is the stage's footprint.
    fn footprint(&self) -> Result<Option<u64>> {
        self.child
            .cgroup
            .memory_current()
            .map(Some)
            .map_err(|e| MorunaError::Io {
                op: "read",
                target: "memory.current".into(),
                msg: e.to_string(),
            })
    }
}

impl Kernel for ImageKernel {
    fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    /// One process per stage.
    fn kind(&self) -> KernelKind {
        KernelKind::Stateful {
            max_instances: core::num::NonZeroUsize::MIN,
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

    /// SDD 19 f.6: one batch to the process and one back, in lockstep, across the plane.
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
        let own = st.child.cgroup.memory_current().unwrap_or(0);
        let ceiling = self.stage_limits(own)?.0;
        if ceiling != st.ceiling {
            st.ceiling = self.write_limits(&st.child.cgroup, own)?;
        }
        let record = st.load(&batch)?;
        drop(batch);
        let offered = st.plane.offer(st.task, INPUT, record);
        // The engine's hold ends now: the record is freed once the task has released it.
        let _ = st.plane.drop_batch(record);
        if let Err(e) = offered {
            let words = plane_words(&e, &st.plane);
            return Err(st.failed(true, format!("its input could not be offered: {words}")));
        }
        st.sent += 1;
        st.grant()?;
        match st.next()? {
            Event::Put { batch, .. } => {
                st.granted = false;
                st.received += 1;
                st.take(batch)
            }
            Event::Refused { reason, .. } => Err(st.failed(
                false,
                format!("its output batch was refused by the engine's check: {reason}"),
            )),
            Event::Ended { .. } => {
                let cause = format!(
                    "its output ended after {} batches, for {} sent",
                    st.received, st.sent
                );
                Err(st.failed(true, cause))
            }
            other => Err(st.failed(false, format!("its plane said {other:?}"))),
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
