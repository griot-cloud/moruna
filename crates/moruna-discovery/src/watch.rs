//! The limits watcher: discovery re-read every controller tick while the run is in progress, so
//! the budget follows the machine (MH 4.4).
//!
//! `discover` reads the host once. A host that resizes the machine under a run (a cgroup whose
//! `memory.max` or `cpu.max` is rewritten, a microVM whose RAM or vCPUs are hot-plugged) changes
//! the numbers that reading produced, and nothing would notice. The watcher reads the same
//! sources again, derives `Limits` through the same precedence (`limits::derive_from`), and
//! publishes the result into an `Arc<ArcSwap<Limits>>` that the sampler and the facade share.
//!
//! Two sources are read. Under a cgroup: `memory.max`, `memory.high` and `cpu.max`. In a guest
//! with no cgroup of its own, where the machine is the budget: `/proc/meminfo` `MemTotal` and
//! `/sys/devices/system/cpu/online`. Which one supplies each figure is decided by e.3 exactly as
//! at start, because the derivation is the same function.
//!
//! A change smaller than one huge page of memory or one CPU is not a change: it is
//! noise, a kernel's own accounting drifting, and a controller that re-planned on every page
//! would never settle. The comparison is against the limits last *published*, so a slow drift
//! still crosses the threshold once it adds up to a huge page.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use moruna_kernel::{Limits, LimitsChangeReason, LimitsChanged, Result};

use crate::cgroup::{self, Cgroup};
use crate::env::{EnvSource, ProcessEnv};
use crate::{Discovered, DiscoveryInput, limits};

/// The memory threshold: one huge page. A change of the ceiling or the kill line
/// smaller than this is ignored.
pub const HUGE_PAGE_BYTES: u64 = 2 * 1024 * 1024;

/// The CPU threshold: one CPU. A change of the quota smaller than this is ignored.
pub const CPU_STEP: f64 = 1.0;

/// One reading of every source e.3 derives from. Public so that a source other than the host's
/// files (a test's scripted machine, a host that reports its own figures) can hand the watcher
/// the same thing the files would.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LimitsReading {
    /// cgroup `memory.max`, the kill line; `None` when unset or unreadable.
    pub memory_max: Option<u64>,
    /// cgroup `memory.high`; `None` when unset.
    pub memory_high: Option<u64>,
    /// cgroup `cpu.max` as cores; `None` when unset (`max`).
    pub cpu_max: Option<f64>,
    /// `/proc/meminfo` `MemTotal` in bytes (the platform's figure where there is no `/proc`).
    pub total_ram: Option<u64>,
    /// CPUs online: the count `/sys/devices/system/cpu/online` lists, or the operating system's
    /// logical core count where that file cannot be read.
    pub cpus_online: f64,
}

/// Where the watcher reads the machine from.
pub trait LimitsSource: Send + Sync {
    /// Read every source once. Never fails: a file that cannot be read is a `None` field, which
    /// e.3 already knows how to derive around.
    fn read(&self) -> LimitsReading;
}

/// The host's own files: the cgroup this process is in, `/proc/meminfo` and
/// `/sys/devices/system/cpu/online`.
pub struct HostLimitsSource {
    cgroup: Cgroup,
    proc_root: PathBuf,
    sys_root: PathBuf,
}

impl HostLimitsSource {
    /// The real host: `/sys/fs/cgroup`, `/proc` and `/sys`.
    pub fn new() -> HostLimitsSource {
        HostLimitsSource::with_roots(
            Path::new("/sys/fs/cgroup"),
            Path::new("/proc"),
            Path::new("/sys"),
        )
    }

    /// The same, with the three roots as parameters so the whole path is tested against a
    /// fixture directory on a host that has none of them (section l).
    pub fn with_roots(cgroup_root: &Path, proc_root: &Path, sys_root: &Path) -> HostLimitsSource {
        HostLimitsSource {
            cgroup: cgroup::locate(cgroup_root, proc_root),
            proc_root: proc_root.to_path_buf(),
            sys_root: sys_root.to_path_buf(),
        }
    }
}

