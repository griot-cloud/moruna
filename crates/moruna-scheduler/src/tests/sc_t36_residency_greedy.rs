//! SC-T36 residency_greedy (f.23; adaptive-runtime 9.4 step 2, G8; RA 9 (C)).
//!
//! With memory free the critical path leads; with memory full the unit that frees the most memory
//! soonest leads, an output a sink takes counting as freed; a head that has waited its ageing
//! bound goes first, the longest-waiting of several first; a drifting key's output is weighed at
//! its 95th percentile; ties go to the later stage; and every ready unit is in the order.

use moruna_kernel::{
    Action, BufferId, Head, Machine, PredictedBy, Prediction, Ready, SchedulerState,
    SchedulingPolicy, Spread, Topology,
};
use moruna_testkit::FakePlacement;

use crate::policies::{AGE_FLOOR_NS, AGE_LIMIT, RESIDENCY_GREEDY, ResidencyGreedy};

/// One ready unit of `stage`: a head of `bytes` (its last take when `last`), predicted to make
/// `out` bytes in `ns` nanoseconds, `critical_ns` of work on its path, waiting `waited_ns`.
pub(crate) fn unit(stage: u16, bytes: u64, last: bool, out: u64, ns: u64) -> Ready {
    Ready {
        stage,
        port: 0,
        queue: stage - 1,
        head: Head {
            id: BufferId(u64::from(stage)),
            seq: 0,
            bytes,
            last,
            arrived: 0,
        },
        queue_bytes: bytes,
        out_bytes: 0,
        predicted: Prediction {
            out_bytes: Spread::exact(out),
            duration_ns: Spread::exact(ns),
            by: PredictedBy::Model,
            drifting: false,
            observations: 10,
        },
        critical_ns: 0,
        waited_ns: 0,
    }
}

fn decide(topology: &Topology, ready: &[Ready], held: u64, budget: u64) -> Vec<u16> {
    let fake = FakePlacement::new();
    let state = SchedulerState {
        worker: 0,
        ready,
        topology,
        residency: &fake,
        budget_ns: 1_000_000,
        machine: Machine {
            workers: 2,
            budget_bytes: budget,
            held_bytes: held,
            disk_bytes_per_second: 1 << 30,
            claim_cap_bytes: budget,
        },
        stages: &[],
    };
    let actions = super::common::decided(&mut ResidencyGreedy::default(), &state);
    assert_eq!(
        actions.len(),
        ready.len(),
        "every ready unit is in the order"
    );
    actions
        .into_iter()
        .map(|a| match a {
            Action::Run { stage, .. } => stage,
            other => panic!("a run, not {other:?}"),
        })
        .collect()
}

#[test]
fn sc_t36_residency_greedy_weighs_the_path_memory_and_age() {
    assert_eq!(ResidencyGreedy::default().id().name, RESIDENCY_GREEDY);
    let chain = Topology::chain(3);
    // Memory free: the critical path leads, whatever memory each unit frees.
    let mut a = unit(1, 100, true, 300, 1_000);
    a.critical_ns = 9_000;
    let mut b = unit(2, 100, true, 0, 1_000);
    b.critical_ns = 1_000;
    assert_eq!(decide(&chain, &[b, a], 0, 10_000), vec![1, 2]);
    // Memory full: the unit freeing the most memory soonest leads.
    assert_eq!(decide(&chain, &[a, b], 10_000, 10_000), vec![2, 1]);
    // An empty budget reads as full.
    assert_eq!(decide(&chain, &[a, b], 0, 0), vec![2, 1]);

    // What a sink takes counts as freed: stage 3 of the chain gives to the sink, so its output
    // weighs nothing; stage 2's gives to stage 3 and is kept.
    let to_kernel = unit(2, 100, true, 100, 1_000);
    let to_sink = unit(3, 100, true, 100, 1_000);
    assert_eq!(
        decide(&chain, &[to_kernel, to_sink], 10_000, 10_000),
        vec![3, 2]
    );
    // Sooner is better: the same bytes freed in a tenth of the time lead.
    let slow = unit(3, 100, true, 0, 10_000);
    let quick = unit(2, 100, true, 0, 1_000);
    assert_eq!(decide(&chain, &[slow, quick], 10_000, 10_000), vec![2, 3]);
    // A take that is not its batch's last frees nothing.
    let shared = unit(3, 100, false, 0, 1_000);
    let last = unit(2, 100, true, 0, 1_000);
    assert_eq!(decide(&chain, &[shared, last], 10_000, 10_000), vec![2, 3]);

    // A drifting key's output is weighed at its 95th percentile. On a chain of four, stages 2
    // and 3 both give to a kernel.
    let to_kernels = Topology::chain(4);
    let mut steady = unit(3, 100, true, 10, 1_000);
    steady.predicted.out_bytes = Spread::new(10, 1_000);
    let calm = unit(2, 100, true, 50, 1_000);
    assert_eq!(
        decide(&to_kernels, &[calm, steady], 10_000, 10_000),
        vec![3, 2]
    );
    let mut drifting = steady;
    drifting.predicted.drifting = true;
    assert_eq!(
        decide(&to_kernels, &[calm, drifting], 10_000, 10_000),
        vec![2, 3],
        "drifting, its 95th percentile weighs"
    );

    // Ageing: a head that waited its bound goes first, the longest wait first.
    let mut old = unit(1, 100, false, 1_000, 2_000_000);
    old.waited_ns = AGE_LIMIT * 2_000_000;
    let mut older = unit(2, 100, false, 1_000, 2_000_000);
    older.waited_ns = AGE_LIMIT * 2_000_000 + 1;
    let fresh = unit(3, 100, true, 0, 1_000);
    assert_eq!(
        decide(&chain, &[fresh, old, older], 10_000, 10_000),
        vec![2, 1, 3]
    );
    // A stage with no measured time ages as one of the floor.
    let mut unmeasured = unit(1, 100, false, 1_000, 0);
    unmeasured.waited_ns = AGE_LIMIT * AGE_FLOOR_NS - 1;
    assert_eq!(
        decide(&chain, &[fresh, unmeasured], 10_000, 10_000),
        vec![3, 1]
    );
    unmeasured.waited_ns += 1;
    assert_eq!(
        decide(&chain, &[fresh, unmeasured], 10_000, 10_000),
        vec![1, 3]
    );

    // Ties go to the later stage.
    let x = unit(1, 100, true, 100, 1_000);
    let y = unit(2, 100, true, 100, 1_000);
    assert_eq!(decide(&to_kernels, &[x, y], 0, 10_000), vec![2, 1]);
    assert_eq!(decide(&to_kernels, &[x, y], 10_000, 10_000), vec![2, 1]);
    assert!(decide(&chain, &[], 0, 1).is_empty());
}
