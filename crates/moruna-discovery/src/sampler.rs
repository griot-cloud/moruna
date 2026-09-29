//! The `moruna_kernel::Sampler` implementation (d.1, f.2, DS-I3, DS-I4).
//!
//! One instance per run, shared as `Arc<dyn moruna_kernel::Sampler>` by the controller thread and
//! by every worker (g). Interior mutability is one mutex around the open file handles, the fixed
//! read buffer, the last sample and the running peak; it is never held across anything but the
//! reads, so a worker's sample waits at most for one other sample to finish.

use std::fs::File;
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use moruna_kernel::{
    Device, LimitSource, Limits, ProcessPeak, ProcessUsage, Result, Sample, Sampler as SamplerTrait,
};

use crate::cgroup;
use crate::devices;

/// The fixed read buffer of DS-I3: `memory.stat` is a few hundred bytes on every kernel.
const BUFFER_BYTES: usize = 4096;

/// Which files a sample reads.
enum Source {
    /// cgroup v2: `memory.stat`, `memory.peak` (when the kernel has it) and `cpu.stat`.
    V2 {
        stat: File,
        peak: Option<File>,
        peak_path: PathBuf,
        cpu: Option<File>,
    },
    /// cgroup v1: `memory.stat` and `cpu.stat` with the v1 field names.
    V1 { stat: File, cpu: Option<File> },
    /// The process's own memory: `/proc/self/status`, or the platform's own interface (os.rs).
    /// Outside a cgroup, and inside one whose limit is not the ceiling (DS-I4); there `cpu` is
    /// still the cgroup's `cpu.stat`, since the cgroup throttles the process whoever set its
    /// budget.
    Os {
        proc_root: PathBuf,
        cpu: Option<CpuStat>,
    },
}

/// A cgroup's `cpu.stat`, and whether it uses the v1 field names.
struct CpuStat {
    file: File,
    v1: bool,
}

/// The mutable half of the sampler.
struct Inner {
    source: Source,
    buffer: [u8; BUFFER_BYTES],
    running_peak: u64,
    last: Sample,
    page_bytes: usize,
    /// Set once when `memory.peak` turned out not to be writable, so the note is made once (f.2).
    peak_reset_refused: bool,
    /// The whole process's high-water mark since the sampler was created, which `reset_peak`
    /// leaves alone: what the run report's peak is (`Sampler::process_peak`).
    process_peak: ProcessPeak,
    /// The platform's lifetime high-water mark when the sampler was created, where the platform
    /// keeps one (macOS). A rise above it is the process's exact peak since then, however short
    /// the burst that made it.
    lifetime_at_create: Option<u64>,
    /// The process's CPU time when the sampler was created, nanoseconds; `None` where the
    /// operating system would not say.
    cpu_at_create_ns: Option<u64>,
    /// Anonymous memory integrated over the samples so far, byte-nanoseconds.
    mem_integral: u128,
    /// The last successful sample's time and anonymous bytes, the left edge of the next
    /// step of the integral.
    integral_edge: Option<(u64, u64)>,
}

/// Live resource sampling over the cgroup files (d.1).
pub struct Sampler {
    inner: Mutex<Inner>,
    read_errors: AtomicU64,
    last_at_ns: AtomicU64,
    devices: Vec<Device>,
    /// The limits in force: the ones `discover` found, or the cell a `LimitsWatch` publishes
    /// into, so every sample carries the ceiling and quota of its moment.
    limits: Arc<ArcSwap<Limits>>,
}

impl Sampler {
    /// Open the handles one sample needs. Never fails after this: a later read error repeats the
    /// last sample and is counted in [`Sampler::read_errors`] (DS-I3).
    pub fn new(discovered: &crate::Discovered) -> Result<Sampler> {
        Sampler::with_roots(discovered, Path::new("/proc"))
    }

    /// As [`Sampler::new`], reading the ceiling and quota every sample carries from `limits`,
    /// the cell a [`crate::LimitsWatch`] publishes into.
    pub fn following(
        discovered: &crate::Discovered,
        limits: Arc<ArcSwap<Limits>>,
    ) -> Result<Sampler> {
        let mut sampler = Sampler::with_roots(discovered, Path::new("/proc"))?;
        sampler.limits = limits;
        Ok(sampler)
    }

