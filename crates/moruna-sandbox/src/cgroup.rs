//! A stage's cgroup (17 SB-I6): created by Moruna under the sandbox cgroup before the child is
//! forked, `memory.max` the stage ceiling, swap off, the stage's CPU weight and the run's quota;
//! removed after the child exits.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// `cpu.weight`: the cgroup default, what Moruna's own threads weigh (17 i).
pub const CPU_WEIGHT: u32 = 100;
/// `cpu.max`'s period, microseconds.
const CPU_PERIOD_US: u64 = 100_000;
/// How long a removal waits for the cgroup to empty after its process was killed.
const EMPTY_WAIT: Duration = Duration::from_secs(5);

fn with_path(path: &Path, e: io::Error) -> io::Error {
    io::Error::new(e.kind(), format!("{}: {e}", path.display()))
}

/// A refused write, named for what it is: a cgroup file the kernel does not provide cannot be
/// created, and the kernel says EACCES for it, which reads as a permission it is not.
fn write_refused(path: &Path, e: io::Error) -> io::Error {
    if path.exists() {
        return with_path(path, e);
    }
    io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "{}: this kernel has no such cgroup file (a controller or kernel option it was \
             built without): {e}",
            path.display()
        ),
    )
}

/// One stage's cgroup.
#[derive(Debug)]
pub struct StageCgroup {
    dir: PathBuf,
}

/// `cpu.max` for a quota of `cores`: `max` when the quota is unknown (0).
pub fn cpu_max(cores: f64) -> String {
    if !(cores.is_finite() && cores > 0.0) {
        return format!("max {CPU_PERIOD_US}");
    }
    let quota = ((cores * CPU_PERIOD_US as f64).ceil() as u64).max(1000);
    format!("{quota} {CPU_PERIOD_US}")
}

impl StageCgroup {
    /// Create `<parent>/<name>` and write its limits. The parent must have `memory` and `cpu`
    /// in its `cgroup.subtree_control`; any write that fails refuses the stage (SB-I6).
    pub fn create(parent: &Path, name: &str, ceiling: u64, cores: f64) -> io::Result<StageCgroup> {
        let control = fs::read_to_string(parent.join("cgroup.subtree_control"))
            .map_err(|e| with_path(&parent.join("cgroup.subtree_control"), e))?;
        for controller in ["memory", "cpu"] {
            if !control.split_whitespace().any(|c| c == controller) {
                return Err(io::Error::other(format!(
                    "{}: the `{controller}` controller is not enabled for its children \
                     (cgroup.subtree_control is `{}`)",
                    parent.display(),
                    control.trim()
                )));
            }
        }
        let dir = parent.join(name);
        fs::create_dir(&dir).map_err(|e| with_path(&dir, e))?;
        let cgroup = StageCgroup { dir };
        cgroup.write("memory.max", &ceiling.to_string())?;
        cgroup.write("memory.swap.max", "0")?;
        cgroup.write("cpu.weight", &CPU_WEIGHT.to_string())?;
        cgroup.write("cpu.max", &cpu_max(cores))?;
        Ok(cgroup)
    }

