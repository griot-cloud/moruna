//! SDD 24 k: the default model (CM-T3 to CM-T11).

mod common;

use common::{Rng, batch, key, median, took};
use moruna_cost::{
    Basis, Column, CostError, CostModel, Defaults, Estimate, Inputs, LogLinear, ModelId, Origin,
    Outcome, Target, UnitCost,
};

fn model() -> LogLinear {
    LogLinear::new(Defaults::standard())
}

/// One synthetic family: a known cost function of rows.
struct Family {
    name: &'static str,
    truth: fn(f64) -> f64,
}

const FAMILIES: [Family; 5] = [
    Family {
        name: "linear",
        truth: |r| 2e-7 * r,
    },
    Family {
        name: "n log n",
        truth: |r| 1e-8 * r * r.log2(),
    },
    Family {
        name: "power 1.5",
        truth: |r| 1e-9 * r.powf(1.5),
    },
    Family {
        name: "power 0.7",
        truth: |r| 1e-5 * r.powf(0.7),
    },
    Family {
        name: "constant plus linear",
        truth: |r| 0.01 + 1e-7 * r,
    },
];

/// Multiplicative noise of 10% (log-normal, sigma 0.1).
const NOISE: f64 = 0.1;
const TRAIN: usize = 400;
const HELD_OUT: usize = 1000;
/// The p50 error bound: the median of |p50 / measured - 1| over held-out units. Noise alone
/// gives 0.674 * 10% = 6.7%.
const P50_ERROR_BOUND: f64 = 0.12;

/// CM-T4 accuracy_on_synthetic_truth. CM-I2. Per family: the held-out median p50 error is
/// under the bound and the p95 covers between 90% and 99% of held-out units.
#[test]
fn cm_t4_accuracy_on_synthetic_truth() {
    for (f, family) in FAMILIES.iter().enumerate() {
        let mut rng = Rng::new(0xC057 + f as u64);
        let mut m = model();
        let k = key("std", 1);
        for _ in 0..TRAIN {
            let rows = rng.log_uniform(1e3, 1e6);
            let y = (family.truth)(rows) * (NOISE * rng.normal()).exp();
            m.observe(&k, &batch(rows), &took(Origin::Run, y));
        }
        let mut errors = Vec::new();
        let mut covered = 0;
        for _ in 0..HELD_OUT {
            let rows = rng.log_uniform(1e3, 1e6);
            let y = (family.truth)(rows) * (NOISE * rng.normal()).exp();
            let e = m.predict(&k, &batch(rows)).duration;
            assert_eq!(e.basis, Basis::Measured);
            assert!(
                !e.extrapolated,
                "{}: inside the measured range",
                family.name
            );
            errors.push((e.p50 / y - 1.0).abs());
            if y <= e.p95 {
                covered += 1;
            }
        }
        let error = median(errors);
        let coverage = covered as f64 / HELD_OUT as f64;
        println!(
            "cm_t4 {:<22} median p50 error {:>5.1}%  p95 coverage {:>5.1}%",
            family.name,
            error * 100.0,
            coverage * 100.0
        );
        assert!(
            error < P50_ERROR_BOUND,
            "{}: median p50 error {error}",
            family.name
        );
        assert!(
            (0.90..=0.99).contains(&coverage),
            "{}: p95 coverage {coverage}",
            family.name
        );
    }
}

/// CM-T5 extrapolation_is_marked_and_wider. CM-I5. Fitted on 1,000 to 100,000 rows, a
/// prediction at ten million is marked and its spread is wider than inside.
#[test]
fn cm_t5_extrapolation_is_marked_and_wider() {
    let mut rng = Rng::new(5);
    let mut m = model();
    let k = key("std", 1);
    for _ in 0..TRAIN {
        let rows = rng.log_uniform(1e3, 1e5);
        let y = 3e-7 * rows * (NOISE * rng.normal()).exp();
        m.observe(&k, &batch(rows), &took(Origin::Run, y));
    }
    let spread = |e: &Estimate| (e.p95 / e.p50).ln();
    let inside = m.predict(&k, &batch(1e4)).duration;
    let edge = m.predict(&k, &batch(1e5)).duration;
    let outside = m.predict(&k, &batch(1e7)).duration;
    assert!(!inside.extrapolated && !edge.extrapolated);
    assert!(outside.extrapolated);
    assert!(
        spread(&outside) > 1.5 * spread(&inside),
        "inside {} outside {}",
        spread(&inside),
        spread(&outside)
    );
    // A feature that never varied is extrapolation too when it moves.
    let mut wider = batch(1e4);
    wider.data.columns_read = Some(16);
    let trained_columns = m.predict(&k, &batch(1e4)).duration;
    m.observe(&k, &batch(1e4), &took(Origin::Run, 3e-3));
    let wide = m.predict(&k, &wider).duration;
    assert!(!trained_columns.extrapolated);
    assert!(
        !wide.extrapolated,
        "a feature never seen is missing history, not range"
    );
    let mut eight = batch(1e4);
    eight.data.columns_read = Some(8);
    m.observe(&k, &eight, &took(Origin::Run, 3e-3));
    assert!(m.predict(&k, &wider).duration.extrapolated);
}