    /// As [`Sampler::new`], with the `/proc` root as a parameter so the operating system path is
    /// tested against a fixture directory on a host that has no `/proc`.
    pub(crate) fn with_roots(discovered: &crate::Discovered, proc_root: &Path) -> Result<Sampler> {
        // DS-I4: `memory.stat` counts every process in the cgroup, which is what the ceiling
        // counts only when the ceiling is the cgroup's own limit, since the kernel enforces
        // that limit on all of them together. An explicit budget is the process's (DS-I8: on a
        // Databricks driver the JVM beside it holds most of the cgroup), and so is a ceiling
        // taken from the machine's RAM; both count the process's own memory, as macOS's
        // `phys_footprint` does, and never a `cargo`, a test harness's parent or a JVM that
        // happens to share the cgroup (2026-09-30). Decided once, from the ceiling discovery
        // found: an explicit budget stays explicit for the whole run.
        let own = discovered.limits.source != LimitSource::Cgroup;
        let source = match discovered.cgroup_path.as_deref() {
            Some(dir) if own => Source::Os {
                proc_root: proc_root.to_path_buf(),
                cpu: File::open(dir.join("cpu.stat")).ok().map(|file| CpuStat {
                    file,
                    v1: dir.join("memory.limit_in_bytes").is_file(),
                }),
            },
            Some(dir) if dir.join("memory.limit_in_bytes").is_file() => Source::V1 {
                stat: open(&dir.join("memory.stat"))?,
                cpu: File::open(dir.join("cpu.stat")).ok(),
            },
            Some(dir) => {
                let peak_path = dir.join("memory.peak");
                Source::V2 {
                    stat: open(&dir.join("memory.stat"))?,
                    peak: File::open(&peak_path).ok(),
                    peak_path,
                    cpu: File::open(dir.join("cpu.stat")).ok(),
                }
            }
            None => Source::Os {
                proc_root: proc_root.to_path_buf(),
                cpu: None,
            },
        };
        let sampler = Sampler {
            inner: Mutex::new(Inner {
                source,
                buffer: [0u8; BUFFER_BYTES],
                running_peak: 0,
                last: Sample::default(),
                page_bytes: discovered.limits.page_bytes,
                peak_reset_refused: false,
                process_peak: ProcessPeak::default(),
                lifetime_at_create: None,
                cpu_at_create_ns: process_cpu_ns(),
                mem_integral: 0,
                integral_edge: None,
            }),
            read_errors: AtomicU64::new(0),
            last_at_ns: AtomicU64::new(0),
            devices: discovered.limits.devices.clone(),
            limits: Arc::new(ArcSwap::from_pointee(discovered.limits.clone())),
        };
        {
            let mut inner = sampler.held();
            inner.lifetime_at_create = inner.lifetime_peak();
        }
        // Prime the running peak and the last sample, so a read error on the very first call
        // still returns a coherent value.
        let _ = sampler.sample();
        Ok(sampler)
    }

    fn held(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// How many samples repeated the previous one because a file could not be read (DS-I3).
    pub fn read_errors(&self) -> u64 {
        self.read_errors.load(Ordering::Relaxed)
    }

    /// A timestamp that is strictly greater than every timestamp handed out before it (DS-I3).
    fn next_at_ns(&self) -> u64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let mut previous = self.last_at_ns.load(Ordering::Relaxed);
        loop {
            let candidate = if now > previous { now } else { previous + 1 };
            match self.last_at_ns.compare_exchange_weak(
                previous,
                candidate,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return candidate,
                Err(seen) => previous = seen,
            }
        }
    }
}

impl core::fmt::Debug for Sampler {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Sampler")
            .field("read_errors", &self.read_errors())
            .field("devices", &self.devices.len())
            .finish()
    }
}

