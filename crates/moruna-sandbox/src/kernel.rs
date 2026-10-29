//! A `.pex` package as a kernel, Moruna's side (17 d.1, e.1, f.7, f.8): one confined child per
//! stage, forked from the warm template, batches through the shared mapping.

use std::io::{self, Cursor};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use moruna_adapters::{UNKNOWN_SEQ, UNKNOWN_STAGE};
use moruna_kernel::arrow::ipc::writer::StreamWriter;
use moruna_kernel::declare::Declared;
use moruna_kernel::{
    Allocator, Fingerprint, InitCtx, Kernel, KernelAlloc, KernelHints, KernelKind, KernelState,
    MorunaError, OutsideMemory, Payload, PayloadKind, PayloadSpec, Result, ResumePolicy, Sampler,
    SourceSchema, TierPref, set_kernel_alloc,
};
use moruna_package::{Format, Inputs, Package, Tree};

use crate::cgroup::StageCgroup;
use crate::mapping::{Mapping, PAGE, field};
use crate::protocol::{FromChild, ToChild, ToTemplate, read_frame, write_frame};
use crate::regions::Regions;
use crate::template::{Template, template};
use crate::{STOP_GRACE, SandboxHost};

/// How long a call waits for its kernel to release a leased input before it is refused (17 i,
/// SB-I13).
pub const LEASE_WAIT: Duration = Duration::from_secs(10);
/// How often a waiting call asks the child for released leases.
pub const LEASE_POLL: Duration = Duration::from_millis(10);

/// Names this process's stage cgroups and mount points apart, across kernels.
static STAGES: AtomicU64 = AtomicU64::new(0);

/// How long a child has to confine itself and load its package.
const READY_TIMEOUT: Duration = Duration::from_secs(120);

fn kernel_error(msg: String) -> MorunaError {
    MorunaError::Kernel {
        stage: UNKNOWN_STAGE,
        seq: UNKNOWN_SEQ,
        msg,
        features: None,
    }
}

fn config(msg: String) -> MorunaError {
    MorunaError::Config {
        name: "sandbox",
        msg,
    }
}

/// What a run's ceiling is computed from (f.7).
#[derive(Clone)]
struct Binding {
    sampler: Arc<dyn Sampler>,
    ceiling: u64,
}

/// What Moruna and the sampler share about one child.
struct Shared {
    mapping: Mapping,
    cgroup: StageCgroup,
}

impl Shared {
    /// The child's memory now: the larger of what it last wrote and what its cgroup holds
    /// (SB-I9).
    fn in_use(&self) -> u64 {
        let page = self.mapping.counter(field::IN_USE).load(Ordering::Acquire);
        let cgroup = self.cgroup.memory_current().unwrap_or(0);
        page.max(cgroup)
    }
}

/// The sampler's view of a stage's child (SB-I9): zero until the child exists.
#[derive(Default)]
struct StageMemory {
    child: Mutex<Option<Arc<Shared>>>,
}

impl OutsideMemory for StageMemory {
    fn in_use(&self) -> u64 {
        lock(&self.child).as_ref().map_or(0, |c| c.in_use())
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A `.pex` package as a kernel.
pub struct SandboxKernel {
    package: Arc<Package>,
    host: SandboxHost,
    tree: Tree,
    target: String,
    template: Arc<Template>,
    hints: KernelHints,
    declared: Declared,
    stateful: bool,
    notes: Vec<String>,
    binding: Mutex<Option<Binding>>,
    memory: Arc<StageMemory>,
    inputs: Inputs,
}

impl std::fmt::Debug for SandboxKernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxKernel")
            .field("package", &self.package)
            .field("target", &self.target)
            .finish()
    }
}

