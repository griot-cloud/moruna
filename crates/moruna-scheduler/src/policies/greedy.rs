//! `residency_greedy` (f.23; adaptive-runtime 9.3, 9.4 step 2; RA 5, 9 (C)): one step.
//!
//! A ready unit's head is resident by construction (the scheduler offers only resident heads,
//! f.3), so what residency decides between ready units is what each does to memory: the bytes
//! its take frees (its batch's bytes, when the take is the batch's last) less the bytes it makes
//! that only more units can free (its output to the queues of kernel stages; what it gives a sink
//! leaves at the sink's pace, needing no worker), per nanosecond of its predicted time, so the
//! unit that frees the most memory soonest leads. That is weighed against the critical path (the predicted work queued on the longest
//! path from its stage to a sink) by how full memory is: with memory free, the critical path
//! leads, which keeps every worker fed; as memory fills, the memory a unit frees leads, which
//! keeps batches out of the disk. Ageing adds credit for the time a head has waited, and a head
//! that has waited [`AGE_LIMIT`] times its predicted time goes first, so no ready branch
//! starves (G8). Every ready unit is in the order, so one whose claim is refused gives way to
//! the next.

use moruna_kernel::{Action, Consumer, PolicyId, Reads, Ready, SchedulerState, SchedulingPolicy};

/// A ready head that has waited this many times its unit's predicted time goes first (G8).
pub const AGE_LIMIT: u64 = 16;
/// The least time a unit is taken to last for its ageing bound, nanoseconds: a stage that has
/// not run yet has no measured time, and its head would otherwise be overdue at once.
pub const AGE_FLOOR_NS: u64 = 1_000_000;

/// The one-step residency-aware policy (module docs). It reads the critical path and the
/// waiting time beside the heads, and orders them in buffers it keeps.
#[derive(Debug, Default)]
pub struct ResidencyGreedy {
    weighed: Vec<Weighed>,
    order: Vec<usize>,
}

/// How the policy weighs one ready unit.
#[derive(Clone, Copy, Debug)]
struct Weighed {
    /// Its place in `state.ready`.
    at: usize,
    /// It has waited its ageing bound.
    overdue: bool,
    waited: u64,
    score: f64,
}

impl ResidencyGreedy {
    /// The ready units in the order this policy tries them, as places in `state.ready`.
    pub fn order(&mut self, state: &SchedulerState<'_>) -> &[usize] {
        let ready = state.ready;
        let pressure = if state.machine.budget_bytes == 0 {
            1.0
        } else {
            (state.machine.held_bytes as f64 / state.machine.budget_bytes as f64).clamp(0.0, 1.0)
        };
        // The share of a stage's output queues that feed kernel stages.
        let kept = |r: &Ready| {
            let outputs = state.topology.outputs(r.stage);
            let to_kernels = outputs
                .iter()
                .filter(|&&q| {
                    matches!(
                        state.topology.link(q).map(|l| l.to),
                        Some(Consumer::Kernel { .. })
                    )
                })
                .count();
            if outputs.is_empty() {
                0.0
            } else {
                to_kernels as f64 / outputs.len() as f64
            }
        };
        let frees = |r: &Ready| {
            let freed = if r.head.last { r.head.bytes } else { 0 };
            // A drifting key's output is taken at its 95th percentile: conservative (9.6).
            let made = if r.predicted.drifting {
                r.predicted.out_bytes.p95
            } else {
                r.predicted.out_bytes.p50
            };
            (freed as f64 - made as f64 * kept(r)) / r.predicted.duration_ns.p50.max(1) as f64
        };
        let most_critical = ready.iter().map(|r| r.critical_ns).max().unwrap_or(0);
        let most_freeing = ready.iter().map(|r| frees(r).abs()).fold(0.0, f64::max);
        let weighed = &mut self.weighed;
        weighed.clear();
        weighed.extend(ready.iter().enumerate().map(|(at, r)| {
            let bound = AGE_LIMIT.saturating_mul(r.predicted.duration_ns.p50.max(AGE_FLOOR_NS));
            let critical = if most_critical == 0 {
                0.0
            } else {
                r.critical_ns as f64 / most_critical as f64
            };
            let freeing = if most_freeing > 0.0 {
                frees(r) / most_freeing
            } else {
                0.0
            };
            let age = (r.waited_ns as f64 / bound as f64).min(1.0);
            Weighed {
                at,
                overdue: r.waited_ns >= bound,
                waited: r.waited_ns,
                score: (1.0 - pressure) * critical + pressure * freeing + age,
            }
        }));
        weighed.sort_by(|a, b| {
            let (ra, rb) = (&ready[a.at], &ready[b.at]);
            b.overdue
                .cmp(&a.overdue)
                .then_with(|| {
                    if a.overdue && b.overdue {
                        b.waited.cmp(&a.waited)
                    } else {
                        std::cmp::Ordering::Equal
                    }
                })
                .then_with(|| b.score.total_cmp(&a.score))
                .then(rb.stage.cmp(&ra.stage))
                .then(rb.queue_bytes.cmp(&ra.queue_bytes))
                .then(ra.port.cmp(&rb.port))
        });
        self.order.clear();
        self.order.extend(weighed.iter().map(|w| w.at));
        &self.order
    }
}

/// The `Run` actions of `order`, places in `state.ready`, appended to `actions`.
pub(crate) fn runs(state: &SchedulerState<'_>, order: &[usize], actions: &mut Vec<Action>) {
    actions.extend(order.iter().map(|&at| Action::Run {
        stage: state.ready[at].stage,
        port: state.ready[at].port,
    }));
}

impl SchedulingPolicy for ResidencyGreedy {
    fn id(&self) -> PolicyId {
        PolicyId {
            name: super::RESIDENCY_GREEDY,
            version: 1,
        }
    }

    fn decide(&mut self, state: &SchedulerState<'_>, actions: &mut Vec<Action>) {
        self.order(state);
        runs(state, &self.order, actions);
    }

    fn reads(&self) -> Reads {
        Reads {
            stages: false,
            critical_path: true,
            waiting: true,
        }
    }
}
