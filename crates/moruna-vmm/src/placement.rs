//! Where a node's vCPUs run on the host (contract C5's host half, stream A's stage (c)).
//!
//! The guest pins its workers to its CPUs and stops its own tick on them; what a pinned worker
//! still loses is the host's tick on whichever host core runs that vCPU (about 1.5% on
//! `moruna-bench`, 2026-11-16). The launcher's answer is a placement: vCPU `i` runs only on host
//! CPU `vcpu_cpus[i]`, a core the host keeps free of its own tick and work (`nohz_full`,
//! `isolcpus` or a cpuset, `rcu_nocbs`: `kernel/node/HOST.md`), and every other thread of QEMU
//! and of the vsock back end runs on the CPUs left over, so nothing of the run's own competes
//! with a vCPU.
//!
//! QEMU starts paused (`-S`) with its threads named (`-name ...,debug-threads=on`, a vCPU is
//! `CPU N/KVM`). The launcher finds the vCPU threads through `/proc`, sets each one's
//! affinity, moves the rest, and only then lets the guest run (QMP `cont`), so no guest
//! instruction runs on an unplaced vCPU. Without a placement nothing changes: QEMU starts
//! running and the host's scheduler places it.

use crate::error::{Result, VmmError};

/// The most host CPUs a list may name: the size of the kernel's affinity mask here.
pub const CPUS_MAX: u32 = 1024;

fn bad(message: impl Into<String>) -> VmmError {
    VmmError::config("vcpu placement", message)
}

/// A CPU list as the kernel's command line writes one (`2-5,7`): CPUs in the order written,
/// each once.
pub fn parse_cpu_list(text: &str) -> Result<Vec<u32>> {
    let mut cpus = Vec::new();
    let number = |s: &str| -> Result<u32> {
        let n: u32 = s
            .trim()
            .parse()
            .map_err(|_| bad(format!("`{s}` in `{text}` is not a CPU number")))?;
        if n >= CPUS_MAX {
            return Err(bad(format!("CPU {n} is past the last this host can name")));
        }
        Ok(n)
    };
    for part in text.split(',') {
        let (first, last) = match part.split_once('-') {
            Some((a, b)) => (number(a)?, number(b)?),
            None => {
                let n = number(part)?;
                (n, n)
            }
        };
        if last < first {
            return Err(bad(format!("`{part}` runs backwards")));
        }
        for cpu in first..=last {
            if cpus.contains(&cpu) {
                return Err(bad(format!("CPU {cpu} is named twice in `{text}`")));
            }
            cpus.push(cpu);
        }
    }
    Ok(cpus)
}

/// The vCPU a QEMU thread is, from its name (`CPU 3/KVM`), or `None` for any other thread.
pub fn vcpu_of(comm: &str) -> Option<u32> {
    let rest = comm.trim_end().strip_prefix("CPU ")?;
    let (index, accel) = rest.split_once('/')?;
    if accel.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    index.parse().ok()
}

/// A placement checked against the node: one host CPU per vCPU, each one the host has online
/// (an isolated core is in no process's affinity until it is placed there: `isolcpus`), and the
/// CPUs this process may run on less the vCPUs' for everything else (empty when none is left).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    /// The host CPU of vCPU `i`.
    pub vcpus: Vec<u32>,
    /// The CPUs every other thread of the run is kept on.
    pub housekeeping: Vec<u32>,
}

impl Placement {
    /// Check `vcpu_cpus` for a node of `cpus` vCPUs, against `online`, the host's online CPUs;
    /// the rest of the run is kept on `allowed`, the CPUs this process may run on, less them.
    pub fn check(
        vcpu_cpus: &[u32],
        cpus: u32,
        online: &[u32],
        allowed: &[u32],
    ) -> Result<Placement> {
        if vcpu_cpus.len() != cpus as usize {
            return Err(bad(format!(
                "{} host CPUs for a node of {cpus} vCPUs; name one per vCPU",
                vcpu_cpus.len()
            )));
        }
        for (i, cpu) in vcpu_cpus.iter().enumerate() {
            if vcpu_cpus[..i].contains(cpu) {
                return Err(bad(format!("host CPU {cpu} is given to two vCPUs")));
            }
            if !online.contains(cpu) {
                return Err(bad(format!("host CPU {cpu} is not online ({online:?})")));
            }
        }
        Ok(Placement {
            vcpus: vcpu_cpus.to_vec(),
            housekeeping: allowed
                .iter()
                .copied()
                .filter(|c| !vcpu_cpus.contains(c))
                .collect(),
        })
    }
}

