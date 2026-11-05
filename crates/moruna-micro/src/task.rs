//! An `image` stage on the Moruna kernel (SDD 30 f.5 to f.8): the image's command as a task
//! of the data plane, with SDD 19's contract, the same as on a Linux node.
//!
//! The stage's task is a process the Linux personality starts with `run_plane` (SDD 28 d.3)
//! on its own connection to the personality, whose closing ends it: one input slot, which its
//! descriptor 0 reads as an Arrow IPC stream, one output slot, which its descriptor 1 writes
//! as one, its port, and a log ring for its text. Each `apply` lays the batch out as a
//! page-aligned record in an arena object of the stage's [`KernelPlane`] (one copy of each
//! buffer), offers it to the task (its pages mapped into the task read only, no copy), and
//! waits for the one record the task puts back, which the plane checks before the engine sees
//! it (DP1); the engine decodes it in place and lands it in the run's arena (the one copy
//! every image's output has, as on Linux). `finish` ends the input and requires the task to
//! end its stream and exit 0.
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
use moruna_plane::{BatchId, CallId, DataPlane, Event, KernelPlane, PlaneError, Shape, TaskId};
use moruna_svc::{
    Account, Channel, ClientCore, Encoder, Error as KernelError, Rights, Usage, Wire, map,
};