/// CM-T6 forgetting_follows_a_change. CM-I11. A function three times slower than its history
/// is followed: the median error over a probe set is back under 15% within 150 units.
#[test]
fn cm_t6_forgetting_follows_a_change() {
    const WITHIN: usize = 150;
    let mut rng = Rng::new(6);
    let mut m = model();
    let k = key("std", 1);
    for _ in 0..300 {
        let rows = rng.log_uniform(1e3, 1e6);
        m.observe(
            &k,
            &batch(rows),
            &took(Origin::Run, 1e-7 * rows * (NOISE * rng.normal()).exp()),
        );
    }
    let probes: Vec<f64> = (0..50).map(|_| rng.log_uniform(1e3, 1e6)).collect();
    let error = |m: &LogLinear| {
        median(
            probes
                .iter()
                .map(|&r| (m.predict(&k, &batch(r)).duration.p50 / (3e-7 * r) - 1.0).abs())
                .collect(),
        )
    };
    assert!(
        error(&m) > 0.5,
        "before the change it predicts the old cost"
    );
    let mut followed = None;
    for n in 1..=WITHIN {
        let rows = rng.log_uniform(1e3, 1e6);
        m.observe(
            &k,
            &batch(rows),
            &took(Origin::Run, 3e-7 * rows * (NOISE * rng.normal()).exp()),
        );
        if followed.is_none() && error(&m) < 0.15 {
            followed = Some(n);
        }
    }
    println!(
        "cm_t6 followed after {followed:?} units (lambda {})",
        moruna_cost::loglinear::FORGETTING
    );
    assert!(followed.is_some(), "not followed within {WITHIN} units");
    assert!(error(&m) < 0.1, "settled: {}", error(&m));
}

/// CM-T7 fallbacks_answer_in_order. CM-I4. The function's history, then the kind's prior, then
/// the declared default, per target, with `basis` saying which.
#[test]
fn cm_t7_fallbacks_answer_in_order() {
    let mut m = model();
    let a = key("std", 1);
    let b = key("std", 2);
    let other = key("package", 3);
    let inputs = batch(1e4);
    let fresh = m.predict(&a, &inputs);
    for target in Target::ALL {
        assert_eq!(fresh.get(target).basis, Basis::Default, "{target}");
        assert_eq!(fresh.get(target).observations, 0);
    }
    // Warm-up only: this function's history answers, as warm-up.
    let mut warm = took(Origin::WarmUp, 2e-3);
    warm.set(Target::RowsOut, 5_000.0);
    for rows in [1e3, 1e4, 1e5] {
        m.observe(&a, &batch(rows), &warm);
    }
    let after = m.predict(&a, &inputs);
    assert_eq!(after.duration.basis, Basis::WarmUp);
    assert_eq!(after.duration.observations, 3);
    assert_eq!(after.rows_out.basis, Basis::WarmUp);
    // A target the history never measured falls through to the default.
    assert_eq!(after.spill_bytes.basis, Basis::Default);
    // Another function of the kind: the kind's prior.
    let prior = m.predict(&b, &inputs);
    assert_eq!(prior.duration.basis, Basis::Prior);
    assert_eq!(prior.rows_out.basis, Basis::Prior);
    assert_eq!(prior.spill_bytes.basis, Basis::Default);
    // Another kind: the declared default.
    assert_eq!(m.predict(&other, &inputs).duration.basis, Basis::Default);
    // One unit from a real run makes the history measured.
    m.observe(&a, &inputs, &took(Origin::Run, 2e-3));
    assert_eq!(m.predict(&a, &inputs).duration.basis, Basis::Measured);
    // The declared default reads the kernel's declared figures.
    let mut declared = inputs.clone();
    declared.kernel.per_batch_seconds = Some(0.0);
    declared.kernel.per_row_seconds = Some(1e-6);
    let d = m.predict(&other, &declared).duration;
    assert_eq!(d.basis, Basis::Default);
    assert!((d.p50 - 1e-2).abs() < 1e-12, "{}", d.p50);
}

