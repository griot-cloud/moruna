//! SDD 24 k: learning during a run (CM-T40 to CM-T42): online updates without IO, the shadow
//! beside the deciding model, drift, and a store that refuses.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use common::{Rng, batch, key, scratch};
use moruna_cost::online::{DRIFT_WIDEN, SETTLE_UNITS};
use moruna_cost::{
    CostError, CostKey, CostModel, Defaults, History, InUse, Inputs, LocalStore, LogLinear,
    ModelId, Observation, ObservationSource, ObservationStore, Online, Origin, Outcome, Target,
};

/// A unit of `rows` rows that takes `per_row` seconds a row, with 5% noise.
fn outcome(rng: &mut Rng, rows: f64, per_row: f64) -> Outcome {
    let mut o = Outcome::new(Origin::Run);
    o.set(
        Target::Duration,
        per_row * rows * (0.05 * rng.normal()).exp(),
    );
    o.set(Target::BytesOut, 64.0 * rows);
    o
}

fn loglinear() -> Box<dyn CostModel> {
    Box::new(LogLinear::new(Defaults::standard()))
}

/// Settle `units` units of `k` costing `per_row` a row; the median relative error of the
/// deciding prediction over the last `tail` of them.
fn stream(online: &Online, k: &CostKey, rng: &mut Rng, units: u64, per_row: f64, tail: u64) -> f64 {
    let mut errors = Vec::new();
    for n in 0..units {
        let rows = rng.log_uniform(1e3, 1e5);
        let inputs = batch(rows);
        let prediction = online.predict(k, &inputs);
        let o = outcome(rng, rows, per_row);
        if n + tail >= units {
            let p50 = prediction.deciding.cost.duration.p50;
            errors.push((p50 / o.duration.expect("measured") - 1.0).abs());
        }
        online
            .settle(k, &inputs, Some(&prediction), &o, n)
            .expect("settles");
    }
    errors.sort_by(f64::total_cmp);
    errors.get(errors.len() / 2).copied().unwrap_or(0.0)
}

/// CM-T40 (CM-I25): a run learns online, and nothing is written until it flushes. 300 units
/// through `loglinear` deciding and `loglinear` in shadow: the deciding prediction's median
/// error falls under 10%, nothing reaches the store until `flush`, which writes all 300 in
/// order, each with both predictions, deciding first; the fitted state is saved for both, and a
/// model loaded from it predicts what the deciding one does. An outcome that is not a
/// measurement is refused, counted and learned from by no one.
#[test]
fn cm_t40_a_run_learns_online_and_writes_off_the_hot_path() {
    let root = scratch("online");
    let store: Arc<dyn ObservationStore> = Arc::new(LocalStore::open(&root).expect("opens"));
    let online = Online::new(
        InUse::new(loglinear(), Some(loglinear())),
        Arc::clone(&store),
    );
    let k = key("std", 1);
    let mut rng = Rng::new(40);
    let error = stream(&online, &k, &mut rng, 300, 2e-7, 50);
    assert!(error < 0.10, "median error after 300 units: {error}");
    assert_eq!(online.pending(), 300);
    assert!(
        store.scan(&k).expect("scans").observations.is_empty(),
        "no IO yet"
    );
    assert_eq!(online.flush().expect("flushes"), 300);
    assert_eq!(online.flush().expect("flushes"), 0, "nothing twice");
    let kept = store.scan(&k).expect("scans").observations;
    assert_eq!(kept.len(), 300);
    assert!(kept.windows(2).all(|w| w[0].at_ms < w[1].at_ms), "in order");
    assert!(
        kept.iter()
            .all(|o| o.predictions.len() == 2 && o.model == Some(LogLinear::model_id()))
    );
    online.save().expect("saves");
    let mut loaded = LogLinear::new(Defaults::standard());
    let state = store
        .load_state(&LogLinear::model_id())
        .expect("reads")
        .expect("saved");
    loaded.load(&state).expect("loads");
    let probe = batch(2e4);
    assert_eq!(
        loaded.predict(&k, &probe),
        online.predict(&k, &probe).deciding.cost,
        "the saved state predicts what the deciding model does"
    );
    let report = online.report();
    assert_eq!(
        (report.observed, report.written, report.unwritten),
        (300, 300, 0)
    );
    assert_eq!(report.model, Some(LogLinear::model_id()));
    assert_eq!(report.shadow, Some(LogLinear::model_id()));
    assert!(
        report.drifts.is_empty(),
        "a steady key never drifts: {:?}",
        report.drifts
    );

    let mut bad = Outcome::new(Origin::Run);
    bad.set(Target::Duration, f64::NAN);
    let refused = online.settle(&k, &batch(1e3), None, &bad, 1).err();
    assert!(
        matches!(refused, Some(CostError::Invalid(_))),
        "{refused:?}"
    );
    let report = online.report();
    assert_eq!((report.refused, report.observed), (1, 300));
    assert!(report.refusal.expect("a reason").contains("duration"));
    assert_eq!(online.model(), LogLinear::model_id());
    assert!(format!("{online:?}").contains("loglinear"));
    std::fs::remove_dir_all(&root).ok();
}

