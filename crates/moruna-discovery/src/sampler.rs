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
use moruna_kernel::{Device, Limits, ProcessPeak, Result, Sample, Sampler as SamplerTrait};

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
    /// No cgroup: `/proc/self/status`, or the platform's own interface (os.rs).
    Os { proc_root: PathBuf },
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
    /// The platform's lifetime high-water mark when the sampler was created and when the peak
    /// was last reset, where the platform keeps one (macOS). A rise above either is the
    /// process's exact peak since then, however short the burst that made it.
    lifetime_at_create: Option<u64>,
    lifetime_at_reset: Option<u64>,
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
        let source = match discovered.cgroup_path.as_deref() {
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
                lifetime_at_reset: None,
            }),
            read_errors: AtomicU64::new(0),
            last_at_ns: AtomicU64::new(0),
            devices: discovered.limits.devices.clone(),
            limits: Arc::new(ArcSwap::from_pointee(discovered.limits.clone())),
        };
        {
            let mut inner = sampler.held();
            let lifetime = inner.lifetime_peak();
            inner.lifetime_at_create = lifetime;
            inner.lifetime_at_reset = lifetime;
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
            last,
            process_peak,
            lifetime_at_create,
            lifetime_at_reset,
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
            Source::Os { proc_root } => crate::os::process_memory(proc_root, page_bytes)
                .map(|(anon, file)| (cgroup::MemoryStat { anon, file }, None, 0u64)),
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
        // A lifetime mark that has risen since the last reset is the peak since then, to the
        // byte; one that has not says the peak since then is below it, which the samples bound.
        let risen = |mark: &Option<u64>| match (lifetime, *mark) {
            (Some(now), Some(then)) if now > then => Some(now),
            _ => None,
        };
        let since_reset = kernel_peak
            .unwrap_or(*running_peak)
            .max(risen(lifetime_at_reset).unwrap_or(0));
        match risen(lifetime_at_create) {
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
            peak_anon_bytes: since_reset,
            throttled_us,
            device_used,
            at_ns,
            ceiling_bytes,
            cpu_limit,
        };
        *last = sample;
        sample
    }

    fn process_peak(&self) -> ProcessPeak {
        self.held().process_peak
    }

    fn reset_peak(&self) {
        let mut inner = self.held();
        let current = inner.last.anon_bytes;
        inner.running_peak = current;
        inner.lifetime_at_reset = inner.lifetime_peak();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cgroup::fixtures::{TempDir, write};
    use crate::{Discovered, DiscoveryInput};
    use moruna_kernel::{HostProfile, LimitSource, Limits, TierKind};
    use std::sync::Arc;

    /// A `Discovered` pointing at a fixture cgroup directory.
    fn discovered_at(cgroup_path: Option<PathBuf>) -> Discovered {
        Discovered {
            limits: Limits {
                memory_ceiling: 1024 * 1024 * 1024,
                memory_kill: None,
                cpu_quota: 1.0,
                page_bytes: 4096,
                devices: Vec::new(),
                source: LimitSource::Os,
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
}
