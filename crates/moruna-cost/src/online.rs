//! Learning during a run (design section 9.6; SDD 24 f.11, f.12).
//!
//! A run asks the models in use for every unit it decides on and tells them every unit it
//! finishes. [`Online`] is that loop, shared by the threads of a run:
//!
//! - **Online updates.** Each finished unit is one `observe` on the deciding model and on the
//!   one in shadow ([`InUse::settle`]): a constant-time update, never a refit. The shadow learns
//!   beside the deciding model and never steers.
//! - **The store, off the hot path.** The observation is kept in memory; [`Online::flush`]
//!   appends what is kept to the store on a thread that may do IO, and [`Online::save`] writes
//!   the fitted state. A unit's worker never waits on the store.
//! - **Drift.** Per key and target, a one-sided cumulative sum each way (CUSUM) of how far the
//!   outcome's logarithm lies from the prediction's median, in the prediction's own spread
//!   ([`Drift`]). Only the part beyond the 95th percentile accumulates, and one unit adds at most
//!   one percentile's worth, so one outlier never marks a key: three units in a row far outside
//!   the spread do, or more a little outside it. While a key drifts its estimates are widened
//!   ([`Estimate::widened`]) and marked, the deciding model is told so it forgets faster
//!   ([`crate::CostModel::set_drifting`]), and the run's report says when it began, for how
//!   many units, and whether it settled. It settles after [`SETTLE_UNITS`] units in a row
//!   inside the spread. Nothing about drift can stop a run: it changes estimates, not rules.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use crate::error::{CostError, Result};
use crate::estimate::{Estimate, Outcome, Target};
use crate::inputs::Inputs;
use crate::key::CostKey;
use crate::model::ModelId;
use crate::morsel::{Morsel, morsel};
use crate::registry::{InUse, Prediction};
use crate::sample::Z95;
use crate::store::{Observation, ObservationStore};

/// The part of a unit's standardised error beyond this accumulates (the 95th percentile's z):
/// an outcome inside the predicted spread adds nothing, and one inside it takes back.
pub const DRIFT_ALLOWANCE: f64 = Z95;
/// The most one unit adds to either sum: one 95th percentile's worth, so one outlier, however
/// far out, never marks a key by itself.
pub const DRIFT_STEP: f64 = Z95;
/// Either sum past this marks the key drifting: at least three units in a row at the most a
/// unit adds, more when they are only a little outside the spread.
pub const DRIFT_THRESHOLD: f64 = 2.5 * Z95;
/// Units in a row inside the spread after which a drifting key is settled again.
pub const SETTLE_UNITS: u32 = 8;
/// A drifting estimate's sigma is this many times the larger of its own and
/// [`DRIFT_SIGMA_FLOOR`].
pub const DRIFT_WIDEN: f64 = 2.0;
/// The least sigma a drifting estimate is widened from: a third of a decade at the 95th
/// percentile, so a key predicted with no spread gets one.
pub const DRIFT_SIGMA_FLOOR: f64 = 0.1;
/// An outcome is judged against at least this sigma, so a prediction with no spread (an exact
/// ratio) is not drifting for every unit that differs from it by a rounding.
pub const JUDGED_SIGMA_FLOOR: f64 = 0.05;
/// Observations kept in memory for the store before the next flush. A run whose drives do not
/// flush loses what passes it, and the report counts it as unwritten, with the reason.
pub const PENDING_LIMIT: usize = 65_536;

/// What one unit did to a key's drift.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// The key began to drift.
    Started,
    /// The key settled.
    Settled,
}

/// The drift statistic of one key and target: a CUSUM each way of the outcome's standardised
/// log-error beyond the spread (module docs).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Drift {
    up: f64,
    down: f64,
    calm: u32,
    drifting: bool,
}

impl Drift {
    /// A key with no evidence.
    pub fn new() -> Drift {
        Drift::default()
    }

    /// Whether the key drifts now.
    pub fn drifting(&self) -> bool {
        self.drifting
    }

    /// The larger of the two sums.
    pub fn statistic(&self) -> f64 {
        self.up.max(self.down)
    }

    /// The standardised log-error of `measured` against `estimate`: how many of the estimate's
    /// sigmas the outcome's logarithm lies above its median's (below, negative). A widened
    /// estimate is judged at the spread it was widened from, so a key settles when the model's
    /// own spread covers the outcomes again, not the learner's widening of it.
    pub fn error(estimate: &Estimate, measured: f64) -> f64 {
        let floor = estimate.unit.floor();
        let sigma = if estimate.drifting {
            estimate.sigma() / DRIFT_WIDEN
        } else {
            estimate.sigma()
        };
        let r = (measured.max(0.0) + floor).ln() - (estimate.p50.max(0.0) + floor).ln();
        r / sigma.max(JUDGED_SIGMA_FLOOR)
    }

