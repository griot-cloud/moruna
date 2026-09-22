//! `FakePlacement`, the `Placement` fake of contracts d.15.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use amoru_kernel::{
    AmoruError, CheckpointExtras, Fingerprint, Locality, Morsel, Origin, PayloadSpec, Placement,
    PlacementStats, QueueStats, Result, ResumePoint, Seq, Split, StageId, TierBudgets, TierKind,
};

/// A manifest as this fake keeps it: what `checkpoint` was given, and what was in the queues.
#[derive(Clone, Debug, Default)]
struct Manifest {
    extras: CheckpointExtras,
    /// Every morsel the engine held at the checkpoint, and whether it had a disk copy.
    lineage: Vec<(Seq, Origin, StageId, bool)>,
    plan: Vec<(u32, u64)>,
    fingerprints: Vec<Fingerprint>,
}

/// The manifest store `with_manifest_store` turns on: in memory, keyed by path, shared by every
/// engine in the process so `checkpoint` and `restore` round-trip across engine instances.
fn manifest_store() -> &'static Mutex<HashMap<PathBuf, Manifest>> {
    static STORE: OnceLock<Mutex<HashMap<PathBuf, Manifest>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// One queue entry.
struct Entry {
    morsel: Option<Morsel>,
    seq: Seq,
    origin: Origin,
    stage: StageId,
    /// When the entry was pushed, for `with_delay`.
    pushed: Instant,
    /// Set when `with_pressure` evicted the entry: its bytes are gone, its origin remains.
    evicted: bool,
    on_disk: bool,
}

#[derive(Default)]
struct Queue {
    entries: VecDeque<Entry>,
    bytes_by_tier: [u64; amoru_kernel::TIER_COUNT],
    closed: bool,
    misses: u64,
    miss_wait_us: u64,
    consumer: Option<PayloadSpec>,
    water: BTreeMap<usize, (u64, u64)>,
    staging: bool,
    promotion_window: u16,
    resident_bytes: u64,
}

#[derive(Default)]
struct State {
    queues: BTreeMap<StageId, Queue>,
    pushed: BTreeMap<StageId, Vec<Seq>>,
    popped: BTreeMap<StageId, Vec<Seq>>,
    committed: Option<Seq>,
    budgets_set: Vec<TierBudgets>,
    manifests_written: Vec<PathBuf>,
    shutting_down: bool,
}

struct Inner {
    state: Mutex<State>,
    waiters: Condvar,
    pressure: BTreeMap<StageId, u64>,
    delay: Duration,
    manifest_store: bool,
    staging_dir: Option<PathBuf>,
    shutdown_calls: AtomicU64,
}

/// The placement engine as a test sees it: FIFO queues per stage, with knobs for the two
/// behaviours a consumer has to handle, eviction under pressure and a pop that waits.
///
/// Knobs: `with_pressure(stage, evict_after_bytes)`, `with_delay(Duration)`,
/// `with_manifest_store()`. Observables: `pushed(stage)`, `popped(stage)`, `committed()`,
/// `manifests_written()`, `budgets_set()`, `shutdown_calls()`.
#[derive(Clone)]
pub struct FakePlacement {
    inner: Arc<Inner>,
}

impl Default for FakePlacement {
    fn default() -> Self {
        FakePlacement::new()
    }
}

impl FakePlacement {
    /// An engine with no pressure, no delay and no manifest store.
    pub fn new() -> FakePlacement {
        FakePlacement {
            inner: Arc::new(Inner {
                state: Mutex::new(State::default()),
                waiters: Condvar::new(),
                pressure: BTreeMap::new(),
                delay: Duration::ZERO,
                manifest_store: false,
                staging_dir: None,
                shutdown_calls: AtomicU64::new(0),
            }),
        }
    }

    /// Knob: entries beyond `evict_after_bytes` on this queue become `Evicted`, which is what a
    /// Q0 eviction looks like to the scheduler (D5).
    pub fn with_pressure(self, stage: StageId, evict_after_bytes: u64) -> FakePlacement {
        self.rebuild(|b| {
            b.pressure.insert(stage, evict_after_bytes);
        })
    }

    /// Knob: a pop of an entry younger than this waits for it, and the wait is counted as a
    /// miss (PL-I9).
    pub fn with_delay(self, delay: Duration) -> FakePlacement {
        self.rebuild(|b| b.delay = delay)
    }

