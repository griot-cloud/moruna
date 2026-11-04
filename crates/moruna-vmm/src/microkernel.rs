//! The Moruna 1.0 production launcher is Linux/x86_64 KVM only, with no fallback.
//! A nondefault explicit TCG proof entry shares the same owned launch machinery,
//! jail included.
//! QEMU supplies the existing q35 PCI and Multiboot boot boundary.
use crate::{
    Budget,
    error::{Result, VmmError},
    jail::{self, Cgroup},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
        net::UnixStream,
    },
    path::{Component, Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

/// An immutable descriptor-bound artifact; paths are relative to its descriptor.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    /// Exact relative file name; absolute paths and traversal are refused.
    pub path: PathBuf,
    /// Exact content length before use.
    pub bytes: u64,
    /// Lowercase hexadecimal SHA256 of the exact content.
    pub sha256: String,
}
/// One explicit, read-only PCI layer disk. Mutable node homes are not inferred.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayerDisk {
    /// Exact disk bytes to attach, copied into the private run directory.
    pub artifact: Artifact,
}
/// An explicitly provisioned persistent NVMe backing file. Contents are mutable
/// and therefore are not mislabeled as an immutable digest-bound boot artifact.
/// The native storage service validates its actual superblock/role/publications.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WritableDisk {
    /// Exact relative regular-file path; never an inferred host block device.
    pub path: PathBuf,
    /// Fixed capacity; launch never truncates or formats this file.
    pub bytes: u64,
    /// Explicit bounded NVMe controller serial identity.
    pub serial: String,
}
/// Fixed resources actually provisioned in the immutable initial node image.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    /// Physical VM RAM, including native services and their accounts.
    pub node_memory_bytes: u64,
    /// Initial engine's maximum job memory; smaller documents remain bounded.
    pub job_memory_bytes: u64,
    /// Initial engine's actual carved account, including its scratch/overhead.
    pub job_account_bytes: u64,
    /// Initial engine's explicit scratch quota.
    pub scratch_bytes: u64,
    /// Fixed boot CPUs; no dynamic CPU injection/hotplug is implemented.
    pub cpus: u32,
}
impl Resources {
    /// Admit a document only inside the actual fixed engine budget. Construct the
    /// VM budget separately so QEMU RAM cannot silently stand in for job limits.
    pub fn admit(&self, job: Budget) -> Result<Budget> {
        if job.memory_bytes == 0
            || job.memory_bytes > self.job_memory_bytes
            || job.memory_max_bytes < job.memory_bytes
            || job.memory_max_bytes > self.job_memory_bytes
            || job.cpus == 0
            || job.cpus > self.cpus
            || job.cpus_max < job.cpus
            || job.cpus_max > self.cpus
        {
            return Err(bad(
                "document exceeds fixed initial engine resource contract",
            ));
        }
        Ok(Budget {
            memory_bytes: self.node_memory_bytes,
            memory_max_bytes: self.node_memory_bytes,
            cpus: self.cpus,
            cpus_max: self.cpus,
        })
    }
}