/// CM-T41 (CM-I26): drift is found, widens, forgets faster and settles. A key steady for 200
/// units turns three times slower. Within three to eight units it is marked drifting, its
/// predictions are marked and their spread is at least `DRIFT_WIDEN` times what it was, and the
/// deciding model forgets at the drifting rate, so twenty units after the change its median
/// error is under 15% where a model told nothing is still over 25%. Once its outcomes are back
/// inside the model's own spread for `SETTLE_UNITS` units it settles, the mark goes, and the
/// report has the one spell, settled, begun at unit 201 or after. A shadow model is never told.
#[test]
fn cm_t41_drift_is_found_widens_forgets_and_settles() {
    let root = scratch("drift");
    let store: Arc<dyn ObservationStore> = Arc::new(LocalStore::open(&root).expect("opens"));
    let online = Online::new(InUse::new(loglinear(), None), Arc::clone(&store));
    let k = key("std", 2);
    let mut rng = Rng::new(41);
    stream(&online, &k, &mut rng, 200, 1e-6, 10);
    let probe = batch(2e4);
    let before = online.predict(&k, &probe).deciding.cost.duration;
    assert!(!before.drifting && !online.is_drifting(&k));

    // A model told nothing, fed the same units, for comparison.
    let mut plain = LogLinear::new(Defaults::standard());
    let mut twin = Rng::new(41);
    for n in 0..200 {
        let rows = twin.log_uniform(1e3, 1e5);
        let o = outcome(&mut twin, rows, 1e-6);
        let _ = n;
        plain.observe(&k, &batch(rows), &o);
    }

    let mut began = None;
    let mut errors = Vec::new();
    let mut plain_errors = Vec::new();
    for n in 0..20u64 {
        let rows = rng.log_uniform(1e3, 1e5);
        let inputs = batch(rows);
        let prediction = online.predict(&k, &inputs);
        let o = outcome(&mut rng, rows, 3e-6);
        let measured = o.duration.expect("measured");
        if n >= 12 {
            errors.push((prediction.deciding.cost.duration.p50 / measured - 1.0).abs());
            let p = plain.predict(&k, &inputs).duration.p50;
            plain_errors.push((p / measured - 1.0).abs());
        }
        plain.observe(&k, &inputs, &o);
        online
            .settle(&k, &inputs, Some(&prediction), &o, 200 + n)
            .expect("settles");
        if began.is_none() && online.is_drifting(&k) {
            began = Some(n + 1);
            let now = online.predict(&k, &probe).deciding.cost.duration;
            assert!(now.drifting, "marked");
            let spread = |e: &moruna_cost::Estimate| e.sigma();
            assert!(
                spread(&now) >= DRIFT_WIDEN * spread(&before) * 0.999,
                "widened: {} against {}",
                spread(&now),
                spread(&before)
            );
        }
    }
    let began = began.expect("the change was found");
    assert!((3..=8).contains(&began), "found after {began} units");
    let median = |mut v: Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    let (error, plain_error) = (median(errors), median(plain_errors));
    println!("cm_t41: found after {began} units; error {error:.3}, told nothing {plain_error:.3}");
    assert!(
        error < 0.15,
        "drifting model's error 12 to 20 units on: {error}"
    );
    assert!(plain_error > 0.25, "the model told nothing: {plain_error}");

    let mut settled = None;
    for n in 0..200u64 {
        let rows = rng.log_uniform(1e3, 1e5);
        let inputs = batch(rows);
        let prediction = online.predict(&k, &inputs);
        let o = outcome(&mut rng, rows, 3e-6);
        online
            .settle(&k, &inputs, Some(&prediction), &o, 300 + n)
            .expect("settles");
        if !online.is_drifting(&k) {
            settled = Some(n);
            break;
        }
    }
    println!("cm_t41: settled {settled:?} units after the twentieth");
    assert!(settled.is_some(), "it settles");
    assert!(!online.predict(&k, &probe).deciding.cost.duration.drifting);
    let report = online.report();
    let spells: Vec<_> = report
        .drifts
        .iter()
        .filter(|d| d.target == Target::Duration)
        .collect();
    assert_eq!(spells.len(), 1, "{:?}", report.drifts);
    let spell = spells[0];
    assert!(spell.settled && spell.at_unit > 200 && spell.units >= u64::from(SETTLE_UNITS));
    assert_eq!(spell.key, k);
    std::fs::remove_dir_all(&root).ok();
}

