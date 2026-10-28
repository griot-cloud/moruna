//! An OCI image's command as a kernel (SDD 19): one confined process per stage, started from an
//! unpacked image, exchanging Arrow IPC with Moruna over standard input and output.
//!
//! The contract with the process (SDD 19 f.6): Moruna writes one Arrow IPC stream to its standard
//! input, the schema, then one record batch for each `apply`, then the end-of-stream marker at
//! `finish`. The process writes one Arrow IPC stream to its standard output, the schema, then
//! exactly one record batch for each it read, in order; when its input ends it ends its stream
//! and exits with status 0. A run that applied no batch ends the input without a byte, and the
//! process writes nothing and exits with status 0.

mod output;
mod spawn;

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use moruna_adapters::{UNKNOWN_SEQ, UNKNOWN_STAGE};
use moruna_image::Image;
use moruna_kernel::arrow::ipc::writer::StreamWriter;
use moruna_kernel::declare::Declared;
use moruna_kernel::{
    Allocator, Fingerprint, InitCtx, Kernel, KernelHints, KernelKind, KernelState, MorunaError,
    OutsideMemory, Payload, PayloadKind, PayloadSpec, Result, Sampler, SourceSchema, TierPref,
};
use seccompiler::BpfProgram;

use crate::STOP_GRACE;
use crate::cgroup::StageCgroup;
use crate::output::{TAIL_LINES, Tail, follow_stderr};
use output::{Next, Output};
use spawn::{Plan, spawn};

/// The size of an image's `/tmp` (SDD 19 f.5 step 4): the stage has no scratch setting yet.
pub const TMP_BYTES: u64 = 64 << 20;
/// How long an image has to exit once its input has ended (SDD 19 f.7).
pub const EXIT_WAIT: Duration = Duration::from_secs(60);
/// How long the end of a child's standard error is waited for once the child is gone.
const STDERR_WAIT: Duration = Duration::from_millis(500);
/// The `PATH` a command is looked for in when the image's environment names none.
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
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
    fn in_use(&self) -> u64 {
        lock(&self.cgroup)
            .as_ref()
            .map_or(0, |c| c.memory_current().unwrap_or(0))
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

/// The user an image runs as, from its config (SDD 19 IM-I4): root, or a numeric uid and gid.
fn identity(user: &str) -> Result<(u32, u32)> {
    let numeric = |s: &str| -> Result<u32> {
        s.parse::<u32>().map_err(|_| {
            config(format!(
                "the image's user `{user}` is not numeric: name its user by uid (and gid), for \
                 the sandbox has no passwd to look a name up in"
            ))
        })
    };
    match user {
        "" | "root" => Ok((0, 0)),
        _ => match user.split_once(':') {
            Some((u, g)) => Ok((numeric(u)?, numeric(g)?)),
            None => {
                let uid = numeric(user)?;
                Ok((uid, uid))
            }
        },
    }
}

/// The image's environment and the settings', each `NAME=value`, a later name replacing an
/// earlier one; the process gets this and nothing of Moruna's.
fn environment(
    image: &[String],
    settings: &BTreeMap<String, String>,
) -> Result<Vec<(String, String)>> {
    let mut env: Vec<(String, String)> = Vec::new();
    let mut put = |name: &str, value: &str| match env.iter_mut().find(|(n, _)| n == name) {
        Some(entry) => entry.1 = value.to_string(),
        None => env.push((name.to_string(), value.to_string())),
    };
    for entry in image {
        let Some((name, value)) = entry.split_once('=') else {
            return Err(config(format!(
                "the image's Env entry `{entry}` is not NAME=value"
            )));
        };
        put(name, value);
    }
    for (name, value) in settings {
        put(name, value);
    }
    Ok(env)
}

fn cstring(text: &str, what: &str) -> Result<CString> {
    CString::new(text).map_err(|_| config(format!("{what} holds a NUL byte")))
}

/// The paths `command` is tried at: itself when it names a directory, else one per `PATH`
/// entry.
fn candidates(command: &str, path: &str) -> Result<Vec<CString>> {
    if command.contains('/') {
        return Ok(vec![cstring(command, "the command")?]);
    }
    path.split(':')
        .map(|dir| {
            let dir = if dir.is_empty() { "." } else { dir };
            cstring(&format!("{dir}/{command}"), "the command")
        })
        .collect()
}

/// An image's command as a kernel.
pub struct ImageKernel {
    name: String,
    fingerprint: Fingerprint,
    rootfs: PathBuf,
    cgroup: PathBuf,
    uid: u32,
    gid: u32,
    working_dir: String,
    command: Vec<String>,
    env: Vec<(String, String)>,
    filters: Vec<BpfProgram>,
    binding: Mutex<Option<Binding>>,
    memory: Arc<ImageMemory>,
}

impl std::fmt::Debug for ImageKernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageKernel")
            .field("name", &self.name)
            .field("command", &self.command)
            .finish()
    }
}