    /// The directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn write(&self, file: &str, value: &str) -> io::Result<()> {
        let path = self.dir.join(file);
        fs::write(&path, value).map_err(|e| write_refused(&path, e))
    }

    fn read(&self, file: &str) -> io::Result<String> {
        let path = self.dir.join(file);
        fs::read_to_string(&path).map_err(|e| with_path(&path, e))
    }

    /// The stage ceiling moved (f.7).
    pub fn set_memory_max(&self, bytes: u64) -> io::Result<()> {
        self.write("memory.max", &bytes.to_string())
    }

    /// The run's CPU quota moved.
    pub fn set_cpu_max(&self, cores: f64) -> io::Result<()> {
        self.write("cpu.max", &cpu_max(cores))
    }

    /// `memory.current`.
    pub fn memory_current(&self) -> io::Result<u64> {
        self.read("memory.current")?
            .trim()
            .parse()
            .map_err(|e| io::Error::other(format!("memory.current: {e}")))
    }

    /// `memory.max`, as written.
    pub fn memory_max(&self) -> io::Result<String> {
        Ok(self.read("memory.max")?.trim().to_string())
    }

    /// `cpu.max`, as written.
    pub fn cpu_max_now(&self) -> io::Result<String> {
        Ok(self.read("cpu.max")?.trim().to_string())
    }

    /// Kill every process in the cgroup (`cgroup.kill`, Linux 5.14); a kernel without the file
    /// leaves the caller's own signal as the only way.
    pub fn kill(&self) {
        let _ = self.write("cgroup.kill", "1");
    }

    /// How many of the cgroup's processes the kernel killed for exceeding `memory.max`.
    pub fn oom_kills(&self) -> u64 {
        self.read("memory.events")
            .ok()
            .and_then(|events| {
                events
                    .lines()
                    .find_map(|l| l.strip_prefix("oom_kill "))
                    .and_then(|n| n.trim().parse().ok())
            })
            .unwrap_or(0)
    }

    /// Whether a process is still in the cgroup.
    pub fn populated(&self) -> io::Result<bool> {
        Ok(self
            .read("cgroup.events")?
            .lines()
            .any(|l| l.trim() == "populated 1"))
    }

    /// Remove the cgroup, waiting up to `EMPTY_WAIT` for its processes to be gone.
    pub fn remove(&self) -> io::Result<()> {
        let deadline = Instant::now() + EMPTY_WAIT;
        loop {
            match fs::remove_dir(&self.dir) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(e) if Instant::now() >= deadline => return Err(with_path(&self.dir, e)),
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }
}