/// What a placement did: each vCPU's host thread and CPU, and how many other threads moved.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Placed {
    /// `(vcpu, host thread id, host CPU)`, vCPU 0 first.
    pub vcpus: Vec<(u32, i32, u32)>,
    /// Other threads of QEMU and the back end kept on the housekeeping CPUs.
    pub others: usize,
}

#[cfg(target_os = "linux")]
pub use linux::{allowed_cpus, online_cpus, place};

#[cfg(target_os = "linux")]
mod linux {
    use super::{Placed, Placement, bad};
    use crate::error::{Result, VmmError};
    use std::time::{Duration, Instant};

    fn set(cpus: &[u32]) -> libc::cpu_set_t {
        // SAFETY: cpu_set_t is plain bits; CPU_ZERO and CPU_SET touch only the set.
        let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        for cpu in cpus {
            unsafe { libc::CPU_SET(*cpu as usize, &mut set) };
        }
        set
    }

    fn affinity(tid: i32, cpus: &[u32]) -> std::io::Result<()> {
        let set = set(cpus);
        // SAFETY: the set is a valid cpu_set_t of the size given.
        let rc =
            unsafe { libc::sched_setaffinity(tid, std::mem::size_of::<libc::cpu_set_t>(), &set) };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    /// The host's online CPUs.
    pub fn online_cpus() -> Result<Vec<u32>> {
        let path = "/sys/devices/system/cpu/online";
        let text = std::fs::read_to_string(path).map_err(|e| VmmError::io("read", path, &e))?;
        super::parse_cpu_list(text.trim())
    }

    /// The CPUs this process may run on.
    pub fn allowed_cpus() -> Result<Vec<u32>> {
        // SAFETY: a zeroed cpu_set_t is filled by the kernel.
        let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        let rc =
            unsafe { libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) };
        if rc != 0 {
            return Err(VmmError::io(
                "sched_getaffinity",
                "the launcher",
                &std::io::Error::last_os_error(),
            ));
        }
        Ok((0..super::CPUS_MAX.min(libc::CPU_SETSIZE as u32))
            .filter(|c| unsafe { libc::CPU_ISSET(*c as usize, &set) })
            .collect())
    }

