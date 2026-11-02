//! SC-T38 learning (f.22; adaptive-runtime 9.4, 9.6, 9.7).
//!
//! A run given a cost model predicts every unit it admits from the model, learns every unit it
//! finishes, the shadow beside it, and writes every observation to the store and the fitted
//! state beside them by its end; the report says so. A model that turns wrong mid-run is found
//! drifting, by stage, and the run still completes with every batch once. The model is set before
//! the run, once, with a key for each kernel stage, or refused.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use moruna_cost::{
    CostKey, CostModel, Defaults, Digest, FitReport, InUse, Inputs, LocalStore, LogLinear, ModelId,
    ObservationSource, ObservationStore, Online, Outcome, Target, UnitCost,
};
use moruna_job::Role;
use moruna_kernel::{CancelToken, Knob, Knobs, StatsSource};
use moruna_testkit::{FakeKernel, FakeSource};

use super::common::RigBuilder;
use crate::policies::{LOOKAHEAD, RESIDENCY_GREEDY};

fn key(stage: u8) -> CostKey {
    CostKey {
        role: Role::Kernel,
        kind: "fake".into(),
        fingerprint: Digest([stage; 32]),
        settings: Digest([0; 32]),
        schema: Digest([0; 32]),
    }
}

/// A store directory only this test uses, removed when it goes.
struct Dir(std::path::PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "moruna-sc-t38-{name}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        Dir(path)
    }

    fn store(&self) -> Arc<LocalStore> {
        Arc::new(LocalStore::open(&self.0).expect("opens"))
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn loglinear() -> Box<dyn CostModel> {
    Box::new(LogLinear::new(Defaults::standard()))
}

#[test]
fn sc_t38_a_run_learns_every_unit_and_keeps_it() {
    for policy in [RESIDENCY_GREEDY, LOOKAHEAD] {
        let dir = Dir::new(policy);
        let store = dir.store();
        let online = Arc::new(Online::new(
            InUse::new(loglinear(), Some(loglinear())),
            store.clone(),
        ));
        let rig = RigBuilder::new()
            .cfg(|cfg| cfg.policy = policy.into())
            .source(FakeSource::new().splits(20, 16, 128).sub_splittable(false))
            .stages(2)
            .go();
        rig.scheduler
            .set_cost(Arc::clone(&online), vec![key(1), key(2)])
            .expect("set before the run");
        match rig.scheduler.run(CancelToken::new()) {
            Ok(crate::RunOutcome::Completed { .. }) => {}
            other => panic!("{policy}: expected Completed, got {other:?}"),
        }
        let stats = rig.scheduler.scheduler_stats().policy;
        let learning = stats
            .learning
            .clone()
            .expect("a run with a model reports learning");
        assert_eq!(learning.model, "loglinear@1");
        assert_eq!(learning.shadow.as_deref(), Some("loglinear@1"));
        assert_eq!(
            (learning.observed, learning.written, learning.unwritten),
            (40, 40, 0),
            "{policy}: {learning:?}"
        );
        assert!(learning.store_error.is_none() && learning.refused == 0);
        assert_eq!(stats.predicted_by_model, stats.admitted, "{stats:?}");
        assert_eq!(stats.predicted_by_ratio, 0);
        let mut kept = Vec::new();
        store
            .scan_all(&mut |h| {
                kept.extend(h.observations);
                Ok(())
            })
            .expect("scans");
        assert_eq!(kept.len(), 40);
        assert!(
            kept.iter().all(|o| o.predictions.len() == 2
                && o.outcome.get(Target::Duration).is_some()
                && o.outcome.get(Target::PeakMemory).is_none()
                && o.outcome.get(Target::BytesOut) == o.inputs.data.bytes.map(|b| b as f64)),
            "each with both predictions and what it measured: {:?}",
            kept.first()
        );
        assert!(
            store
                .load_state(&LogLinear::model_id())
                .expect("reads")
                .is_some(),
            "the fitted state is saved at the end"
        );
        // The model learned the run: a unit of stage 1 is predicted at what it measured (the
        // fake kernel gives back what it takes).
        let bytes = kept[0].inputs.data.bytes.expect("measured");
        let p = online.predict(&key(1), &Inputs::batch(16, bytes));
        assert!(p.deciding.cost.bytes_out.observations >= 20);
        assert!(
            (p.deciding.cost.bytes_out.p50 / bytes as f64 - 1.0).abs() < 0.05,
            "{p:?}"
        );
        assert_eq!(rig.sink.written().len(), 20);
    }
}

#[test]
fn sc_t38_the_model_is_set_once_before_the_run() {
    let dir = Dir::new("refusals");
    let online = Arc::new(Online::new(InUse::new(loglinear(), None), dir.store()));
    let rig = RigBuilder::new()
        .source(FakeSource::new().splits(2, 16, 128).sub_splittable(false))
        .stages(2)
        .go();
    let refused = rig
        .scheduler
        .set_cost(Arc::clone(&online), vec![key(1)])
        .expect_err("one key for two stages");
    assert!(
        refused
            .to_string()
            .contains("2 kernel stages and 1 cost keys"),
        "{refused}"
    );
    rig.scheduler
        .set_cost(Arc::clone(&online), vec![key(1), key(2)])
        .expect("set");
    let again = rig
        .scheduler
        .set_cost(Arc::clone(&online), vec![key(1), key(2)])
        .expect_err("set once");
    assert!(again.to_string().contains("set already"), "{again}");
    match rig.scheduler.run(CancelToken::new()) {
        Ok(crate::RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }
    let late = RigBuilder::new()
        .source(FakeSource::new().splits(2, 16, 128).sub_splittable(false))
        .stages(1)
        .go();
    match late.scheduler.run(CancelToken::new()) {
        Ok(crate::RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }
    let after = late
        .scheduler
        .set_cost(online, vec![key(1)])
        .expect_err("not after the run");
    assert!(
        after.to_string().contains("before the run starts"),
        "{after}"
    );
    let stats = late.scheduler.scheduler_stats().policy;
    assert!(stats.learning.is_none(), "no model, no learning");
    assert_eq!(stats.predicted_by_model, 0);
    assert_eq!(stats.predicted_by_ratio, stats.admitted);
}

/// `loglinear`, its predictions true for the first `after` units and a thousand times too large
/// from then on.
struct Turns {
    inner: LogLinear,
    after: u64,
    seen: u64,
}

impl CostModel for Turns {
    fn id(&self) -> ModelId {
        ModelId::new("turns", 1)
    }
    fn predict(&self, key: &CostKey, inputs: &Inputs) -> UnitCost {
        let mut cost = self.inner.predict(key, inputs);
        if self.seen >= self.after {
            for t in Target::ALL {
                *cost.get_mut(t) = cost.get(t).scaled(1000.0);
            }
        }
        cost
    }
    fn observe(&mut self, key: &CostKey, inputs: &Inputs, outcome: &Outcome) {
        self.seen += 1;
        self.inner.observe(key, inputs, outcome);
    }
    fn fit(&mut self, history: &dyn ObservationSource) -> moruna_cost::Result<FitReport> {
        self.inner.fit(history)
    }
    fn save(&self) -> moruna_cost::Result<Vec<u8>> {
        self.inner.save()
    }
    fn load(&mut self, bytes: &[u8]) -> moruna_cost::Result<()> {
        self.inner.load(bytes)
    }
}

#[test]
fn sc_t38_a_model_that_turns_wrong_drifts_and_the_run_completes() {
    let dir = Dir::new("drift");
    let model = Turns {
        inner: LogLinear::new(Defaults::standard()),
        after: 20,
        seen: 0,
    };
    let online = Arc::new(Online::new(InUse::new(Box::new(model), None), dir.store()));
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.policy = LOOKAHEAD.into();
            // This assertion measures three wrong predictions after the model's
            // twentieth settled observation. Concurrent admissions may already
            // carry correct predictions from before that turn, so use one worker
            // to make the prediction/settlement boundary deterministic.
            cfg.workers_max = 1;
            cfg.workers_active = 1;
        })
        .source(FakeSource::new().splits(40, 16, 128).sub_splittable(false))
        .kernel(Arc::new(FakeKernel::new()))
        .go();
    rig.scheduler
        .set_cost(Arc::clone(&online), vec![key(1)])
        .expect("set");
    match rig.scheduler.run(CancelToken::new()) {
        Ok(crate::RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }
    let mut written = rig.sink.written();
    written.sort_unstable();
    assert_eq!(written, (0..40).collect::<Vec<u64>>(), "every batch once");
    let learning = rig
        .scheduler
        .scheduler_stats()
        .policy
        .learning
        .expect("learning");
    let bytes = learning
        .drifts
        .iter()
        .find(|d| d.target == "bytes_out")
        .unwrap_or_else(|| panic!("the wrong output is found drifting: {learning:?}"));
    assert_eq!(bytes.stage, 1);
    assert!(bytes.at_unit > 20 && bytes.at_unit <= 24, "{bytes:?}");
    assert!(!bytes.settled, "a model that stays wrong does not settle");
    assert_eq!(learning.observed, 40);
}

/// A store that refuses everything it is given to keep.
struct Full(LocalStore);

impl ObservationSource for Full {
    fn scan(&self, key: &CostKey) -> moruna_cost::Result<moruna_cost::History> {
        self.0.scan(key)
    }
    fn scan_all(
        &self,
        each: &mut dyn FnMut(moruna_cost::History) -> moruna_cost::Result<()>,
    ) -> moruna_cost::Result<()> {
        self.0.scan_all(each)
    }
}

impl ObservationStore for Full {
    fn append(&self, _observation: &moruna_cost::Observation) -> moruna_cost::Result<()> {
        Err(moruna_cost::CostError::Invalid("the disk is full".into()))
    }
    fn save_state(&self, _model: &ModelId, _state: &[u8]) -> moruna_cost::Result<()> {
        Err(moruna_cost::CostError::Invalid("the disk is full".into()))
    }
    fn load_state(&self, model: &ModelId) -> moruna_cost::Result<Option<Vec<u8>>> {
        self.0.load_state(model)
    }
}

#[test]
fn sc_t38_a_store_that_refuses_is_reported_and_the_run_completes() {
    let dir = Dir::new("full");
    let store = Arc::new(Full(LocalStore::open(&dir.0).expect("opens")));
    let online = Arc::new(Online::new(InUse::new(loglinear(), None), store));
    let rig = RigBuilder::new()
        .source(FakeSource::new().splits(6, 16, 128).sub_splittable(false))
        .stages(1)
        .go();
    rig.scheduler
        .set_cost(Arc::clone(&online), vec![key(1)])
        .expect("set");
    match rig.scheduler.run(CancelToken::new()) {
        Ok(crate::RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }
    assert_eq!(
        rig.sink.written().len(),
        6,
        "the output does not depend on the store"
    );
    let learning = rig
        .scheduler
        .scheduler_stats()
        .policy
        .learning
        .expect("learning");
    assert_eq!(
        (learning.observed, learning.written, learning.unwritten),
        (6, 0, 6)
    );
    assert!(
        learning
            .store_error
            .is_some_and(|e| e.contains("the disk is full")),
        "the reason is reported"
    );
}

/// A store whose appends wait at a gate until it opens.
struct Gate {
    inner: LocalStore,
    open: std::sync::Mutex<bool>,
    opened: std::sync::Condvar,
}

impl Gate {
    fn release(&self) {
        *self.open.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.opened.notify_all();
    }
}

impl ObservationSource for Gate {
    fn scan(&self, key: &CostKey) -> moruna_cost::Result<moruna_cost::History> {
        self.inner.scan(key)
    }
    fn scan_all(
        &self,
        each: &mut dyn FnMut(moruna_cost::History) -> moruna_cost::Result<()>,
    ) -> moruna_cost::Result<()> {
        self.inner.scan_all(each)
    }
}

impl ObservationStore for Gate {
    fn append(&self, observation: &moruna_cost::Observation) -> moruna_cost::Result<()> {
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        while !*open {
            open = self.opened.wait(open).unwrap_or_else(|e| e.into_inner());
        }
        drop(open);
        self.inner.append(observation)
    }
    fn save_state(&self, model: &ModelId, state: &[u8]) -> moruna_cost::Result<()> {
        self.inner.save_state(model, state)
    }
    fn load_state(&self, model: &ModelId) -> moruna_cost::Result<Option<Vec<u8>>> {
        self.inner.load_state(model)
    }
}

/// A clock that moves only when it is waited on, so the run's wait for the store at its end
/// passes at once.
#[derive(Debug, Default)]
struct Stepped(AtomicU64);

impl crate::decide::Clock for Stepped {
    fn now(&self) -> std::time::Duration {
        std::time::Duration::from_nanos(self.0.load(Ordering::SeqCst))
    }
    fn pause(&self, pause: std::time::Duration) {
        self.0.fetch_add(pause.as_nanos() as u64, Ordering::SeqCst);
        std::thread::yield_now();
    }
}

/// SC-T38's store that does not keep up (SC-I15, f.22, 24 CM-I25): a store that blocks every
/// append, with room for four observations between writes. The run completes with every batch
/// once; past the four kept, the units are counted unwritten saying the store did not keep up;
/// at the run's end the learning thread is waited for `DRAIN` by the run's clock and then
/// abandoned, everything it had not written counted unwritten, so written and unwritten add up
/// to the units observed; opening the store afterwards adds nothing to what the run reported.
#[test]
fn sc_t38_a_store_that_blocks_never_holds_the_run() {
    let dir = Dir::new("gate");
    let gate = Arc::new(Gate {
        inner: LocalStore::open(&dir.0).expect("opens"),
        open: std::sync::Mutex::new(false),
        opened: std::sync::Condvar::new(),
    });
    let online =
        Arc::new(Online::new(InUse::new(loglinear(), None), gate.clone()).with_pending_limit(4));
    let rig = RigBuilder::new()
        .cfg(|cfg| cfg.clock = Arc::new(Stepped::default()))
        .source(FakeSource::new().splits(40, 16, 128).sub_splittable(false))
        .stages(1)
        .go();
    rig.scheduler
        .set_cost(Arc::clone(&online), vec![key(1)])
        .expect("set");
    let (send, receive) = std::sync::mpsc::channel();
    let running = std::thread::spawn({
        let scheduler = rig.scheduler;
        move || {
            let outcome = scheduler.run(CancelToken::new());
            let _ = send.send(());
            (outcome, scheduler)
        }
    });
    if receive
        .recv_timeout(std::time::Duration::from_secs(60))
        .is_err()
    {
        gate.release();
        panic!("a store that blocks held the run");
    }
    let (outcome, scheduler) = running.join().expect("joins");
    match outcome {
        Ok(crate::RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }
    let mut written = rig.sink.written();
    written.sort_unstable();
    assert_eq!(written, (0..40).collect::<Vec<u64>>(), "every batch once");
    let stats = scheduler.scheduler_stats().policy;
    let learning = stats.learning.expect("learning");
    assert_eq!(learning.observed, 40);
    assert_eq!(learning.written, 0);
    assert_eq!(
        learning.written + learning.unwritten,
        learning.observed,
        "every unit counted once: {learning:?}"
    );
    assert!(
        learning
            .store_error
            .as_deref()
            .is_some_and(|e| e.contains("more than 4 observations")),
        "{learning:?}"
    );
    assert!(
        online.is_abandoned(),
        "the run stopped waiting for the store"
    );
    gate.release();
    let again = scheduler
        .scheduler_stats()
        .policy
        .learning
        .expect("learning");
    assert_eq!(
        (again.written, again.unwritten),
        (0, 40),
        "nothing changes after"
    );
}

/// A model whose visible predictions depend on the inputs that may change before
/// any unit has finished: active workers and the source's known rows per byte.
struct InputSensitive;

impl CostModel for InputSensitive {
    fn id(&self) -> ModelId {
        ModelId::new("input-sensitive", 1)
    }

    fn predict(&self, _key: &CostKey, inputs: &Inputs) -> UnitCost {
        let mut cost = UnitCost::zero();
        cost.duration = moruna_cost::Estimate::exact(
            f64::from(inputs.resources.workers.expect("workers are known")),
            moruna_cost::Unit::Seconds,
        );
        cost.bytes_out = moruna_cost::Estimate::exact(
            inputs.data.rows.unwrap_or(0) as f64,
            moruna_cost::Unit::Bytes,
        );
        cost
    }

    fn observe(&mut self, _key: &CostKey, _inputs: &Inputs, _outcome: &Outcome) {}

    fn fit(&mut self, _history: &dyn ObservationSource) -> moruna_cost::Result<FitReport> {
        Ok(FitReport::default())
    }

    fn save(&self) -> moruna_cost::Result<Vec<u8>> {
        Ok(Vec::new())
    }

    fn load(&mut self, _bytes: &[u8]) -> moruna_cost::Result<()> {
        Ok(())
    }
}

#[test]
fn sc_t38_predictions_follow_workers_and_new_source_statistics() {
    let dir = Dir::new("prediction-inputs");
    let online = Arc::new(Online::new(
        InUse::new(Box::new(InputSensitive), None),
        dir.store(),
    ));
    let rig = RigBuilder::new().stages(1).go();
    rig.scheduler.set_cost(online, vec![key(1)]).expect("set");
    let shared = &rig.scheduler.shared;
    let learning = shared.learning.get().expect("a model is installed");
    let first = learning.predict(shared, 0, 128);
    assert_eq!(first.prediction.duration_ns.p50, 2_000_000_000);
    assert_eq!(first.prediction.out_bytes.p50, 0, "rows are not known yet");
    let same = learning.predict(shared, 0, 128);
    assert!(
        Arc::ptr_eq(&first.model, &same.model),
        "unchanged inputs reuse the prediction"
    );

    rig.scheduler.set(Knob::ActiveWorkers(1));
    let resized = learning.predict(shared, 0, 128);
    assert_eq!(
        resized.prediction.duration_ns.p50, 1_000_000_000,
        "the next decision sees the new worker count even before a unit settles"
    );
    shared.sources[0].out.rows.store(16, Ordering::SeqCst);
    shared.sources[0].out.bytes.store(128, Ordering::SeqCst);
    let known = learning.predict(shared, 0, 128);
    assert_eq!(
        known.prediction.out_bytes.p50, 16,
        "the next decision sees newly available source statistics"
    );
    let same = learning.predict(shared, 0, 128);
    assert!(
        Arc::ptr_eq(&known.model, &same.model),
        "stable inputs still reuse the prediction"
    );
}
