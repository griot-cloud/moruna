//! SDD 24 k: the swap and shadow (CM-T15 to CM-T17), proved with a second adapter that lives
//! in this file and nowhere else.

mod common;

use std::sync::Arc;

use common::{Rng, batch, key, scratch};
use moruna_cost::{
    Basis, CostError, CostKey, CostModel, CostModels, CostSetting, Estimate, FitReport, InUse,
    Inputs, LocalStore, LogLinear, ModelAdapter, ModelId, ObservationSource, ObservationStore,
    Origin, Outcome, Readiness, Target, Unit, UnitCost,
};
use serde::{Deserialize, Serialize};

/// The test model: whatever is asked, it answers each target's median over everything it was
/// fitted on.
#[derive(Default)]
struct Constant {
    medians: Vec<(Target, f64)>,
}

#[derive(Serialize, Deserialize)]
struct ConstantState {
    model: ModelId,
    medians: Vec<(Target, f64)>,
}

impl CostModel for Constant {
    fn id(&self) -> ModelId {
        ModelId::new("constant", 1)
    }

    fn predict(&self, _key: &CostKey, _inputs: &Inputs) -> UnitCost {
        let mut cost = UnitCost::zero();
        for (target, value) in &self.medians {
            *cost.get_mut(*target) = Estimate {
                p50: *value,
                p95: *value * 2.0,
                unit: target.unit(),
                basis: Basis::Measured,
                observations: 1,
                extrapolated: false,
            };
        }
        cost
    }

    fn observe(&mut self, _key: &CostKey, _inputs: &Inputs, _outcome: &Outcome) {}

    fn fit(&mut self, history: &dyn ObservationSource) -> moruna_cost::Result<FitReport> {
        let mut values: Vec<(Target, Vec<f64>)> =
            Target::ALL.into_iter().map(|t| (t, Vec::new())).collect();
        let mut report = FitReport::default();
        history.scan_all(&mut |h| {
            report.keys += 1;
            report.observations += h.observations.len() as u64;
            for o in &h.observations {
                for (t, v) in values.iter_mut() {
                    v.extend(o.outcome.get(*t));
                }
            }
            Ok(())
        })?;
        self.medians = values
            .into_iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(t, v)| (t, common::median(v)))
            .collect();
        Ok(report)
    }

    fn save(&self) -> moruna_cost::Result<Vec<u8>> {
        Ok(serde_json::to_vec(&ConstantState {
            model: self.id(),
            medians: self.medians.clone(),
        })
        .expect("serialises"))
    }

    fn load(&mut self, bytes: &[u8]) -> moruna_cost::Result<()> {
        let state: ConstantState = serde_json::from_slice(bytes).map_err(|e| CostError::State {
            model: self.id(),
            reason: e.to_string(),
        })?;
        if state.model != self.id() {
            return Err(CostError::WrongModel {
                expected: self.id(),
                found: state.model,
            });
        }
        self.medians = state.medians;
        Ok(())
    }
}

struct ConstantAdapter;

impl ModelAdapter for ConstantAdapter {
    fn name(&self) -> &str {
        "constant"
    }

    fn make(&self) -> Box<dyn CostModel> {
        Box::new(Constant::default())
    }
}

/// Run `units` units through the models in use, the way a run does: predict, measure, settle.
fn run(in_use: &mut InUse, store: &dyn ObservationStore, rng: &mut Rng, units: u64) {
    for n in 0..units {
        let k = key("std", (n % 3) as u8);
        let rows = rng.log_uniform(1e3, 1e6);
        let inputs = batch(rows);
        let prediction = in_use.predict(&k, &inputs);
        let mut outcome = Outcome::new(Origin::Run);
        outcome.set(Target::Duration, 2e-7 * rows * (0.1 * rng.normal()).exp());
        outcome.set(Target::PeakMemory, 128.0 * rows);
        in_use
            .settle(store, &k, &inputs, Some(&prediction), &outcome, n)
            .expect("settles");
    }
}