    /// Knob: keep manifests in memory, keyed by path, so `checkpoint` and `restore` round-trip
    /// across engine instances.
    pub fn with_manifest_store(self) -> FakePlacement {
        self.rebuild(|b| {
            b.manifest_store = true;
            b.staging_dir = Some(PathBuf::from("/amoru-testkit/staging"));
        })
    }

    /// Observable: the sequence numbers pushed to a stage, in call order.
    pub fn pushed(&self, stage: StageId) -> Vec<Seq> {
        self.lock().pushed.get(&stage).cloned().unwrap_or_default()
    }

    /// Observable: the sequence numbers popped from a stage, in call order.
    pub fn popped(&self, stage: StageId) -> Vec<Seq> {
        self.lock().popped.get(&stage).cloned().unwrap_or_default()
    }

    /// Observable: the commit watermark `set_committed` last recorded.
    pub fn committed(&self) -> Option<Seq> {
        self.lock().committed
    }

    /// Observable: every manifest path written, in call order.
    pub fn manifests_written(&self) -> Vec<PathBuf> {
        self.lock().manifests_written.clone()
    }

    /// Observable: every `set_budgets` argument, in call order.
    pub fn budgets_set(&self) -> Vec<TierBudgets> {
        self.lock().budgets_set.clone()
    }

    /// Observable: how many times `shutdown` was called.
    pub fn shutdown_calls(&self) -> u64 {
        self.inner.shutdown_calls.load(Ordering::SeqCst)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn rebuild(self, f: impl FnOnce(&mut Builder)) -> FakePlacement {
        let mut builder = Builder {
            pressure: self.inner.pressure.clone(),
            delay: self.inner.delay,
            manifest_store: self.inner.manifest_store,
            staging_dir: self.inner.staging_dir.clone(),
        };
        f(&mut builder);
        let state = std::mem::take(&mut *self.lock());
        FakePlacement {
            inner: Arc::new(Inner {
                state: Mutex::new(state),
                waiters: Condvar::new(),
                pressure: builder.pressure,
                delay: builder.delay,
                manifest_store: builder.manifest_store,
                staging_dir: builder.staging_dir,
                shutdown_calls: AtomicU64::new(self.shutdown_calls()),
            }),
        }
    }

    /// Whether the head of `stage` can be handed to a consumer that wants `want`.
    fn head_ready(&self, queue: &Queue, want: PayloadSpec) -> bool {
        let Some(entry) = queue.entries.front() else {
            return false;
        };
        if entry.evicted || entry.morsel.is_none() {
            return false;
        }
        if entry.pushed.elapsed() < self.inner.delay {
            return false;
        }
        let Some(morsel) = entry.morsel.as_ref() else {
            return false;
        };
        satisfies(morsel.payload.tier(), want)
    }

    fn take_head(&self, state: &mut State, stage: StageId) -> Option<Morsel> {
        let queue = state.queues.get_mut(&stage)?;
        let entry = queue.entries.pop_front()?;
        let morsel = entry.morsel?;
        let index = morsel.payload.tier().index();
        queue.bytes_by_tier[index] = queue.bytes_by_tier[index].saturating_sub(morsel.bytes);
        queue.resident_bytes = queue.resident_bytes.saturating_sub(morsel.bytes);
        state.popped.entry(stage).or_default().push(morsel.seq);
        Some(morsel)
    }
}

struct Builder {
    pressure: BTreeMap<StageId, u64>,
    delay: Duration,
    manifest_store: bool,
    staging_dir: Option<PathBuf>,
}

/// `TierPref::Any` accepts any resident tier, `Host` accepts either host tier, `Device` accepts
/// a device (placement f.3).
fn satisfies(tier: amoru_kernel::Tier, want: PayloadSpec) -> bool {
    match want.tier {
        amoru_kernel::TierPref::Any => tier.is_resident(),
        amoru_kernel::TierPref::Host => tier.is_host(),
        amoru_kernel::TierPref::Device => matches!(tier, amoru_kernel::Tier::Device(_)),
    }
}

impl Placement for FakePlacement {
    fn push(&self, stage: StageId, morsel: Morsel) -> Result<()> {
        let mut state = self.lock();
        if state.shutting_down {
            return Err(AmoruError::Cancelled);
        }
        let seq = morsel.seq;
        let bytes = morsel.bytes;
        let origin = morsel.origin.clone();
        let queue = state.queues.entry(stage).or_default();
        let evict_after = self.inner.pressure.get(&stage).copied();
        let evicted = evict_after.is_some_and(|limit| queue.resident_bytes + bytes > limit);
        let index = morsel.payload.tier().index();
        if evicted {
            queue.entries.push_back(Entry {
                morsel: None,
                seq,
                origin,
                stage,
                pushed: Instant::now(),
                evicted: true,
                on_disk: false,
            });
        } else {
            queue.bytes_by_tier[index] += bytes;
            queue.resident_bytes += bytes;
            queue.entries.push_back(Entry {
                morsel: Some(morsel),
                seq,
                origin,
                stage,
                pushed: Instant::now(),
                evicted: false,
                on_disk: false,
            });
        }
        state.pushed.entry(stage).or_default().push(seq);
        drop(state);
        self.inner.waiters.notify_all();
        Ok(())
    }