/// CM-T8 missing_inputs_widen. CM-I5. The same unit with its rows unknown is predicted at the
/// history's typical rows with a wider spread.
#[test]
fn cm_t8_missing_inputs_widen() {
    let mut rng = Rng::new(8);
    let mut m = model();
    let k = key("std", 1);
    for _ in 0..TRAIN {
        let rows = rng.log_uniform(1e3, 1e6);
        m.observe(
            &k,
            &batch(rows),
            &took(Origin::Run, 1e-7 * rows * (NOISE * rng.normal()).exp()),
        );
    }
    let known = m.predict(&k, &batch(3e4)).duration;
    let unknown = m.predict(&k, &Inputs::default()).duration;
    assert!(
        unknown.p95 / unknown.p50 > 2.0 * (known.p95 / known.p50),
        "known {:?} unknown {:?}",
        known,
        unknown
    );
    assert!(!unknown.extrapolated);
}

/// CM-T9 ratios_are_ratios. Selectivity and output bytes per row are kept as median and p95,
/// not regressed on size; a selectivity measured in the run answers as measured.
#[test]
fn cm_t9_ratios_are_ratios() {
    let mut rng = Rng::new(9);
    let mut m = model();
    let k = key("std", 1);
    for _ in 0..200 {
        let rows = rng.log_uniform(1e3, 1e6);
        let selectivity = 0.1 + 0.2 * rng.unit();
        let mut o = took(Origin::Run, 1e-7 * rows);
        let out = (rows * selectivity).round();
        o.set(Target::RowsOut, out);
        o.set(Target::BytesOut, out * 24.0);
        m.observe(&k, &batch(rows), &o);
    }
    for rows in [1e3, 1e5, 1e7] {
        let c = m.predict(&k, &batch(rows));
        let sel = c.rows_out.p50 / rows;
        let sel95 = c.rows_out.p95 / rows;
        assert!(
            (sel - 0.2).abs() < 0.03,
            "median selectivity {sel} at {rows}"
        );
        assert!(
            (sel95 - 0.29).abs() < 0.02,
            "p95 selectivity {sel95} at {rows}"
        );
        assert!((c.bytes_out.p50 / c.rows_out.p50 - 24.0).abs() < 1e-6);
        assert_eq!(c.rows_out.basis, Basis::Measured);
    }
    let mut measured = batch(1e4);
    measured.data.selectivity = Some(0.5);
    let c = m.predict(&k, &measured);
    assert_eq!(c.rows_out.p50, 5_000.0);
    assert!(c.rows_out.p95 > c.rows_out.p50);
    assert_eq!(c.rows_out.basis, Basis::Measured);
    // Rows unknown: the history's typical rows times the ratio.
    let c = m.predict(&k, &Inputs::default());
    assert!(c.rows_out.p50 > 0.0 && c.rows_out.p95 > c.rows_out.p50);
}

/// CM-T3 estimates_are_distributions. CM-I2. At every basis and for odd inputs, every estimate
/// is finite, its p95 is at least its p50, which is not negative, and its unit is its target's.
#[test]
fn cm_t3_estimates_are_distributions() {
    let mut rng = Rng::new(3);
    let mut m = model();
    let check = |c: &UnitCost| {
        for target in Target::ALL {
            let e = c.get(target);
            assert!(e.p50.is_finite() && e.p95.is_finite(), "{target}: {e:?}");
            assert!(e.p50 >= 0.0 && e.p95 >= e.p50, "{target}: {e:?}");
            assert_eq!(e.unit, target.unit());
        }
    };
    let odd = {
        let mut i = Inputs::batch(0, 0);
        i.resources.workers = Some(0);
        i.data.columns = vec![Column {
            name: "k".into(),
            distinct: Some(0),
            key: true,
            ..Column::default()
        }];
        i
    };
    for round in 0..300 {
        let k = key(
            if round % 2 == 0 { "std" } else { "package" },
            (round % 5) as u8,
        );
        let rows = rng.log_uniform(1.0, 1e9);
        let mut o = Outcome::new(if round % 3 == 0 {
            Origin::WarmUp
        } else {
            Origin::Run
        });
        for target in Target::ALL {
            if rng.unit() < 0.8 {
                let v = if rng.unit() < 0.2 {
                    0.0
                } else {
                    rng.log_uniform(1e-6, 1e12)
                };
                o.set(target, v);
            }
        }
        check(&m.predict(&k, &batch(rows)));
        check(&m.predict(&k, &Inputs::default()));
        check(&m.predict(&k, &odd));
        m.observe(&k, &batch(rows), &o);
    }
    check(&m.predict(&key("std", 99), &batch(1e30)));
}