/// CM-T15 registry_by_name. CM-I8. Models register by name like kinds; a taken name, a name that
/// is not one, an unknown one and an adapter that makes a model of another name are refused.
#[test]
fn cm_t15_registry_by_name() {
    let mut models = CostModels::standard();
    models
        .register(Arc::new(ConstantAdapter))
        .expect("registers");
    assert_eq!(
        models.names().collect::<Vec<_>>(),
        vec!["constant", "loglinear"]
    );
    assert!(matches!(
        models.register(Arc::new(ConstantAdapter)),
        Err(CostError::Taken { .. })
    ));
    struct Named(&'static str);
    impl ModelAdapter for Named {
        fn name(&self) -> &str {
            self.0
        }
        fn make(&self) -> Box<dyn CostModel> {
            Box::new(Constant::default())
        }
    }
    assert!(matches!(
        models.register(Arc::new(Named("Not A Name"))),
        Err(CostError::BadName { .. })
    ));
    models.register(Arc::new(Named("liar"))).expect("registers");
    let err = models.make("liar").err().expect("refused");
    assert!(matches!(err, CostError::Mismatch { .. }), "{err}");
    let err = models.make("nope").err().expect("refused");
    assert_eq!(
        err.to_string(),
        "no cost model `nope` is registered; the registry has constant, liar, loglinear"
    );
}

/// CM-T16 the_swap. CM-I8. A second adapter is registered, chosen by a setting, fitted with
/// `fit` over a store the default model filled, and used through the same calls; opened again
/// it loads its saved state instead of fitting.
#[test]
fn cm_t16_the_swap() {
    let root = scratch("swap");
    let store = LocalStore::open(&root).expect("opens");
    let mut rng = Rng::new(16);
    let models = CostModels::standard();
    let mut first = models
        .open(&CostSetting::default(), "loglinear", &store)
        .expect("opens");
    assert_eq!(first.deciding().id(), LogLinear::model_id());
    assert!(matches!(
        first.readiness(),
        [Readiness::Fitted(
            _,
            FitReport {
                observations: 0,
                ..
            }
        )]
    ));
    run(&mut first, &store, &mut rng, 300);
    first.save(&store).expect("saves");

    // The swap: register, choose by setting. Nothing else changes.
    let mut models = models;
    models
        .register(Arc::new(ConstantAdapter))
        .expect("registers");
    let setting: CostSetting = serde_json::from_str(r#"{"model": "constant"}"#).expect("setting");
    let mut swapped = models.open(&setting, "loglinear", &store).expect("opens");
    assert_eq!(swapped.deciding().id(), ModelId::new("constant", 1));
    match swapped.readiness() {
        [Readiness::Fitted(id, report)] => {
            assert_eq!(id, &ModelId::new("constant", 1));
            assert_eq!((report.keys, report.observations), (3, 300));
        }
        other => panic!("{other:?}"),
    }
    let k = key("std", 0);
    let c = swapped.deciding().predict(&k, &batch(1e4));
    assert!(c.duration.p50 > 0.0 && c.peak_memory.p50 > 0.0);
    assert_eq!(c.duration.unit, Unit::Seconds);
    run(&mut swapped, &store, &mut rng, 10);
    swapped.save(&store).expect("saves");
    let again = models.open(&setting, "loglinear", &store).expect("opens");
    assert_eq!(
        again.readiness(),
        &[Readiness::Loaded(ModelId::new("constant", 1))]
    );
    assert_eq!(again.deciding().predict(&k, &batch(1e4)), c);

    // The default model, opened again, loads what it saved; a corrupt state is refitted and
    // says why.
    let loaded = models
        .open(&CostSetting::default(), "loglinear", &store)
        .expect("opens");
    assert_eq!(
        loaded.readiness(),
        &[Readiness::Loaded(LogLinear::model_id())]
    );
    store
        .save_state(&LogLinear::model_id(), b"{\"model\": 7}")
        .expect("writes");
    let refitted = models
        .open(&CostSetting::default(), "loglinear", &store)
        .expect("opens");
    match refitted.readiness() {
        [Readiness::Refitted(_, report, why)] => {
            assert_eq!(report.observations, 310);
            assert!(why.contains("unreadable"), "{why}");
        }
        other => panic!("{other:?}"),
    }
    let unknown = CostSetting {
        model: Some("nope".into()),
        shadow: None,
    };
    assert!(models.open(&unknown, "loglinear", &store).is_err());
    std::fs::remove_dir_all(&root).ok();
}

/// CM-T17 shadow_mode. CM-I9. Two models predict, the configured one decides, both predictions
/// are kept with the outcome, and the comparison judges them on the same units.
#[test]
fn cm_t17_shadow_mode() {
    let root = scratch("shadow");
    let store = LocalStore::open(&root).expect("opens");
    let mut models = CostModels::standard();
    models
        .register(Arc::new(ConstantAdapter))
        .expect("registers");
    let mut rng = Rng::new(17);
    let mut alone = models
        .open(&CostSetting::default(), "loglinear", &store)
        .expect("opens");
    run(&mut alone, &store, &mut rng, 100);
    alone.save(&store).expect("saves");
    let setting = CostSetting {
        model: None,
        shadow: Some("constant".into()),
    };
    let mut in_use = models.open(&setting, "loglinear", &store).expect("opens");
    assert!(matches!(
        in_use.readiness(),
        [
            Readiness::Loaded(_),
            Readiness::Fitted(
                _,
                FitReport {
                    observations: 100,
                    ..
                }
            )
        ]
    ));
    assert_eq!(in_use.deciding().id(), LogLinear::model_id());
    assert_eq!(
        in_use.shadow().map(|m| m.id()),
        Some(ModelId::new("constant", 1))
    );
    run(&mut in_use, &store, &mut rng, 200);

    let k = key("std", 0);
    let p = in_use.predict(&k, &batch(1e4));
    assert_eq!(p.deciding.cost, in_use.deciding().predict(&k, &batch(1e4)));
    assert_eq!(
        p.shadow.as_ref().map(|s| &s.model),
        Some(&ModelId::new("constant", 1))
    );

    let h = store.scan(&k).expect("scans");
    let shadowed: Vec<_> = h
        .observations
        .iter()
        .filter(|o| o.predictions.len() == 2)
        .collect();
    assert!(!shadowed.is_empty());
    for o in shadowed {
        assert_eq!(o.model, Some(LogLinear::model_id()));
        let ids: Vec<&ModelId> = o.predictions.iter().map(|p| &p.model).collect();
        assert_eq!(
            ids,
            vec![&LogLinear::model_id(), &ModelId::new("constant", 1)]
        );
    }
    let comparison = moruna_cost::compare(&store).expect("compares");
    let score = |id: &ModelId| {
        comparison.models[id]
            .iter()
            .find(|s| s.target == Target::Duration)
            .cloned()
            .expect("scored")
    };
    let loglinear = score(&LogLinear::model_id());
    let constant = score(&ModelId::new("constant", 1));
    assert_eq!((loglinear.n, constant.n), (300, 200));
    println!(
        "cm_t17 median log error: loglinear {:.3} constant {:.3}; p95 coverage {:.2} {:.2}",
        loglinear.median_log_error,
        constant.median_log_error,
        loglinear.p95_coverage,
        constant.p95_coverage
    );
    assert!(loglinear.median_log_error < constant.median_log_error);

    // A settle with no prediction (warm-up, say) keeps the outcome alone.
    let mut plain = InUse::new(Box::new(Constant::default()), None);
    plain
        .settle(
            &store,
            &k,
            &batch(1e3),
            None,
            &Outcome::new(Origin::WarmUp),
            1,
        )
        .expect("settles");
    plain.save(&store).expect("saves");
    assert!(plain.shadow().is_none() && plain.readiness().is_empty());
    let last = store
        .scan(&k)
        .expect("scans")
        .observations
        .pop()
        .expect("kept");
    assert!(last.predictions.is_empty());
    std::fs::remove_dir_all(&root).ok();
}
