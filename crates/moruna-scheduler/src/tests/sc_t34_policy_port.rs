//! SC-T34 the policy port (f.3, f.20; contracts d.18; adaptive-runtime 9.3, 9.4, 12.5).
//!
//! The policy is a setting made from a registry, and an unknown one is refused by name; the
//! `admission` and `fifo` adapters order the ready units as their rules say; a decision over its
//! budget hands the next decision to the baseline and the run counts both; a push the catalog
//! cannot admit is written straight to disk, and the run completes with every batch delivered.

use std::sync::Arc;
use std::time::Duration;

use moruna_kernel::{
    Action, BufferId, CancelToken, Head, Machine, PolicyAdapter, PolicyId, Prediction, Ready,
    SchedulerState, SchedulingPolicies, SchedulingPolicy, Spread, StatsSource, Topology,
};
use moruna_testkit::{FakeKernel, FakePlacement, FakeSource};

use super::common::RigBuilder;
use crate::policies::{ADMISSION, Admission, FIFO, Fifo, LOOKAHEAD, RESIDENCY_GREEDY, standard};

fn ready(stage: u16, port: u16, out_bytes: u64, queue_bytes: u64, arrived: u64) -> Ready {
    Ready {
        stage,
        port,
        queue: stage - 1 + port,
        head: Head {
            id: BufferId(arrived),
            seq: arrived,
            bytes: 8,
            last: true,
            arrived,
        },
        queue_bytes,
        out_bytes,
        predicted: Prediction {
            out_bytes: Spread::exact(8),
            ..Prediction::default()
        },
        critical_ns: 0,
        waited_ns: 0,
    }
}

fn order(policy: &mut dyn SchedulingPolicy, units: &[Ready]) -> Vec<(u16, u16)> {
    let fake = FakePlacement::new();
    let topology = Topology::chain(3);
    let state = SchedulerState {
        worker: 0,
        ready: units,
        topology: &topology,
        residency: &fake,
        budget_ns: 1_000,
        machine: Machine::default(),
        stages: &[],
    };
    super::common::decided(policy, &state)
        .into_iter()
        .map(|action| match action {
            Action::Run { stage, port } => (stage, port),
            other => panic!("a baseline only runs units, got {other:?}"),
        })
        .collect()
}

#[test]
fn sc_t34_the_adapters_order_as_their_rules_say() {
    let units = [
        ready(1, 0, 64, 10, 5),
        ready(2, 0, 16, 10, 9),
        ready(3, 0, 16, 10, 2),
        ready(2, 1, 16, 40, 1),
    ];
    // admission: fewest output bytes, ties to the later stage, then the fullest port.
    assert_eq!(
        order(&mut Admission::default(), &units),
        vec![(3, 0), (2, 1), (2, 0), (1, 0)]
    );
    // fifo: the batch that arrived first.
    assert_eq!(
        order(&mut Fifo::default(), &units),
        vec![(2, 1), (3, 0), (1, 0), (2, 0)]
    );
    assert_eq!(Admission::default().id().name, ADMISSION);
    assert_eq!(Fifo::default().id().name, FIFO);
    assert!(order(&mut Fifo::default(), &[]).is_empty());
}

