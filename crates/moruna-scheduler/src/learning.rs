//! The run's cost model, as the scheduler uses it (f.22; adaptive-runtime 9.4, 9.6).
//!
//! The embedder opens the models and the store and hands them over as one [`Online`] with each
//! kernel stage's [`CostKey`] (`Scheduler::set_cost`). From then on every unit a policy weighs
//! is predicted by the deciding model through its port, with a spread, and every unit a worker
//! finishes is settled into it: an online update, the drift judged, the unit kept in memory.
//!
//! **What it costs.** A stage's prediction is cached per input size (to within 1.1%) and kept
//! while the model and all its inputs are unchanged. A settled unit changes its key's fit or
//! drift; active workers and source statistics may change before any unit settles. Each must
//! be visible to the next prediction. A hit allocates nothing; a settle
//! allocates nothing once its key has been seen.
//!
//! **The store.** A thread of its own, the learning thread, writes what was kept every
//! [`FLUSH_EVERY`], and at the run's end what is left and the fitted state: no worker and no
//! drive waits on the store (SC-I1). The run's end waits for it at most [`DRAIN`] by the run's
//! clock, after the run's output is complete; a store slower than that is abandoned, what it did
//! not write is counted unwritten with the reason, and the run ends as it would have.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use moruna_cost::{CostKey, Inputs, Online, Origin, Outcome, Target};
use moruna_kernel::{
    DriftStat, LearningStats, MorunaError, PredictedBy, Prediction, Result, Spread, StageId,
};

use crate::shared::Shared;

/// How often the learning thread writes what was kept.
pub(crate) const FLUSH_EVERY: Duration = Duration::from_millis(10);
/// How long the run's end waits for the learning thread to write what is left, after the run's
/// output is complete.
pub const DRAIN: Duration = Duration::from_secs(5);
/// Input sizes a stage's cache keeps a prediction for at once.
const CACHED_SIZES: usize = 4;
/// Sizes are bucketed to 1/64 of a doubling (about 1.1%): a prediction is made at its bucket's
/// size, so two batches within the bucket share it.
const BUCKETS_PER_DOUBLING: f64 = 64.0;

/// One cached prediction.
#[derive(Clone)]
struct Cached {
    version: u64,
    bucket: i32,
    inputs: Inputs,
    prediction: Prediction,
    model: Arc<moruna_cost::Prediction>,
}

/// A stage's cached predictions.
#[derive(Default)]
struct StageCache {
    entries: [Option<Cached>; CACHED_SIZES],
    next: usize,
}

/// The learning thread's handle and its flags.
struct Writer {
    handle: Mutex<Option<JoinHandle<()>>>,
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
}

/// The models, each stage's key, the stages' caches and the learning thread.
pub(crate) struct Learning {
    online: Arc<Online>,
    keys: Vec<CostKey>,
    /// By stage, the place of its key among the distinct keys.
    key_of: Vec<usize>,
    /// By distinct key, settles so far: what a cached prediction was made at.
    versions: Vec<AtomicU64>,
    caches: Vec<Mutex<StageCache>>,
    writer: Writer,
    learn_ns: AtomicU64,
    learned: AtomicU64,
    drain_ns: AtomicU64,
}

/// A prediction for a unit, and what asking the model for it cost.
pub(crate) struct Asked {
    pub(crate) prediction: Prediction,
    pub(crate) model: Arc<moruna_cost::Prediction>,
    /// Nanoseconds spent asking the model; zero on a cache hit.
    pub(crate) ns: u64,
    pub(crate) cached: bool,
}

fn bucket(bytes: u64) -> i32 {
    if bytes == 0 {
        return i32::MIN;
    }
    ((bytes as f64).log2() * BUCKETS_PER_DOUBLING).round() as i32
}

fn bucket_bytes(bucket: i32) -> u64 {
    if bucket == i32::MIN {
        return 0;
    }
    (f64::from(bucket) / BUCKETS_PER_DOUBLING).exp2().round() as u64
}