/// A store that refuses every append after its first `ok`.
struct Refusing {
    inner: LocalStore,
    ok: u64,
    seen: AtomicU64,
}

impl ObservationSource for Refusing {
    fn scan(&self, key: &CostKey) -> moruna_cost::Result<History> {
        self.inner.scan(key)
    }

    fn scan_all(
        &self,
        each: &mut dyn FnMut(History) -> moruna_cost::Result<()>,
    ) -> moruna_cost::Result<()> {
        self.inner.scan_all(each)
    }
}

impl ObservationStore for Refusing {
    fn append(&self, observation: &Observation) -> moruna_cost::Result<()> {
        if self.seen.fetch_add(1, Ordering::SeqCst) >= self.ok {
            return Err(CostError::Invalid("the disk is full".into()));
        }
        self.inner.append(observation)
    }

    fn save_state(&self, _model: &ModelId, _state: &[u8]) -> moruna_cost::Result<()> {
        Err(CostError::Invalid("the state disk is full".into()))
    }

    fn load_state(&self, model: &ModelId) -> moruna_cost::Result<Option<Vec<u8>>> {
        self.inner.load_state(model)
    }
}

/// CM-T42 (CM-I25): a store that refuses is reported, never hidden. A store that takes four
/// appends and refuses the rest: a flush of ten writes four, fails with the store's error, and
/// the report counts four written and six unwritten with the reason; the model learned all ten.
/// A store that refuses the fitted state fails the save, and the first reason stays the report's.
#[test]
fn cm_t42_a_store_that_refuses_is_reported() {
    let root = scratch("refusing");
    let store = Arc::new(Refusing {
        inner: LocalStore::open(&root).expect("opens"),
        ok: 4,
        seen: AtomicU64::new(0),
    });
    let online = Online::new(InUse::new(loglinear(), None), store.clone());
    let k = key("std", 3);
    let mut rng = Rng::new(42);
    stream(&online, &k, &mut rng, 10, 1e-6, 1);
    let failed = online.flush().expect_err("the store refused");
    assert!(failed.to_string().contains("the disk is full"), "{failed}");
    let report = online.report();
    assert_eq!(
        (report.observed, report.written, report.unwritten),
        (10, 4, 6)
    );
    assert!(
        report
            .store_error
            .expect("a reason")
            .contains("the disk is full")
    );
    assert_eq!(store.scan(&k).expect("scans").observations.len(), 4);
    let inputs: Inputs = batch(1e4);
    let p = online.predict(&k, &inputs).deciding.cost.duration;
    assert!(
        p.observations >= 10,
        "learned every unit: {}",
        p.observations
    );
    let refused = online.save().expect_err("the state was refused");
    assert!(
        refused.to_string().contains("state disk is full"),
        "{refused}"
    );
    let report = online.report();
    assert!(
        report
            .store_error
            .expect("a reason")
            .contains("the disk is full"),
        "the first reason stays"
    );
    // On a store with nothing kept before it, the save's reason is the first.
    let fresh = Online::new(InUse::new(loglinear(), None), store);
    assert!(fresh.save().is_err());
    assert!(
        fresh
            .report()
            .store_error
            .expect("a reason")
            .contains("fitted state was not saved")
    );
    std::fs::remove_dir_all(&root).ok();
}