impl SandboxKernel {
    /// The package's tree, the template for `host.python`, and the target this host offers the
    /// package (16 f.6, f.7; 17 f.9).
    pub fn new(package: Arc<Package>, host: SandboxHost, inputs: Inputs) -> Result<SandboxKernel> {
        inputs.check(&package.manifest).map_err(MorunaError::from)?;
        if package.format != Format::Pex {
            return Err(config(format!(
                "{}: only a .pex runs in the sandbox",
                package.path.display()
            )));
        }
        let template = template(&host.python).map_err(config)?;
        let offered = template.hello.target().map_err(config)?;
        let target = moruna_package::host::choose(&package.manifest.platforms, &offered)
            .map_err(MorunaError::from)?
            .to_string();
        let tree = moruna_package::tree(&package).map_err(MorunaError::from)?;
        let manifest = &package.manifest;
        let h = manifest.hints();
        let resume = match h.resume.as_deref() {
            Some("checkpoint") => ResumePolicy::Checkpoint,
            Some("forbid") => ResumePolicy::Forbid,
            _ => ResumePolicy::Reinit,
        };
        let hints = KernelHints {
            expected_amplification: h.expected_amplification,
            uses_device_memory: false,
            releases_gil: None,
            preferred_rows: h.preferred_rows,
            resume,
            state_bytes: h.state_bytes,
        };
        let declared = manifest.declared().map_err(MorunaError::from)?;
        let notes = tree.notes.clone();
        Ok(SandboxKernel {
            stateful: h.stateful,
            package,
            host,
            tree,
            target,
            template,
            hints,
            declared,
            notes,
            binding: Mutex::new(None),
            memory: Arc::new(StageMemory::default()),
            inputs,
        })
    }

    /// The package.
    pub fn package(&self) -> &Arc<Package> {
        &self.package
    }

    /// What a run should say (a tree's place).
    pub fn notes(&self) -> Vec<String> {
        self.notes.clone()
    }

    /// The target whose wheels the child imports.
    pub fn target(&self) -> &str {
        &self.target
    }

    /// The run's sampler and its starting ceiling, which the stage ceiling is computed from
    /// (f.7). A child is not started before this.
    pub fn bind_run(&self, sampler: Arc<dyn Sampler>, ceiling: u64) {
        *lock(&self.binding) = Some(Binding { sampler, ceiling });
    }

    /// The child's memory, for the run's sampler (SB-I9).
    pub fn outside(&self) -> Arc<dyn OutsideMemory> {
        Arc::clone(&self.memory) as Arc<dyn OutsideMemory>
    }

    /// The limits moved (MH 4.4): the stage ceiling is recomputed and sent now.
    pub fn limits_changed(&self) {
        let child = lock(&self.memory.child).clone();
        if let Some(shared) = child
            && let Err(e) = self.send_ceiling(&shared)
        {
            tracing::warn!(target: "sandbox", "the stage ceiling could not be sent: {e}");
        }
    }