    /// Every thread of process `pid` (as this process's PID namespace names it) and its name.
    fn threads(pid: i32) -> Result<Vec<(i32, String)>> {
        let dir = format!("/proc/{pid}/task");
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir).map_err(|e| VmmError::io("read", dir.clone(), &e))? {
            let entry = entry.map_err(|e| VmmError::io("read", dir.clone(), &e))?;
            let Some(tid) = entry.file_name().to_str().and_then(|s| s.parse().ok()) else {
                continue;
            };
            // A thread may end between the listing and the read; it is then not one to place.
            if let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) {
                out.push((tid, comm.trim_end().to_string()));
            }
        }
        Ok(out)
    }

    /// Place QEMU `qemu`'s vCPU threads and move its other threads and those of `others` (the
    /// vsock back end) to the housekeeping CPUs. Waits until `deadline` for all the vCPU
    /// threads to exist, while `alive` says QEMU runs.
    pub fn place(
        placement: &Placement,
        qemu: i32,
        others: &[i32],
        deadline: Instant,
        alive: &dyn Fn() -> bool,
    ) -> Result<Placed> {
        let want = placement.vcpus.len();
        let found = loop {
            let threads = threads(qemu)?;
            let vcpus: Vec<(u32, i32)> = threads
                .iter()
                .filter_map(|(tid, comm)| super::vcpu_of(comm).map(|v| (v, *tid)))
                .filter(|(v, _)| (*v as usize) < want)
                .collect();
            if vcpus.len() == want {
                break (threads, vcpus);
            }
            if Instant::now() >= deadline || !alive() {
                return Err(bad(format!(
                    "QEMU showed {} of its {want} vCPU threads",
                    vcpus.len()
                )));
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let (listed, mut vcpus) = found;
        vcpus.sort();
        let mut placed = Placed::default();
        for (vcpu, tid) in vcpus {
            let cpu = placement.vcpus[vcpu as usize];
            affinity(tid, &[cpu])
                .map_err(|e| VmmError::io("sched_setaffinity", format!("vCPU {vcpu}"), &e))?;
            placed.vcpus.push((vcpu, tid, cpu));
        }
        if !placement.housekeeping.is_empty() {
            let mut rest: Vec<i32> = listed
                .iter()
                .filter(|(tid, _)| !placed.vcpus.iter().any(|(_, t, _)| t == tid))
                .map(|(tid, _)| *tid)
                .collect();
            for pid in others {
                rest.extend(threads(*pid)?.into_iter().map(|(tid, _)| tid));
            }
            for tid in rest {
                // A thread that ended meanwhile has nothing left to place.
                match affinity(tid, &placement.housekeeping) {
                    Ok(()) => placed.others += 1,
                    Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {}
                    Err(e) => {
                        return Err(VmmError::io(
                            "sched_setaffinity",
                            format!("thread {tid}"),
                            &e,
                        ));
                    }
                }
            }
        }
        Ok(placed)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn this_process_may_run_somewhere_and_its_threads_are_listed() {
            let allowed = allowed_cpus().unwrap();
            assert!(!allowed.is_empty());
            let me = std::process::id() as i32;
            let listed = threads(me).unwrap();
            assert!(listed.iter().any(|(tid, _)| *tid == me));
            // Placing this test process's own threads: the thread named like a vCPU is pinned,
            // the rest kept on the CPUs left.
            let cpu = allowed[0];
            let named = std::thread::Builder::new()
                .name("CPU 0/KVM".into())
                .spawn(|| std::thread::sleep(Duration::from_millis(300)))
                .unwrap();
            let placement = Placement::check(&[cpu], 1, &online_cpus().unwrap(), &allowed).unwrap();
            let placed = place(
                &placement,
                me,
                &[],
                Instant::now() + Duration::from_secs(5),
                &|| true,
            )
            .unwrap();
            assert_eq!(placed.vcpus.len(), 1);
            assert_eq!(placed.vcpus[0].2, cpu);
            named.join().unwrap();
            // Give every thread back every CPU it had.
            for (tid, _) in threads(me).unwrap() {
                let _ = affinity(tid, &allowed);
            }
            let none = Placement::check(&[cpu, cpu + 1], 2, &[cpu, cpu + 1], &[cpu]).unwrap();
            assert!(none.housekeeping.is_empty());
            let e = place(&none, me, &[], Instant::now(), &|| true).unwrap_err();
            assert!(e.to_string().contains("of its 2 vCPU threads"), "{e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_lists_read_as_the_kernel_writes_them() {
        assert_eq!(parse_cpu_list("2").unwrap(), [2]);
        assert_eq!(parse_cpu_list("2-5,7").unwrap(), [2, 3, 4, 5, 7]);
        assert_eq!(parse_cpu_list("7,2").unwrap(), [7, 2], "the order written");
        for bad in ["", "a", "5-2", "2,2", "1-3,3", "1024", "-1", "2-"] {
            assert!(parse_cpu_list(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_vcpu_thread_is_named_by_qemu() {
        assert_eq!(vcpu_of("CPU 0/KVM"), Some(0));
        assert_eq!(vcpu_of("CPU 12/KVM\n"), Some(12));
        assert_eq!(vcpu_of("CPU 1/TCG"), Some(1));
        for other in [
            "qemu-system-x86",
            "CPU x/KVM",
            "CPU 1/",
            "CPU 1",
            "IO mon_iothread",
        ] {
            assert_eq!(vcpu_of(other), None, "{other}");
        }
    }

    #[test]
    fn a_placement_names_one_allowed_cpu_per_vcpu() {
        let online = [0, 1, 2, 3, 4, 5, 6, 7];
        let p = Placement::check(&[6, 7], 2, &online, &online).unwrap();
        assert_eq!(p.vcpus, [6, 7]);
        assert_eq!(p.housekeeping, [0, 1, 2, 3, 4, 5]);
        // Isolated cores are in no process's affinity: the vCPUs go there, the rest stays.
        let isolated = Placement::check(&[4, 6], 2, &online, &[0, 1, 2, 3]).unwrap();
        assert_eq!(isolated.housekeeping, [0, 1, 2, 3]);
        let all = Placement::check(&[0, 1], 2, &[0, 1], &[0, 1]).unwrap();
        assert!(all.housekeeping.is_empty());
        for (cpus, n, why) in [
            (&[6][..], 2, "one per vCPU"),
            (&[6, 6][..], 2, "two vCPUs"),
            (&[6, 9][..], 2, "not online"),
        ] {
            let e = Placement::check(cpus, n, &online, &online)
                .unwrap_err()
                .to_string();
            assert!(e.contains(why), "{e}");
        }
    }
}