impl SamplerTrait for Sampler {
    fn sample(&self) -> Sample {
        let at_ns = self.next_at_ns();
        let device_used = devices::device_used(&self.devices);
        let (ceiling_bytes, cpu_limit) = {
            let limits = self.limits.load();
            (limits.memory_ceiling, limits.cpu_quota)
        };
        let mut inner = self.held();
        let page_bytes = inner.page_bytes;
        let lifetime = inner.lifetime_peak();
        let Inner {
            source,
            buffer,
            running_peak,
            peak_reset_refused,
            last,
            process_peak,
            lifetime_at_create,
            mem_integral,
            integral_edge,
            ..
        } = &mut *inner;

        let reading = match source {
            Source::V2 {
                stat, peak, cpu, ..
            } => read_at_zero(stat, buffer)
                .map(cgroup::parse_memory_stat_v2)
                .map(|memory| {
                    let kernel_peak = peak
                        .as_ref()
                        .and_then(|file| read_at_zero(file, buffer))
                        .and_then(|text| text.trim().parse::<u64>().ok());
                    let throttled = cpu
                        .as_ref()
                        .and_then(|file| read_at_zero(file, buffer))
                        .map(cgroup::parse_throttled_usec_v2)
                        .unwrap_or(0);
                    (memory, kernel_peak, throttled)
                }),
            Source::V1 { stat, cpu } => read_at_zero(stat, buffer)
                .map(cgroup::parse_memory_stat_v1)
                .map(|memory| {
                    let throttled = cpu
                        .as_ref()
                        .and_then(|file| read_at_zero(file, buffer))
                        .map(cgroup::parse_throttled_usec_v1)
                        .unwrap_or(0);
                    (memory, None, throttled)
                }),
            Source::Os { proc_root, cpu } => {
                crate::os::process_memory(proc_root, page_bytes).map(|(anon, file)| {
                    let throttled = cpu
                        .as_ref()
                        .and_then(|cpu| {
                            let text = read_at_zero(&cpu.file, buffer)?;
                            Some(if cpu.v1 {
                                cgroup::parse_throttled_usec_v1(text)
                            } else {
                                cgroup::parse_throttled_usec_v2(text)
                            })
                        })
                        .unwrap_or(0);
                    (cgroup::MemoryStat { anon, file }, None, throttled)
                })
            }
        };

        let Some((memory, kernel_peak, throttled_us)) = reading else {
            self.read_errors.fetch_add(1, Ordering::Relaxed);
            let repeated = Sample {
                at_ns,
                device_used,
                ceiling_bytes,
                cpu_limit,
                ..*last
            };
            *last = repeated;
            return repeated;
        };

        *running_peak = (*running_peak).max(memory.anon);
        if let Some((then, bytes)) = *integral_edge
            && at_ns > then
        {
            *mem_integral += u128::from(bytes) * u128::from(at_ns - then);
        }
        *integral_edge = Some((at_ns, memory.anon));
        // A lifetime mark that has risen since the sampler was created is the process's peak
        // since then, to the byte; one that has not says the peak is below it, which the samples
        // bound. It is the run's peak and never a record's: the mark only rises, so between two
        // resets it would charge every `apply` with the highest moment of the whole interval.
        let risen = match (lifetime, *lifetime_at_create) {
            (Some(now), Some(then)) if now > then => Some(now),
            _ => None,
        };
        match risen {
            Some(exact) if exact > process_peak.bytes || !process_peak.exact => {
                *process_peak = ProcessPeak {
                    bytes: exact.max(process_peak.bytes),
                    at_ns,
                    exact: true,
                };
            }
            _ if memory.anon > process_peak.bytes => {
                *process_peak = ProcessPeak {
                    bytes: memory.anon,
                    at_ns,
                    exact: false,
                };
            }
            _ => {}
        }
        let sample = Sample {
            anon_bytes: memory.anon,
            file_bytes: memory.file,
            // `memory.peak` is the peak since it was last reset. Where the reset is refused, it
            // is the cgroup's peak since the cgroup was made, which on a shared cgroup predates
            // this process: a GitHub runner's, after the Rust build that ran there, read 13.4
            // GiB, and a probe of a few hundred bytes was "measured" to cost that. The sampler
            // then tracks its own peak from its samples, as the reset's note already said it
            // does (f.2; 2026-09-29).
            peak_anon_bytes: match kernel_peak {
                Some(kernel) if !*peak_reset_refused => kernel,
                _ => *running_peak,
            },
            throttled_us,
            device_used,
            at_ns,
            ceiling_bytes,
            cpu_limit,
        };
        *last = sample;
        sample
    }

    fn process_usage(&self) -> ProcessUsage {
        let inner = self.held();
        let cpu = match (inner.cpu_at_create_ns, process_cpu_ns()) {
            (Some(then), Some(now)) => Some(now.saturating_sub(then)),
            _ => None,
        };
        match cpu {
            Some(cpu_ns) if inner.integral_edge.is_some() => ProcessUsage {
                cpu_ns,
                mem_byte_seconds: u64::try_from(inner.mem_integral / 1_000_000_000)
                    .unwrap_or(u64::MAX),
                measured: true,
            },
            _ => ProcessUsage::default(),
        }
    }

    fn process_peak(&self) -> ProcessPeak {
        self.held().process_peak
    }

    fn reset_peak(&self) {
        let mut inner = self.held();
        let current = inner.last.anon_bytes;
        inner.running_peak = current;
        let refused = match &inner.source {
            Source::V2 {
                peak, peak_path, ..
            } if peak.is_some() => std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(peak_path)
                .and_then(|mut file| file.write_all(b"reset"))
                .is_err(),
            _ => false,
        };
        if refused && !inner.peak_reset_refused {
            inner.peak_reset_refused = true;
            tracing::debug!(
                target: "discovery.probe",
                "memory.peak is not writable on this kernel; the sampler tracks its own peak"
            );
        }
    }
}

impl Inner {
    /// The platform's lifetime high-water mark of the process's anonymous memory, where the
    /// sampler reads the platform and the platform keeps one (macOS's
    /// `ri_lifetime_max_phys_footprint`). A cgroup's own peak is `memory.peak`, read above.
    fn lifetime_peak(&self) -> Option<u64> {
        match self.source {
            Source::Os { .. } => crate::probes::platform_lifetime_peak(),
            Source::V2 { .. } | Source::V1 { .. } => None,
        }
    }
}

/// Open one cgroup file, or say which one could not be opened.
fn open(path: &Path) -> Result<File> {
    File::open(path).map_err(|err| moruna_kernel::MorunaError::Io {
        op: "open",
        target: path.display().to_string(),
        msg: err.to_string(),
    })
}

/// Read a cgroup file from offset 0 into the fixed buffer, with no allocation and no path lookup
/// (f.2). `None` on a read error or non-UTF-8 content, which the caller counts.
fn read_at_zero<'a>(file: &File, buffer: &'a mut [u8; BUFFER_BYTES]) -> Option<&'a str> {
    let read = file.read_at(buffer, 0).ok()?;
    std::str::from_utf8(&buffer[..read]).ok()
}