    /// One unit's outcome: the sums move, and the change, if the key began to drift or settled.
    pub fn update(&mut self, estimate: &Estimate, measured: f64) -> Option<Change> {
        let z = Drift::error(estimate, measured);
        if !z.is_finite() {
            return None;
        }
        let excess = |e: f64| (e - DRIFT_ALLOWANCE).min(DRIFT_STEP);
        self.up = (self.up + excess(z)).max(0.0);
        self.down = (self.down + excess(-z)).max(0.0);
        if !self.drifting {
            if self.statistic() > DRIFT_THRESHOLD {
                self.drifting = true;
                self.calm = 0;
                return Some(Change::Started);
            }
            return None;
        }
        if z.abs() <= DRIFT_ALLOWANCE {
            self.calm += 1;
        } else {
            self.calm = 0;
        }
        if self.calm >= SETTLE_UNITS {
            *self = Drift::new();
            return Some(Change::Settled);
        }
        None
    }
}

/// One spell of drift of a key's target, for the run's report.
#[derive(Clone, Debug, PartialEq)]
pub struct DriftEvent {
    /// The key.
    pub key: CostKey,
    /// The target whose outcomes left the spread.
    pub target: Target,
    /// The key's units this run had observed when it began, counting the one that began it.
    pub at_unit: u64,
    /// Units observed while it drifted.
    pub units: u64,
    /// Whether it settled before the run ended.
    pub settled: bool,
}

/// What learning did over a run (design 9.7).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LearningReport {
    /// The deciding model.
    pub model: Option<ModelId>,
    /// The model in shadow.
    pub shadow: Option<ModelId>,
    /// Units observed.
    pub observed: u64,
    /// Observations written to the store.
    pub written: u64,
    /// Observations not written: the store refused them, or more waited than
    /// [`PENDING_LIMIT`].
    pub unwritten: u64,
    /// Why the first of them was not written.
    pub store_error: Option<String>,
    /// Outcomes the learner refused (not finite, negative), with the first reason.
    pub refused: u64,
    /// Why the first was refused.
    pub refusal: Option<String>,
    /// Every spell of drift, in the order they began.
    pub drifts: Vec<DriftEvent>,
    /// Nanoseconds spent appending to the store, on the thread that flushed.
    pub store_ns: u64,
    /// Nanoseconds spent writing the fitted state.
    pub save_ns: u64,
}

/// One key's place in the run: its units, its drift per target, and its open spells.
struct KeyState {
    key: CostKey,
    units: u64,
    drift: [Drift; Target::ALL.len()],
    /// Per target, its open spell's place in `report.drifts`.
    open: [Option<usize>; Target::ALL.len()],
    /// Targets drifting now, one bit per target in [`Target::ALL`]'s order.
    drifting: u16,
}

/// A settled unit kept for the store: interned, so keeping it allocates nothing, and made into
/// an [`Observation`] only when it is written.
struct Kept {
    key: usize,
    inputs: Inputs,
    outcome: Outcome,
    prediction: Option<Arc<Prediction>>,
    at_ms: u64,
    run: Option<Arc<str>>,
}

struct State {
    models: InUse,
    deciding: ModelId,
    keys: HashMap<CostKey, usize>,
    states: Vec<KeyState>,
    pending: Vec<Kept>,
    /// The buffer the next flush takes the kept units into, so flushing allocates nothing.
    spare: Vec<Kept>,
    /// What the flush under way has still to write.
    flushing: u64,
    limit: usize,
    report: LearningReport,
}

impl State {
    /// The place of `key`, made the first time it is seen.
    fn place(&mut self, key: &CostKey) -> usize {
        if let Some(&at) = self.keys.get(key) {
            return at;
        }
        let at = self.states.len();
        self.keys.insert(key.clone(), at);
        self.states.push(KeyState {
            key: key.clone(),
            units: 0,
            drift: [Drift::new(); Target::ALL.len()],
            open: [None; Target::ALL.len()],
            drifting: 0,
        });
        at
    }
}

/// Releases the flush reservation even if the store unwinds. This guards the IO
/// operation without holding the state lock while the store waits.
struct FlushGuard<'a>(&'a AtomicBool);