use crate::layout::{self, PAGE};
use crate::ring::{LogRing, Stream};

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
        Ok(lock(&self.account).as_ref().map_or(0, |(_, limit)| *limit))
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
    /// The task's account, once it has started.
    memory: Arc<TaskMemory>,
    /// Native PEX controls reserve each reply explicitly; ordinary image behavior is unchanged.
    managed_windows: bool,
    /// Source path is only resolved through the actual filesystem capability at start.
    assets_path: Option<std::path::PathBuf>,
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
    /// its arguments and `env` as its whole environment, and may call `port_names`.
    pub fn new(
        name: &str,
        identity: &str,
        path: String,
        command: Vec<String>,
        env: Vec<String>,
        mut port_names: Vec<String>,
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
            memory: Arc::new(TaskMemory::default()),
            managed_windows: false,
            assets_path: None,
        })
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
        if !Arc::ptr_eq(&st.memory, &self.memory) {
            return Err(kernel_error(
                "image: state belongs to another actual task owner".into(),
            ));
        }
        let Payload::Table(batch, _) = input else {
            return Err(MorunaError::Plan(
                "an image takes a table; its batches cross the data plane as records".into(),
            ));
        };
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
        // Explicit output admission is complete BEFORE the child can read this request.
        let len = st.offer(&batch)?;
        drop(batch);
        if reply_bytes == 0 {
            st.grant(len)?;
        }
        st.sent += 1;
        match st.wait(self.ports.get(), &self.port_names)? {
            Seen::Put(out) => {
                st.received += 1;
                let landed = st.land(out)?;
                // The next record's room, before the task asks for it.
                if !self.managed_windows {
                    st.grant(len)?;
                }
                Ok(landed)
            }
            Seen::Ended => {
                let cause = format!(
                    "it ended after answering {} of the {} batches it was sent",
                    st.received, st.sent
                );
                Err(st.failed(&cause))
            }
        }
    }

    /// The memory the stage holds outside the engine's process: its task's account.
    pub fn outside(&self) -> Arc<dyn moruna_kernel::OutsideMemory> {
        Arc::clone(&self.memory) as Arc<dyn moruna_kernel::OutsideMemory>
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
    }

    /// The run's ceiling, which bounds the stage's plane (SDD 30 f.6).
    pub fn bind_run(&self, ceiling: u64) {
        *lock(&self.ceiling) = Some(ceiling);
    }

    fn start(&self, ctx: &InitCtx) -> Result<TaskState> {
        let name = &self.name;
        if lock(&self.memory.account).is_some() || lock(&self.memory.failure).is_some() {
            return Err(kernel_error(format!(
                "image `{name}`: previous task account has no certified retirement"
            )));
        }
        let ceiling = lock(&self.ceiling).ok_or_else(|| {
            kernel_error(format!(
                "image `{name}`: the stage has no ceiling; the run binds it before the task starts"
            ))
        })?;
        let budget = ceiling.min(PLANE_BUDGET_MAX);
        let mut plane = KernelPlane::new(Calls, budget);
        let (task, port) = plane
            .open(Shape {
                inputs: 1,
                outputs: 1,
                port: !self.port_names.is_empty(),
            })
            .map_err(|e| plane_error(name, "opening the task's plane", e))?;
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
        // The task's account, carved out of the engine's (SDD 30 f.11).
        let room = Usage::own()
            .map_err(|e| {
                kernel_error(format!(
                    "image `{name}`: the engine's account: {}",
                    e.name()
                ))
            })?
            .room();
        let limit = layout::task_limit(ceiling, room).ok_or_else(|| {
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
        let mut body = Encoder::new();
        let plane_arg = LinuxPlane {
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
        *lock(&self.memory.account) = Some((kept, limit));
        tracing::info!(target: "micro", image = %name, command = ?self.command, "image task asked of the personality");
        Ok(TaskState {
            memory: Arc::clone(&self.memory),
            name: name.clone(),
            plane,
            task,
            client: Some(client),
            run,
            ended: None,
            terminal_confirmed: false,
            ring,
            out: moruna_kernel::output::current(),
            tail: VecDeque::new(),
            alloc: Arc::clone(&ctx.alloc),
            sent: 0,
            received: 0,
            window: None,
            stage: self.ports.get().map_or(UNKNOWN_STAGE, |p| p.stage()),
            cancel: self.cancel.get().cloned(),
        })
    }
}

/// One stage's task, the engine's side.
pub struct TaskState {
    /// Its account, which the stage reads its peak from.
    memory: Arc<TaskMemory>,
    name: String,
    plane: KernelPlane<Calls>,
    task: TaskId,
    /// The task's own connection to the personality: closing it ends the task.
    client: Option<ClientCore>,
    /// The `run_plane` request, answered when the task has ended.
    run: u32,
    ended: Option<std::result::Result<Ended, String>>,
    terminal_confirmed: bool,
    ring: LogRing,
    out: Option<(Arc<dyn OutputSink>, StageId)>,
    tail: VecDeque<String>,
    alloc: Arc<dyn Allocator>,
    sent: u64,
    received: u64,
    /// The bytes of the window the task holds for its next record.
    window: Option<u64>,
    stage: StageId,
    cancel: Option<CancelToken>,
}

impl Drop for TaskState {
    fn drop(&mut self) {
        // Read an already-available ORIGINAL terminal reply, never manufacture one
        // from a close or a transient zero while a start request may still be queued.
        let _ = self.answer(0);
        drop(self.client.take());
        // Release our original plane/record owners before asking whether physical
        // child charges retired. No alias, fake reset or fabricated completion.
        let original = core::mem::replace(&mut self.plane, KernelPlane::new(Calls, 0));
        drop(original);
        let result = (|| -> std::result::Result<(), String> {
            if !self.terminal_confirmed {
                return Err("owner closed without an actual terminal acknowledgement".into());
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let usage = {
                    let held = lock(&self.memory.account);
                    held.as_ref()
                        .map(|(account, _)| account.usage())
                        .transpose()
                        .map_err(|e| format!("actual Account read: {}", e.name()))?
                };
                match usage {
                    Some(usage) if usage.used == 0 && usage.reserved == 0 => {
                        lock(&self.memory.account).take();
                        return Ok(());
                    }
                    None => {
                        return Err(
                            "original Account observer disappeared before retirement".into()
                        );
                    }
                    Some(_) => {}
                }
                if std::time::Instant::now() >= deadline {
                    return Err("actual child Account did not retire within bounded cleanup".into());
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        })();
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
    Ended,
}

/// A mapping of a batch the engine holds, owning the pages an Arrow buffer points into.
struct Mapped(#[allow(dead_code)] moruna_svc::Mapping<'static>);

impl TaskState {
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
        let head = match self.answer(END_SECONDS * SECONDS) {
            Some(Ok(e)) => match e.how {
                how::EXITED => format!("image `{name}`: {cause}; exited with status {}", e.code),
                how::SIGNALED => format!("image `{name}`: {cause}; was ended by signal {}", e.code),
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

    /// Lays `batch` out as a record in an arena object and offers it to the task.
    fn offer(&mut self, batch: &moruna_kernel::arrow::record_batch::RecordBatch) -> Result<u64> {
        let (framing, placed) =
            moruna_kernel::ipc::encode_framing(batch, PAGE as usize, 0, self.alloc.as_ref())?;
        let pieces: Vec<(u64, u64)> = placed
            .iter()
            .map(|(at, b)| (*at as u64, b.len() as u64))
            .collect();
        let len = layout::record_len(framing.len() as u64, &pieces)
            .ok_or_else(|| kernel_error(format!("image `{}`: a record past u64", self.name)))?;
        let (id, _) = self
            .plane
            .load_with(len, |_, memory| {
                write_record(memory, len, &framing, &placed)
            })
            .map_err(|e| plane_error(&self.name, "laying the batch out", e))?;
        self.plane
            .offer(self.task, 0, id)
            .map_err(|e| plane_error(&self.name, "offering the batch", e))?;
        // The task's share keeps it until the task takes its next or ends.
        self.plane
            .drop_batch(id)
            .map_err(|e| plane_error(&self.name, "the engine's hold", e))?;
        Ok(len)
    }

    /// The batch the task put, decoded in place and landed in the run's arena.
    fn land(&mut self, out: BatchId) -> Result<Payload> {
        let (memory, offset, len) = self
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
        let landed = moruna_kernel::ipc::decode(buffer, PAGE as usize)
            .and_then(|b| moruna_adapters::land::land_table(b, &self.alloc));
        self.plane
            .drop_batch(out)
            .map_err(|e| plane_error(&self.name, "dropping the record put", e))?;
        Ok(landed?.0)
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

    /// Steps the plane until the task puts a record or ends, serving its port calls,
    /// looking at the run's cancel and the personality's answer meanwhile.
    fn wait(&mut self, ports: Option<&Arc<StagePorts>>, names: &[String]) -> Result<Seen> {
        loop {
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
                Some(Event::Released { .. } | Event::Freed { .. }) => {}
                None => {
                    if self.cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
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
                }
            }
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

    /// One task per stage.
    fn kind(&self) -> KernelKind {
        KernelKind::Stateful {
            max_instances: core::num::NonZeroUsize::MIN,
        }
    }

    fn hints(&self) -> KernelHints {
        KernelHints::default()
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

    /// SDD 19 f.6 on the kernel: one record to the task and one back, in lockstep.
    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        self.exchange(state, input, 0)
    }

    /// SDD 19 f.6, f.7 on the kernel: the input ends; the task puts nothing more, ends its
    /// stream and exits 0. It gives nothing at its end: every batch an
    /// image writes answers one it was sent.
    fn finish(&self, state: &mut dyn KernelState) -> Result<Option<Payload>> {
        let ended = (|| -> Result<()> {
            let st = state
                .as_any_mut()
                .downcast_mut::<TaskState>()
                .ok_or_else(|| kernel_error("image: a state this kernel did not make".into()))?;
            if !Arc::ptr_eq(&st.memory, &self.memory) {
                return Err(kernel_error(
                    "image: state belongs to another actual task owner".into(),
                ));
            }
            st.plane
                .finish(st.task, 0)
                .map_err(|e| plane_error(&st.name, "ending the task's input", e))?;
            match st.wait(self.ports.get(), &self.port_names)? {
                Seen::Put(out) => {
                    let _ = st.plane.drop_batch(out);
                    let cause = format!(
                        "its output holds more record batches than the {} sent",
                        st.sent
                    );
                    Err(st.failed(&cause))
                }
                Seen::Ended => match st.answer(END_SECONDS * SECONDS) {
                    Some(Ok(e)) if e.how == how::EXITED && e.code == 0 => {
                        let (served, refused) = (e.served, e.refused.clone());
                        st.drain_log();
                        let footprint = st.footprint();
                        tracing::info!(
                            target: "micro",
                            image = %st.name,
                            served,
                            refused = ?refused,
                            stage = st.stage,
                            %footprint,
                            "image task exited 0"
                        );
                        Ok(())
                    }
                    _ => Err(st.failed("it ended")),
                },
            }
        })();
        ended.map(|()| None)
    }
}