/// CM-T41's model side (CM-I26): `loglinear` marks a key drifting and settled as told, and keeps
/// no mark across a load or a fit.
#[test]
fn cm_t41_loglinear_is_told_of_drift() {
    let k = key("std", 4);
    let mut model = LogLinear::new(Defaults::standard());
    let mut rng = Rng::new(43);
    for _ in 0..100 {
        let rows = rng.log_uniform(1e3, 1e5);
        model.observe(&k, &batch(rows), &outcome(&mut rng, rows, 1e-6));
    }
    assert!(!model.is_drifting(&k));
    model.set_drifting(&k, true);
    assert!(model.is_drifting(&k));
    model.set_drifting(&k, true);
    let state = model.save().expect("saves");
    model.set_drifting(&k, false);
    assert!(!model.is_drifting(&k));
    model.set_drifting(&k, true);
    model.load(&state).expect("loads");
    assert!(!model.is_drifting(&k), "a load keeps no mark");
    model.set_drifting(&k, true);
    let dir = scratch("drift-fit");
    let empty = LocalStore::open(&dir).expect("opens");
    model.fit(&empty).expect("fits");
    assert!(!model.is_drifting(&k), "a fit keeps no mark");
    // A key the model has never seen can be marked; it has no fit to forget.
    let unseen = key("std", 5);
    model.set_drifting(&unseen, true);
    assert!(model.is_drifting(&unseen));
    std::fs::remove_dir_all(&dir).ok();
}

/// A store whose appends wait at a gate until it opens, counting those that got through.
struct Gate {
    inner: LocalStore,
    open: std::sync::Mutex<bool>,
    opened: std::sync::Condvar,
    waiting: std::sync::Mutex<bool>,
    arrived: std::sync::Condvar,
    appended: AtomicU64,
}

impl Gate {
    fn wait_until_waiting(&self) {
        let waiting = self.waiting.lock().expect("arrival gate");
        let (waiting, _) = self
            .arrived
            .wait_timeout_while(waiting, std::time::Duration::from_secs(5), |waiting| {
                !*waiting
            })
            .expect("arrival gate");
        assert!(*waiting, "the flush never reached the store");
    }

    fn release(&self) {
        *self.open.lock().expect("gate") = true;
        self.opened.notify_all();
    }
}

impl ObservationSource for Gate {
    fn scan(&self, key: &CostKey) -> moruna_cost::Result<History> {
        self.inner.scan(key)
    }

    fn scan_all(
        &self,
        each: &mut dyn FnMut(History) -> moruna_cost::Result<()>,
    ) -> moruna_cost::Result<()> {
        self.inner.scan_all(each)
    }
}

impl ObservationStore for Gate {
    fn append(&self, observation: &Observation) -> moruna_cost::Result<()> {
        *self.waiting.lock().expect("arrival gate") = true;
        self.arrived.notify_all();
        let mut open = self.open.lock().expect("gate");
        while !*open {
            open = self.opened.wait(open).expect("gate");
        }
        drop(open);
        self.appended.fetch_add(1, Ordering::SeqCst);
        self.inner.append(observation)
    }

    fn save_state(&self, model: &ModelId, state: &[u8]) -> moruna_cost::Result<()> {
        self.inner.save_state(model, state)
    }

    fn load_state(&self, model: &ModelId) -> moruna_cost::Result<Option<Vec<u8>>> {
        self.inner.load_state(model)
    }
}