    fn pop(
        &self,
        stage: StageId,
        want: PayloadSpec,
        _locality: Locality,
    ) -> Result<Option<Morsel>> {
        let mut state = self.lock();
        if state.shutting_down {
            return Err(AmoruError::Cancelled);
        }
        let Some(queue) = state.queues.get(&stage) else {
            return Ok(None);
        };
        if !self.head_ready(queue, want) {
            return Ok(None);
        }
        Ok(self.take_head(&mut state, stage))
    }

    fn pop_blocking(
        &self,
        stage: StageId,
        want: PayloadSpec,
        _locality: Locality,
    ) -> Result<Option<(Morsel, u64)>> {
        let mut state = self.lock();
        let started = Instant::now();
        let mut waited = false;
        loop {
            if state.shutting_down {
                return Err(AmoruError::Cancelled);
            }
            let queue = state.queues.entry(stage).or_default();
            if self.head_ready(queue, want) {
                let elapsed = if waited {
                    started.elapsed().as_micros() as u64
                } else {
                    0
                };
                if waited {
                    let queue = state.queues.entry(stage).or_default();
                    queue.misses += 1;
                    queue.miss_wait_us += elapsed;
                }
                return Ok(self
                    .take_head(&mut state, stage)
                    .map(|morsel| (morsel, elapsed)));
            }
            let empty = queue.entries.is_empty();
            if queue.closed && empty {
                return Ok(None);
            }
            waited = true;
            let (guard, _) = self
                .inner
                .waiters
                .wait_timeout(state, Duration::from_millis(10))
                .unwrap_or_else(|e| e.into_inner());
            state = guard;
        }
    }

    fn peek_resident(&self, stage: StageId, want: PayloadSpec, _locality: Locality) -> bool {
        let state = self.lock();
        state
            .queues
            .get(&stage)
            .is_some_and(|queue| self.head_ready(queue, want))
    }

    fn evicted(&self, stage: StageId) -> Vec<(Seq, Origin)> {
        let state = self.lock();
        state.queues.get(&stage).map_or_else(Vec::new, |queue| {
            queue
                .entries
                .iter()
                .filter(|entry| entry.evicted)
                .map(|entry| (entry.seq, entry.origin.clone()))
                .collect()
        })
    }

    fn replace(&self, stage: StageId, morsel: Morsel) -> Result<()> {
        let mut state = self.lock();
        let Some(queue) = state.queues.get_mut(&stage) else {
            return Err(AmoruError::Staging("replace: no such queue".into()));
        };
        let Some(entry) = queue
            .entries
            .iter_mut()
            .find(|entry| entry.seq == morsel.seq && entry.evicted)
        else {
            return Err(AmoruError::Staging("replace: no evicted entry".into()));
        };
        let index = morsel.payload.tier().index();
        let bytes = morsel.bytes;
        entry.evicted = false;
        entry.morsel = Some(morsel);
        queue.bytes_by_tier[index] += bytes;
        queue.resident_bytes += bytes;
        drop(state);
        self.inner.waiters.notify_all();
        Ok(())
    }

    fn shutdown(&self) {
        self.inner.shutdown_calls.fetch_add(1, Ordering::SeqCst);
        {
            let mut state = self.lock();
            state.shutting_down = true;
        }
        self.inner.waiters.notify_all();
    }

    fn set_committed(&self, seq: Seq) {
        let mut state = self.lock();
        if state.committed.is_none_or(|watermark| seq >= watermark) {
            state.committed = Some(seq);
        }
    }