/// The process's own CPU time, user plus system, from `getrusage(RUSAGE_SELF)`.
fn process_cpu_ns() -> Option<u64> {
    // SAFETY: `getrusage` fills the zeroed struct it is handed and reads nothing else.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
        return None;
    }
    let ns = |t: libc::timeval| {
        u64::try_from(t.tv_sec)
            .ok()?
            .checked_mul(1_000_000_000)?
            .checked_add(u64::try_from(t.tv_usec).ok()?.checked_mul(1_000)?)
    };
    ns(usage.ru_utime)?.checked_add(ns(usage.ru_stime)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cgroup::fixtures::{TempDir, write};
    use crate::{Discovered, DiscoveryInput};
    use moruna_kernel::{HostProfile, LimitSource, Limits, TierKind};
    use std::sync::Arc;

    /// A `Discovered` pointing at a fixture cgroup directory.
    /// A `Discovered` pointing at a fixture cgroup directory whose limit is the ceiling, as in a
    /// pod, or at none.
    fn discovered_at(cgroup_path: Option<PathBuf>) -> Discovered {
        let source = match cgroup_path {
            Some(_) => LimitSource::Cgroup,
            None => LimitSource::Os,
        };
        discovered_with(cgroup_path, source)
    }

    /// A `Discovered` pointing at a fixture cgroup directory, with the ceiling from `source`.
    fn discovered_with(cgroup_path: Option<PathBuf>, source: LimitSource) -> Discovered {
        Discovered {
            limits: Limits {
                memory_ceiling: 1024 * 1024 * 1024,
                memory_kill: None,
                cpu_quota: 1.0,
                page_bytes: 4096,
                devices: Vec::new(),
                source,
                observed_at: 0,
            },
            profile: HostProfile::default(),
            host_tier: TierKind::Host,
            cgroup_path,
            disk_budget: 0,
            notes: Vec::new(),
        }
    }

    /// DS-T3 sample_cost (the reference host timing is E1; everything else runs anywhere):
    /// `at_ns` strictly increasing; peak tracking correct against a synthetic `memory.stat` that
    /// changes between reads; `reset_peak` then `sample` reports `peak_anon_bytes == anon_bytes`;
    /// 8 threads through `Arc<dyn Sampler>` see no torn sample. DS-I3.
    #[test]
    fn ds_t3_sample_cost() {
        let tmp = TempDir::new("ds-t3");
        let dir = tmp.path().join("cgroup");
        write(&dir, "memory.stat", "anon 1000\nfile 2000\nunevictable 0\n");
        write(&dir, "cpu.stat", "throttled_usec 7\n");
        let sampler = Sampler::with_roots(&discovered_at(Some(dir.clone())), tmp.path())
            .expect("sampler over the fixture");

        // Monotonic time and the first reading.
        let first = sampler.sample();
        assert_eq!(first.anon_bytes, 1000);
        assert_eq!(first.file_bytes, 2000);
        assert_eq!(first.throttled_us, 7);
        assert_eq!(first.peak_anon_bytes, 1000);
        let second = sampler.sample();
        assert!(second.at_ns > first.at_ns, "at_ns strictly increases");

        // The peak follows the synthetic state up and stays there when it falls back.
        write(&dir, "memory.stat", "anon 9000\nfile 1\nunevictable 1000\n");
        let high = sampler.sample();
        assert_eq!(high.anon_bytes, 10_000, "anon + unevictable, DS-I4");
        assert_eq!(high.peak_anon_bytes, 10_000);
        write(&dir, "memory.stat", "anon 500\nfile 1\nunevictable 0\n");
        let low = sampler.sample();
        assert_eq!(low.anon_bytes, 500);
        assert_eq!(low.peak_anon_bytes, 10_000, "the peak is remembered");

        // reset_peak then sample reports the peak at the current anonymous bytes.
        sampler.reset_peak();
        let after = sampler.sample();
        assert_eq!(after.peak_anon_bytes, after.anon_bytes);

        // Eight threads sampling and resetting at once: every sample is one of the states the
        // fixture ever held, and time never goes backwards for any of them.
        let shared: Arc<dyn SamplerTrait> = Arc::new(
            Sampler::with_roots(&discovered_at(Some(dir.clone())), tmp.path()).expect("sampler"),
        );
        let mut handles = Vec::new();
        for thread in 0..8 {
            let sampler = Arc::clone(&shared);
            handles.push(std::thread::spawn(move || {
                let mut previous = 0u64;
                for _ in 0..200 {
                    let sample = sampler.sample();
                    assert!(sample.at_ns > previous);
                    previous = sample.at_ns;
                    assert_eq!(sample.anon_bytes, 500, "one of the synthetic states");
                    assert_eq!(sample.file_bytes, 1);
                    if thread % 2 == 0 {
                        sampler.reset_peak();
                    }
                }
            }));
        }
        for handle in handles {
            handle.join().expect("no torn sample");
        }

        // The cost claim's structural half: 100,000 samples with no growth in read errors. The
        // 2 s bound belongs to the reference host (E1) and is reported as provisional elsewhere.
        let started = std::time::Instant::now();
        for _ in 0..100_000 {
            let _ = sampler.sample();
        }
        let elapsed = started.elapsed();
        assert_eq!(sampler.read_errors(), 0);
        println!("DS-T3: 100000 samples in {elapsed:?} (provisional, this host)");
    }

    /// DS-T4 anon_not_current: a synthetic `memory.stat` with a large `file` does not inflate
    /// `anon_bytes`, and `memory.current` is never the budget number. DS-I4.
    #[test]
    fn ds_t4_anon_not_current() {
        let tmp = TempDir::new("ds-t4");
        let dir = tmp.path().join("cgroup");
        write(
            &dir,
            "memory.stat",
            "anon 1048576\nfile 1073741824\nunevictable 2097152\nslab 4096\n",
        );
        write(&dir, "memory.current", "1077936128");
        let sampler = Sampler::with_roots(&discovered_at(Some(dir)), tmp.path()).expect("sampler");
        let sample = sampler.sample();
        assert_eq!(sample.anon_bytes, 1_048_576 + 2_097_152);
        assert_eq!(sample.file_bytes, 1_073_741_824);
        assert_ne!(sample.anon_bytes, 1_077_936_128, "never memory.current");
    }

    /// DS-T13 anon_excludes_mapped_file: a process that maps a 256 MiB file and reads every
    /// page of it does not see `anon_bytes` rise by the size of the file. DS-I4 makes the budget
    /// quantity anonymous plus unevictable, and a mapped Parquet source or a loaded dylib is
    /// evictable memory the runtime never allocated.
    ///
    /// This runs on every supported target, over the real host's sampler, and it is the test
    /// that would have caught the macOS path returning `pti_resident_size`: until 2026-09-23 it
    /// did, so every figure measured on a developer's macOS host was inflated by whatever the
    /// process had mapped, the Parquet source included.
    #[test]
    fn ds_t13_anon_excludes_mapped_file() {
        const MAPPED: usize = 256 << 20;
        // A quarter of the mapping is a generous allowance for the page tables and the buffers
        // the write below leaves behind, and still an order of magnitude below the file itself.
        const ALLOWED_RISE: u64 = (MAPPED / 4) as u64;

        let tmp = TempDir::new("ds-t13");
        let path = tmp.path().join("mapped.bin");
        std::fs::create_dir_all(tmp.path()).expect("directory");
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("the file to map");
        let block = vec![0xABu8; 1 << 20];
        for offset in (0..MAPPED).step_by(block.len()) {
            file.write_at(&block, offset as u64).expect("write");
        }
        file.sync_all().expect("sync");

        let sampler = Sampler::new(&discovered_at(None)).expect("sampler over the real host");
        let before = sampler.sample().anon_bytes;
        let during = crate::probes::with_mapped_file(&file, MAPPED, || sampler.sample())
            .expect("the mapping")
            .anon_bytes;

        let rise = during.saturating_sub(before);
        assert!(
            rise < ALLOWED_RISE,
            "anon_bytes rose by {rise} bytes while {MAPPED} bytes of file were mapped and read: \
             a mapped file is not anonymous memory (DS-I4)"
        );
    }

    /// f.2: `memory.peak` supplies the peak when the kernel has it, and `reset_peak` writes it.
    #[test]
    fn reads_and_resets_the_kernel_peak() {
        let tmp = TempDir::new("peak");
        let dir = tmp.path().join("cgroup");
        write(&dir, "memory.stat", "anon 100\nfile 0\nunevictable 0\n");
        write(&dir, "memory.peak", "999999\n");
        let sampler =
            Sampler::with_roots(&discovered_at(Some(dir.clone())), tmp.path()).expect("sampler");
        assert_eq!(sampler.sample().peak_anon_bytes, 999_999);
        sampler.reset_peak();
        let written = std::fs::read_to_string(dir.join("memory.peak")).expect("memory.peak");
        assert_eq!(written, "reset");
    }

    /// f.2, the peak the kernel keeps but will not let go of: where `memory.peak` cannot be
    /// reset, it is the cgroup's peak since the cgroup was made, which on a shared cgroup is
    /// someone else's high-water mark (a GitHub runner's, 13.4 GiB after the Rust build, which
    /// a probe of a few hundred bytes was then "measured" to cost on 2026-09-29). After a refused
    /// reset the sampler's own running peak is the peak, and the file is not read into it again.
    #[test]
    fn a_peak_that_cannot_be_reset_is_not_the_peak() {
        let tmp = TempDir::new("peak-refused");
        let dir = tmp.path().join("cgroup");
        write(&dir, "memory.stat", "anon 100\nfile 0\nunevictable 0\n");
        write(&dir, "memory.peak", "999999\n");
        let peak_path = dir.join("memory.peak");
        let mut perms = std::fs::metadata(&peak_path)
            .expect("memory.peak")
            .permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&peak_path, perms).expect("read-only memory.peak");
        if std::fs::OpenOptions::new()
            .write(true)
            .open(&peak_path)
            .is_ok()
        {
            // Root writes through a read-only bit: this process cannot stage the refusal.
            eprintln!("skipped: this process can write a read-only file");
            return;
        }
        let sampler =
            Sampler::with_roots(&discovered_at(Some(dir.clone())), tmp.path()).expect("sampler");
        // Until a reset is refused the kernel's figure stands, as before.
        assert_eq!(sampler.sample().peak_anon_bytes, 999_999);

        sampler.reset_peak();
        let after = sampler.sample();
        assert_eq!(
            after.peak_anon_bytes, 100,
            "after a refused reset the peak is the sampler's own, not the cgroup's lifetime mark"
        );
        assert_eq!(
            std::fs::read_to_string(&peak_path).expect("memory.peak"),
            "999999\n",
            "the refused write left the file alone"
        );

        // And the running peak keeps following the samples from there.
        write(&dir, "memory.stat", "anon 250\nfile 0\nunevictable 0\n");
        assert_eq!(sampler.sample().peak_anon_bytes, 250);
        write(&dir, "memory.stat", "anon 120\nfile 0\nunevictable 0\n");
        assert_eq!(sampler.sample().peak_anon_bytes, 250);
    }

    /// f.2, v1 fallback: the v1 field names are read and `throttled_time` is nanoseconds.
    #[test]
    fn samples_a_cgroup_v1_directory() {
        let tmp = TempDir::new("v1-sample");
        let dir = tmp.path().join("memory");
        write(&dir, "memory.limit_in_bytes", "2147483648");
        write(&dir, "memory.stat", "cache 2048\nrss 4096\n");
        write(&dir, "cpu.stat", "throttled_time 5000\n");
        let sampler = Sampler::with_roots(&discovered_at(Some(dir)), tmp.path()).expect("sampler");
        let sample = sampler.sample();
        assert_eq!(sample.anon_bytes, 4096);
        assert_eq!(sample.file_bytes, 2048);
        assert_eq!(sample.throttled_us, 5);
        sampler.reset_peak();
        assert_eq!(sampler.sample().peak_anon_bytes, 4096);
    }

    /// f.2, outside a cgroup: `/proc/self/status` where there is one, the platform otherwise, and
    /// `throttled_us` is zero either way.
    #[test]
    fn samples_outside_a_cgroup() {
        let tmp = TempDir::new("os-sample");
        let proc_dir = tmp.path().join("proc");
        write(&proc_dir, "self/status", crate::os::STATUS);
        let sampler = Sampler::with_roots(&discovered_at(None), &proc_dir).expect("sampler");
        let sample = sampler.sample();
        assert_eq!(sample.anon_bytes, 1600 * 1024);
        assert_eq!(sample.file_bytes, 400 * 1024);
        assert_eq!(sample.throttled_us, 0);
        assert_eq!(sample.device_used, [0u64; 8]);

        // And over the real host, which on macOS goes through the platform interface.
        let real = Sampler::new(&discovered_at(None)).expect("sampler over the real host");
        let sample = real.sample();
        assert!(sample.anon_bytes > 0, "the process has resident memory");
        assert!(sample.peak_anon_bytes >= sample.anon_bytes);
        real.reset_peak();
        assert!(real.sample().peak_anon_bytes > 0);
    }

    /// DS-T15 ceiling_scope: the sampler counts what the ceiling counts. In a cgroup whose limit
    /// is the ceiling, the cgroup's `anon + unevictable`; with an explicit budget, or a ceiling
    /// from the machine's RAM, the process's own `RssAnon`, however much the rest of the cgroup
    /// holds and whatever its `memory.peak` says, while `cpu.stat` is still the cgroup's. The
    /// fixture is the one that refused a 256 MiB run on 2026-09-30: `cargo test` and the test
    /// harness's parent held a gibibyte of the container's cgroup beside a child holding 1.6 MB.
    /// DS-I4.
    #[test]
    fn ds_t15_ceiling_scope() {
        let tmp = TempDir::new("ds-t15");
        let proc_dir = tmp.path().join("proc");
        write(&proc_dir, "self/status", crate::os::STATUS);
        let v2 = tmp.path().join("v2");
        write(
            &v2,
            "memory.stat",
            "anon 1073741824\nfile 0\nunevictable 0\n",
        );
        write(&v2, "memory.peak", "2147483648\n");
        write(&v2, "cpu.stat", "usage_usec 10\nthrottled_usec 42\n");
        let v1 = tmp.path().join("v1");
        write(&v1, "memory.limit_in_bytes", "4294967296");
        write(&v1, "memory.stat", "rss 1073741824\ncache 0\n");
        write(&v1, "cpu.stat", "throttled_time 5000\n");

        for (dir, throttled) in [(&v2, 42), (&v1, 5)] {
            for source in [LimitSource::Explicit, LimitSource::Os] {
                let sampler =
                    Sampler::with_roots(&discovered_with(Some(dir.clone()), source), &proc_dir)
                        .expect("sampler");
                let sample = sampler.sample();
                assert_eq!(
                    sample.anon_bytes,
                    1600 * 1024,
                    "{source:?}: RssAnon, not the cgroup"
                );
                assert_eq!(sample.file_bytes, 400 * 1024);
                assert_eq!(
                    sample.peak_anon_bytes,
                    1600 * 1024,
                    "{source:?}: the cgroup's memory.peak is not this process's peak"
                );
                assert_eq!(
                    sample.throttled_us, throttled,
                    "the cgroup throttles the process"
                );
                if !cfg!(target_vendor = "apple") {
                    // macOS joins its real lifetime mark to the samples, which a fixture file
                    // cannot stand in for; Linux keeps none, so the samples are the peak.
                    assert_eq!(sampler.process_peak().bytes, 1600 * 1024);
                }
            }
            let pod = Sampler::with_roots(
                &discovered_with(Some(dir.clone()), LimitSource::Cgroup),
                &proc_dir,
            )
            .expect("sampler");
            assert_eq!(
                pod.sample().anon_bytes,
                1 << 30,
                "the cgroup's limit is enforced on every process in it"
            );
        }

        // A cgroup with no `cpu.stat` the process can open is no throttling, as outside one.
        let bare = tmp.path().join("bare");
        write(
            &bare,
            "memory.stat",
            "anon 1073741824\nfile 0\nunevictable 0\n",
        );
        let sampler = Sampler::with_roots(
            &discovered_with(Some(bare), LimitSource::Explicit),
            &proc_dir,
        )
        .expect("sampler");
        let sample = sampler.sample();
        assert_eq!((sample.anon_bytes, sample.throttled_us), (1600 * 1024, 0));
    }

    /// DS-T16 status_file_pages_are_not_anon: `/proc/self/status` of a large debug test binary,
    /// whose mapped text and libraries are gigabytes of `RssFile` and whose `VmRSS` is mostly
    /// that, gives the budget only `RssAnon`; the file backed pages, the shared memory with them
    /// and the swapped pages are none of it, and neither the peak nor the process peak rises when
    /// a sibling maps more files. The same holds in a cgroup whose limit is not the ceiling.
    /// DS-I4.
    #[test]
    fn ds_t16_status_file_pages_are_not_anon() {
        const MIB: u64 = 1024 * 1024;
        let status = |file_kib: u64| {
            format!(
                "Name:\twhole_process-1\nVmHWM:\t{hwm} kB\nVmRSS:\t{rss} kB\n\
                 RssAnon:\t 40960 kB\nRssFile:\t {file_kib} kB\nRssShmem:\t 65536 kB\n\
                 VmSwap:\t 1024 kB\nThreads:\t9\n",
                hwm = 40960 + file_kib + 65536,
                rss = 40960 + file_kib + 65536,
            )
        };
        let tmp = TempDir::new("ds-t16");
        let proc_dir = tmp.path().join("proc");
        let cgroup = tmp.path().join("cgroup");
        write(
            &cgroup,
            "memory.stat",
            "anon 999999999\nfile 0\nunevictable 0\n",
        );
        for discovered in [
            discovered_at(None),
            discovered_with(Some(cgroup.clone()), LimitSource::Explicit),
        ] {
            write(&proc_dir, "self/status", &status(3 * 1024 * 1024));
            let sampler = Sampler::with_roots(&discovered, &proc_dir).expect("sampler");
            let sample = sampler.sample();
            assert_eq!(sample.anon_bytes, 40 * MIB, "RssAnon alone, never VmRSS");
            assert_eq!(sample.file_bytes, 3 * 1024 * MIB + 64 * MIB);

            // More mapped file, the same anonymous memory: nothing the budget counts moved.
            write(&proc_dir, "self/status", &status(6 * 1024 * 1024));
            let sample = sampler.sample();
            assert_eq!(sample.anon_bytes, 40 * MIB);
            assert_eq!(sample.peak_anon_bytes, 40 * MIB);
            if !cfg!(target_vendor = "apple") {
                // As in DS-T15: macOS joins its real lifetime mark, Linux keeps none.
                assert_eq!(sampler.process_peak().bytes, 40 * MIB);
            }
        }
    }

    /// F8.9: the process peak is the whole process's high-water mark since the sampler was
    /// created, whatever `reset_peak` does to the probe's peak. On macOS it is the kernel's own
    /// lifetime mark, so a burst no sample saw is still in it; elsewhere it is the highest sample.
    #[test]
    fn the_process_peak_is_the_whole_process_and_survives_a_reset() {
        const BURST: usize = 64 << 20;
        let sampler = Sampler::new(&discovered_at(None)).expect("sampler over the real host");
        let before = sampler.sample().anon_bytes;
        let burst = vec![7u8; BURST];
        if !cfg!(target_vendor = "apple") {
            // Only a sample sees the burst where the platform keeps no mark of its own.
            let _ = sampler.sample();
        }
        drop(std::hint::black_box(burst));
        sampler.reset_peak();
        let after = sampler.sample();
        let peak = sampler.process_peak();
        assert!(
            peak.bytes >= before + (BURST as u64) / 2,
            "the burst of {BURST} bytes over {before} is in the peak: {peak:?}"
        );
        assert!(peak.bytes >= after.anon_bytes);
        assert!(peak.at_ns > 0);
        assert_eq!(peak.exact, cfg!(target_vendor = "apple"), "{peak:?}");
        // A fixture cgroup keeps no lifetime mark: the peak is the highest sample.
        let tmp = TempDir::new("process-peak");
        let dir = tmp.path().join("cgroup");
        write(&dir, "memory.stat", "anon 5000\nfile 0\nunevictable 0\n");
        let fixture =
            Sampler::with_roots(&discovered_at(Some(dir.clone())), tmp.path()).expect("sampler");
        write(&dir, "memory.stat", "anon 900\nfile 0\nunevictable 0\n");
        fixture.reset_peak();
        let _ = fixture.sample();
        let peak = fixture.process_peak();
        assert_eq!((peak.bytes, peak.exact), (5000, false));
    }

    /// DS-I3: a read error repeats the last sample with a fresh timestamp and is counted.
    #[test]
    fn a_read_error_repeats_the_last_sample() {
        let tmp = TempDir::new("read-error");
        let proc_dir = tmp.path().join("proc-absent");
        let sampler = Sampler::with_roots(&discovered_at(None), &proc_dir).expect("sampler");
        // On a host with a real /proc or a platform interface there is no error to provoke here;
        // the fixture path is the one that can fail, so drive it through a removed file.
        let dir = tmp.path().join("cgroup");
        write(&dir, "memory.stat", "anon 42\nfile 0\nunevictable 0\n");
        let over_cgroup =
            Sampler::with_roots(&discovered_at(Some(dir.clone())), tmp.path()).expect("sampler");
        let good = over_cgroup.sample();
        assert_eq!(good.anon_bytes, 42);
        // Truncating the file to nothing parses as zeros rather than failing, so make the read
        // itself fail by replacing the handle's content with invalid UTF-8.
        std::fs::write(dir.join("memory.stat"), [0xff, 0xfe, 0xfd]).expect("invalid utf-8");
        let repeated = over_cgroup.sample();
        assert_eq!(over_cgroup.read_errors(), 1);
        assert_eq!(repeated.anon_bytes, good.anon_bytes);
        assert!(repeated.at_ns > good.at_ns);
        // The operating system sampler answers or counts an error, never panics.
        let _ = sampler.sample();
    }

    /// The sampler refuses to start when a cgroup file it needs cannot be opened.
    #[test]
    fn new_fails_on_an_unreadable_cgroup() {
        let tmp = TempDir::new("unreadable");
        let discovered = discovered_at(Some(tmp.path().join("absent")));
        let err = Sampler::with_roots(&discovered, tmp.path()).expect_err("no memory.stat");
        assert!(matches!(
            err,
            moruna_kernel::MorunaError::Io { op: "open", .. }
        ));
    }

    /// d.1: the sampler is the shape the facade shares (`Arc<dyn Sampler>`), and `DiscoveryInput`
    /// defaults to "discover everything".
    #[test]
    fn is_shareable_as_the_contract_trait() {
        let tmp = TempDir::new("shape");
        let sampler: Arc<dyn SamplerTrait> =
            Arc::new(Sampler::with_roots(&discovered_at(None), tmp.path()).expect("sampler"));
        let _ = sampler.sample();
        sampler.reset_peak();
        let input = DiscoveryInput::default();
        assert!(input.explicit_budget.is_none());
    }

    /// sample_carries_the_limits. Every sample carries the ceiling and the quota in force
    /// when it was taken: the discovered ones for a sampler of its own, and whatever the watcher
    /// last published for one that follows its cell, from the very next sample.
    #[test]
    fn sample_carries_the_limits() {
        let tmp = TempDir::new("ds-t19");
        let dir = tmp.path().join("cgroup");
        write(&dir, "memory.stat", "anon 1000\nfile 2000\nunevictable 0\n");
        let own =
            Sampler::with_roots(&discovered_at(Some(dir.clone())), tmp.path()).expect("sampler");
        let sample = own.sample();
        assert_eq!(sample.ceiling_bytes, 1024 * 1024 * 1024);
        assert!((sample.cpu_limit - 1.0).abs() < f64::EPSILON);

        let discovered = discovered_at(Some(dir.clone()));
        let cell = Arc::new(ArcSwap::from_pointee(discovered.limits.clone()));
        let following = Sampler::following(&discovered, Arc::clone(&cell)).expect("sampler");
        assert_eq!(following.sample().ceiling_bytes, 1024 * 1024 * 1024);
        cell.store(Arc::new(Limits {
            memory_ceiling: 3 * 1024 * 1024 * 1024,
            cpu_quota: 6.0,
            ..discovered.limits.clone()
        }));
        let sample = following.sample();
        assert_eq!(sample.ceiling_bytes, 3 * 1024 * 1024 * 1024);
        assert!((sample.cpu_limit - 6.0).abs() < f64::EPSILON);
    }
    /// The process's usage is measured from the operating system: CPU time grows while the
    /// process works, and the memory integral grows between samples. Never an estimate.
    #[test]
    fn process_usage_is_measured_not_estimated() {
        let sampler = Sampler::new(&discovered_at(None)).expect("sampler over the real host");
        let _ = sampler.sample();
        let mut x = 0u64;
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_millis(60) {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
        }
        std::hint::black_box(x);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let _ = sampler.sample();
        let usage = sampler.process_usage();
        assert!(usage.measured, "{usage:?}");
        assert!(
            usage.cpu_ns >= 30_000_000,
            "60 ms of work is at least 30 ms of CPU: {usage:?}"
        );
        assert!(usage.mem_byte_seconds > 0, "{usage:?}");
    }
}
