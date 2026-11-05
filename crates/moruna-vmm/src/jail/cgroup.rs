//! A run's cgroup v2 leaf under a delegated cgroup: memory, CPUs and tasks sized to the run.

use crate::error::{Result, VmmError};
use std::{
    fs::{self, File},
    os::fd::{AsRawFd, OwnedFd},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

/// QEMU's own memory beside the guest's RAM (device emulation, the translation cache of a TCG
/// proof boot, I/O buffers) and the vsock back end's.
pub(crate) const MONITOR_OVERHEAD_BYTES: u64 = 512 << 20;
/// Tasks beside one per vCPU: QEMU's main loop, RCU, I/O and worker-pool threads, and the back
/// end.
pub(crate) const TASKS_BESIDE_VCPUS: u64 = 96;

static NEXT: AtomicU64 = AtomicU64::new(0);

fn bad(message: impl Into<String>) -> VmmError {
    VmmError::config("--cgroup", message)
}

/// The values a run's leaf is given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Size {
    /// `memory.max`, bytes.
    pub memory_bytes: u64,
    /// `cpu.max` in CPUs.
    pub cpus: u32,
    /// `pids.max`.
    pub tasks: u64,
}

impl Size {
    /// The leaf for a VM of `memory_bytes` of RAM and `cpus` vCPUs.
    pub(crate) fn for_vm(memory_bytes: u64, cpus: u32) -> Result<Self> {
        Ok(Size {
            memory_bytes: memory_bytes
                .checked_add(MONITOR_OVERHEAD_BYTES)
                .ok_or_else(|| bad("memory size overflow"))?,
            cpus,
            tasks: TASKS_BESIDE_VCPUS + u64::from(cpus),
        })
    }
}

/// One run's leaf: created sized, entered by `clone3` through its directory descriptor, killed
/// and removed when the run ends.
pub(crate) struct Leaf {
    path: PathBuf,
    dir: OwnedFd,
    removed: bool,
}

impl Leaf {
    /// Create a new leaf under the delegated `parent`, which must offer the `cpu`, `memory` and
    /// `pids` controllers to its children.
    pub(crate) fn create(parent: &Path, size: Size) -> Result<Self> {
        let control = parent.join("cgroup.subtree_control");
        let enabled = fs::read_to_string(&control).map_err(|e| {
            bad(format!(
                "{} is not a delegated cgroup v2 directory: {}: {e}",
                parent.display(),
                control.display()
            ))
        })?;
        let enabled: Vec<&str> = enabled.split_whitespace().collect();
        let missing: Vec<&str> = ["cpu", "memory", "pids"]
            .into_iter()
            .filter(|c| !enabled.contains(c))
            .collect();
        if !missing.is_empty() {
            return Err(bad(format!(
                "{} does not enable {} in cgroup.subtree_control",
                parent.display(),
                missing.join(", ")
            )));
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| bad("clock before epoch"))?
            .as_nanos();
        let name = format!(
            "moruna-run-{}-{}-{nanos}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let path = parent.join(name);
        fs::create_dir(&path)
            .map_err(|e| VmmError::io("create cgroup leaf", path.display().to_string(), &e))?;
        let dir = File::open(&path)
            .map_err(|e| VmmError::io("open cgroup leaf", path.display().to_string(), &e))?;
        let mut leaf = Leaf {
            path,
            dir: dir.into(),
            removed: false,
        };
        let sized = leaf
            .write("memory.max", &size.memory_bytes.to_string())
            .and_then(|()| {
                leaf.write("memory.swap.max", "0")?;
                leaf.write(
                    "cpu.max",
                    &format!("{} 100000", u64::from(size.cpus) * 100_000),
                )?;
                leaf.write("pids.max", &size.tasks.to_string())
            });
        if let Err(error) = sized {
            leaf.remove()?;
            return Err(error);
        }
        Ok(leaf)
    }

    fn write(&self, file: &str, value: &str) -> Result<()> {
        let path = self.path.join(file);
        fs::write(&path, value)
            .map_err(|e| VmmError::io("write", format!("{} = {value}", path.display()), &e))
    }

    /// The directory descriptor `clone3` places a jailed process into.
    pub(crate) fn fd(&self) -> i32 {
        self.dir.as_raw_fd()
    }

    /// The leaf's path, for inspection.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Kill whatever is still in the leaf and remove it.
    pub(crate) fn remove(&mut self) -> Result<()> {
        if self.removed {
            return Ok(());
        }
        let populated = |path: &Path| -> Result<bool> {
            let events = fs::read_to_string(path.join("cgroup.events")).map_err(|e| {
                VmmError::io("read", format!("{}/cgroup.events", path.display()), &e)
            })?;
            Ok(events.lines().any(|l| l == "populated 1"))
        };
        if populated(&self.path)? {
            self.write("cgroup.kill", "1")?;
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match fs::remove_dir(&self.path) {
                Ok(()) => break,
                Err(e) if e.raw_os_error() == Some(libc::EBUSY) && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => {
                    return Err(VmmError::io(
                        "remove cgroup leaf",
                        self.path.display().to_string(),
                        &e,
                    ));
                }
            }
        }
        self.removed = true;
        Ok(())
    }
}

impl Drop for Leaf {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}