impl Drop for FlushGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// The models a run uses, learning online, with the store they are written to (module docs).
/// Shared by the run's threads: predictions take a read lock, a finished unit a write lock for
/// one constant-time update that allocates nothing once its key has been seen, and nothing
/// holds either across IO.
pub struct Online {
    state: RwLock<State>,
    store: Arc<dyn ObservationStore>,
    /// Only one caller may drain observations into the store at a time.
    flush_active: AtomicBool,
    /// Set when the run gives up waiting for the store: a flush under way stops at its next
    /// record, and what is left is counted unwritten.
    abandoned: AtomicBool,
}

impl std::fmt::Debug for Online {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let report = self.report();
        f.debug_struct("Online")
            .field("model", &report.model)
            .field("shadow", &report.shadow)
            .field("observed", &report.observed)
            .finish()
    }
}

impl Online {
    /// `models`, ready (opened by [`crate::CostModels::open`] or loaded by the caller), learning
    /// into `store`, keeping at most [`PENDING_LIMIT`] observations between flushes.
    pub fn new(models: InUse, store: Arc<dyn ObservationStore>) -> Online {
        let deciding = models.deciding().id();
        let report = LearningReport {
            model: Some(deciding.clone()),
            shadow: models.shadow().map(|m| m.id()),
            ..LearningReport::default()
        };
        Online {
            state: RwLock::new(State {
                models,
                deciding,
                keys: HashMap::new(),
                states: Vec::new(),
                pending: Vec::new(),
                spare: Vec::new(),
                flushing: 0,
                limit: PENDING_LIMIT,
                report,
            }),
            store,
            flush_active: AtomicBool::new(false),
            abandoned: AtomicBool::new(false),
        }
    }

    /// The same, keeping at most `limit` observations between flushes (an embedder short of
    /// memory, or a test of the limit).
    pub fn with_pending_limit(self, limit: usize) -> Online {
        self.write().limit = limit;
        self
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(|e| e.into_inner())
    }

    /// What the models in use predict for one unit, the deciding model's estimates of every
    /// target that drifts widened and marked; shared, so a caller can keep it and settle the
    /// unit against it without copying it.
    pub fn predict(&self, key: &CostKey, inputs: &Inputs) -> Arc<Prediction> {
        let state = self.read();
        let mut prediction = state.models.predict(key, inputs);
        let drifting = state
            .keys
            .get(key)
            .map_or(0, |&at| state.states[at].drifting);
        if drifting != 0 {
            for (bit, target) in Target::ALL.into_iter().enumerate() {
                if drifting & (1 << bit) != 0 {
                    let cost = &mut prediction.deciding.cost;
                    *cost.get_mut(target) =
                        cost.get(target).widened(DRIFT_WIDEN, DRIFT_SIGMA_FLOOR);
                }
            }
        }
        Arc::new(prediction)
    }

    /// Whether any target of `key` drifts now.
    pub fn is_drifting(&self, key: &CostKey) -> bool {
        let state = self.read();
        state
            .keys
            .get(key)
            .is_some_and(|&at| state.states[at].drifting != 0)
    }

    /// One unit finished: the models learn it, its drift is judged against `prediction` (what
    /// [`Online::predict`] said when the unit was chosen), and it is kept for the next
    /// [`Online::flush`], naming `run`, the run that measured it, when it has one. An outcome
    /// that is not a measurement (not finite, negative) is refused
    /// with the reason and counted, and nothing learns from it. Once the key has been seen this
    /// allocates nothing: the prediction is shared, and the record is made when it is written.
    pub fn settle(
        &self,
        key: &CostKey,
        inputs: &Inputs,
        prediction: Option<&Arc<Prediction>>,
        outcome: &Outcome,
        at_ms: u64,
        run: Option<&Arc<str>>,
    ) -> Result<()> {
        if let Err(why) = outcome.check() {
            let mut state = self.write();
            state.report.refused += 1;
            let refusal = format!("{key}: {why}");
            state.report.refusal.get_or_insert(refusal.clone());
            return Err(CostError::Invalid(refusal));
        }
        let mut guard = self.write();
        let state = &mut *guard;
        state.models.observe(key, inputs, outcome);
        state.report.observed += 1;
        let at = state.place(key);
        let entry = &mut state.states[at];
        entry.units += 1;
        if let Some(prediction) = prediction {
            for (bit, target) in Target::ALL.into_iter().enumerate() {
                let Some(measured) = outcome.get(target) else {
                    continue;
                };
                let change =
                    entry.drift[bit].update(prediction.deciding.cost.get(target), measured);
                if let Some(open) = entry.open[bit] {
                    state.report.drifts[open].units += 1;
                }
                match change {
                    Some(Change::Started) => {
                        if entry.drifting == 0 {
                            state.models.deciding_mut().set_drifting(key, true);
                        }
                        entry.drifting |= 1 << bit;
                        entry.open[bit] = Some(state.report.drifts.len());
                        state.report.drifts.push(DriftEvent {
                            key: key.clone(),
                            target,
                            at_unit: entry.units,
                            units: 1,
                            settled: false,
                        });
                    }
                    Some(Change::Settled) => {
                        if let Some(open) = entry.open[bit].take() {
                            state.report.drifts[open].settled = true;
                        }
                        entry.drifting &= !(1 << bit);
                        if entry.drifting == 0 {
                            state.models.deciding_mut().set_drifting(key, false);
                        }
                    }
                    None => {}
                }
            }
        }
        if state.pending.len() >= state.limit {
            state.report.unwritten += 1;
            let limit = state.limit;
            state.report.store_error.get_or_insert_with(|| {
                format!(
                    "more than {limit} observations waited to be written: the store did not keep \
                     up, and the rest were not kept"
                )
            });
        } else {
            state.pending.push(Kept {
                key: at,
                inputs: inputs.clone(),
                outcome: outcome.clone(),
                prediction: prediction.cloned(),
                at_ms,
                run: run.cloned(),
            });
        }
        Ok(())
    }

