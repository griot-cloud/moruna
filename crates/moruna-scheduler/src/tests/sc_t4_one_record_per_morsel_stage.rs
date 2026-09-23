//! SC-T4: every morsel leaves exactly one trace record per stage, plus one probe record per
//! stage carrying the probing worker's id. SC-I4, G-I4.

use std::collections::HashSet;

use moruna_kernel::{CancelToken, Outcome, Prober};
use moruna_testkit::{FakeSource, FakeTrace};

use super::common::RigBuilder;

/// 2,000 morsels rather than the 10,000 of section k: the property is the same and the fake
/// source allocates one buffer per morsel, so the larger figure buys nothing but minutes.
const MORSELS: u64 = 2_000;

#[test]
fn sc_t4_one_record_per_morsel_stage() {
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 4;
            cfg.workers_active = 4;
            cfg.initial_morsel_target = 8;
        })
        .source(FakeSource::new().splits(1, MORSELS, MORSELS * 8))
        .trace_capacity(1 << 20)
        .stages(3)
        .go();

    // One probe per stage, before the run; each is a record of its own.
    for stage in 1..=3u16 {
        if let Err(e) = rig.scheduler.probe(stage, 8) {
            panic!("probe {stage}: {e}");
        }
    }
    match rig.scheduler.run(CancelToken::new()) {
        Ok(crate::RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }

    let records = rig.trace.records();
    let probes: Vec<_> = records
        .iter()
        .filter(|r| r.outcome == Outcome::Probe)
        .collect();
    assert_eq!(probes.len(), 3, "one probe record per stage");
    let probe_stages: HashSet<u16> = probes.iter().map(|r| r.stage).collect();
    assert_eq!(probe_stages, HashSet::from([1, 2, 3]));
    for probe in &probes {
        assert_eq!(
            probe.worker, 0,
            "the probing worker is the one left running"
        );
    }

    // The probe's morsel travels on as a normal morsel, so its records for the stages it was
    // probed at are the probe records; every other morsel leaves a normal record per stage.
    let keys: HashSet<(u16, u64)> = records.iter().map(|r| (r.stage, r.seq)).collect();
    assert_eq!(
        keys.len(),
        records.len(),
        "no (stage, seq) pair was recorded twice"
    );
    assert_eq!(
        records.len(),
        3 * MORSELS as usize,
        "exactly one record per morsel per stage"
    );
    for stage in 1..=3u16 {
        let count = records.iter().filter(|r| r.stage == stage).count();
        assert_eq!(
            count, MORSELS as usize,
            "stage {stage} recorded {count} morsels, not {MORSELS}"
        );
    }
    let _ = FakeTrace::new();
}
