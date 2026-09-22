//! Knob handling: clamping, storage and forwarding (f.15, SC-I5, SC-I10).
//!
//! The controller is the only writer (G-I5). Every value is clamped to the preamble's range
//! here, counted in `SchedulerStats::knob_clamps`, and either stored atomically (so the next
//! worker pick and the next drive iteration see it) or forwarded once to the placement engine,
//! which no other component does.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};

use amoru_kernel::{Knob, KnobSnapshot, StageId, TierKind};

use crate::pipeline::SchedulerConfig;
use crate::shared::{RunState, Shared};

/// The preamble's range for `readahead.splits`.
const READ_AHEAD_MAX: u16 = 64;
/// The preamble's range for `queue.promotion_window`.
const PROMOTION_MIN: u16 = 1;
const PROMOTION_MAX: u16 = 32;

/// Every knob's current value, in the form the reader wants it (f.15).
pub(crate) struct KnobState {
    morsel_target: Vec<AtomicU64>,
    active_workers: AtomicU16,
    read_ahead: AtomicU16,
    workers_max: u16,
    morsel_min: u64,
    morsel_max: u64,
    staging: Mutex<Vec<(StageId, bool)>>,
    high_water: Mutex<Vec<(StageId, TierKind, u64)>>,
    promotion_window: Mutex<Vec<(StageId, u16)>>,
    clamps: AtomicU64,
    /// One `sched.knob_clamp` warning per knob kind (f.15).
    logged: [AtomicBool; 6],
}

impl KnobState {
    pub(crate) fn new(cfg: &SchedulerConfig, queues: usize) -> KnobState {
        let initial = cfg
            .initial_morsel_target
            .clamp(cfg.morsel_min, cfg.morsel_max);
        KnobState {
            morsel_target: (0..queues).map(|_| AtomicU64::new(initial)).collect(),
            active_workers: AtomicU16::new(cfg.workers_active.clamp(1, cfg.workers_max)),
            read_ahead: AtomicU16::new(cfg.read_ahead.min(READ_AHEAD_MAX)),
            workers_max: cfg.workers_max,
            morsel_min: cfg.morsel_min,
            morsel_max: cfg.morsel_max,
            staging: Mutex::new(Vec::new()),
            high_water: Mutex::new(Vec::new()),
            promotion_window: Mutex::new(Vec::new()),
            clamps: AtomicU64::new(0),
            logged: [
                AtomicBool::new(false),
                AtomicBool::new(false),
                AtomicBool::new(false),
                AtomicBool::new(false),
                AtomicBool::new(false),
                AtomicBool::new(false),
            ],
        }
    }

    pub(crate) fn morsel_target(&self, stage: StageId) -> u64 {
        self.morsel_target
            .get(stage as usize)
            .map_or(self.morsel_min, |cell| cell.load(Ordering::SeqCst))
    }

    pub(crate) fn active_workers(&self) -> u16 {
        self.active_workers.load(Ordering::SeqCst)
    }

    pub(crate) fn read_ahead(&self) -> u16 {
        self.read_ahead.load(Ordering::SeqCst)
    }

    pub(crate) fn clamps(&self) -> u64 {
        self.clamps.load(Ordering::SeqCst)
    }

    /// Count a clamp and warn once for this knob kind (f.15).
    fn clamped(&self, kind: usize, knob: &'static str, value: u64, bound: u64) {
        self.clamps.fetch_add(1, Ordering::SeqCst);
        if let Some(flag) = self.logged.get(kind)
            && !flag.swap(true, Ordering::SeqCst)
        {
            tracing::warn!(target: "sched.knob_clamp", knob, value, bound, "knob clamped");
        }
    }

    pub(crate) fn snapshot(&self) -> KnobSnapshot {
        KnobSnapshot {
            morsel_target: self
                .morsel_target
                .iter()
                .enumerate()
                .map(|(stage, cell)| (stage as StageId, cell.load(Ordering::SeqCst)))
                .collect(),
            active_workers: self.active_workers(),
            read_ahead: self.read_ahead(),
            staging: self
                .staging
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            high_water: self
                .high_water
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            promotion_window: self
                .promotion_window
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
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

/// `Knobs::set` (f.15). Clamps, counts, stores or forwards. A set after the run has exited is a
/// no-op (RC h).
pub(crate) fn set(shared: &Shared, knob: Knob) {
    let state = &shared.knobs;
    if matches!(
        shared.run_state(),
        RunState::Completed | RunState::Terminated | RunState::Cancelled
    ) {
        return;
    }
    match knob {
        Knob::MorselTarget { stage, bytes } => {
            let clamped = bytes.clamp(state.morsel_min, state.morsel_max);
            if clamped != bytes {
                state.clamped(0, "morsel_target", bytes, clamped);
            }
            if let Some(cell) = state.morsel_target.get(stage as usize) {
                cell.store(clamped, Ordering::SeqCst);
            }
        }
        Knob::ActiveWorkers(workers) => {
            let clamped = workers.clamp(1, state.workers_max);
            if clamped != workers {
                state.clamped(1, "active_workers", workers as u64, clamped as u64);
            }
            state.active_workers.store(clamped, Ordering::SeqCst);
            // SC-I5: the next pick sees it, so a raise must wake the parked workers now.
            shared.unpark_all();
        }
        Knob::ReadAhead(splits) => {
            let clamped = splits.min(READ_AHEAD_MAX);
            if clamped != splits {
                state.clamped(2, "read_ahead", splits as u64, clamped as u64);
            }
            state.read_ahead.store(clamped, Ordering::SeqCst);
        }
        Knob::StagingTrigger { stage, on } => {
            // SC-I10: forwarded by the scheduler and by nobody else.
            shared.placement.set_staging(stage, on);
            upsert(
                &mut state.staging.lock().unwrap_or_else(|e| e.into_inner()),
                stage,
                on,
            );
        }
        Knob::HighWater { stage, tier, bytes } => {
            // f.15: passed as given; low is half of high (contracts d.11).
            shared.placement.set_water(stage, tier, bytes / 2, bytes);
            upsert_water(
                &mut state.high_water.lock().unwrap_or_else(|e| e.into_inner()),
                stage,
                tier,
                bytes,
            );
        }
        Knob::PromotionWindow { stage, morsels } => {
            let clamped = morsels.clamp(PROMOTION_MIN, PROMOTION_MAX);
            if clamped != morsels {
                state.clamped(3, "promotion_window", morsels as u64, clamped as u64);
            }
            shared.placement.set_promotion_window(stage, clamped);
            upsert(
                &mut state
                    .promotion_window
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()),
                stage,
                clamped,
            );
        }
    }
}