impl Default for HostLimitsSource {
    fn default() -> HostLimitsSource {
        HostLimitsSource::new()
    }
}

impl LimitsSource for HostLimitsSource {
    fn read(&self) -> LimitsReading {
        limits::read(&self.cgroup, &self.proc_root, Some(&self.sys_root))
    }
}

/// A source whose reading is set by hand: a machine a test scripts, or a host that knows its
/// own figures and pushes them. `set` is visible to the next `poll`.
#[derive(Clone, Default)]
pub struct ManualLimitsSource {
    reading: Arc<Mutex<LimitsReading>>,
}

impl ManualLimitsSource {
    /// A source that reads `initial` until `set` says otherwise.
    pub fn new(initial: LimitsReading) -> ManualLimitsSource {
        ManualLimitsSource {
            reading: Arc::new(Mutex::new(initial)),
        }
    }

    /// Replace what the next `read` returns.
    pub fn set(&self, reading: LimitsReading) {
        *self
            .reading
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = reading;
    }

    /// Change one part of the reading in place.
    pub fn update(&self, change: impl FnOnce(&mut LimitsReading)) {
        let mut reading = self
            .reading
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        change(&mut reading);
    }
}

impl LimitsSource for ManualLimitsSource {
    fn read(&self) -> LimitsReading {
        self.reading
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

/// The largest figures a run may follow the machine up to (`budget.elastic`). A reading
/// above either is clamped to it, with a note: the host sized the spec, and a machine that grew
/// past it is growing for somebody else.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ElasticBounds {
    /// The highest memory ceiling the run follows the machine to, in bytes.
    pub memory_max_bytes: u64,
    /// The highest CPU quota the run follows the machine to, in cores.
    pub cpu_max: f64,
}

/// Called once per accepted change, after it is published (the seam the host protocol's
/// `limits_changed` message hangs on, MH 4.3).
pub type LimitsSubscriber = Box<dyn Fn(&LimitsChanged) + Send + Sync>;

/// The mutable half of the watcher.
struct WatchState {
    notes: Vec<String>,
    polls: u64,
    changes: u64,
}

/// Discovery, re-read every controller tick while the run is in progress.
pub struct LimitsWatch {
    source: Arc<dyn LimitsSource>,
    current: Arc<ArcSwap<Limits>>,
    explicit_budget: Option<u64>,
    explicit_cpu: Option<f64>,
    bounds: Option<ElasticBounds>,
    subscribers: Mutex<Vec<LimitsSubscriber>>,
    state: Mutex<WatchState>,
}

impl core::fmt::Debug for LimitsWatch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LimitsWatch")
            .field("current", &self.current.load())
            .field("bounds", &self.bounds)
            .finish()
    }
}

impl LimitsWatch {
    /// A watcher over the real host's files, starting from what `discover` found.
    pub fn host(discovered: &Discovered, input: &DiscoveryInput) -> Result<LimitsWatch> {
        LimitsWatch::new(discovered, input, Arc::new(HostLimitsSource::new()))
    }

    /// A watcher over `source`, starting from what `discover` found. `input` is the same one
    /// `discover` was given: an explicit budget or CPU quota (the constructor's, or
    /// `MORUNA_BUDGET` and `MORUNA_CPU`) still beats the machine (DS-I1), and the watcher reads
    /// the environment the same way `discover` did.
    pub fn new(
        discovered: &Discovered,
        input: &DiscoveryInput,
        source: Arc<dyn LimitsSource>,
    ) -> Result<LimitsWatch> {
        LimitsWatch::with_env(&ProcessEnv, discovered, input, source)
    }