impl Learning {
    /// `online` for the run's `stages` kernel stages, keyed by `keys`, stage 1 first; refused
    /// when the counts differ.
    pub(crate) fn new(online: Arc<Online>, keys: Vec<CostKey>, stages: usize) -> Result<Learning> {
        if keys.len() != stages {
            return Err(MorunaError::Config {
                name: "cost.model",
                msg: format!(
                    "the run has {stages} kernel stages and {} cost keys were given",
                    keys.len()
                ),
            });
        }
        let mut distinct: Vec<&CostKey> = Vec::new();
        let key_of = keys
            .iter()
            .map(|k| match distinct.iter().position(|d| *d == k) {
                Some(at) => at,
                None => {
                    distinct.push(k);
                    distinct.len() - 1
                }
            })
            .collect();
        let versions = (0..distinct.len()).map(|_| AtomicU64::new(0)).collect();
        Ok(Learning {
            online,
            caches: (0..keys.len())
                .map(|_| Mutex::new(StageCache::default()))
                .collect(),
            keys,
            key_of,
            versions,
            writer: Writer {
                handle: Mutex::new(None),
                stop: Arc::new(AtomicBool::new(false)),
                done: Arc::new(AtomicBool::new(false)),
            },
            learn_ns: AtomicU64::new(0),
            learned: AtomicU64::new(0),
            drain_ns: AtomicU64::new(0),
        })
    }

    /// The deciding model's prediction for a unit of stage `stage_ix` (0 for stage 1) on a batch
    /// of `bytes`, and the model's own to settle it against: from the stage's cache when the
    /// stage's model is unchanged since it was made at this size, else asked of the model at the
    /// size's bucket and kept.
    pub(crate) fn predict(&self, shared: &Shared, stage_ix: usize, bytes: u64) -> Asked {
        let version = self.versions[self.key_of[stage_ix]].load(Ordering::SeqCst);
        let at = bucket(bytes);
        let inputs = inputs_of(shared, stage_ix, bucket_bytes(at));
        let mut cache = self.caches[stage_ix]
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(hit) = cache
            .entries
            .iter()
            .flatten()
            .find(|c| c.version == version && c.bucket == at && c.inputs == inputs)
        {
            return Asked {
                prediction: hit.prediction,
                model: Arc::clone(&hit.model),
                ns: 0,
                cached: true,
            };
        }
        let began = Instant::now();
        let model = self.online.predict(&self.keys[stage_ix], &inputs);
        let prediction = contract(&model);
        let ns = began.elapsed().as_nanos() as u64;
        let slot = cache.next;
        cache.next = (slot + 1) % CACHED_SIZES;
        cache.entries[slot] = Some(Cached {
            version,
            bucket: at,
            inputs,
            prediction,
            model: Arc::clone(&model),
        });
        Asked {
            prediction,
            model,
            ns,
            cached: false,
        }
    }

    /// A unit of stage `stage_ix` finished: settled into the models, and every stage of its key
    /// predicted afresh from now on. A refused outcome is counted, with its reason, in the
    /// report the run gives (`stats`), and changes nothing else.
    pub(crate) fn settle(
        &self,
        stage_ix: usize,
        inputs: &Inputs,
        prediction: &Arc<moruna_cost::Prediction>,
        outcome: &Outcome,
    ) {
        let began = Instant::now();
        let at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let _counted = self.online.settle(
            &self.keys[stage_ix],
            inputs,
            Some(prediction),
            outcome,
            at_ms,
        );
        self.versions[self.key_of[stage_ix]].fetch_add(1, Ordering::SeqCst);
        self.learn_ns
            .fetch_add(began.elapsed().as_nanos() as u64, Ordering::Relaxed);
        self.learned.fetch_add(1, Ordering::Relaxed);
    }