    /// Append every kept observation to the store, outside the lock, in the order the units
    /// finished, each made into its record here. Their count; the store's error when it refuses
    /// one, with every observation from that one on counted unwritten and the first error kept
    /// for the report. Every observation is counted once, written or unwritten, even when the
    /// run abandons the store while a flush waits on it ([`Online::abandon`]). A concurrent
    /// flush is refused immediately without changing pending observations or their counts.
    pub fn flush(&self) -> Result<u64> {
        self.flush_active
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .map_err(|_| {
                CostError::Invalid("an observation flush is already in progress".into())
            })?;
        let _flushing = FlushGuard(&self.flush_active);
        let began = Instant::now();
        let (mut batch, deciding, keys) = {
            let mut state = self.write();
            let mut batch = std::mem::take(&mut state.spare);
            std::mem::swap(&mut batch, &mut state.pending);
            state.flushing = batch.len() as u64;
            let keys: Vec<CostKey> = if batch.is_empty() {
                Vec::new()
            } else {
                state.states.iter().map(|k| k.key.clone()).collect()
            };
            (batch, state.deciding.clone(), keys)
        };
        let mut written = 0u64;
        let mut failure = None;
        for kept in batch.drain(..) {
            if failure.is_some() || self.abandoned.load(Ordering::SeqCst) {
                continue;
            }
            let observation = Observation::settled(
                keys[kept.key].clone(),
                (kept.inputs, kept.outcome),
                kept.at_ms,
                kept.run.as_deref().map(str::to_string),
                deciding.clone(),
                kept.prediction.as_deref(),
            );
            let appended = self.store.append(&observation);
            let mut state = self.write();
            if state.flushing == 0 {
                // Abandoned while this append waited: it was counted then.
                break;
            }
            match appended {
                Ok(()) => {
                    state.flushing -= 1;
                    state.report.written += 1;
                    written += 1;
                }
                Err(e) => {
                    let left = state.flushing;
                    state.flushing = 0;
                    state.report.unwritten += left;
                    state
                        .report
                        .store_error
                        .get_or_insert_with(|| e.to_string());
                    failure = Some(e);
                }
            }
        }
        let mut state = self.write();
        state.spare = batch;
        let left = std::mem::take(&mut state.flushing);
        state.report.unwritten += left;
        state.report.store_ns += began.elapsed().as_nanos() as u64;
        match failure {
            Some(e) => Err(e),
            None => Ok(written),
        }
    }

    /// The run stops waiting for the store (it did not finish in time): what is kept, and what a
    /// flush under way has not yet written, is counted unwritten with `why`; that flush stops at
    /// its next record, and later flushes and saves write nothing.
    pub fn abandon(&self, why: &str) {
        self.abandoned.store(true, Ordering::SeqCst);
        let mut state = self.write();
        let left = state.pending.len() as u64 + std::mem::take(&mut state.flushing);
        state.pending.clear();
        state.report.unwritten += left;
        state
            .report
            .store_error
            .get_or_insert_with(|| why.to_string());
    }

    /// Whether the run has abandoned the store.
    pub fn is_abandoned(&self) -> bool {
        self.abandoned.load(Ordering::SeqCst)
    }

    /// The store it writes to: what a reader of the run's history scans.
    pub fn store(&self) -> &Arc<dyn ObservationStore> {
        &self.store
    }

    /// Observations kept and not yet flushed.
    pub fn pending(&self) -> usize {
        self.read().pending.len()
    }