    /// [`LimitsWatch::new`] with the environment supplied (the crate's tests).
    pub(crate) fn with_env(
        env: &dyn EnvSource,
        discovered: &Discovered,
        input: &DiscoveryInput,
        source: Arc<dyn LimitsSource>,
    ) -> Result<LimitsWatch> {
        let (explicit_budget, explicit_cpu) = crate::explicit(env, input)?;
        let mut initial = discovered.limits.clone();
        if initial.observed_at == 0 {
            initial.observed_at = limits::now_ns();
        }
        Ok(LimitsWatch {
            source,
            current: Arc::new(ArcSwap::from_pointee(initial)),
            explicit_budget,
            explicit_cpu,
            bounds: None,
            subscribers: Mutex::new(Vec::new()),
            state: Mutex::new(WatchState {
                notes: Vec::new(),
                polls: 0,
                changes: 0,
            }),
        })
    }

    /// Clamp every later reading to `bounds` (`budget.elastic`). A run that sets none still
    /// follows the machine down; it follows it up only as far as the bounds say, and the facade
    /// passes the starting figures when the spec has no `elastic`, so an inelastic run never
    /// grows (MH H-Q4).
    pub fn with_bounds(mut self, bounds: ElasticBounds) -> LimitsWatch {
        self.bounds = Some(bounds);
        self
    }

    /// The bounds in force, if any.
    pub fn bounds(&self) -> Option<ElasticBounds> {
        self.bounds
    }

    /// The shared cell the watcher publishes into. The sampler reads it on every sample
    /// (`Sample::ceiling_bytes`, `Sample::cpu_limit`), and the facade holds it.
    pub fn limits(&self) -> Arc<ArcSwap<Limits>> {
        Arc::clone(&self.current)
    }

    /// The limits in force now.
    pub fn current(&self) -> Arc<Limits> {
        self.current.load_full()
    }

    /// Register a callback for every change the watcher accepts. It is called on the thread that
    /// polls, after the new limits are published, and must not block: the host protocol's
    /// `limits_changed` message is sent from one (MH 4.3).
    pub fn subscribe(&self, subscriber: LimitsSubscriber) {
        self.subscribers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(subscriber);
    }

    /// Notes the watcher made (a clamp to the elastic bounds), each once, for the run report.
    pub fn notes(&self) -> Vec<String> {
        self.lock_state().notes.clone()
    }

    /// How many times `poll` has run, and how many changes it accepted.
    pub fn counts(&self) -> (u64, u64) {
        let state = self.lock_state();
        (state.polls, state.changes)
    }

    /// Read the machine once. When the derived limits differ from the published ones by at
    /// least one huge page of memory or one CPU, publish them, tell every subscriber and
    /// return the change; otherwise publish nothing and return `None`.
    pub fn poll(&self) -> Option<LimitsChanged> {
        let reading = self.source.read();
        let old = self.current.load_full();
        let mut notes = Vec::new();
        let mut new = limits::derive_from(
            &reading,
            self.explicit_budget,
            self.explicit_cpu,
            old.page_bytes,
            old.devices.clone(),
            &mut notes,
        );
        let clamp_note = self.clamp(&mut new);

        let memory_moved = moved_bytes(old.memory_ceiling, new.memory_ceiling)
            || match (old.memory_kill, new.memory_kill) {
                (Some(a), Some(b)) => moved_bytes(a, b),
                (None, None) => false,
                (Some(_), None) | (None, Some(_)) => true,
            };
        let cpu_moved = (old.cpu_quota - new.cpu_quota).abs() >= CPU_STEP;
        let reason = match (memory_moved, cpu_moved) {
            (true, true) => Some(LimitsChangeReason::MemoryAndCpu),
            (true, false) => Some(LimitsChangeReason::Memory),
            (false, true) => Some(LimitsChangeReason::Cpu),
            (false, false) => None,
        };

        {
            let mut state = self.lock_state();
            state.polls = state.polls.saturating_add(1);
            if reason.is_some()
                && let Some(note) = clamp_note
                && !state.notes.contains(&note)
            {
                state.notes.push(note);
            }
            if reason.is_some() {
                state.changes = state.changes.saturating_add(1);
            }
        }
        let reason = reason?;

        // A figure that did not move by a whole step keeps its published value, so a small drift
        // in one never rides in on a real change of the other.
        if !memory_moved {
            new.memory_ceiling = old.memory_ceiling;
            new.memory_kill = old.memory_kill;
        }
        if !cpu_moved {
            new.cpu_quota = old.cpu_quota;
        }
        let change = LimitsChanged {
            at_ns: new.observed_at,
            old: (*old).clone(),
            new: new.clone(),
            reason,
        };
        self.current.store(Arc::new(new));
        tracing::info!(
            target: "discovery.limits_changed",
            reason = reason.name(),
            old_ceiling = change.old.memory_ceiling,
            new_ceiling = change.new.memory_ceiling,
            old_cpu = change.old.cpu_quota,
            new_cpu = change.new.cpu_quota,
            "limits changed"
        );
        let subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for subscriber in subscribers.iter() {
            subscriber(&change);
        }
        Some(change)
    }