#[test]
fn sc_t34_the_policy_is_a_setting() {
    let policies = standard();
    assert_eq!(
        policies.names(),
        vec![ADMISSION, FIFO, LOOKAHEAD, RESIDENCY_GREEDY]
    );
    let refused = match policies.make("nope") {
        Ok(_) => panic!("nope is not a policy of this build"),
        Err(e) => e.to_string(),
    };
    assert!(
        refused.contains("admission, fifo, lookahead, residency_greedy"),
        "{refused}"
    );
    let mut mine = SchedulingPolicies::empty();
    assert!(mine.register(Arc::new(Slow)).is_ok());
    assert!(
        mine.register(Arc::new(Slow)).is_err(),
        "a name is taken once"
    );
    struct Bad;
    impl PolicyAdapter for Bad {
        fn name(&self) -> &str {
            "Not A Name"
        }
        fn make(&self) -> Box<dyn SchedulingPolicy> {
            Box::new(Fifo::default())
        }
    }
    assert!(
        mine.register(Arc::new(Bad)).is_err(),
        "names are lowercase words"
    );
    assert!(moruna_kernel::is_policy_name("residency_greedy2"));
    assert!(!moruna_kernel::is_policy_name("2fast"));
    assert!(format!("{mine:?}").contains("slow"));

    // A run names its policy; an unknown name is refused before anything opens.
    let refused = RigBuilder::new()
        .cfg(|cfg| cfg.policy = "nope".into())
        .stages(1)
        .build();
    assert!(refused.is_err(), "an unknown policy is refused");
    let rig = RigBuilder::new()
        .cfg(|cfg| cfg.policy = FIFO.into())
        .source(FakeSource::new().splits(4, 16, 128).sub_splittable(false))
        .stages(2)
        .go();
    match rig.scheduler.run(CancelToken::new()) {
        Ok(crate::RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }
    let stats = rig.scheduler.scheduler_stats().policy;
    assert_eq!(stats.name, FIFO);
    assert!(stats.decisions > 0 && stats.admitted > 0, "{stats:?}");
    // The real monotonic timer includes gathering and host descheduling. Even FIFO can
    // truthfully overrun the floor; selecting it must preserve that accounting, not promise
    // that an arbitrarily preempted thread finishes within fifty microseconds.
    assert!(stats.over_budget <= stats.decisions, "{stats:?}");
    assert!(stats.fallbacks <= stats.over_budget, "{stats:?}");
    assert!(
        stats.decision_ns >= stats.over_budget * (crate::decide::DECISION_BUDGET_FLOOR_NS + 1),
        "every overrun exceeds the actual decision-budget floor: {stats:?}"
    );
    let mut written = rig.sink.written();
    written.sort_unstable();
    assert_eq!(written, (0..4).collect::<Vec<u64>>(), "every batch once");
}

/// A policy that takes longer than any budget: what 9.4 step 4 falls back from.
struct Slow;

impl PolicyAdapter for Slow {
    fn name(&self) -> &str {
        "slow"
    }
    fn make(&self) -> Box<dyn SchedulingPolicy> {
        Box::new(SlowPolicy)
    }
}

struct SlowPolicy;

impl SchedulingPolicy for SlowPolicy {
    fn id(&self) -> PolicyId {
        PolicyId {
            name: "slow",
            version: 1,
        }
    }
    fn decide(&mut self, state: &SchedulerState<'_>, actions: &mut Vec<Action>) {
        std::thread::sleep(Duration::from_nanos(state.budget_ns * 4));
        Fifo::default().decide(state, actions);
    }
}

#[test]
fn sc_t34_a_decision_over_budget_falls_back() {
    let mut policies = standard();
    if let Err(e) = policies.register(Arc::new(Slow)) {
        panic!("register: {e}");
    }
    let rig = RigBuilder::new()
        .cfg(move |cfg| {
            cfg.policy = "slow".into();
            cfg.policies = policies;
        })
        .source(FakeSource::new().splits(6, 16, 128).sub_splittable(false))
        .stages(2)
        .go();
    match rig.scheduler.run(CancelToken::new()) {
        Ok(crate::RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }
    let stats = rig.scheduler.scheduler_stats().policy;
    assert_eq!(stats.name, "slow");
    assert!(stats.over_budget > 0, "{stats:?}");
    assert!(
        stats.fallbacks > 0 && stats.fallbacks <= stats.over_budget,
        "each decision over budget hands the next to the baseline: {stats:?}"
    );
    assert_eq!(rig.sink.written().len(), 6);
}

#[test]
fn sc_t34_what_memory_cannot_admit_goes_to_disk() {
    // A budget smaller than one batch: no claim for a read or an output is ever granted, so
    // every push goes straight to disk, and the run still delivers every batch once.
    let placement = FakePlacement::new().with_budget(16);
    let fake = placement.clone();
    let rig = RigBuilder::new()
        .source(FakeSource::new().splits(5, 16, 128).sub_splittable(false))
        .placement(placement)
        .kernel(Arc::new(FakeKernel::new()))
        .go();
    match rig.scheduler.run(CancelToken::new()) {
        Ok(crate::RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }
    let mut written = rig.sink.written();
    written.sort_unstable();
    assert_eq!(written, vec![0, 1, 2, 3, 4]);
    assert_eq!(fake.spilled(0).len(), 5, "every read went straight to disk");
    assert_eq!(fake.spilled(1).len(), 5, "and every output");
    let stats = rig.scheduler.scheduler_stats().policy;
    assert_eq!(stats.overflow_spills, 10, "{stats:?}");
    assert_eq!(fake.reserved(), 0, "no claim is left");
}