/// The one checked Moruna 1.0 boot descriptor, never a fuzzy image directory.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MicrokernelArtifacts {
    /// Must equal one.
    pub version: u32,
    /// Must equal x86_64.
    pub architecture: String,
    /// Must equal multiboot2.
    pub boot_mode: String,
    /// Original, unpatched OSTD x86_64 ELF.
    pub kernel: Artifact,
    /// Complete native initramfs with the actual node process manifest.
    pub initramfs: Artifact,
    /// Ordered read-only PCI layer disks.
    pub layers: Vec<LayerDisk>,
    /// Explicit mutable NVMe devices, independently locked until VM teardown.
    pub writable_disks: Vec<WritableDisk>,
    /// Explicit outbound QEMU user networking, without host port forwarding.
    pub user_network: bool,
    /// The vsock guest CID; reserved CIDs zero through two are refused.
    pub cid: u32,
    /// Actual runtime vsock listener configured in the boot-granted engine job.
    pub agent_port: u32,
    /// Checked fixed initial-job/node resources; no dynamic run-config mutation.
    pub resources: Resources,
}
fn bad(message: impl Into<String>) -> VmmError {
    VmmError::config("microkernel", message)
}
/// Read the bounded exact descriptor and validate its fixed resource schema.
/// Content digests are checked again against private snapshots during launch.
pub fn inspect(descriptor: &Path) -> Result<MicrokernelArtifacts> {
    let metadata = fs::metadata(descriptor)
        .map_err(|e| VmmError::io("metadata", descriptor.display().to_string(), &e))?;
    if !metadata.is_file() || metadata.len() > 64 * 1024 {
        return Err(bad("descriptor must be bounded regular JSON"));
    }
    let artifacts: MicrokernelArtifacts = serde_json::from_reader(
        File::open(descriptor)
            .map_err(|e| VmmError::io("open", descriptor.display().to_string(), &e))?,
    )
    .map_err(|e| bad(e.to_string()))?;
    let r = &artifacts.resources;
    if artifacts.version != 1
        || artifacts.architecture != "x86_64"
        || artifacts.boot_mode != "multiboot2"
        || artifacts.cid <= 2
        || artifacts.agent_port == 0
        || artifacts.layers.len() > 16
        || artifacts.writable_disks.len() > 4
        || r.node_memory_bytes < crate::limits::MIN_MEMORY_BYTES
        || r.node_memory_bytes > crate::limits::MAX_MEMORY_BYTES
        || !r
            .node_memory_bytes
            .is_multiple_of(crate::limits::MEMORY_ALIGN_BYTES)
        || r.job_memory_bytes == 0
        || !r.job_memory_bytes.is_multiple_of(4096)
        || !r.job_account_bytes.is_multiple_of(4096)
        || r.scratch_bytes == 0
        || !r.scratch_bytes.is_multiple_of(4096)
        || r.job_memory_bytes
            .checked_add(r.scratch_bytes)
            .is_none_or(|n| n >= r.job_account_bytes)
        || r.job_account_bytes >= r.node_memory_bytes
        || r.cpus == 0
        || r.cpus > crate::limits::MAX_VCPUS
    {
        return Err(bad(
            "unsupported or inconsistent node resource/artifact contract",
        ));
    }
    Ok(artifacts)
}
fn snapshot(root: &Path, artifact: &Artifact, destination: &Path) -> Result<()> {
    if artifact.bytes == 0
        || artifact.bytes > 16 * 1024 * 1024 * 1024
        || artifact.sha256.len() != 64
        || !artifact
            .sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || artifact
            .path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(bad("invalid artifact size, digest or relative path"));
    }
    let source = root.join(&artifact.path);
    if !fs::symlink_metadata(&source)
        .map_err(|e| VmmError::io("metadata", source.display().to_string(), &e))?
        .file_type()
        .is_file()
    {
        return Err(bad("artifact must be a regular file, not a symlink"));
    }
    let source = source
        .canonicalize()
        .map_err(|e| VmmError::io("resolve", source.display().to_string(), &e))?;
    if !source.starts_with(root) {
        return Err(bad("artifact escapes descriptor directory"));
    }
    let mut source_file =
        File::open(&source).map_err(|e| VmmError::io("open", source.display().to_string(), &e))?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|e| VmmError::io("create", destination.display().to_string(), &e))?;
    let mut digest = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = source_file
            .read(&mut buffer)
            .map_err(|e| VmmError::io("read", source.display().to_string(), &e))?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .ok_or_else(|| bad("artifact size overflow"))?;
        if total > artifact.bytes {
            return Err(bad("artifact longer than descriptor"));
        }
        digest.update(&buffer[..count]);
        output
            .write_all(&buffer[..count])
            .map_err(|e| VmmError::io("write", destination.display().to_string(), &e))?;
    }
    // sha2 0.11 returns an array that deliberately has no LowerHex impl.
    // Format each actual digest byte, retaining exact lowercase 64-digit comparison.
    use std::fmt::Write as _;
    let mut actual = String::with_capacity(64);
    for byte in digest.finalize().iter() {
        write!(&mut actual, "{byte:02x}").map_err(|_| bad("digest hex formatting failed"))?;
    }
    if total != artifact.bytes || actual != artifact.sha256 {
        return Err(bad("artifact digest or size mismatch"));
    }
    Ok(())
}