/// CM-T10 save_and_load_round_trip. CM-I3. A model loaded from another's saved state predicts
/// what it predicted, to the bit.
#[test]
fn cm_t10_save_and_load_round_trip() {
    let mut rng = Rng::new(10);
    let mut m = model();
    for n in 0..4u8 {
        let k = key(if n < 2 { "std" } else { "package" }, n);
        for _ in 0..100 {
            let rows = rng.log_uniform(1e3, 1e6);
            let mut o = took(
                Origin::Run,
                0.01 + 1e-7 * rows * (NOISE * rng.normal()).exp(),
            );
            o.set(Target::PeakMemory, rows * 128.0);
            o.set(Target::RowsOut, rows * 0.5);
            m.observe(&k, &batch(rows), &o);
        }
    }
    let bytes = m.save().expect("saves");
    let mut back = model();
    back.load(&bytes).expect("loads");
    for n in 0..6u8 {
        let k = key(if n % 2 == 0 { "std" } else { "package" }, n);
        for rows in [10.0, 1e4, 1e8] {
            assert_eq!(
                m.predict(&k, &batch(rows)),
                back.predict(&k, &batch(rows)),
                "{k} at {rows}"
            );
        }
    }
    assert_eq!(
        back.save().expect("saves"),
        bytes,
        "saving again is the same bytes"
    );
}

/// CM-T11 a_mismatched_id_is_refused. CM-I3. State saved by another model, or another version
/// of this one, is refused naming both; what is not state is refused as such; the model is
/// unchanged by a refusal.
#[test]
fn cm_t11_a_mismatched_id_is_refused() {
    let mut m = model();
    m.observe(&key("std", 1), &batch(1e4), &took(Origin::Run, 1.0));
    let before = m.predict(&key("std", 1), &batch(1e4));
    let other =
        serde_json::json!({"model": {"name": "loglinear", "version": 2}, "keys": [], "kinds": []});
    let err = m
        .load(other.to_string().as_bytes())
        .expect_err("another version");
    match &err {
        CostError::WrongModel { expected, found } => {
            assert_eq!(expected, &ModelId::new("loglinear", 1));
            assert_eq!(found, &ModelId::new("loglinear", 2));
        }
        e => panic!("{e}"),
    }
    assert!(err.to_string().contains("`loglinear@2`"));
    let constant = serde_json::json!({"model": {"name": "constant", "version": 1}});
    assert!(matches!(
        m.load(constant.to_string().as_bytes()),
        Err(CostError::WrongModel { .. })
    ));
    assert!(matches!(m.load(b"not json"), Err(CostError::State { .. })));
    let truncated = serde_json::json!({"model": {"name": "loglinear", "version": 1}, "keys": 3});
    assert!(matches!(
        m.load(truncated.to_string().as_bytes()),
        Err(CostError::State { .. })
    ));
    assert_eq!(m.predict(&key("std", 1), &batch(1e4)), before);
}

/// CM-T19 overhead_is_the_fixed_term. f.2. A function with a fixed cost per unit keeps it as
/// the overhead, near its true value; a pure power law keeps none.
#[test]
fn cm_t19_overhead_is_the_fixed_term() {
    let mut rng = Rng::new(19);
    let mut m = model();
    let fixed = key("std", 1);
    let pure = key("std", 2);
    for _ in 0..TRAIN {
        let rows = rng.log_uniform(1e3, 1e6);
        let noise = (NOISE * rng.normal()).exp();
        m.observe(
            &fixed,
            &batch(rows),
            &took(Origin::Run, (0.01 + 1e-7 * rows) * noise),
        );
        m.observe(
            &pure,
            &batch(rows),
            &took(Origin::Run, 1e-9 * rows.powf(1.5) * noise),
        );
    }
    let c = m.overhead(&fixed, Target::Duration).expect("fitted");
    assert!((0.005..0.0125).contains(&c), "overhead {c}");
    assert_eq!(m.overhead(&pure, Target::Duration), Some(0.0));
    assert_eq!(m.overhead(&key("std", 3), Target::Duration), None);
    assert_eq!(
        m.overhead(&fixed, Target::RowsOut),
        None,
        "a ratio has none"
    );
}