    /// Start the learning thread: it writes what was kept every [`FLUSH_EVERY`] until it is
    /// told to stop, then what is left and the fitted state.
    pub(crate) fn start(&self) -> Result<()> {
        let online = Arc::clone(&self.online);
        let stop = Arc::clone(&self.writer.stop);
        let done = Arc::clone(&self.writer.done);
        let handle = std::thread::Builder::new()
            .name("moruna-learning".into())
            .spawn(move || {
                loop {
                    let stopping = stop.load(Ordering::SeqCst);
                    if let Err(e) = online.flush() {
                        tracing::error!(target: "sched.learning", error = %e, "observations were not written");
                    }
                    if stopping {
                        if let Err(e) = online.save() {
                            tracing::error!(target: "sched.learning", error = %e, "the fitted state was not saved");
                        }
                        done.store(true, Ordering::SeqCst);
                        return;
                    }
                    std::thread::park_timeout(FLUSH_EVERY);
                }
            })
            .map_err(|e| MorunaError::Config {
                name: "cost.model",
                msg: format!("the learning thread could not be started: {e}"),
            })?;
        *self.writer.handle.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
        Ok(())
    }

    /// The run's output is complete: the learning thread writes what is left and the fitted
    /// state, waited for at most [`DRAIN`] by `clock`. A store that takes longer is abandoned:
    /// what it has not written is counted unwritten with the reason, and its thread is left to
    /// finish on its own, writing nothing more.
    pub(crate) fn finish(&self, clock: &dyn crate::decide::Clock) {
        let handle = self
            .writer
            .handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let Some(handle) = handle else {
            return;
        };
        let began = Instant::now();
        self.writer.stop.store(true, Ordering::SeqCst);
        handle.thread().unpark();
        let start = clock.now();
        while !self.writer.done.load(Ordering::SeqCst) {
            if clock.now().saturating_sub(start) >= DRAIN {
                self.online.abandon(&format!(
                    "the store had not written what the run learned {} s after its output was \
                     complete; the rest was not kept",
                    DRAIN.as_secs()
                ));
                tracing::error!(target: "sched.learning", "the store was abandoned at the run's end");
                self.drain_ns
                    .store(began.elapsed().as_nanos() as u64, Ordering::SeqCst);
                return;
            }
            clock.pause(Duration::from_millis(1));
        }
        let _joined = handle.join();
        self.drain_ns
            .store(began.elapsed().as_nanos() as u64, Ordering::SeqCst);
    }

    /// What learning did, by stage.
    pub(crate) fn stats(&self) -> LearningStats {
        let report = self.online.report();
        let stage_of = |key: &CostKey| {
            self.keys
                .iter()
                .position(|k| k == key)
                .map_or(0, |i| i as StageId + 1)
        };
        LearningStats {
            model: report
                .model
                .map(|m| format!("{}@{}", m.name, m.version))
                .unwrap_or_default(),
            shadow: report.shadow.map(|m| format!("{}@{}", m.name, m.version)),
            observed: report.observed,
            written: report.written,
            unwritten: report.unwritten,
            store_error: report.store_error,
            refused: report.refused,
            refusal: report.refusal,
            drifts: report
                .drifts
                .iter()
                .map(|d| DriftStat {
                    stage: stage_of(&d.key),
                    target: d.target.as_str().to_string(),
                    at_unit: d.at_unit,
                    units: d.units,
                    settled: d.settled,
                })
                .collect(),
        }
    }

    /// What settling, the store and the end's wait cost: `(learn_ns, learned, store_ns,
    /// save_ns, drain_ns)`.
    pub(crate) fn costs(&self) -> (u64, u64, u64, u64, u64) {
        let report = self.online.report();
        (
            self.learn_ns.load(Ordering::Relaxed),
            self.learned.load(Ordering::Relaxed),
            report.store_ns,
            report.save_ns,
            self.drain_ns.load(Ordering::SeqCst),
        )
    }
}

/// A model's prediction as the policy port carries it: bytes out and wall time, median and 95th
/// percentile, and the drifting mark of either.
fn contract(p: &moruna_cost::Prediction) -> Prediction {
    let cost = &p.deciding.cost;
    let (out, time) = (&cost.bytes_out, &cost.duration);
    let bytes = |v: f64| v.max(0.0).round() as u64;
    let ns = |v: f64| (v.max(0.0) * 1e9).round() as u64;
    Prediction {
        out_bytes: Spread::new(bytes(out.p50), bytes(out.p95)),
        duration_ns: Spread::new(ns(time.p50), ns(time.p95)),
        by: PredictedBy::Model,
        drifting: out.drifting || time.drifting,
        observations: out.observations.min(time.observations),
    }
}