/// The run's private directory, created by the launcher and removed with everything in it,
/// and with the run's cgroup leaf, when the run ends or its launch fails.
struct RunDir {
    path: PathBuf,
    leaf: Option<jail::Leaf>,
    removed: bool,
}
impl RunDir {
    fn remove(&mut self) -> Result<()> {
        if self.removed {
            return Ok(());
        }
        let leaf = match &mut self.leaf {
            Some(leaf) => leaf.remove(),
            None => Ok(()),
        };
        let dir = fs::remove_dir_all(&self.path)
            .map_err(|e| VmmError::io("remove run directory", self.path.display().to_string(), &e));
        if dir.is_ok() {
            self.removed = true;
        }
        leaf.and(dir)
    }
}
impl Drop for RunDir {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

/// The last bytes of a log in the run's directory, for an error that names its cause.
fn log_tail(path: &Path) -> String {
    let Ok(bytes) = fs::read(path) else {
        return String::new();
    };
    let tail = &bytes[bytes.len().saturating_sub(2048)..];
    String::from_utf8_lossy(tail).trim().to_string()
}

/// An owned node VM and its owned vsock helper, each in its own jail. Dropping it kills and
/// reaps both, removes the run's cgroup leaf and its directory; no PID files, shared sockets
/// or external processes are signalled.
pub struct RunningNode {
    qemu: Mutex<jail::Jailed>,
    backend: Mutex<jail::Jailed>,
    uds: PathBuf,
    agent_port: u32,
    _disk_leases: Vec<File>,
    qmp: PathBuf,
    run: RunDir,
}
impl RunningNode {
    /// Declared runtime service port; readiness still requires a real connection.
    pub fn agent_port(&self) -> u32 {
        self.agent_port
    }
    /// The jailed QEMU's and vsock back end's pids in this process's PID namespace, for
    /// inspection.
    pub fn processes(&self) -> [i32; 2] {
        [&self.qemu, &self.backend].map(|p| p.lock().unwrap_or_else(|e| e.into_inner()).pid())
    }
    /// The run's cgroup leaf, when the host delegates one.
    pub fn cgroup(&self) -> Option<&Path> {
        self.run.leaf.as_ref().map(jail::Leaf::path)
    }
    /// The run's private directory, removed when the run ends.
    pub fn runtime_dir(&self) -> &Path {
        &self.run.path
    }
    /// What QEMU and the vsock back end wrote to their standard error, before the run's
    /// directory is removed with them.
    pub fn logs(&self) -> String {
        let mut text = String::new();
        for (name, file) in [("qemu", "qemu.log"), ("vsock back end", "backend.log")] {
            let tail = log_tail(&self.run.path.join(file));
            if !tail.is_empty() {
                text.push_str(&format!("{name}: {tail}\n"));
            }
        }
        text
    }
    fn alive(&self) -> bool {
        [&self.qemu, &self.backend].iter().all(|child| {
            child
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .try_wait()
                .ok()
                == Some(None)
        })
    }
    /// Connect only when the guest's actual service accepts the existing CONNECT
    /// handshake. Socket creation or QEMU spawn is never reported as readiness.
    pub fn connect(&self, port: u32, deadline: Instant) -> Result<BufReader<UnixStream>> {
        if port == 0 {
            return Err(bad("guest service port must be nonzero"));
        }
        crate::session::connect_guest(&self.uds, port, deadline, &|| self.alive())
    }
    /// Explicitly stop and reap both owned processes, then remove the run's cgroup leaf and
    /// directory, reporting every error. Drop does the same and ignores them.
    pub fn shutdown(&mut self) -> Result<()> {
        let qemu = self
            .qemu
            .get_mut()
            .map_err(|_| bad("QEMU child lock poisoned"))?;
        let graceful = quit_qemu(qemu, &self.qmp);
        let stopped = stop_checked(qemu);
        let backend = stop_checked(
            self.backend
                .get_mut()
                .map_err(|_| bad("backend child lock poisoned"))?,
        );
        let removed = self.run.remove();
        graceful.and(stopped).and(backend).and(removed)
    }