    /// `(stage ceiling, cpu quota)` from a fresh sample (f.7).
    fn stage_limits(&self, own: u64) -> Result<(u64, f64)> {
        let binding = lock(&self.binding).clone().ok_or_else(|| {
            config(format!(
                "{}: the stage has no ceiling; the run binds it before the child starts",
                self.package.manifest.name
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

    /// Write the stage ceiling to the page (the child's gate reads it) and then `memory.max`,
    /// when it moved.
    fn send_ceiling(&self, shared: &Shared) -> Result<u64> {
        let (ceiling, cores) = self.stage_limits(shared.in_use())?;
        let page = shared.mapping.counter(field::CEILING);
        if page.load(Ordering::Acquire) != ceiling {
            page.store(ceiling, Ordering::Release);
            shared
                .cgroup
                .set_memory_max(ceiling)
                .map_err(|e| config(e.to_string()))?;
            shared
                .cgroup
                .set_cpu_max(cores)
                .map_err(|e| config(e.to_string()))?;
        }
        Ok(ceiling)
    }

    /// e.1: the cgroup, the mapping, the fork, the confinement, the package loaded.
    fn start_child(&self) -> Result<Child> {
        let (ceiling, cores) = self.stage_limits(0)?;
        if ceiling == 0 {
            return Err(config(format!(
                "{}: the stage ceiling is 0: the rest of the run holds the whole ceiling",
                self.package.manifest.name
            )));
        }
        let n = STAGES.fetch_add(1, Ordering::Relaxed);
        // The pid, the moment and a counter: a name no earlier process left behind can hold.
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let name = format!("stage-{}-{at}-{n}", std::process::id());
        let cgroup = StageCgroup::create(&self.host.cgroup, &name, ceiling, cores)
            .map_err(|e| config(format!("the stage's cgroup: {e}")))?;
        let mapping = Mapping::create(ceiling).map_err(|e| config(format!("the mapping: {e}")))?;
        mapping
            .counter(field::CEILING)
            .store(ceiling, Ordering::Release);
        let root =
            std::env::temp_dir().join(format!("moruna-sandbox-{}-{at}-{n}", std::process::id()));
        std::fs::create_dir_all(&root).map_err(|e| config(format!("{}: {e}", root.display())))?;
        let (ours, theirs) = UnixStream::pair().map_err(|e| config(format!("socketpair: {e}")))?;
        let output = crate::output::ChildOutput::open()
            .map_err(|e| config(format!("the child's output pipes: {e}")))?;
        let request = ToTemplate::Start {
            tree: self.tree.root.display().to_string(),
            target: self.target.clone(),
            entry: self.package.manifest.entry.clone(),
            manifest: serde_json::from_str(&self.package.manifest.to_json())
                .map_err(|e| config(e.to_string()))?,
            cgroup: cgroup.dir().display().to_string(),
            root: root.display().to_string(),
            mapping_bytes: mapping.len() as u64,
            libraries: self.template.hello.libraries.clone(),
            args: self.package.manifest.hints().args.then(|| {
                self.inputs
                    .args
                    .clone()
                    .unwrap_or_else(|| serde_json::json!({}))
            }),
            assets: self.inputs.assets.as_ref().map(|a| a.display().to_string()),
        };
        let pid = self
            .template
            .start(
                &request,
                theirs.as_raw_fd(),
                mapping.fd().as_raw_fd(),
                output.write_ends(),
            )
            .map_err(|e| config(format!("the sandbox template: {e}")))?;
        drop(theirs);
        // What the child prints is the stage's whose worker started it (MH 4.3 `log`), read
        // until the child is gone.
        output.read(
            moruna_kernel::output::current(),
            &self.package.manifest.name,
        );
        let pidfd = pidfd_open(pid).map_err(|e| config(format!("pidfd_open {pid}: {e}")))?;
        let data = (mapping.len() - PAGE) as u64;
        let shared = Arc::new(Shared { mapping, cgroup });
        let mut child = Child {
            regions: Regions::new(PAGE as u64, data),
            pid,
            pidfd: Some(pidfd),
            control: ours,
            shared,
            root,
            template: Arc::clone(&self.template),
            dead: None,
            name: self.package.manifest.name.clone(),
        };
        child
            .control
            .set_read_timeout(Some(READY_TIMEOUT))
            .map_err(|e| config(e.to_string()))?;
        match child.receive() {
            Ok(FromChild::Ready) => {}
            Ok(FromChild::Failed { reason }) => {
                return Err(config(format!(
                    "{}: the .pex child {pid} did not start: {reason}",
                    self.package.manifest.name
                )));
            }
            Ok(other) => {
                return Err(config(format!(
                    "the .pex child {pid} said {other:?} before it was ready"
                )));
            }
            Err(e) => return Err(e),
        }
        child
            .control
            .set_read_timeout(None)
            .map_err(|e| config(e.to_string()))?;
        *lock(&self.memory.child) = Some(Arc::clone(&child.shared));
        tracing::info!(target: "sandbox", pid, kernel = %child.name, "child ready");
        Ok(child)
    }
}

fn pidfd_open(pid: u32) -> io::Result<OwnedFd> {
    // SAFETY: pidfd_open takes a pid and flags and returns a new descriptor or -1.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor the kernel just gave this process.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

/// One stage's child, Moruna's side.
struct Child {
    pid: u32,
    pidfd: Option<OwnedFd>,
    control: UnixStream,
    shared: Arc<Shared>,
    root: PathBuf,
    template: Arc<Template>,
    /// Set when the child ended: every later call fails with it (SB-I11).
    dead: Option<String>,
    name: String,
    /// The data area's free ranges and the inputs leased to the child (SB-I13).
    regions: Regions,
}

impl Child {
    fn send(&mut self, message: &ToChild) -> Result<()> {
        if let Some(dead) = &self.dead {
            return Err(kernel_error(dead.clone()));
        }
        let mut writer = &self.control;
        write_frame(&mut writer, message).map_err(|e| self.died(e.to_string()))
    }

    fn receive(&mut self) -> Result<FromChild> {
        if let Some(dead) = &self.dead {
            return Err(kernel_error(dead.clone()));
        }
        let mut reader = &self.control;
        read_frame::<FromChild>(&mut reader).map_err(|e| self.died(e.to_string()))
    }

    /// The child ended or broke the protocol: ask the template how it ended, kill it if it
    /// lives on, and remember the reason for every later call (SB-I11).
    fn died(&mut self, why: String) -> MorunaError {
        let deadline = Instant::now() + STOP_GRACE;
        let status = loop {
            match self.template.status(self.pid) {
                Ok(status) if status != "running" => break status,
                Ok(_) if Instant::now() >= deadline => {
                    self.kill();
                    break self
                        .template
                        .status(self.pid)
                        .unwrap_or_else(|e| format!("killed; its status is unknown: {e}"));
                }
                Ok(_) => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => break format!("status unknown: {e}"),
            }
        };
        let message = format!(
            "the .pex child {} of {} ended ({status}): {why}",
            self.pid, self.name
        );
        self.dead = Some(message.clone());
        kernel_error(message)
    }

    fn kill(&mut self) {
        if let Some(pidfd) = &self.pidfd {
            // SAFETY: a pidfd this process holds; a signal number; no siginfo.
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    pidfd.as_raw_fd(),
                    libc::SIGKILL,
                    core::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            }
        }
    }

    /// Leases the child released: their ranges are free again, their pages given back.
    fn absorb(&mut self, released: &[u64]) {
        for id in released {
            if let Some(range) = self.regions.release(*id) {
                self.punch(range);
            }
        }
    }

    /// Give back the whole pages of a free range (e.4): never a page a leased input shares.
    fn punch(&self, (offset, len): (u64, u64)) {
        if let Err(e) = self.shared.mapping.punch(offset as usize, len as usize) {
            tracing::warn!(target: "sandbox", "the mapping's free pages were not given back: {e}");
        }
    }

    /// Give back the pages of the free range holding `offset`, where an output or a state was.
    fn punch_free_at(&self, offset: u64) {
        if let Some(range) = self.regions.free_at(offset) {
            self.punch(range);
        }
    }

    /// Ask the child which leases it has released (it collects garbage first); how many.
    fn ask_releases(&mut self) -> Result<usize> {
        self.send(&ToChild::Release)?;
        match self.receive()? {
            FromChild::Released { released } => {
                self.absorb(&released);
                Ok(released.len())
            }
            other => Err(kernel_error(format!(
                "release: the child answered {other:?}"
            ))),
        }
    }

    /// The data area's size.
    fn data_bytes(&self) -> u64 {
        (self.shared.mapping.len() - PAGE) as u64
    }

    /// Write `bytes` into the largest free range, unleased (state for `open`).
    fn place(&self, bytes: &[u8]) -> Result<(u64, u64)> {
        let (offset, cap) = self.regions.largest_free().unwrap_or((PAGE as u64, 0));
        if bytes.len() as u64 > cap {
            return Err(kernel_error(format!(
                "{} bytes do not fit the mapping's largest free range of {cap}",
                bytes.len()
            )));
        }
        self.shared
            .mapping
            .bytes_mut(offset as usize, bytes.len())
            .map_err(|e| kernel_error(e.to_string()))?
            .copy_from_slice(bytes);
        Ok((offset, bytes.len() as u64))
    }

    /// The input as one IPC stream, written straight into the largest free range (SB-I7);
    /// `None` when it does not fit there.
    fn try_place_batch(
        &self,
        batch: &moruna_kernel::arrow::record_batch::RecordBatch,
    ) -> Result<Option<(u64, u64)>> {
        let Some((offset, cap)) = self.regions.largest_free() else {
            return Ok(None);
        };
        let area = self
            .shared
            .mapping
            .bytes_mut(offset as usize, cap as usize)
            .map_err(|e| kernel_error(e.to_string()))?;
        let mut cursor = Cursor::new(area);
        let fits = (|| -> std::result::Result<(), moruna_kernel::arrow::error::ArrowError> {
            let mut writer = StreamWriter::try_new(&mut cursor, &batch.schema())?;
            writer.write(batch)?;
            writer.finish()
        })();
        Ok(fits.ok().map(|()| (offset, cursor.position())))
    }

    /// SB-I13: the input placed in a free region and leased to the child. When no free region
    /// is large enough and the child holds leases, wait for it to release them (asking it, which
    /// collects its garbage), and refuse after `LEASE_WAIT` without a release. Returns the lease,
    /// where the input is and the microseconds waited.
    fn lease_batch(
        &mut self,
        batch: &moruna_kernel::arrow::record_batch::RecordBatch,
    ) -> Result<(u64, u64, u64, u64)> {
        let started = Instant::now();
        let mut progress = started;
        let mut waited = false;
        loop {
            if let Some((offset, len)) = self.try_place_batch(batch)? {
                let lease = self.regions.lease(offset, len).map_err(kernel_error)?;
                let us = if waited {
                    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
                } else {
                    0
                };
                return Ok((lease, offset, len, us));
            }
            if self.regions.leased_count() == 0 {
                return Err(kernel_error(format!(
                    "the input batch ({} bytes in memory) does not fit the mapping's data area of \
                     {} bytes, the stage ceiling",
                    batch.get_array_memory_size(),
                    self.data_bytes()
                )));
            }
            if progress.elapsed() >= LEASE_WAIT {
                // The refused call's wait is in its trace record too.
                set_kernel_alloc(KernelAlloc {
                    lease_wait_us: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
                    ..KernelAlloc::NONE
                });
                return Err(MorunaError::Refused {
                    stage: UNKNOWN_STAGE,
                    seq: UNKNOWN_SEQ,
                    kernel: self.name.clone(),
                    requested: batch.get_array_memory_size() as u64,
                    in_use: self.regions.leased_bytes(),
                    ceiling: self.data_bytes(),
                    features: None,
                });
            }
            waited = true;
            if self.ask_releases()? > 0 {
                progress = Instant::now();
            } else {
                std::thread::sleep(LEASE_POLL);
            }
        }
    }

    fn stop(&mut self) {
        if self.dead.is_none() {
            let mut writer = &self.control;
            let _ = write_frame(&mut writer, &ToChild::Stop);
            let _ = self.control.set_read_timeout(Some(STOP_GRACE));
            let mut reader = &self.control;
            // The child exits on `stop`: end of stream is the answer.
            let _ = read_frame::<FromChild>(&mut reader);
        }
        self.kill();
        let _ = self.template.status(self.pid);
        let _ = std::fs::remove_dir(&self.root);
    }
}

/// A stage's state: its child, and the arena outputs land in.
pub struct SandboxState {
    child: Child,
    alloc: Arc<dyn Allocator>,
    stateful_checkpoint: bool,
    memory: Arc<StageMemory>,
}

impl SandboxState {
    /// The child's pid.
    pub fn pid(&self) -> u32 {
        self.child.pid
    }

    /// The child's memory now.
    pub fn in_use(&self) -> u64 {
        self.child.shared.in_use()
    }

    /// The counters page's `peak`.
    pub fn peak(&self) -> u64 {
        self.child
            .shared
            .mapping
            .counter(field::PEAK)
            .load(Ordering::Acquire)
    }

    /// The inputs the child holds now: leases and bytes (SB-I13).
    pub fn leased(&self) -> (usize, u64) {
        (
            self.child.regions.leased_count(),
            self.child.regions.leased_bytes(),
        )
    }

    /// The stage cgroup's directory.
    pub fn cgroup(&self) -> PathBuf {
        self.child.shared.cgroup.dir().to_path_buf()
    }

    /// `memory.max` and `cpu.max` of the stage's cgroup, as written.
    pub fn cgroup_limits(&self) -> io::Result<(String, String)> {
        Ok((
            self.child.shared.cgroup.memory_max()?,
            self.child.shared.cgroup.cpu_max_now()?,
        ))
    }
}

impl Drop for Child {
    /// A child goes when its owner does, whatever the path: stopped, killed if it lingers, its
    /// mount point removed; its cgroup goes with the last `Arc` of `Shared`.
    fn drop(&mut self) {
        self.stop();
    }
}

impl Drop for SandboxState {
    fn drop(&mut self) {
        let mut held = lock(&self.memory.child);
        if held
            .as_ref()
            .is_some_and(|s| Arc::ptr_eq(s, &self.child.shared))
        {
            *held = None;
        }
    }
}

impl KernelState for SandboxState {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    /// SB-I12: the kernel's `checkpoint`, in the child, its bytes through the mapping.
    fn checkpoint(&mut self) -> Result<Option<Vec<u8>>> {
        if !self.stateful_checkpoint {
            return Ok(None);
        }
        let (offset, len) = self
            .child
            .regions
            .largest_free()
            .unwrap_or((PAGE as u64, 0));
        self.child.send(&ToChild::Checkpoint { offset, len })?;
        match self.child.receive()? {
            FromChild::State {
                offset,
                len,
                released,
            } => {
                let bytes = self
                    .child
                    .shared
                    .mapping
                    .bytes(offset as usize, len as usize)
                    .map_err(|e| kernel_error(e.to_string()))?
                    .to_vec();
                self.child.absorb(&released);
                self.child.punch_free_at(offset);
                Ok(Some(bytes))
            }
            FromChild::Error { message, .. } => Err(kernel_error(message)),
            other => Err(kernel_error(format!(
                "checkpoint: the child answered {other:?}"
            ))),
        }
    }

    /// The child's memory and the mapping it holds in leased inputs (SB-I13): what a kernel
    /// that keeps its inputs costs, in the profile.
    fn footprint(&self) -> Option<u64> {
        Some(self.in_use() + self.child.regions.leased_bytes())
    }
}

impl SandboxKernel {
    fn state(&self, ctx: &InitCtx, restore: Option<&[u8]>) -> Result<Box<dyn KernelState>> {
        let mut child = self.start_child()?;
        if let Some(bytes) = restore {
            let (offset, len) = child.place(bytes)?;
            child.send(&ToChild::Open { offset, len })?;
            let opened = child.receive();
            child.punch_free_at(offset);
            match opened? {
                FromChild::Opened => {}
                FromChild::Error { message, .. } => {
                    return Err(MorunaError::Resume(format!("open: {message}")));
                }
                other => {
                    return Err(kernel_error(format!("open: the child answered {other:?}")));
                }
            }
        }
        Ok(Box::new(SandboxState {
            child,
            alloc: Arc::clone(&ctx.alloc),
            stateful_checkpoint: self.stateful && self.hints.resume == ResumePolicy::Checkpoint,
            memory: Arc::clone(&self.memory),
        }))
    }
}

impl Kernel for SandboxKernel {
    fn fingerprint(&self) -> Fingerprint {
        self.package.kernel_fingerprint()
    }

    /// One child per stage (e.1).
    fn kind(&self) -> KernelKind {
        KernelKind::Stateful {
            max_instances: core::num::NonZeroUsize::MIN,
        }
    }

    fn hints(&self) -> KernelHints {
        self.hints.clone()
    }

    fn declared(&self) -> Declared {
        self.declared.clone()
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    /// The input's schema, as for a Python kernel in process (05 d.1).
    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        self.accepts().check(input)?;
        Ok(input.clone())
    }

    fn init(&self, ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        self.state(ctx, None)
    }

    fn restore(&self, ctx: &InitCtx, state: &[u8]) -> Result<Box<dyn KernelState>> {
        if self.hints.resume != ResumePolicy::Checkpoint {
            return Err(MorunaError::Resume(format!(
                "{}: the package does not checkpoint its state",
                self.package.manifest.name
            )));
        }
        self.state(ctx, Some(state))
    }

    /// f.8.
    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        let st = state
            .as_any_mut()
            .downcast_mut::<SandboxState>()
            .ok_or_else(|| kernel_error("sandbox: a state this kernel did not make".into()))?;
        let Payload::Table(batch, _) = input else {
            return Err(MorunaError::Plan(
                "a .pex kernel takes a table; its batches cross as Arrow IPC".into(),
            ));
        };
        self.send_ceiling(&st.child.shared)?;
        let (lease, offset, len, waited) = st.child.lease_batch(&batch)?;
        drop(batch);
        let (out_offset, out_len) = st.child.regions.largest_free().unwrap_or((0, 0));
        st.child.send(&ToChild::Call {
            lease,
            offset,
            len,
            out_offset,
            out_len,
        })?;
        let reply = st.child.receive()?;
        let counted = |alloc: &crate::protocol::AllocDoc| {
            let mut counts = KernelAlloc::from(alloc);
            counts.lease_wait_us = waited;
            set_kernel_alloc(counts);
        };
        match reply {
            FromChild::Done {
                offset,
                len,
                alloc,
                released,
            } => {
                counted(&alloc);
                let batch = read_one(
                    shared_mapping(&st.child.shared),
                    offset as usize,
                    len as usize,
                    &*st.alloc,
                )?;
                let landed = moruna_adapters::land::land_table(batch, &st.alloc);
                st.child.absorb(&released);
                st.child.punch_free_at(offset);
                Ok(landed?.0)
            }
            FromChild::Error {
                message,
                refusal,
                alloc,
                released,
            } => {
                counted(&alloc);
                st.child.absorb(&released);
                if out_len > 0 {
                    st.child.punch_free_at(out_offset);
                }
                Err(match refusal {
                    Some(r) => MorunaError::Refused {
                        stage: UNKNOWN_STAGE,
                        seq: UNKNOWN_SEQ,
                        kernel: self.package.manifest.name.clone(),
                        requested: r.requested,
                        in_use: r.in_use,
                        ceiling: r.ceiling,
                        features: None,
                    },
                    None => kernel_error(message),
                })
            }
            other => Err(kernel_error(format!(
                "the child answered a call with {other:?}"
            ))),
        }
    }
}

/// The output stream at `offset`, decoded in place over the mapping (SB-I7). The child wrote
/// it and the child runs the tenant's code, so every message is checked before arrow-ipc reads
/// it, and a malformed stream is the kernel's error naming the fault, never a panic (SB-I14,
/// contracts CT-I13). A function may compress its output (`LZ4_FRAME` or `ZSTD`, 16 e.5): the
/// check inflates it into `alloc`, within half of what the arena can still give (contracts e.8).
fn read_one(
    owner: crate::mapping::Owner,
    offset: usize,
    len: usize,
    alloc: &dyn Allocator,
) -> Result<moruna_kernel::arrow::record_batch::RecordBatch> {
    let buffer = crate::mapping::arrow_buffer(owner, offset, len)
        .map_err(|e| kernel_error(format!("the output: {e}")))?;
    let inflate = moruna_kernel::ipc::Inflate::share(alloc);
    let mut batches = moruna_kernel::ipc::read_stream(&buffer, &inflate)
        .map_err(|e| kernel_error(format!("the output is not a valid Arrow IPC stream: {e}")))?;
    match batches.len() {
        1 => Ok(batches.remove(0)),
        n => Err(kernel_error(format!(
            "the output stream holds {n} record batches and a call returns exactly one"
        ))),
    }
}

/// The mapping of a child, kept alive by the `Arc` for as long as a buffer over it lives.
fn shared_mapping(shared: &Arc<Shared>) -> crate::mapping::Owner {
    let owner: Arc<dyn crate::mapping::HasMapping> = Arc::clone(shared) as _;
    owner
}

impl crate::mapping::HasMapping for Shared {
    fn mapping(&self) -> &Mapping {
        &self.mapping
    }
}

#[cfg(test)]
mod tests {
    use moruna_kernel::arrow::ipc::MetadataVersion;
    use moruna_testkit::ipc::{corruptions, panics, small, stream, streams, tally, unknown_type};

    use super::*;
    use crate::mapping::{HasMapping, Owner};

    /// A mapping that holds one stream at the start of its data area, as a child leaves one.
    struct Output(Mapping);

    impl HasMapping for Output {
        fn mapping(&self) -> &Mapping {
            &self.0
        }
    }

    fn output(bytes: &[u8]) -> (Owner, usize) {
        let mapping = Mapping::create(bytes.len().max(4096) as u64).expect("a mapping");
        let offset = mapping.data_offset();
        mapping
            .bytes_mut(offset, bytes.len())
            .expect("the data area")
            .copy_from_slice(bytes);
        let owner: Owner = Arc::new(Output(mapping));
        (owner, offset)
    }

    fn read(bytes: Vec<u8>) -> Result<moruna_kernel::arrow::record_batch::RecordBatch> {
        let (owner, offset) = output(&bytes);
        read_one(
            owner,
            offset,
            bytes.len(),
            &moruna_testkit::FakeAllocator::new(),
        )
    }

    /// SB-T26: a child's output whose schema names a type id no Arrow type has is the kernel's
    /// error naming the field, not a panic in the IPC reader's schema converter.
    #[test]
    fn sb_t26_unknown_type_refused() {
        let bytes = unknown_type(&stream(&[small()], MetadataVersion::V5));
        let err = read(bytes).expect_err("an unknown type is refused");
        assert!(matches!(err, MorunaError::Kernel { .. }), "{err:?}");
        assert!(err.to_string().contains("field \"i\""), "{err}");
    }

    /// SB-T26: every corruption and truncation of valid output streams, read in place over a
    /// mapping, is an error or a batch.
    #[test]
    fn sb_t26_malformed_output_never_panics() {
        let mut failed = Vec::new();
        for (label, valid) in streams() {
            let cases = corruptions(&valid, 0x5B26_0000 ^ valid.len() as u64, 300);
            for case in panics(cases, read) {
                failed.push(format!("{label}, {case}"));
            }
        }
        assert!(failed.is_empty(), "{}", tally(&failed));
    }
}
