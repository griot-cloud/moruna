//! The policies this build ships (contracts d.18; adaptive-runtime 9.3): `admission`, the rule
//! the scheduler always had; `fifo`; `residency_greedy`, one step weighing the critical path,
//! the memory a unit frees and how long it has waited (f.23); and `lookahead`, which plays the
//! best few next units forward in the simulator from a snapshot of the run (f.24). Each is an
//! adapter of `SchedulingPolicy`, registered by name; the run's `policy` setting names the one in
//! use (`SchedulerConfig::policy`).

mod greedy;
mod lookahead;

use std::sync::Arc;

pub use greedy::{AGE_FLOOR_NS, AGE_LIMIT, ResidencyGreedy};
pub use lookahead::{BEAM, HORIZON_BATCHES, Lookahead, WIDTH};

/// The snapshot a lookahead plays forward from (f.24), for the tests that read it.
#[cfg(test)]
pub(crate) fn snapshot_of(
    state: &moruna_kernel::SchedulerState<'_>,
) -> Result<lookahead::Snapshot, String> {
    lookahead::Snapshot::of(state)
}

use moruna_kernel::{
    Action, PolicyAdapter, PolicyId, SchedulerState, SchedulingPolicies, SchedulingPolicy,
};

/// The name of the baseline: today's admission rule (9.3), and the policy a decision over its
/// budget falls back to (9.4 step 4).
pub const ADMISSION: &str = "admission";
/// The name of first-in, first-out over the batches ready to be taken.
pub const FIFO: &str = "fifo";
/// The name of the one-step residency-aware policy (f.23).
pub const RESIDENCY_GREEDY: &str = "residency_greedy";
/// The name of the bounded lookahead (f.24).
pub const LOOKAHEAD: &str = "lookahead";

/// Every policy this build has: `admission`, `fifo`, `residency_greedy` and `lookahead`.
pub fn standard() -> SchedulingPolicies {
    let mut policies = SchedulingPolicies::empty();
    for adapter in [
        Arc::new(Named(ADMISSION, || Box::new(Admission::default()))) as Arc<dyn PolicyAdapter>,
        Arc::new(Named(FIFO, || Box::new(Fifo::default()))) as Arc<dyn PolicyAdapter>,
        Arc::new(Named(RESIDENCY_GREEDY, || {
            Box::new(ResidencyGreedy::default())
        })) as Arc<dyn PolicyAdapter>,
        Arc::new(Named(LOOKAHEAD, || Box::new(Lookahead::default()))) as Arc<dyn PolicyAdapter>,
    ] {
        if let Err(e) = policies.register(adapter) {
            unreachable!("the standard policies have distinct, valid names: {e}");
        }
    }
    policies
}

/// An adapter that makes a policy by a constructor.
struct Named(&'static str, fn() -> Box<dyn SchedulingPolicy>);

impl PolicyAdapter for Named {
    fn name(&self) -> &str {
        self.0
    }

    fn make(&self) -> Box<dyn SchedulingPolicy> {
        (self.1)()
    }
}

/// The admission rule (10 f.3, SC-I2), behind the port: among the ready units, the stage whose
/// output queues hold the fewest bytes first, ties toward the later stage (the one nearer the
/// sinks); within a stage the port whose queue holds the most bytes, ties toward the lower port.
/// Every ready unit is in the order, so a unit whose claim is refused gives way to the next. It
/// reads the heads only, and orders them in a buffer it keeps.
#[derive(Debug, Default)]
pub struct Admission {
    order: Vec<usize>,
}

impl SchedulingPolicy for Admission {
    fn id(&self) -> PolicyId {
        PolicyId {
            name: ADMISSION,
            version: 1,
        }
    }

    fn decide(&mut self, state: &SchedulerState<'_>, actions: &mut Vec<Action>) {
        let ready = state.ready;
        self.order.clear();
        self.order.extend(0..ready.len());
        self.order.sort_by(|&a, &b| {
            let (a, b) = (&ready[a], &ready[b]);
            a.out_bytes
                .cmp(&b.out_bytes)
                .then(b.stage.cmp(&a.stage))
                .then(b.queue_bytes.cmp(&a.queue_bytes))
                .then(a.port.cmp(&b.port))
        });
        greedy::runs(state, &self.order, actions);
    }
}

/// First in, first out: the ready unit whose batch arrived first runs first, ties toward the
/// lower stage and port. Blind to residency and to the DAG: the comparator RA 9 (A) names. It
/// reads the heads only.
#[derive(Debug, Default)]
pub struct Fifo {
    order: Vec<usize>,
}

impl SchedulingPolicy for Fifo {
    fn id(&self) -> PolicyId {
        PolicyId {
            name: FIFO,
            version: 1,
        }
    }

    fn decide(&mut self, state: &SchedulerState<'_>, actions: &mut Vec<Action>) {
        let ready = state.ready;
        self.order.clear();
        self.order.extend(0..ready.len());
        self.order
            .sort_by_key(|&i| (ready[i].head.arrived, ready[i].stage, ready[i].port));
        greedy::runs(state, &self.order, actions);
    }
}