/// CM-T43 (CM-I25): what a store that does not keep up costs is counted, never hidden, and
/// every unit is counted once. With room for four observations between flushes, ten units keep
/// four and count six unwritten, saying the store did not keep up. A flush that waits on a
/// closed store, three more units, and the run abandoning the store: the four the flush holds
/// and the three kept are counted unwritten with the run's reason, and once the store opens the
/// waiting flush writes nothing more; written and unwritten add up to the units observed, and
/// later flushes and saves write nothing.
#[test]
fn cm_t43_a_store_that_does_not_keep_up_is_counted() {
    let root = scratch("gate");
    let gate = Arc::new(Gate {
        inner: LocalStore::open(&root).expect("opens"),
        open: std::sync::Mutex::new(false),
        opened: std::sync::Condvar::new(),
        waiting: std::sync::Mutex::new(false),
        arrived: std::sync::Condvar::new(),
        appended: AtomicU64::new(0),
    });
    let online =
        Arc::new(Online::new(InUse::new(loglinear(), None), gate.clone()).with_pending_limit(4));
    let k = key("std", 6);
    let mut rng = Rng::new(44);
    stream(&online, &k, &mut rng, 10, 1e-6, 1);
    assert_eq!(online.pending(), 4);
    let report = online.report();
    assert_eq!(
        (report.observed, report.written, report.unwritten),
        (10, 0, 6)
    );
    assert!(
        report
            .store_error
            .as_deref()
            .is_some_and(|e| e.contains("more than 4 observations")),
        "{report:?}"
    );

    let flushing = Arc::clone(&online);
    let flush = std::thread::spawn(move || flushing.flush());
    gate.wait_until_waiting();
    stream(&online, &k, &mut rng, 3, 1e-6, 1);
    assert_eq!(
        online.pending(),
        3,
        "the flush took the four, three more are kept"
    );
    online.abandon("the run stopped waiting for the store");
    assert!(online.is_abandoned());
    gate.release();
    assert_eq!(flush.join().expect("joins").expect("no store error"), 0);
    let report = online.report();
    assert_eq!(report.observed, 13);
    assert_eq!(report.written + report.unwritten, 13, "{report:?}");
    assert_eq!((report.written, report.unwritten), (0, 13));
    assert_eq!(
        gate.appended.load(Ordering::SeqCst),
        1,
        "only the waiting append got through"
    );
    assert_eq!(online.flush().expect("nothing to write"), 0);
    online.save().expect("an abandoned store saves nothing");
    assert!(
        gate.load_state(&LogLinear::model_id())
            .expect("reads")
            .is_none()
    );
    std::fs::remove_dir_all(&root).ok();
}

/// CM-T44 (CM-I25): a second flush cannot take the pending batch while the first
/// waits in the store. It refuses without waiting on IO or changing either batch;
/// the first flush and a later flush write every observation once, in order.
#[test]
fn cm_t44_concurrent_flushes_preserve_order_and_accounting() {
    let root = scratch("concurrent-flush");
    let gate = Arc::new(Gate {
        inner: LocalStore::open(&root).expect("opens"),
        open: std::sync::Mutex::new(false),
        opened: std::sync::Condvar::new(),
        waiting: std::sync::Mutex::new(false),
        arrived: std::sync::Condvar::new(),
        appended: AtomicU64::new(0),
    });
    let online = Arc::new(Online::new(InUse::new(loglinear(), None), gate.clone()));
    let k = key("std", 7);
    let inputs = batch(1e3);
    let mut measured = Outcome::new(Origin::Run);
    measured.set(Target::Duration, 1e-6);
    for at in 0..2 {
        online
            .settle(&k, &inputs, None, &measured, at)
            .expect("settles");
    }
    let first_online = Arc::clone(&online);
    let first = std::thread::spawn(move || first_online.flush());
    gate.wait_until_waiting();
    for at in 2..4 {
        online
            .settle(&k, &inputs, None, &measured, at)
            .expect("settles");
    }
    assert_eq!(online.pending(), 2);
    let second_online = Arc::clone(&online);
    let (sent, received) = std::sync::mpsc::sync_channel(1);
    let second = std::thread::spawn(move || {
        sent.send(second_online.flush()).expect("reports result");
    });
    // The bound detects a flush waiting on the blocked store, not machine speed.
    // Always release and join both callers before checking the result.
    let concurrent = received.recv_timeout(std::time::Duration::from_secs(5));
    let pending = online.pending();
    gate.release();
    let first_result = first.join().expect("first joins");
    second.join().expect("second joins");
    let refused = concurrent
        .expect("the second flush must not wait on the store")
        .expect_err("a flush is already in progress");
    assert!(
        refused.to_string().contains("already in progress"),
        "{refused}"
    );
    assert_eq!(pending, 2, "a refused flush leaves the next batch pending");
    assert_eq!(first_result.expect("first flush succeeds"), 2);
    assert_eq!(online.flush().expect("the next batch flushes"), 2);
    let observations = gate.scan(&k).expect("reads").observations;
    assert_eq!(
        observations.iter().map(|o| o.at_ms).collect::<Vec<_>>(),
        vec![0, 1, 2, 3]
    );
    let report = online.report();
    assert_eq!(
        (report.observed, report.written, report.unwritten),
        (4, 4, 0)
    );
    assert!(report.store_error.is_none());
    std::fs::remove_dir_all(&root).expect("removes own scratch");
}