    /// Write each model's fitted state beside the records: made under the read lock, written
    /// outside it. A failure is returned and kept in the report's `store_error` when it is the
    /// first; the time it took is the report's `save_ns`. Once the store is abandoned, nothing.
    pub fn save(&self) -> Result<()> {
        let began = Instant::now();
        let saved = self.save_states();
        let mut state = self.write();
        state.report.save_ns += began.elapsed().as_nanos() as u64;
        if let Err(e) = &saved {
            state
                .report
                .store_error
                .get_or_insert_with(|| format!("the fitted state was not saved: {e}"));
        }
        saved
    }

    fn save_states(&self) -> Result<()> {
        if self.is_abandoned() {
            return Ok(());
        }
        let states = {
            let state = self.read();
            let mut states = vec![(
                state.models.deciding().id(),
                state.models.deciding().save()?,
            )];
            if let Some(shadow) = state.models.shadow() {
                states.push((shadow.id(), shadow.save()?));
            }
            states
        };
        for (id, bytes) in states {
            self.store.save_state(&id, &bytes)?;
        }
        Ok(())
    }

    /// The morsel of a task of `key` (SDD 24 f.10), from the deciding model.
    pub fn morsel(
        &self,
        key: &CostKey,
        inputs: &Inputs,
        total_rows: u64,
        memory_per_unit: f64,
        workers: u32,
    ) -> Morsel {
        let state = self.read();
        morsel(
            state.models.deciding(),
            key,
            inputs,
            total_rows,
            memory_per_unit,
            workers,
        )
    }

    /// The deciding model's id.
    pub fn model(&self) -> ModelId {
        self.read().deciding.clone()
    }

    /// What learning did so far.
    pub fn report(&self) -> LearningReport {
        self.read().report.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estimate::Unit;

    fn estimate(p50: f64, p95: f64) -> Estimate {
        Estimate {
            p50,
            p95,
            ..Estimate::exact(p50, Unit::Seconds)
        }
    }

    #[test]
    fn one_outlier_never_marks_a_key() {
        let e = estimate(1.0, 2.0);
        let mut d = Drift::new();
        assert_eq!(d.update(&e, 1e9), None, "one outlier, however far");
        assert!(!d.drifting());
        assert!(d.statistic() <= DRIFT_STEP + 1e-12);
        for _ in 0..50 {
            assert_eq!(d.update(&e, 1.0), None);
        }
        assert_eq!(d.statistic(), 0.0, "the median takes it back");
        assert_eq!(
            d.update(&e, f64::NAN),
            None,
            "a value that is not one moves nothing"
        );
    }

    #[test]
    fn three_far_units_mark_and_eight_calm_settle() {
        let e = estimate(1.0, 2.0);
        let mut d = Drift::new();
        assert_eq!(d.update(&e, 100.0), None);
        assert_eq!(d.update(&e, 100.0), None);
        assert_eq!(d.update(&e, 100.0), Some(Change::Started));
        assert!(d.drifting());
        for i in 0..SETTLE_UNITS - 1 {
            assert_eq!(d.update(&e, 1.1), None, "calm unit {i}");
        }
        assert_eq!(d.update(&e, 100.0), None, "a far unit restarts the count");
        for _ in 0..SETTLE_UNITS - 1 {
            assert_eq!(d.update(&e, 1.0), None);
        }
        assert_eq!(d.update(&e, 1.0), Some(Change::Settled));
        assert!(!d.drifting() && d.statistic() == 0.0);
        // Below the median drifts too.
        let mut low = Drift::new();
        let started = (0..10).find(|_| low.update(&e, 1e-6) == Some(Change::Started));
        assert_eq!(started, Some(2));
    }

    #[test]
    fn a_widened_estimate_is_judged_at_its_own_spread() {
        let e = estimate(1.0, 2.0);
        let wide = e.widened(DRIFT_WIDEN, DRIFT_SIGMA_FLOOR);
        assert!(wide.drifting && wide.p95 > e.p95 && wide.p50 == e.p50);
        let z = Drift::error(&e, 3.0);
        assert!((Drift::error(&wide, 3.0) - z).abs() < 1e-9);
        let exact = Estimate::exact(10.0, Unit::Bytes);
        let widened = exact.widened(DRIFT_WIDEN, DRIFT_SIGMA_FLOOR);
        assert!(widened.p95 > 10.0, "an exact estimate gets a spread");
        let huge = estimate(1.0, 1e300).widened(1e6, 0.0);
        assert!(huge.p95.is_finite());
        assert!(Drift::error(&exact, 10.0).abs() < 1e-12);
    }
}