    /// Hold `limits` to the elastic bounds, returning the note when a figure was clamped.
    fn clamp(&self, limits: &mut Limits) -> Option<String> {
        let bounds = self.bounds?;
        let mut clamped = Vec::new();
        if limits.memory_ceiling > bounds.memory_max_bytes {
            clamped.push(format!(
                "the machine offers a memory ceiling of {} bytes and budget.elastic.memory_max_bytes \
                 is {}; the ceiling is clamped to it",
                limits.memory_ceiling, bounds.memory_max_bytes
            ));
            limits.memory_ceiling = bounds.memory_max_bytes;
        }
        if limits.cpu_quota > bounds.cpu_max {
            clamped.push(format!(
                "the machine offers {} CPUs and budget.elastic.cpu_max is {}; the quota is clamped \
                 to it",
                limits.cpu_quota, bounds.cpu_max
            ));
            limits.cpu_quota = bounds.cpu_max;
        }
        if clamped.is_empty() {
            None
        } else {
            Some(clamped.join("; "))
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, WatchState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Whether two byte figures differ by at least one huge page.
fn moved_bytes(a: u64, b: u64) -> bool {
    a.abs_diff(b) >= HUGE_PAGE_BYTES
}

/// Count the CPUs a kernel CPU list names: `"0-3,6,8-9"` is seven. `None` for a list that does
/// not parse, which the caller answers with the operating system's own count.
pub fn parse_cpu_list(text: &str) -> Option<u32> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut count = 0u32;
    for part in text.split(',') {
        let part = part.trim();
        match part.split_once('-') {
            Some((lo, hi)) => {
                let lo: u32 = lo.trim().parse().ok()?;
                let hi: u32 = hi.trim().parse().ok()?;
                if hi < lo {
                    return None;
                }
                count = count.checked_add(hi - lo + 1)?;
            }
            None => {
                let _: u32 = part.parse().ok()?;
                count = count.checked_add(1)?;
            }
        }
    }
    Some(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cgroup::fixtures::{TempDir, cgroup_v2, proc_root, write};
    use crate::env::MapEnv;
    use moruna_kernel::{HostProfile, LimitSource, TierKind};
    use std::sync::atomic::{AtomicU64, Ordering};

    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;

    fn discovered(limits: Limits) -> Discovered {
        Discovered {
            limits,
            profile: HostProfile::default(),
            host_tier: TierKind::Host,
            cgroup_path: None,
            disk_budget: 0,
            notes: Vec::new(),
        }
    }

    fn start(ceiling: u64, cpu: f64) -> Limits {
        Limits {
            memory_ceiling: ceiling,
            memory_kill: None,
            cpu_quota: cpu,
            page_bytes: 4096,
            devices: Vec::new(),
            source: LimitSource::Os,
            observed_at: 0,
        }
    }

    fn guest(total: u64, cpus: f64) -> LimitsReading {
        LimitsReading {
            total_ram: Some(total),
            cpus_online: cpus,
            ..LimitsReading::default()
        }
    }

    fn watch(source: &ManualLimitsSource, initial: Limits) -> LimitsWatch {
        LimitsWatch::with_env(
            &MapEnv::default(),
            &discovered(initial),
            &DiscoveryInput::default(),
            Arc::new(source.clone()),
        )
        .expect("watch")
    }

    /// watch_follows_the_machine. A guest whose RAM and CPUs are hot-plugged: the watcher
    /// re-derives through e.3 (0.9 x MemTotal, the online count), publishes into the swap the
    /// sampler reads, tells its subscribers, and stamps `observed_at`.
    #[test]
    fn watch_follows_the_machine() {
        let source = ManualLimitsSource::new(guest(8 * GIB, 4.0));
        let w = watch(&source, start(fraction(8 * GIB), 4.0));
        assert!(w.current().observed_at > 0, "the first reading is stamped");
        let seen = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&seen);
        w.subscribe(Box::new(move |change| {
            counter.store(change.new.memory_ceiling, Ordering::SeqCst);
        }));

        assert!(w.poll().is_none(), "an unchanged machine is no change");
        source.set(guest(16 * GIB, 4.0));
        let change = w.poll().expect("RAM doubled");
        assert_eq!(change.reason, LimitsChangeReason::Memory);
        assert_eq!(change.old.memory_ceiling, fraction(8 * GIB));
        assert_eq!(change.new.memory_ceiling, fraction(16 * GIB));
        assert_eq!(change.new.source, LimitSource::Os);
        assert_eq!(w.limits().load().memory_ceiling, fraction(16 * GIB));
        assert_eq!(
            seen.load(Ordering::SeqCst),
            fraction(16 * GIB),
            "the subscriber was told, after the publish"
        );

        source.set(guest(16 * GIB, 8.0));
        let change = w.poll().expect("CPUs doubled");
        assert_eq!(change.reason, LimitsChangeReason::Cpu);
        assert!((w.current().cpu_quota - 8.0).abs() < f64::EPSILON);

        source.set(guest(4 * GIB, 2.0));
        let change = w.poll().expect("both lowered");
        assert_eq!(change.reason, LimitsChangeReason::MemoryAndCpu);
        assert_eq!(change.reason.name(), "memory,cpu");
        assert!(change.at_ns >= change.old.observed_at);
        assert_eq!(w.counts(), (4, 3));
        assert!(format!("{w:?}").contains("LimitsWatch"));
    }

    /// small_changes_ignored. Less than a huge page of memory or less than a CPU is not a
    /// change; a drift that adds up to one is, and the figure that did not move keeps its
    /// published value.
    #[test]
    fn small_changes_ignored() {
        let base = 8 * GIB;
        let source = ManualLimitsSource::new(LimitsReading {
            memory_max: Some(base),
            memory_high: Some(base / 2),
            cpu_max: Some(2.0),
            ..LimitsReading::default()
        });
        let w = watch(
            &source,
            Limits {
                memory_kill: Some(base),
                ..start(base / 2, 2.0)
            },
        );
        source.update(|r| r.memory_high = Some(base / 2 + MIB));
        assert!(w.poll().is_none(), "one MiB is below a huge page");
        source.update(|r| r.cpu_max = Some(2.5));
        assert!(w.poll().is_none(), "half a CPU is below one CPU");
        source.update(|r| r.memory_high = Some(base / 2 + 3 * MIB));
        let change = w.poll().expect("the drift reached a huge page");
        assert_eq!(change.reason, LimitsChangeReason::Memory);
        assert_eq!(change.new.memory_ceiling, base / 2 + 3 * MIB);
        assert!(
            (change.new.cpu_quota - 2.0).abs() < f64::EPSILON,
            "the half CPU did not ride in on the memory change"
        );
        // The kill line appearing or going away is a change on its own.
        source.update(|r| r.memory_max = None);
        let change = w.poll().expect("the kill line went away");
        assert_eq!(change.new.memory_kill, None);
        assert_eq!(change.reason, LimitsChangeReason::Memory);
    }

    /// watch_precedence. The watcher derives through e.3 exactly as `discover` did: an
    /// explicit budget and CPU quota still win over a resized machine, `memory.high` still beats
    /// `0.9 x memory.max`, and DS-I2 still holds. DS-I1, DS-I2.
    #[test]
    fn watch_precedence() {
        let source = ManualLimitsSource::new(LimitsReading {
            memory_max: Some(4 * GIB),
            cpu_max: Some(2.0),
            total_ram: Some(64 * GIB),
            cpus_online: 16.0,
            ..LimitsReading::default()
        });
        let input = DiscoveryInput {
            explicit_budget: Some(2 * GIB),
            explicit_cpu: Some(3.0),
            ..DiscoveryInput::default()
        };
        let w = LimitsWatch::with_env(
            &MapEnv::default(),
            &discovered(Limits {
                memory_kill: Some(4 * GIB),
                source: LimitSource::Explicit,
                ..start(2 * GIB, 3.0)
            }),
            &input,
            Arc::new(source.clone()),
        )
        .expect("watch");
        source.update(|r| r.cpu_max = Some(12.0));
        assert!(w.poll().is_none(), "the explicit budget and quota stand");
        source.update(|r| r.memory_max = Some(16 * GIB));
        let change = w.poll().expect("the kill line moved");
        assert_eq!(change.new.memory_kill, Some(16 * GIB));
        assert_eq!(
            change.new.memory_ceiling,
            2 * GIB,
            "the explicit budget stands"
        );
        assert!((change.new.cpu_quota - 3.0).abs() < f64::EPSILON);
        // A kill line lowered under the explicit budget still clamps it (DS-I1, DS-I2).
        source.update(|r| r.memory_max = Some(GIB));
        let change = w.poll().expect("clamped to the kill line");
        assert_eq!(change.new.memory_ceiling, fraction_of(GIB, 0.95));
        assert_eq!(change.new.source, LimitSource::Explicit);

        // The environment is read the way discover reads it.
        let env = MapEnv::with(&[("MORUNA_BUDGET", "1GiB"), ("MORUNA_CPU", "2")]);
        let w = LimitsWatch::with_env(
            &env,
            &discovered(start(GIB, 2.0)),
            &DiscoveryInput::default(),
            Arc::new(ManualLimitsSource::new(guest(64 * GIB, 32.0))),
        )
        .expect("watch");
        assert!(w.poll().is_none(), "MORUNA_BUDGET and MORUNA_CPU stand");
        let env = MapEnv::with(&[("MORUNA_BUDGET", "lots")]);
        assert!(
            LimitsWatch::with_env(
                &env,
                &discovered(start(GIB, 2.0)),
                &DiscoveryInput::default(),
                Arc::new(ManualLimitsSource::default()),
            )
            .is_err(),
            "a malformed variable fails as it does in discover"
        );

        // memory.high beats 0.9 x memory.max, as at start.
        let source = ManualLimitsSource::new(LimitsReading {
            memory_max: Some(8 * GIB),
            memory_high: Some(6 * GIB),
            cpu_max: Some(4.0),
            ..LimitsReading::default()
        });
        let w = watch(&source, start(6 * GIB, 4.0));
        source.update(|r| r.memory_high = Some(3 * GIB));
        assert_eq!(w.poll().expect("high lowered").new.memory_ceiling, 3 * GIB);
        source.update(|r| r.memory_high = None);
        assert_eq!(
            w.poll().expect("high removed").new.memory_ceiling,
            fraction_of(8 * GIB, 0.9)
        );
    }

    /// elastic_bounds. A machine that grows past `budget.elastic` is followed only as far as
    /// the bounds, with a note made once; a machine that shrinks is always followed.
    #[test]
    fn elastic_bounds() {
        let source = ManualLimitsSource::new(guest(8 * GIB, 4.0));
        let w = watch(&source, start(fraction(8 * GIB), 4.0)).with_bounds(ElasticBounds {
            memory_max_bytes: fraction(12 * GIB),
            cpu_max: 6.0,
        });
        assert!(w.bounds().is_some());
        source.set(guest(32 * GIB, 16.0));
        let change = w.poll().expect("grew up to the bounds");
        assert_eq!(change.new.memory_ceiling, fraction(12 * GIB));
        assert!((change.new.cpu_quota - 6.0).abs() < f64::EPSILON);
        assert_eq!(w.notes().len(), 1);
        assert!(w.notes()[0].contains("memory_max_bytes"));
        source.set(guest(64 * GIB, 32.0));
        assert!(w.poll().is_none(), "still at the bounds: no change");
        source.set(guest(2 * GIB, 1.0));
        let change = w.poll().expect("shrinking is always followed");
        assert_eq!(change.new.memory_ceiling, fraction(2 * GIB));
        assert_eq!(w.notes().len(), 1, "the note is made once");
    }

    /// The guest and cgroup sources: the host source reads `memory.max`, `memory.high` and
    /// `cpu.max` from the cgroup, `MemTotal` from `/proc/meminfo` and the online CPU list from
    /// `/sys`, and a rewrite of any of them is the next reading.
    #[test]
    fn host_source_reads_the_cgroup_and_the_guest_files() {
        let tmp = TempDir::new("watch-host");
        let root = cgroup_v2(tmp.path(), &(4 * GIB).to_string(), "max", "200000 100000");
        let proc_dir = proc_root(tmp.path(), "0::/\n");
        write(&proc_dir, "meminfo", "MemTotal: 16777216 kB\n");
        let sys = tmp.path().join("sys");
        write(&sys, "devices/system/cpu/online", "0-3\n");
        let source = HostLimitsSource::with_roots(&root, &proc_dir, &sys);
        let reading = source.read();
        assert_eq!(reading.memory_max, Some(4 * GIB));
        assert_eq!(reading.memory_high, None);
        assert_eq!(reading.cpu_max, Some(2.0));
        assert_eq!(reading.total_ram, Some(16 * GIB));
        assert!((reading.cpus_online - 4.0).abs() < f64::EPSILON);

        // The limits are rewritten mid-run: the next reading has them.
        write(&root, "memory.max", &(2 * GIB).to_string());
        write(&root, "cpu.max", "400000 100000");
        write(&sys, "devices/system/cpu/online", "0-7\n");
        let reading = source.read();
        assert_eq!(reading.memory_max, Some(2 * GIB));
        assert_eq!(reading.cpu_max, Some(4.0));
        assert!((reading.cpus_online - 8.0).abs() < f64::EPSILON);

        // An unreadable online list falls back to the operating system's count.
        let other = HostLimitsSource::with_roots(&root, &proc_dir, &tmp.path().join("absent"));
        assert!(other.read().cpus_online >= 1.0);
        // The real host answers on every supported target.
        assert!(HostLimitsSource::default().read().cpus_online >= 1.0);
    }

    /// The kernel's CPU list format (`cpu/online`).
    #[test]
    fn cpu_lists_parse() {
        assert_eq!(parse_cpu_list("0\n"), Some(1));
        assert_eq!(parse_cpu_list("0-3"), Some(4));
        assert_eq!(parse_cpu_list("0-3,6,8-9"), Some(7));
        assert_eq!(parse_cpu_list(" 0-1 , 4 "), Some(3));
        assert_eq!(parse_cpu_list(""), None);
        assert_eq!(parse_cpu_list("3-1"), None);
        assert_eq!(parse_cpu_list("a-b"), None);
        assert_eq!(parse_cpu_list("0,x"), None);
    }

    fn fraction(total: u64) -> u64 {
        fraction_of(total, 0.9)
    }

    fn fraction_of(total: u64, f: f64) -> u64 {
        (total as f64 * f) as u64
    }
}