    fn checkpoint(&self, extras: &CheckpointExtras) -> Result<PathBuf> {
        if !self.inner.manifest_store {
            return Err(AmoruError::Resume("no staging directory".into()));
        }
        let Some(dir) = self.inner.staging_dir.as_ref() else {
            return Err(AmoruError::Resume("no staging directory".into()));
        };
        let path = dir.join("manifest.json");
        let mut state = self.lock();
        let lineage: Vec<(Seq, Origin, StageId, bool)> = state
            .queues
            .values()
            .flat_map(|queue| queue.entries.iter())
            .map(|entry| (entry.seq, entry.origin.clone(), entry.stage, entry.on_disk))
            .collect();
        let manifest = Manifest {
            extras: extras.clone(),
            lineage,
            plan: Vec::new(),
            fingerprints: Vec::new(),
        };
        state.manifests_written.push(path.clone());
        drop(state);
        let mut store = manifest_store().lock().unwrap_or_else(|e| e.into_inner());
        store.insert(path.clone(), manifest);
        Ok(path)
    }

    fn restore(
        &self,
        manifest: &Path,
        plan: &[Split],
        fingerprints: &[Fingerprint],
    ) -> Result<ResumePoint> {
        let store = manifest_store().lock().unwrap_or_else(|e| e.into_inner());
        let Some(found) = store.get(manifest).cloned() else {
            return Err(AmoruError::Resume(format!(
                "{}: no manifest at this path",
                manifest.display()
            )));
        };
        drop(store);
        if !found.plan.is_empty() {
            let current: Vec<(u32, u64)> =
                plan.iter().map(|split| (split.id, split.rows)).collect();
            if current != found.plan {
                return Err(AmoruError::Resume(format!(
                    "{}: the plan differs from the one the manifest recorded",
                    manifest.display()
                )));
            }
        }
        if !found.fingerprints.is_empty() && found.fingerprints != fingerprints {
            return Err(AmoruError::Resume(format!(
                "{}: the kernel fingerprints differ from the manifest's",
                manifest.display()
            )));
        }
        let to_recompute: Vec<(Seq, Origin)> = found
            .lineage
            .iter()
            .filter(|(_, _, _, on_disk)| !on_disk)
            .map(|(seq, origin, _, _)| (*seq, origin.clone()))
            .collect();
        Ok(ResumePoint {
            extras: found.extras,
            to_recompute,
        })
    }

    fn is_full(&self, stage: StageId) -> bool {
        let state = self.lock();
        state.queues.get(&stage).is_some_and(|queue| {
            if queue.closed {
                return true;
            }
            queue
                .water
                .get(&TierKind::Host.index())
                .is_some_and(|(_, high)| *high > 0 && queue.resident_bytes > *high)
        })
    }

    fn set_consumer(&self, stage: StageId, want: PayloadSpec) {
        let mut state = self.lock();
        state.queues.entry(stage).or_default().consumer = Some(want);
    }

    fn set_budgets(&self, budgets: TierBudgets) {
        self.lock().budgets_set.push(budgets);
    }

    fn set_water(&self, stage: StageId, tier: TierKind, low: u64, high: u64) {
        let mut state = self.lock();
        state
            .queues
            .entry(stage)
            .or_default()
            .water
            .insert(tier.index(), (low, high));
    }

    fn set_staging(&self, stage: StageId, enabled: bool) {
        let mut state = self.lock();
        state.queues.entry(stage).or_default().staging = enabled;
    }

    fn set_promotion_window(&self, stage: StageId, morsels: u16) {
        let mut state = self.lock();
        state.queues.entry(stage).or_default().promotion_window = morsels;
    }

    fn close(&self, stage: StageId) {
        {
            let mut state = self.lock();
            state.queues.entry(stage).or_default().closed = true;
        }
        self.inner.waiters.notify_all();
    }

    fn stats(&self) -> PlacementStats {
        let state = self.lock();
        let queues = state
            .queues
            .iter()
            .map(|(stage, queue)| QueueStats {
                stage: *stage,
                bytes_by_tier: queue.bytes_by_tier,
                count: queue.entries.len() as u64,
                misses: queue.misses,
                miss_wait_us: queue.miss_wait_us,
                demotions: 0,
                promotions: 0,
            })
            .collect();
        PlacementStats {
            queues,
            in_flight_bytes: 0,
        }
    }
}