impl Drop for StageCgroup {
    fn drop(&mut self) {
        if let Err(e) = self.remove() {
            tracing::warn!(target: "sandbox", "a stage's cgroup outlived its child: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_max_from_cores() {
        assert_eq!(cpu_max(0.0), "max 100000");
        assert_eq!(cpu_max(f64::NAN), "max 100000");
        assert_eq!(cpu_max(2.0), "200000 100000");
        assert_eq!(cpu_max(0.001), "1000 100000");
    }

    /// The files a stage's cgroup is made of, over a plain directory that stands in for a
    /// cgroup v2 parent: what is written, read and refused.
    #[test]
    fn a_stage_cgroup_writes_its_limits() {
        let dir = std::env::temp_dir().join(format!("moruna-cg-files-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("cgroup.subtree_control"), "cpu memory\n").unwrap();
        let cg = StageCgroup::create(&dir, "stage", 1 << 20, 1.5).unwrap();
        assert_eq!(cg.memory_max().unwrap(), (1u64 << 20).to_string());
        assert_eq!(cg.cpu_max_now().unwrap(), "150000 100000");
        assert_eq!(
            fs::read_to_string(cg.dir().join("cpu.weight")).unwrap(),
            "100"
        );
        assert_eq!(
            fs::read_to_string(cg.dir().join("memory.swap.max")).unwrap(),
            "0"
        );
        cg.set_memory_max(7).unwrap();
        assert_eq!(cg.memory_max().unwrap(), "7");
        cg.set_cpu_max(0.0).unwrap();
        assert_eq!(cg.cpu_max_now().unwrap(), "max 100000");
        assert!(cg.memory_current().is_err(), "no memory.current yet");
        fs::write(cg.dir().join("memory.current"), "123\n").unwrap();
        assert_eq!(cg.memory_current().unwrap(), 123);
        fs::write(cg.dir().join("memory.current"), "x").unwrap();
        assert!(cg.memory_current().is_err());
        fs::write(cg.dir().join("cgroup.events"), "populated 1\nfrozen 0\n").unwrap();
        assert!(cg.populated().unwrap());
        fs::write(cg.dir().join("cgroup.events"), "populated 0\n").unwrap();
        assert!(!cg.populated().unwrap());
        assert_eq!(cg.oom_kills(), 0, "no memory.events yet");
        fs::write(
            cg.dir().join("memory.events"),
            "low 0\nmax 4\noom 2\noom_kill 2\n",
        )
        .unwrap();
        assert_eq!(cg.oom_kills(), 2);
        cg.kill();
        assert_eq!(
            fs::read_to_string(cg.dir().join("cgroup.kill")).unwrap(),
            "1"
        );
        fs::remove_file(cg.dir().join("cgroup.kill")).unwrap();
        fs::remove_file(cg.dir().join("memory.events")).unwrap();
        // Made twice: refused naming the directory.
        let e = StageCgroup::create(&dir, "stage", 1, 1.0).unwrap_err();
        assert!(e.to_string().contains("stage"), "{e}");
        // A plain directory with files in it is not removable as a cgroup is; a removed one is
        // gone and removing it again is nothing.
        for f in [
            "memory.max",
            "memory.swap.max",
            "cpu.weight",
            "cpu.max",
            "memory.current",
            "cgroup.events",
        ] {
            fs::remove_file(cg.dir().join(f)).unwrap();
        }
        cg.remove().unwrap();
        cg.remove().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_write_to_a_file_the_kernel_lacks_says_so() {
        let dir = std::env::temp_dir().join(format!("moruna-cg-absent-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let denied = || io::Error::from(io::ErrorKind::PermissionDenied);
        let absent = dir.join("memory.swap.max");
        let e = write_refused(&absent, denied());
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert!(
            e.to_string()
                .contains("memory.swap.max: this kernel has no such cgroup file"),
            "{e}"
        );
        fs::write(&absent, "").unwrap();
        let e = write_refused(&absent, denied());
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        assert!(!e.to_string().contains("no such cgroup file"), "{e}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Every file `create` writes exists in the guest's cgroups (17 f.10): a cgroup file the
    /// kernel was built without is absent, and writing it is refused with EACCES (kernfs has no
    /// `create`), so the stage is refused (SB-I6). `memory.swap.max` exists only with SWAP.
    #[test]
    fn the_guest_kernel_has_every_file_a_stage_writes() {
        let needs: [(&str, &[&str]); 4] = [
            ("memory.max", &["CGROUPS", "MEMCG"]),
            ("memory.swap.max", &["CGROUPS", "MEMCG", "SWAP"]),
            (
                "cpu.weight",
                &["CGROUPS", "CGROUP_SCHED", "FAIR_GROUP_SCHED"],
            ),
            (
                "cpu.max",
                &[
                    "CGROUPS",
                    "CGROUP_SCHED",
                    "FAIR_GROUP_SCHED",
                    "CFS_BANDWIDTH",
                ],
            ),
        ];
        let common = include_str!("../../../guest/kernel/moruna.config");
        for (arch, fragment) in [
            (
                "x86_64",
                include_str!("../../../guest/kernel/x86_64.config"),
            ),
            ("arm64", include_str!("../../../guest/kernel/arm64.config")),
        ] {
            let mut on = std::collections::BTreeMap::new();
            for line in common.lines().chain(fragment.lines()) {
                if let Some((k, v)) = line.strip_prefix("CONFIG_").and_then(|l| l.split_once('=')) {
                    on.insert(k, v == "y");
                }
            }
            for (file, options) in needs {
                for option in options {
                    assert!(
                        on.get(option).copied().unwrap_or(false),
                        "the {arch} guest kernel has no {file}: CONFIG_{option} is not y"
                    );
                }
            }
        }
    }

    #[test]
    fn a_parent_without_the_controllers_is_refused() {
        let dir = std::env::temp_dir().join(format!("moruna-cg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert!(
            StageCgroup::create(&dir, "x", 1, 1.0).is_err(),
            "no subtree_control"
        );
        fs::write(dir.join("cgroup.subtree_control"), "io pids\n").unwrap();
        let e = StageCgroup::create(&dir, "x", 1, 1.0).unwrap_err();
        assert!(e.to_string().contains("`memory` controller"), "{e}");
        let _ = fs::remove_dir_all(&dir);
    }
}