/// What is known of a unit of stage `stage_ix` on a batch of `bytes` before it runs: its bytes,
/// its rows at the stage's measured rows per byte so far, or before its first unit at what its
/// first port's producer has made so far (unknown before anything has), and the workers taking
/// units.
pub(crate) fn inputs_of(shared: &Shared, stage_ix: usize, bytes: u64) -> Inputs {
    let entry = &shared.stages[stage_ix];
    let (mut rows, mut of) = (
        entry.rows_in.load(Ordering::SeqCst),
        entry.bytes_in.load(Ordering::SeqCst),
    );
    if of == 0
        && let Some(link) = entry.inputs.first().and_then(|&q| shared.topology.link(q))
    {
        (rows, of) = match link.from {
            moruna_kernel::Producer::Source(s) => {
                shared.sources.get(usize::from(s)).map_or((0, 0), |end| {
                    (
                        end.out.rows.load(Ordering::SeqCst),
                        end.out.bytes.load(Ordering::SeqCst),
                    )
                })
            }
            moruna_kernel::Producer::Kernel(k) => usize::from(k)
                .checked_sub(1)
                .and_then(|i| shared.stages.get(i))
                .map_or((0, 0), |p| {
                    (
                        p.rows_out.load(Ordering::SeqCst),
                        p.bytes_out.load(Ordering::SeqCst),
                    )
                }),
        };
    }
    let mut inputs = Inputs::default();
    inputs.data.bytes = Some(bytes);
    if of > 0 {
        inputs.data.rows = Some((u128::from(bytes) * u128::from(rows) / u128::from(of)) as u64);
    }
    inputs.resources.workers = Some(u32::from(shared.knobs.active_workers().max(1)));
    inputs
}

/// What one finished unit measured (adaptive-runtime 9.6): its rows and bytes in, its wall and
/// worker time, and its rows and bytes out. Shared-process endpoint samples cannot measure
/// a unit's transient peak or attribute other workers' allocations, so no PeakMemory value
/// is manufactured here. A true memory observation must come from an isolated measurement.
pub(crate) struct Measured {
    pub(crate) rows_in: u64,
    pub(crate) bytes_in: u64,
    pub(crate) wall_ns: u64,
    pub(crate) cpu_ns: u64,
    pub(crate) rows_out: u64,
    pub(crate) bytes_out: u64,
}

/// Settle a finished unit of stage `stage_ix` into the run's models, when it has some.
pub(crate) fn settle(
    shared: &Shared,
    stage_ix: usize,
    prediction: Option<&Arc<moruna_cost::Prediction>>,
    measured: Measured,
) {
    let (Some(learning), Some(prediction)) = (shared.learning.get(), prediction) else {
        return;
    };
    let mut inputs = Inputs::batch(measured.rows_in, measured.bytes_in);
    inputs.resources.workers = Some(u32::from(shared.knobs.active_workers().max(1)));
    let mut outcome = Outcome::new(Origin::Run);
    outcome
        .set(Target::Duration, measured.wall_ns as f64 / 1e9)
        .set(Target::WorkerTime, measured.cpu_ns as f64 / 1e9)
        .set(Target::RowsOut, measured.rows_out as f64)
        .set(Target::BytesOut, measured.bytes_out as f64);
    learning.settle(stage_ix, &inputs, prediction, &outcome);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_within_a_hundredth() {
        assert_eq!(bucket_bytes(bucket(0)), 0);
        for bytes in [1u64, 7, 128, 1_000, 65_536, 1 << 30, 123_456_789] {
            let back = bucket_bytes(bucket(bytes)) as f64;
            assert!(
                (back / bytes as f64 - 1.0).abs() < 0.011,
                "{bytes} -> {back}"
            );
        }
        assert_eq!(bucket(1_000), bucket(1_005), "within the bucket");
        assert_ne!(bucket(1_000), bucket(1_020));
    }
}