    /// Observe the actual emulator process exit, independently of job result
    /// delivery over the guest service protocol. A vsock back end that has ended while QEMU
    /// runs, or has failed at all (a signal, its filter's `SIGSYS` among them, or a nonzero
    /// code), is an error naming its status and its log, whatever QEMU did after it.
    pub fn try_wait(&self) -> Result<Option<std::process::ExitStatus>> {
        let status = self
            .qemu
            .lock()
            .map_err(|_| VmmError::Control("owned QEMU state poisoned".into()))?
            .try_wait()
            .map_err(|e| VmmError::io("wait", "owned node QEMU", &e))?;
        let backend = self
            .backend
            .lock()
            .map_err(|_| VmmError::Control("owned vsock state poisoned".into()))?
            .try_wait()
            .map_err(|e| VmmError::io("wait", "owned vsock backend", &e))?;
        if let Some(ended) = backend
            && (status.is_none() || !ended.success())
        {
            return Err(VmmError::Control(format!(
                "owned vsock backend {} ({ended}): {}",
                if status.is_none() {
                    "exited while node was running"
                } else {
                    "failed"
                },
                log_tail(&self.run.path.join("backend.log"))
            )));
        }
        Ok(status)
    }
}
fn qmp_reply(reader: &mut BufReader<UnixStream>) -> Result<serde_json::Value> {
    for _ in 0..16 {
        let mut line = String::new();
        let count = std::io::Read::by_ref(reader)
            .take(4096)
            .read_line(&mut line)
            .map_err(|e| VmmError::io("read", "owned QMP", &e))?;
        if count == 0 || !line.ends_with('\n') {
            return Err(bad("QMP closed or exceeded bounded frame"));
        }
        let value: serde_json::Value =
            serde_json::from_str(&line).map_err(|e| bad(format!("invalid QMP frame: {e}")))?;
        if value.get("event").is_some() {
            continue;
        }
        return Ok(value);
    }
    Err(bad("QMP event burst exceeded shutdown bound"))
}
fn quit_qemu(child: &mut jail::Jailed, path: &Path) -> Result<()> {
    if child
        .try_wait()
        .map_err(|e| VmmError::io("wait", "owned QEMU", &e))?
        .is_some()
    {
        return Ok(());
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let stream = loop {
        match UnixStream::connect(path) {
            Ok(stream) => break stream,
            Err(_) if Instant::now() < deadline => {
                if child
                    .try_wait()
                    .map_err(|e| VmmError::io("wait", "owned QEMU", &e))?
                    .is_some()
                {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(VmmError::io("connect", "owned QMP shutdown", &e)),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .map_err(|e| VmmError::io("timeout", "QMP", &e))?;
    stream
        .set_write_timeout(Some(Duration::from_millis(500)))
        .map_err(|e| VmmError::io("timeout", "QMP", &e))?;
    let mut reader = BufReader::new(stream);
    if qmp_reply(&mut reader)?.get("QMP").is_none() {
        return Err(bad("owned QMP omitted greeting"));
    }
    reader
        .get_mut()
        .write_all(b"{\"execute\":\"qmp_capabilities\"}\r\n")
        .map_err(|e| VmmError::io("write", "QMP capabilities", &e))?;
    if qmp_reply(&mut reader)?.get("return").is_none() {
        return Err(bad("QMP capability negotiation refused"));
    }
    reader
        .get_mut()
        .write_all(b"{\"execute\":\"quit\"}\r\n")
        .map_err(|e| VmmError::io("write", "QMP quit", &e))?;
    while Instant::now() < deadline {
        if child
            .try_wait()
            .map_err(|e| VmmError::io("wait", "QMP exit", &e))?
            .is_some()
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(bad(
        "QEMU failed graceful quit/drain deadline; forced teardown required",
    ))
}
fn stop_checked(child: &mut jail::Jailed) -> Result<()> {
    if child
        .try_wait()
        .map_err(|e| VmmError::io("wait", "owned child", &e))?
        .is_none()
    {
        child
            .kill()
            .map_err(|e| VmmError::io("kill", "owned child", &e))?;
    }
    child
        .wait()
        .map_err(|e| VmmError::io("reap", "owned child", &e))?;
    Ok(())
}
impl Drop for RunningNode {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// Start exactly one microkernel node with explicit KVM and q35 PCI. The caller
/// supplies a new private directory, exact tool paths (a bare name is looked up in `PATH`
/// as `execvp` would) and where the run is accounted; no executable is installed or
/// downloaded. Every boot input is snapshotted and digest-verified before spawn, and QEMU
/// and the vsock back end each start in their own jail (`crate::jail`). `budget` is the
/// requested document budget; actual fixed node RAM/CPUs come from the single parsed
/// descriptor after admission. Non-Linux/x86_64 hosts and unavailable KVM fail explicitly.
/// The jailed processes are children of the calling thread's process and are killed and
/// reaped when the returned node is shut down or dropped.
pub fn launch(
    descriptor: &Path,
    budget: Budget,
    runtime_dir: &Path,
    qemu: &Path,
    vhost_backend: &Path,
    cgroup: &Cgroup,
) -> Result<RunningNode> {
    launch_owned(
        descriptor,
        budget,
        runtime_dir,
        qemu,
        vhost_backend,
        cgroup,
        Acceleration::Kvm,
    )
}

/// Explicit TEST-ONLY software emulation proof. Never selected by production
/// `launch`, by host capability discovery, or as a KVM failure fallback. This
/// shares descriptor, budget, device, jail, QMP and owned-child teardown with KVM;
/// results establish boot/protocol correctness, never KVM performance.
#[cfg(feature = "native-tcg-proof")]
pub fn launch_tcg_proof(
    descriptor: &Path,
    budget: Budget,
    runtime_dir: &Path,
    qemu: &Path,
    vhost_backend: &Path,
    cgroup: &Cgroup,
) -> Result<RunningNode> {
    launch_owned(
        descriptor,
        budget,
        runtime_dir,
        qemu,
        vhost_backend,
        cgroup,
        Acceleration::TcgProof,
    )
}

#[derive(Clone, Copy)]
enum Acceleration {
    Kvm,
    #[cfg(feature = "native-tcg-proof")]
    TcgProof,
}
impl Acceleration {
    fn check_host(self) -> Result<()> {
        match self {
            Self::Kvm => {
                if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
                    return Err(VmmError::NoKvm(
                        "native microkernel launch requires Linux x86_64".into(),
                    ));
                }
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(KVM)
                    .map_err(|e| VmmError::NoKvm(e.to_string()))?;
            }
            #[cfg(feature = "native-tcg-proof")]
            Self::TcgProof => {
                if !cfg!(target_os = "linux") {
                    return Err(bad("explicit TCG proof requires Linux"));
                }
            }
        }
        Ok(())
    }
    fn arguments(self) -> [&'static str; 6] {
        match self {
            Self::Kvm => [
                "-accel",
                "kvm",
                "-cpu",
                "host",
                "-machine",
                "q35,kernel-irqchip=split",
            ],
            #[cfg(feature = "native-tcg-proof")]
            Self::TcgProof => [
                "-accel",
                "tcg,thread=multi",
                "-cpu",
                "max",
                "-machine",
                "q35",
            ],
        }
    }
    /// The device nodes QEMU's jail shows: `/dev/null`, on which QEMU probes for
    /// open-file-description locks before it locks its disk images, and `/dev/kvm` to a KVM
    /// boot only. No other jail is shown a device.
    fn devices(self) -> Vec<jail::Entry> {
        let null = jail::Entry::Device {
            path: PathBuf::from(NULL),
        };
        match self {
            Self::Kvm => vec![
                null,
                jail::Entry::Device {
                    path: PathBuf::from(KVM),
                },
            ],
            #[cfg(feature = "native-tcg-proof")]
            Self::TcgProof => vec![null],
        }
    }
}
const KVM: &str = "/dev/kvm";
const NULL: &str = "/dev/null";
/// QEMU's own system-call filter, installed before it creates the machine: no obsolete
/// calls, no change of identity, no new process or program, no change of scheduling or
/// resource limits.
pub const QEMU_SANDBOX: &str =
    "on,obsolete=deny,elevateprivileges=deny,spawn=deny,resourcecontrol=deny";

fn private_dir(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .map_err(|e| VmmError::io("create private directory", path.display().to_string(), &e))
}
fn file_entry(path: &Path, access: jail::Access) -> jail::Entry {
    jail::Entry::File {
        path: path.to_path_buf(),
        access,
        exec: false,
    }
}

#[allow(clippy::too_many_arguments)]
fn launch_owned(
    descriptor: &Path,
    budget: Budget,
    runtime_dir: &Path,
    qemu: &Path,
    vhost_backend: &Path,
    cgroup: &Cgroup,
    acceleration: Acceleration,
) -> Result<RunningNode> {
    acceleration.check_host()?;
    let qemu = jail::resolve_program(qemu)?;
    let vhost_backend = jail::resolve_program(vhost_backend)?;
    let descriptor = descriptor
        .canonicalize()
        .map_err(|e| VmmError::io("resolve", descriptor.display().to_string(), &e))?;
    let artifacts = inspect(&descriptor)?;
    let budget = artifacts.resources.admit(budget)?;
    if artifacts.kernel.bytes > 256 * 1024 * 1024
        || artifacts.initramfs.bytes >= budget.memory_bytes / 2
    {
        return Err(bad("boot artifacts exceed bounded guest memory admission"));
    }
    let immutable_bytes = artifacts.layers.iter().try_fold(
        artifacts
            .kernel
            .bytes
            .checked_add(artifacts.initramfs.bytes)
            .ok_or_else(|| bad("artifact size overflow"))?,
        |sum, layer| {
            sum.checked_add(layer.artifact.bytes)
                .ok_or_else(|| bad("artifact size overflow"))
        },
    )?;
    if immutable_bytes > 16 * 1024 * 1024 * 1024 {
        return Err(bad("immutable boot snapshot exceeds sixteen GiB ceiling"));
    }
    let root = descriptor
        .parent()
        .ok_or_else(|| bad("descriptor parent unavailable"))?;
    private_dir(runtime_dir)?;
    let mut run = RunDir {
        path: runtime_dir
            .canonicalize()
            .map_err(|e| VmmError::io("resolve", runtime_dir.display().to_string(), &e))?,
        leaf: None,
        removed: false,
    };
    let runtime_dir = run.path.clone();
    if runtime_dir
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .any(|b| matches!(b, b',' | b'\n' | b'\r'))
    {
        return Err(bad("runtime path contains QEMU option delimiters"));
    }
    let kernel = runtime_dir.join("kernel.elf");
    snapshot(root, &artifacts.kernel, &kernel)?;
    let mut elf = fs::read(&kernel).map_err(|e| VmmError::io("read", "verified kernel", &e))?;
    if elf.len() < 64 || &elf[..6] != b"\x7fELF\x02\x01" || elf[18..20] != 62u16.to_le_bytes() {
        return Err(bad("kernel must be the original little-endian x86_64 ELF"));
    }
    let header = (0..elf.len().min(32768).saturating_sub(16))
        .step_by(8)
        .any(|at| {
            let word = |offset: usize| {
                u32::from_le_bytes(
                    elf[at + offset..at + offset + 4]
                        .try_into()
                        .unwrap_or([0; 4]),
                )
            };
            word(0) == 0xe852_50d6
                && word(4) == 0
                && word(8) >= 24
                && (at as u64 + u64::from(word(8))) <= elf.len().min(32768) as u64
                && word(0)
                    .wrapping_add(word(4))
                    .wrapping_add(word(8))
                    .wrapping_add(word(12))
                    == 0
        });
    if !header {
        return Err(bad("kernel lacks a valid x86 Multiboot2 header"));
    }
    // QEMU's direct Multiboot loader requires EM_386, as OSDK and the existing
    // kernel boot harness do. Patch only this owned verified copy, never source.
    elf[18..20].copy_from_slice(&3u16.to_le_bytes());
    fs::write(&kernel, elf).map_err(|e| VmmError::io("write", "private QEMU kernel", &e))?;
    let initramfs = runtime_dir.join("initramfs.cpio");
    snapshot(root, &artifacts.initramfs, &initramfs)?;
    let mut disks = Vec::new();
    for (index, layer) in artifacts.layers.iter().enumerate() {
        let path = runtime_dir.join(format!("layer-{index}.raw"));
        snapshot(root, &layer.artifact, &path)?;
        disks.push(path);
    }
    let mut writable = Vec::new();
    let mut disk_leases = Vec::new();
    let mut disk_identities = BTreeSet::new();
    let mut disk_serials = BTreeSet::new();
    for disk in &artifacts.writable_disks {
        if disk.bytes == 0
            || disk.bytes % 4096 != 0
            || disk.serial.is_empty()
            || disk.serial.len() > 32
            || !disk
                .serial
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || disk
                .path
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(bad("invalid explicit NVMe backing identity"));
        }
        if !disk_serials.insert(disk.serial.as_str()) {
            return Err(bad("duplicate explicit NVMe serial"));
        }
        let input = root.join(&disk.path);
        let input_metadata = fs::symlink_metadata(&input)
            .map_err(|e| VmmError::io("metadata", disk.path.display().to_string(), &e))?;
        if !input_metadata.is_file() {
            return Err(bad("NVMe backing must be a regular file, never a symlink"));
        }
        let path = input
            .canonicalize()
            .map_err(|e| VmmError::io("resolve", disk.path.display().to_string(), &e))?;
        if !path.starts_with(root)
            || path
                .as_os_str()
                .as_encoded_bytes()
                .iter()
                .any(|b| matches!(b, b',' | b'\n' | b'\r'))
        {
            return Err(bad(
                "NVMe backing escapes descriptor or contains option delimiters",
            ));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|e| VmmError::io("open", path.display().to_string(), &e))?;
        let metadata = file
            .metadata()
            .map_err(|e| VmmError::io("metadata", path.display().to_string(), &e))?;
        if !metadata.is_file() || metadata.len() != disk.bytes {
            return Err(bad(
                "NVMe backing must be an exact preprovisioned regular file",
            ));
        }
        if metadata.dev() != input_metadata.dev() || metadata.ino() != input_metadata.ino() {
            return Err(bad("NVMe backing changed during admission"));
        }
        if !disk_identities.insert((metadata.dev(), metadata.ino())) {
            return Err(bad("duplicate NVMe backing object"));
        }
        file.try_lock()
            .map_err(|e| bad(format!("NVMe backing already owned or unavailable: {e}")))?;
        writable.push((path, disk.serial.clone()));
        disk_leases.push(file);
    }
    // Each socket lives in a directory of its own, so each jail is shown only the sockets it
    // uses: QEMU its QMP socket and the back end's vhost-user socket, the back end that socket
    // and the host side of the guest's vsock.
    let qmp_dir = runtime_dir.join("qmp");
    let vsock_dir = runtime_dir.join("vsock");
    let guest_dir = runtime_dir.join("guest");
    let qemu_root = runtime_dir.join("jail-qemu");
    let backend_root = runtime_dir.join("jail-vsock");
    for dir in [&qmp_dir, &vsock_dir, &guest_dir, &qemu_root, &backend_root] {
        private_dir(dir)?;
    }
    let socket = vsock_dir.join("vhost.sock");
    let uds = guest_dir.join("guest.uds");
    let qmp = qmp_dir.join("qmp.sock");
    if [&socket, &uds, &qmp]
        .iter()
        .any(|p| p.as_os_str().len() >= 100)
    {
        return Err(bad("private runtime socket path too long"));
    }
    let serial = runtime_dir.join("serial.log");
    File::create(&serial).map_err(|e| VmmError::io("create", "serial log", &e))?;
    if let Cgroup::Delegated(parent) = cgroup {
        run.leaf = Some(jail::Leaf::create(
            parent,
            jail::Size::for_vm(budget.memory_bytes, budget.cpus)?,
        )?);
    }
    let leaf = run.leaf.as_ref().map(jail::Leaf::fd);
    let null = || {
        OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .map_err(|e| VmmError::io("open", "/dev/null", &e))
    };
    let mut backend_entries = jail::program_entries(&vhost_backend)?;
    backend_entries.push(jail::Entry::Dir {
        path: vsock_dir.clone(),
        access: jail::Access::Write,
    });
    backend_entries.push(jail::Entry::Dir {
        path: guest_dir.clone(),
        access: jail::Access::Write,
    });
    let mut backend = jail::spawn(jail::Spec {
        program: &vhost_backend,
        args: vec![
            "--socket".into(),
            socket.clone().into(),
            "--cid".into(),
            artifacts.cid.to_string().into(),
            "--uds".into(),
            uds.clone().into(),
        ],
        entries: jail::settle(backend_entries)?,
        root: &backend_root,
        stdout: null()?,
        stderr: File::create(runtime_dir.join("backend.log"))
            .map_err(|e| VmmError::io("create", "backend log", &e))?,
        cgroup: leaf,
    })?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        let exited = backend
            .try_wait()
            .map_err(|e| VmmError::io("wait", "owned vsock backend", &e))?;
        if exited.is_some() || Instant::now() >= deadline {
            return Err(bad(format!(
                "vsock backend did not bind its owned socket ({}): {}",
                exited.map_or("still running".to_string(), |s| s.to_string()),
                log_tail(&runtime_dir.join("backend.log"))
            )));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut args: Vec<std::ffi::OsString> = acceleration
        .arguments()
        .iter()
        .map(|a| std::ffi::OsString::from(*a))
        .collect();
    let mut push = |items: &[&str]| args.extend(items.iter().map(|a| std::ffi::OsString::from(*a)));
    push(&["-sandbox", QEMU_SANDBOX]);
    push(&[
        "-display",
        "none",
        "-monitor",
        "none",
        "-no-reboot",
        "-nodefaults",
    ]);
    push(&[
        "-m",
        &format!("{}M", budget.memory_bytes / (1024 * 1024)),
        "-smp",
        &budget.cpus.to_string(),
    ]);
    push(&[
        "-object",
        &format!(
            "memory-backend-memfd,id=ram0,size={},share=on",
            budget.memory_bytes
        ),
        "-numa",
        "node,memdev=ram0",
    ]);
    push(&[
        "-chardev",
        &format!("socket,id=vsock0,path={}", socket.display()),
        "-device",
        "vhost-user-vsock-pci,chardev=vsock0,addr=0x6",
    ]);
    push(&["-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"]);
    push(&["-kernel", &kernel.display().to_string()]);
    push(&["-initrd", &initramfs.display().to_string()]);
    push(&[
        "-qmp",
        &format!("unix:{},server=on,wait=off", qmp.display()),
    ]);
    push(&["-serial", &format!("file:{}", serial.display())]);
    let mut qemu_entries = jail::program_entries(&qemu)?;
    qemu_entries.extend(jail::qemu_data_entries(&qemu)?);
    qemu_entries.extend(acceleration.devices());
    qemu_entries.push(file_entry(&kernel, jail::Access::Read));
    qemu_entries.push(file_entry(&initramfs, jail::Access::Read));
    qemu_entries.push(file_entry(&serial, jail::Access::Write));
    qemu_entries.push(jail::Entry::Dir {
        path: qmp_dir,
        access: jail::Access::Write,
    });
    qemu_entries.push(jail::Entry::Dir {
        path: vsock_dir,
        access: jail::Access::Read,
    });
    for (index, disk) in disks.iter().enumerate() {
        push(&[
            "-drive",
            &format!(
                "file={},format=raw,if=none,id=layer{index},readonly=on",
                disk.display()
            ),
            "-device",
            &format!("virtio-blk-pci,drive=layer{index},disable-legacy=on"),
        ]);
        qemu_entries.push(file_entry(disk, jail::Access::Read));
    }
    for (index, (path, serial)) in writable.iter().enumerate() {
        push(&[
            "-drive",
            &format!("file={},format=raw,if=none,id=home{index}", path.display()),
            "-device",
            &format!("nvme,drive=home{index},serial={serial}"),
        ]);
        qemu_entries.push(file_entry(path, jail::Access::Write));
    }
    // The descriptor may ask for QEMU's user-mode network (Moruna knows nothing of the host
    // it runs on). In the jail it is the guest's only network path, and it ends at the jail's
    // new, empty network namespace: the guest reaches slirp's own virtual gateway and nothing
    // of the node or beyond it.
    if artifacts.user_network {
        push(&[
            "-netdev",
            "user,id=net0",
            "-device",
            "virtio-net-pci,netdev=net0,disable-legacy=on",
        ]);
    }
    let qemu = jail::spawn(jail::Spec {
        program: &qemu,
        args,
        entries: jail::settle(qemu_entries)?,
        root: &qemu_root,
        stdout: null()?,
        stderr: File::create(runtime_dir.join("qemu.log"))
            .map_err(|e| VmmError::io("create", "QEMU log", &e))?,
        cgroup: leaf,
    })?;
    Ok(RunningNode {
        qemu: Mutex::new(qemu),
        backend: Mutex::new(backend),
        uds,
        agent_port: artifacts.agent_port,
        _disk_leases: disk_leases,
        qmp,
        run,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn production_acceleration_never_selects_a_fallback() {
        assert_eq!(
            Acceleration::Kvm.arguments(),
            [
                "-accel",
                "kvm",
                "-cpu",
                "host",
                "-machine",
                "q35,kernel-irqchip=split"
            ]
        );
    }
    #[cfg(feature = "native-tcg-proof")]
    #[test]
    fn explicit_proof_acceleration_has_its_own_non_kvm_cpu_profile() {
        assert_eq!(
            Acceleration::TcgProof.arguments(),
            [
                "-accel",
                "tcg,thread=multi",
                "-cpu",
                "max",
                "-machine",
                "q35"
            ]
        );
        assert_eq!(Acceleration::Kvm.arguments()[1], "kvm");
    }

    #[test]
    fn artifact_snapshot_requires_exact_sha256_and_rejects_malformed_digests() {
        struct Scratch(PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "moruna-native-digest-{}-{nonce}",
            std::process::id()
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        let scratch = Scratch(path.canonicalize().unwrap());
        fs::write(scratch.0.join("input"), b"abc").unwrap();
        let good = Artifact {
            path: "input".into(),
            bytes: 3,
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
        };
        snapshot(&scratch.0, &good, &scratch.0.join("good")).unwrap();
        assert_eq!(fs::read(scratch.0.join("good")).unwrap(), b"abc");
        let wrong = Artifact {
            path: "input".into(),
            bytes: 3,
            sha256: "0".repeat(64),
        };
        assert!(
            snapshot(&scratch.0, &wrong, &scratch.0.join("wrong"))
                .unwrap_err()
                .to_string()
                .contains("digest or size mismatch")
        );
        for digest in ["a".repeat(63), "G".repeat(64), good.sha256.to_uppercase()] {
            let malformed = Artifact {
                path: "input".into(),
                bytes: 3,
                sha256: digest,
            };
            assert!(snapshot(&scratch.0, &malformed, &scratch.0.join("malformed")).is_err());
            assert!(!scratch.0.join("malformed").exists());
        }
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    #[test]
    fn native_launcher_refuses_other_hosts_before_any_artifact_or_tool_access() {
        let budget = Budget {
            memory_bytes: 512 << 20,
            memory_max_bytes: 512 << 20,
            cpus: 1,
            cpus_max: 1,
        };
        let error = launch(
            Path::new("unread-native-descriptor"),
            budget,
            Path::new("uncreated-native-owner"),
            Path::new("unexecuted-qemu"),
            Path::new("unexecuted-helper"),
            &Cgroup::Undelegated,
        )
        .err()
        .expect("unsupported host must refuse");
        assert!(error.to_string().contains("Linux x86_64"));
    }
    #[test]
    fn requested_job_cannot_change_fixed_node_or_engine_resources() {
        let resources = Resources {
            node_memory_bytes: 2 << 30,
            job_memory_bytes: 512 << 20,
            job_account_bytes: 768 << 20,
            scratch_bytes: 128 << 20,
            cpus: 4,
        };
        let small = Budget {
            memory_bytes: 256 << 20,
            memory_max_bytes: 512 << 20,
            cpus: 1,
            cpus_max: 2,
        };
        let node = resources.admit(small).unwrap();
        assert_eq!(node.memory_bytes, 2 << 30);
        assert_eq!(node.cpus, 4);
        assert!(
            resources
                .admit(Budget {
                    memory_bytes: 1 << 30,
                    ..small
                })
                .is_err()
        );
        assert!(
            resources
                .admit(Budget {
                    cpus_max: 5,
                    ..small
                })
                .is_err()
        );
    }
    #[test]
    fn legacy_linux_image_config_is_not_a_native_descriptor() {
        assert!(
            serde_json::from_str::<MicrokernelArtifacts>(
                r#"{"image":"/linux","disks":[],"memory_bytes":536870912}"#
            )
            .is_err()
        );
    }
}
