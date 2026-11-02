//! TR-T16 policy_and_learning_in_the_report (adaptive-runtime 9.7; 10 f.22): the report's policy
//! section carries what the policy counted of its own decisions, where its predictions came
//! from, and what the run's cost model learned, every spell of drift with its stage, and says
//! none when no model was set.

use moruna_kernel::{DriftStat, LearningStats, PolicyCounters, PolicyStats};
use moruna_trace::PolicyReport;

#[test]
fn tr_t16_policy_and_learning_in_the_report() {
    let stats = PolicyStats {
        name: "lookahead".into(),
        decisions: 10,
        counters: PolicyCounters {
            shallower: 2,
            simulations: 30,
            simulation_errors: 1,
            simulation_error: Some("task `task 2` is infeasible".into()),
            resizes: 1,
        },
        predicted_by_model: 9,
        predicted_by_ratio: 0,
        learning: Some(LearningStats {
            model: "loglinear@1".into(),
            shadow: Some("constant@1".into()),
            observed: 9,
            written: 8,
            unwritten: 1,
            store_error: Some("the disk is full".into()),
            refused: 0,
            refusal: None,
            drifts: vec![DriftStat {
                stage: 2,
                target: "duration".into(),
                at_unit: 5,
                units: 4,
                settled: true,
            }],
        }),
        overhead: moruna_kernel::Overhead {
            units: 10,
            unit_ns: 1_000_000_000,
            gather_ns: 2_000_000,
            decide_ns: 1_000_000,
            learn_ns: 1_000_000,
            store_ns: 500_000_000,
            ..moruna_kernel::Overhead::default()
        },
        ..PolicyStats::default()
    };
    let report = PolicyReport::of(&stats);
    assert_eq!(
        (
            report.shallower,
            report.simulations,
            report.simulation_errors
        ),
        (2, 30, 1)
    );
    assert!(
        report
            .simulation_error
            .as_deref()
            .is_some_and(|e| e.contains("infeasible"))
    );
    assert_eq!(
        (report.predicted_by_model, report.predicted_by_ratio),
        (9, 0)
    );
    let learning = report.learning.clone().expect("learning");
    assert_eq!(learning.model, "loglinear@1");
    assert_eq!(learning.shadow.as_deref(), Some("constant@1"));
    assert_eq!(
        (learning.observed, learning.written, learning.unwritten),
        (9, 8, 1)
    );
    assert_eq!(learning.drifts.len(), 1);
    let drift = &learning.drifts[0];
    assert_eq!(
        (drift.stage, drift.at_unit, drift.units, drift.settled),
        (2, 5, 4, true)
    );
    assert!(
        (report.overhead.share - 0.004).abs() < 1e-12,
        "the store is not in the share"
    );
    assert_eq!(report.overhead.store_s, 0.5);
    let json = serde_json::to_value(&report).expect("serialises");
    assert_eq!(json["learning"]["drifts"][0]["target"], "duration");
    assert_eq!(json["learning"]["store_error"], "the disk is full");

    let plain = PolicyReport::of(&PolicyStats::default());
    assert!(plain.learning.is_none());
    assert_eq!(
        serde_json::to_value(&plain).expect("serialises")["learning"],
        serde_json::Value::Null
    );
}
