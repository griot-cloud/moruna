//! `FakeKnobs`, the `Knobs`, `StatsSource` and `Prober` fake of contracts d.15.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use amoru_kernel::{
    AmoruError, Knob, KnobSnapshot, Knobs, ProbeResult, Prober, Result, SchedulerStats, StageId,
    StatsSource, TierKind,
};

#[derive(Default)]
struct State {
    writes: Vec<Knob>,
    terminated: Option<AmoruError>,
}

struct Inner {
    state: Mutex<State>,
    stats: SchedulerStats,
    probe_results: BTreeMap<StageId, ProbeResult>,
}

/// The scheduler's side of the controller's interface as a test sees it: it records every knob
/// the controller writes and answers probes from a script.
///
/// Knobs: `stats(SchedulerStats)`, `probe_result(stage, ProbeResult)`.
/// Observables: `writes()`, `terminated()`, and `snapshot()` through the `Knobs` trait.
#[derive(Clone)]
pub struct FakeKnobs {
    inner: Arc<Inner>,
}

impl Default for FakeKnobs {
    fn default() -> Self {
        FakeKnobs::new()
    }
}

impl FakeKnobs {
    /// A scheduler that reports empty statistics and no probe results.
    pub fn new() -> FakeKnobs {
        FakeKnobs {
            inner: Arc::new(Inner {
                state: Mutex::new(State::default()),
                stats: SchedulerStats::default(),
                probe_results: BTreeMap::new(),
            }),
        }
    }

    /// Knob: what `scheduler_stats()` reports.
    pub fn stats(self, stats: SchedulerStats) -> FakeKnobs {
        self.rebuild(|b| b.stats = stats)
    }

    /// Knob: what `probe(stage, bytes)` returns for this stage.
    pub fn probe_result(self, stage: StageId, result: ProbeResult) -> FakeKnobs {
        self.rebuild(|b| {
            b.probe_results.insert(stage, result);
        })
    }

    /// Observable: every knob written, in call order.
    pub fn writes(&self) -> Vec<Knob> {
        self.lock().writes.clone()
    }

    /// Observable: the diagnostic the controller terminated the run with, if it did.
    pub fn terminated(&self) -> Option<String> {
        self.lock().terminated.as_ref().map(|e| e.to_string())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn rebuild(self, f: impl FnOnce(&mut Builder)) -> FakeKnobs {
        let mut builder = Builder {
            stats: self.inner.stats.clone(),
            probe_results: self.inner.probe_results.clone(),
        };
        f(&mut builder);
        let state = std::mem::take(&mut *self.lock());
        FakeKnobs {
            inner: Arc::new(Inner {
                state: Mutex::new(state),
                stats: builder.stats,
                probe_results: builder.probe_results,
            }),
        }
    }
}

struct Builder {
    stats: SchedulerStats,
    probe_results: BTreeMap<StageId, ProbeResult>,
}

impl Knobs for FakeKnobs {
    fn set(&self, knob: Knob) {
        self.lock().writes.push(knob);
    }

    fn snapshot(&self) -> KnobSnapshot {
        let mut snapshot = KnobSnapshot::default();
        for knob in &self.lock().writes {
            match knob {
                Knob::MorselTarget { stage, bytes } => {
                    upsert(&mut snapshot.morsel_target, *stage, *bytes)
                }
                Knob::ActiveWorkers(workers) => snapshot.active_workers = *workers,
                Knob::ReadAhead(splits) => snapshot.read_ahead = *splits,
                Knob::StagingTrigger { stage, on } => upsert(&mut snapshot.staging, *stage, *on),
                Knob::HighWater { stage, tier, bytes } => {
                    upsert_water(&mut snapshot.high_water, *stage, *tier, *bytes)
                }
                Knob::PromotionWindow { stage, morsels } => {
                    upsert(&mut snapshot.promotion_window, *stage, *morsels)
                }
            }
        }
        snapshot
    }

    fn terminate(&self, diagnostic: AmoruError) {
        let mut state = self.lock();
        if state.terminated.is_none() {
            state.terminated = Some(diagnostic);
        }
    }
}

fn upsert<V: Copy>(into: &mut Vec<(StageId, V)>, stage: StageId, value: V) {
    match into.iter_mut().find(|(at, _)| *at == stage) {
        Some(slot) => slot.1 = value,
        None => into.push((stage, value)),
    }
}

fn upsert_water(
    into: &mut Vec<(StageId, TierKind, u64)>,
    stage: StageId,
    tier: TierKind,
    bytes: u64,
) {
    match into
        .iter_mut()
        .find(|(at, kind, _)| *at == stage && *kind == tier)
    {
        Some(slot) => slot.2 = bytes,
        None => into.push((stage, tier, bytes)),
    }
}

impl StatsSource for FakeKnobs {
    fn scheduler_stats(&self) -> SchedulerStats {
        self.inner.stats.clone()
    }
}

impl Prober for FakeKnobs {
    fn probe(&self, stage: StageId, bytes: u64) -> Result<ProbeResult> {
        match self.inner.probe_results.get(&stage) {
            Some(result) => Ok(result.clone()),
            None => Ok(ProbeResult {
                bytes_in: bytes,
                rows_in: 0,
                peak_delta: 0,
                dev_peak_delta: 0,
                wall_ns: 0,
                cpu_ns: 0,
            }),
        }
    }
}