impl ImageKernel {
    /// The kernel of `image`'s command, or `command` where the settings replace it, with the
    /// settings' `env` added to the image's, in stages whose cgroups are made under `cgroup`.
    pub fn new(
        name: &str,
        image: &Image,
        command: Option<Vec<String>>,
        env: &BTreeMap<String, String>,
        cgroup: PathBuf,
    ) -> Result<ImageKernel> {
        let command = command.unwrap_or_else(|| image.config.command());
        let Some(first) = command.first() else {
            return Err(config(format!(
                "{name}: the image names no command (no Entrypoint and no Cmd) and the settings \
                 give none"
            )));
        };
        let (uid, gid) = identity(&image.config.user)?;
        let env = environment(&image.config.env, env)?;
        let working_dir = if image.config.working_dir.is_empty() {
            "/".to_string()
        } else {
            image.config.working_dir.clone()
        };
        if !working_dir.starts_with('/') {
            return Err(config(format!(
                "{name}: the image's working directory `{working_dir}` is not absolute"
            )));
        }
        let path = env
            .iter()
            .find(|(n, _)| n == "PATH")
            .map_or(DEFAULT_PATH, |(_, v)| v.as_str());
        candidates(first, path)?;
        let rootfs = image
            .rootfs
            .canonicalize()
            .map_err(|e| config(format!("{}: {e}", image.rootfs.display())))?;
        let filters = crate::seccomp::image_filters().map_err(config)?;
        let identity = serde_json::json!({
            "image": image.digest.to_string(), "command": command, "env": env,
            "working_dir": working_dir, "user": image.config.user,
        });
        Ok(ImageKernel {
            name: name.to_string(),
            fingerprint: Fingerprint::compute(
                "moruna-sandbox::ImageKernel",
                identity.to_string().as_bytes(),
            ),
            rootfs,
            cgroup,
            uid,
            gid,
            working_dir,
            command,
            env,
            filters,
            binding: Mutex::new(None),
            memory: Arc::new(ImageMemory::default()),
        })
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
        let sample = binding.sampler.sample();
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
        let plan = Plan {
            cgroup: cgroup.dir().to_path_buf(),
            rootfs: self.rootfs.clone(),
            uid: self.uid,
            gid: self.gid,
            working_dir: cstring(&self.working_dir, "the working directory")?,
            candidates: candidates(
                &self.command[0],
                self.env
                    .iter()
                    .find(|(n, _)| n == "PATH")
                    .map_or(DEFAULT_PATH, |(_, v)| v.as_str()),
            )?,
            argv: self
                .command
                .iter()
                .map(|a| cstring(a, "the command"))
                .collect::<Result<_>>()?,
            envp: self
                .env
                .iter()
                .map(|(k, v)| cstring(&format!("{k}={v}"), "the environment"))
                .collect::<Result<_>>()?,
            tmp_bytes: TMP_BYTES,
            filters: self.filters.clone(),
        };
        let started = spawn(&plan).map_err(|e| {
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
        tracing::info!(target: "sandbox", pid = started.pid, image = %self.name, "image process started");
        Ok(ImageState {
            child: Child {
                pid: started.pid,
                input: Input::Pending(started.stdin),
                output: Output::new(started.stdout),
                tail,
                cgroup,
                sent: 0,
                received: 0,
                exit: None,
                name: self.name.clone(),
            },
            alloc: Arc::clone(&ctx.alloc),
            ceiling,
            memory: Arc::clone(&self.memory),
        })
    }
}

/// Moruna's end of the process's standard input.
enum Input {
    /// Nothing written yet: the schema goes with the first batch.
    Pending(File),
    /// The stream is open.
    Open(StreamWriter<File>),
    /// The stream has ended.
    Closed,
}

/// One stage's process, Moruna's side.
struct Child {
    pid: libc::pid_t,
    input: Input,
    output: Output<File>,
    tail: Arc<Tail>,
    cgroup: Arc<StageCgroup>,
    sent: u64,
    received: u64,
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

    /// The failure of a call or of the end of input, in words: how the process ended when it
    /// ended, `cause` when it did not. `ended` says the cause is the process being gone (a
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
        let head = match by_itself {
            Some(exit) => describe(exit, self.cgroup.oom_kills() > 0, &self.name),
            None => format!("image `{}`: {cause}", self.name),
        };
        kernel_error(with_stderr(head, &lines))
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

/// A stage's state: its process, and the arena outputs land in.
pub struct ImageState {
    child: Child,
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
    fn footprint(&self) -> Option<u64> {
        self.child.cgroup.memory_current().ok()
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

    /// SDD 19 f.6: one batch to the process and one back, in lockstep.
    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        let st = state
            .as_any_mut()
            .downcast_mut::<ImageState>()
            .ok_or_else(|| kernel_error("image: a state this kernel did not make".into()))?;
        let Payload::Table(batch, _) = input else {
            return Err(MorunaError::Plan(
                "an image takes a table; its batches cross as Arrow IPC".into(),
            ));
        };
        let own = st.child.cgroup.memory_current().unwrap_or(0);
        let ceiling = self.stage_limits(own)?.0;
        if ceiling != st.ceiling {
            st.ceiling = self.write_limits(&st.child.cgroup, own)?;
        }
        let limit = st.ceiling;
        st.child.sent += 1;
        let child = &mut st.child;
        // The write and the read go together: a process that starts answering before it has read
        // all of a large batch is not left to wait for a reader that is waiting on the writer.
        let (written, read) = std::thread::scope(|scope| {
            let input = &mut child.input;
            let writer = scope.spawn(|| write_batch(input, &batch));
            let read = child.output.next(limit);
            let written = writer
                .join()
                .unwrap_or_else(|_| Err("the writer of the input panicked".into()));
            (written, read)
        });
        drop(batch);
        let out = match (written, read) {
            (_, Ok(Next::Batch(out))) => out,
            (Err(e), Ok(Next::End)) => {
                return Err(child.failed(true, format!("its input could not be written: {e}")));
            }
            (_, Ok(Next::End)) => {
                return Err(child.failed(
                    true,
                    format!(
                        "its output ended after {} batches, for {} sent",
                        child.received, child.sent
                    ),
                ));
            }
            (_, Err(why)) => {
                return Err(child.failed(
                    false,
                    format!("its output is not an Arrow IPC stream: {why}"),
                ));
            }
        };
        child.received += 1;
        let landed = moruna_adapters::land::land_table(out, &st.alloc)?;
        Ok(landed.0)
    }

    /// SDD 19 f.6, f.7: the input ends; the output must end where the input did and the process
    /// must exit with status 0.
    fn finish(&self, state: &mut dyn KernelState) -> Result<()> {
        let st = state
            .as_any_mut()
            .downcast_mut::<ImageState>()
            .ok_or_else(|| kernel_error("image: a state this kernel did not make".into()))?;
        let child = &mut st.child;
        match std::mem::replace(&mut child.input, Input::Closed) {
            Input::Open(mut writer) => {
                if let Err(e) = writer.finish() {
                    return Err(child.failed(true, format!("its input could not be ended: {e}")));
                }
            }
            Input::Pending(_) | Input::Closed => {}
        }
        // Whatever the process writes after its last answer is the end of its stream and
        // nothing else.
        match child.output.next(st.ceiling) {
            Ok(Next::End) => {}
            Ok(Next::Batch(_)) => {
                return Err(child.failed(
                    false,
                    format!(
                        "its output holds more record batches than the {} sent",
                        child.sent
                    ),
                ));
            }
            Err(why) => {
                return Err(child.failed(
                    false,
                    format!("its output is not an Arrow IPC stream: {why}"),
                ));
            }
        }
        match child.reap(EXIT_WAIT) {
            Some(Exit::Code(0)) => Ok(()),
            Some(exit) => {
                let lines = child.tail.lines(STDERR_WAIT);
                let oom = child.cgroup.oom_kills() > 0;
                Err(kernel_error(with_stderr(
                    describe(exit, oom, &child.name),
                    &lines,
                )))
            }
            None => {
                child.kill();
                let lines = child.tail.lines(STDERR_WAIT);
                Err(kernel_error(with_stderr(
                    format!(
                        "image `{}` did not exit within {} seconds of the end of its input",
                        child.name,
                        EXIT_WAIT.as_secs()
                    ),
                    &lines,
                )))
            }
        }
    }
}

/// One batch on the process's input: the schema first when it is the first.
fn write_batch(
    input: &mut Input,
    batch: &moruna_kernel::arrow::record_batch::RecordBatch,
) -> std::result::Result<(), String> {
    if let Input::Pending(_) = input
        && let Input::Pending(file) = std::mem::replace(input, Input::Closed)
    {
        let writer = StreamWriter::try_new(file, &batch.schema()).map_err(|e| e.to_string())?;
        *input = Input::Open(writer);
    }
    match input {
        Input::Open(writer) => writer.write(batch).map_err(|e| e.to_string()),
        _ => Err("the input has ended".into()),
    }
}

#[cfg(test)]
mod tests;
