//! The scheduling policy port (contracts d.18; adaptive-runtime 9.3, 9.4).
//!
//! A policy decides, each time a worker is free, what runs next and where memory goes, from the
//! units that are ready, the run's DAG and the buffer catalog (d.17). It is an adapter of one
//! trait, made by name from a registry as kinds are (SDD 18), so swapping it is a setting.
//! The scheduler keeps the hard rule whatever the policy says: a unit runs only when the bytes
//! it is predicted to make are claimed, and every push is admitted under a claim (12.5).
//!
//! What a policy reads of each unit is a [`Prediction`] with a spread (9.4): from the run's cost
//! model when one is configured, or the stage's measured ratios when none is, and marked with
//! which ([`PredictedBy`]). The run's machine and every stage's backlog and units under way
//! ([`Machine`], [`StageView`]) are the snapshot a lookahead simulates forward from.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::catalog::{BufferId, Head, Residency};
use crate::graph::Topology;
use crate::ids::StageId;

/// A policy's name and version, stored beside every decision it makes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PolicyId {
    /// The name a setting chooses it by.
    pub name: &'static str,
    /// Bumped when its decisions change for the same state.
    pub version: u32,
}

/// A predicted amount as a distribution (adaptive-runtime decision 2): its median and its 95th
/// percentile, never below the median.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Spread {
    /// The median.
    pub p50: u64,
    /// The 95th percentile.
    pub p95: u64,
}

impl Spread {
    /// An amount known exactly.
    pub fn exact(value: u64) -> Spread {
        Spread {
            p50: value,
            p95: value,
        }
    }

    /// A spread of `p50` and `p95`, the 95th percentile held at the median or above.
    pub fn new(p50: u64, p95: u64) -> Spread {
        Spread {
            p50,
            p95: p95.max(p50),
        }
    }
}

/// Where a unit's prediction came from.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum PredictedBy {
    /// The run's cost model, through its port (adaptive-runtime 5): the spread is the model's.
    Model,
    /// No cost model is configured: the stage's measured ratio of bytes out to bytes in so far
    /// (one before it has run) and its mean unit time, each a point (`p50 == p95`).
    #[default]
    MeasuredRatio,
}

/// What a unit is predicted to make and take (9.4).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Prediction {
    /// The bytes it makes.
    pub out_bytes: Spread,
    /// Its wall time, nanoseconds.
    pub duration_ns: Spread,
    /// Where it came from.
    pub by: PredictedBy,
    /// The key's outcomes have left the model's spread persistently: the spread is widened
    /// (adaptive-runtime 9.6), and a policy turns conservative.
    pub drifting: bool,
    /// Observations the model's answer rests on; zero for a measured ratio.
    pub observations: u32,
}

impl Prediction {
    /// What admitting the unit claims from the catalog (9.4 step 1, 12.5): its output's 95th
    /// percentile, so a drifting key, whose spread is widened, claims more and leaves less
    /// headroom.
    pub fn claim(&self) -> u64 {
        self.out_bytes.p95
    }
}

/// One unit that is ready (9.1: one task applied to one batch): stage `stage` may take the head
/// of its input port `port` now, its output queues are not full, and an instance is free.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Ready {
    /// The stage.
    pub stage: StageId,
    /// The input port.
    pub port: u16,
    /// The queue on that port.
    pub queue: StageId,
    /// The batch it would take.
    pub head: Head,
    /// Bytes the input queue holds, every tier.
    pub queue_bytes: u64,
    /// Bytes the stage's output queues hold, every tier.
    pub out_bytes: u64,
    /// What the unit is predicted to make and take; admitting it claims [`Prediction::claim`].
    pub predicted: Prediction,
    /// Its critical-path weight (9.4 step 2, G8): the predicted nanoseconds of work queued on the
    /// longest path from its stage to a sink, its own stage's backlog included.
    pub critical_ns: u64,
    /// Nanoseconds this head has been ready for its stage without being taken: what ageing
    /// credits (G8).
    pub waited_ns: u64,
}

/// What the run is given now (adaptive-runtime 3.3, G12): read at every decision, so a resize is
/// seen at the next one.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Machine {
    /// Workers allowed to take units.
    pub workers: u16,
    /// The host tier's budget.
    pub budget_bytes: u64,
    /// Bytes of it resident or claimed.
    pub held_bytes: u64,
    /// The disk's bandwidth, bytes per second, as the embedder declares it.
    pub disk_bytes_per_second: u64,
    /// The most a unit's claim may be: what the budget holds above the run's floor (10 f.22).
    /// A prediction above it is claimed at it, so a simulation reserves no more.
    pub claim_cap_bytes: u64,
}

/// One input port of a stage, as a snapshot reads it.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct PortView {
    /// The queue feeding the port.
    pub queue: StageId,
    /// Batches waiting in it.
    pub batches: u64,
    /// Their bytes in memory.
    pub memory_bytes: u64,
    /// Their bytes only on disk.
    pub disk_bytes: u64,
}

/// One kernel stage as a snapshot reads it (adaptive-runtime 9.4 step 3): its backlog, its units
/// under way and what one of its units is predicted to cost now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StageView {
    /// The stage.
    pub stage: StageId,
    /// Its input ports, port 0 first.
    pub ports: Vec<PortView>,
    /// Its units under way, each by the nanoseconds it has run.
    pub running_ns: Vec<u64>,
    /// How many of its units may run at once: its instances for a stateful stage, the workers
    /// otherwise.
    pub cap: u16,
    /// A unit of it at its typical batch.
    pub unit: Prediction,
}

/// What a policy decides from (9.1): the units ready for this worker, the DAG, and the catalog,
/// read through its port only when a policy needs more than the heads.
pub struct SchedulerState<'a> {
    /// The free worker the decision is for.
    pub worker: u16,
    /// Every ready unit, in stage and port order.
    pub ready: &'a [Ready],
    /// The run's DAG.
    pub topology: &'a Topology,
    /// The buffer catalog.
    pub residency: &'a dyn Residency,
    /// The time this decision may take, nanoseconds (9.4 step 4).
    pub budget_ns: u64,
    /// What the run is given now.
    pub machine: Machine,
    /// Every kernel stage, stage 1 first.
    pub stages: &'a [StageView],
}

/// What a policy may decide (9.2).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Run the unit of `stage` on the head of its `port`'s queue, if its predicted bytes can be
    /// claimed. A list of these is a preference order: the first that can be claimed runs.
    Run {
        /// The stage.
        stage: StageId,
        /// The input port.
        port: u16,
    },
    /// Write a resident batch to disk.
    Spill(BufferId),
    /// Read a batch on disk back into memory.
    Reload(BufferId),
    /// Run nothing now.
    Wait,
}

/// What a policy counts of its own decisions, beside what the scheduler counts (9.7).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PolicyCounters {
    /// Decisions it made with a shallower procedure than its own to keep to the budget (9.4
    /// step 4: lookahead falls to greedy).
    pub shallower: u64,
    /// Simulations it ran.
    pub simulations: u64,
    /// Simulations that could not be run, each answered by the shallower procedure.
    pub simulation_errors: u64,
    /// Why the first of them could not be.
    pub simulation_error: Option<String>,
    /// Times a simulator it kept was resized to the run's machine and played again
    /// (adaptive-runtime G12).
    pub resizes: u64,
}

/// What a policy reads of the state beyond the ready units' heads and predictions, so the
/// scheduler gathers nothing a policy does not use (9.4 step 4: the decision's cost is the
/// policy's). A part it does not read is empty or zero in the state it is given.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Reads {
    /// Every stage's snapshot (`SchedulerState::stages`).
    pub stages: bool,
    /// Each ready unit's `critical_ns`.
    pub critical_path: bool,
    /// Each ready unit's `waited_ns`.
    pub waiting: bool,
}

impl Reads {
    /// The heads and their predictions only.
    pub const HEADS: Reads = Reads {
        stages: false,
        critical_path: false,
        waiting: false,
    };
}

/// The policy port (9.3). `decide` is called with the scheduler's decision lock held, once per
/// free worker, and appends its actions, in the order to try them, to `actions`, which the
/// scheduler hands over empty and reuses, so a decision allocates nothing it need not.
pub trait SchedulingPolicy: Send {
    /// Its name and version.
    fn id(&self) -> PolicyId;
    /// The actions for the free worker of `state`, in the order to try them.
    fn decide(&mut self, state: &SchedulerState<'_>, actions: &mut Vec<Action>);
    /// What it reads of the state; the heads and their predictions only, unless it says more.
    fn reads(&self) -> Reads {
        Reads::HEADS
    }
    /// What it counts of its own decisions; nothing, for a policy that counts nothing.
    fn counters(&self) -> PolicyCounters {
        PolicyCounters::default()
    }
}

/// Makes policies of one name.
pub trait PolicyAdapter: Send + Sync {
    /// The name a setting chooses it by.
    fn name(&self) -> &str;
    /// A policy, fresh for a run.
    fn make(&self) -> Box<dyn SchedulingPolicy>;
}

/// The registry of policies, shaped like the registry of kinds (SDD 18): a policy is an adapter
/// registered by name and a setting names the one in use.
#[derive(Clone, Default)]
pub struct SchedulingPolicies {
    adapters: BTreeMap<String, Arc<dyn PolicyAdapter>>,
}

impl std::fmt::Debug for SchedulingPolicies {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.adapters.keys()).finish()
    }
}

/// A name a registry takes: a lowercase letter, then lowercase letters, digits and underscores.
pub fn is_policy_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

impl SchedulingPolicies {
    /// A registry with no policy.
    pub fn empty() -> SchedulingPolicies {
        SchedulingPolicies::default()
    }

    /// Register an adapter: refused when its name is not a name or is taken.
    pub fn register(&mut self, adapter: Arc<dyn PolicyAdapter>) -> crate::Result<()> {
        let name = adapter.name().to_string();
        if !is_policy_name(&name) {
            return Err(crate::MorunaError::Config {
                name: "scheduler.policy",
                msg: format!("{name:?} is not a policy name"),
            });
        }
        if self.adapters.contains_key(&name) {
            return Err(crate::MorunaError::Config {
                name: "scheduler.policy",
                msg: format!("a policy named {name} is registered already"),
            });
        }
        self.adapters.insert(name, adapter);
        Ok(())
    }

    /// The registered names, sorted.
    pub fn names(&self) -> Vec<&str> {
        self.adapters.keys().map(String::as_str).collect()
    }

    /// A new policy of `name`, refused naming the registered ones when there is none.
    pub fn make(&self, name: &str) -> crate::Result<Box<dyn SchedulingPolicy>> {
        match self.adapters.get(name) {
            Some(adapter) => Ok(adapter.make()),
            None => Err(crate::MorunaError::Config {
                name: "scheduler.policy",
                msg: format!(
                    "no policy is named {name:?}; the registered ones are {}",
                    self.names().join(", ")
                ),
            }),
        }
    }
}

/// What the scheduler's decisions cost and left on the table (9.7, 10), for the report and the
/// benchmark matrix.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PolicyStats {
    /// The policy in use.
    pub name: String,
    /// Decisions made.
    pub decisions: u64,
    /// Nanoseconds spent deciding, summed.
    pub decision_ns: u64,
    /// Decisions that took longer than their budget.
    pub over_budget: u64,
    /// Decisions made by the baseline because the policy's last one was over budget.
    pub fallbacks: u64,
    /// Units run.
    pub admitted: u64,
    /// Units a policy chose whose claim was not granted.
    pub not_claimed: u64,
    /// Worker nanoseconds idle while a unit was ready.
    pub idle_ready_ns: u64,
    /// Budget bytes neither resident nor claimed, times the nanoseconds they stayed so while a
    /// unit was ready (byte-nanoseconds).
    pub unused_ready_byte_ns: u128,
    /// Pushes that memory could not admit, written straight to disk.
    pub overflow_spills: u64,
    /// Pushes that waited for room to be claimed.
    pub push_waits: u64,
    /// What the policy counted of its own decisions.
    pub counters: PolicyCounters,
    /// Units whose prediction came from the cost model, and from measured ratios.
    pub predicted_by_model: u64,
    /// Units predicted from measured ratios, no cost model being configured.
    pub predicted_by_ratio: u64,
    /// What the run's learning did (9.6); `None` when no cost model is configured.
    pub learning: Option<LearningStats>,
    /// What deciding and learning cost, part by part (9.7, G9).
    pub overhead: Overhead,
}

/// What the adaptive machinery cost a run, part by part, against the time its units took
/// (adaptive-runtime 0: it must cost less than it saves; G9). Nanoseconds and counts; summed
/// over every worker.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Overhead {
    /// Units run, and their wall time summed.
    pub units: u64,
    /// The units' wall time summed.
    pub unit_ns: u64,
    /// Gathering what a decision reads: the ready units, their predictions, what the policy
    /// reads beyond them. Part of the decision's budget.
    pub gather_ns: u64,
    /// Of the gathering, asking the cost model, and how many times it was asked.
    pub predict_ns: u64,
    /// Times the cost model was asked.
    pub predictions: u64,
    /// Predictions answered from a stage's cache, the model unchanged since.
    pub cached: u64,
    /// Of the gathering, every stage's snapshot.
    pub snapshot_ns: u64,
    /// The policy's `decide`.
    pub decide_ns: u64,
    /// Settling finished units into the models (the online update and the drift), on the
    /// workers, and how many.
    pub learn_ns: u64,
    /// Units settled.
    pub learned: u64,
    /// Appending observations to the store, on the learning thread, never a worker's.
    pub store_ns: u64,
    /// Writing the fitted state, on the learning thread.
    pub save_ns: u64,
    /// The run's end waiting for the learning thread to finish writing, after its output was
    /// complete.
    pub drain_ns: u64,
}

impl Overhead {
    /// What deciding and learning took from the units' workers, as a share of the units' time:
    /// gathering, deciding and settling. The store and the saves are on a thread of their own
    /// and are not in it.
    pub fn share(&self) -> f64 {
        if self.unit_ns == 0 {
            return 0.0;
        }
        (self.gather_ns + self.decide_ns + self.learn_ns) as f64 / self.unit_ns as f64
    }
}

/// What learning did over a run (adaptive-runtime 9.6, 9.7), for the report.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LearningStats {
    /// The deciding model, `name@version`.
    pub model: String,
    /// The model in shadow.
    pub shadow: Option<String>,
    /// Units observed.
    pub observed: u64,
    /// Observations written to the store.
    pub written: u64,
    /// Observations not written, with the first reason.
    pub unwritten: u64,
    /// Why the first was not written.
    pub store_error: Option<String>,
    /// Outcomes refused as no measurement, with the first reason.
    pub refused: u64,
    /// Why the first was refused.
    pub refusal: Option<String>,
    /// Every spell of drift, in the order they began.
    pub drifts: Vec<DriftStat>,
}

/// One spell of a stage's drift (adaptive-runtime 9.6).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DriftStat {
    /// The stage whose key drifted.
    pub stage: StageId,
    /// The target whose outcomes left the spread (`duration`, `bytes_out` ...).
    pub target: String,
    /// The key's units observed this run when it began.
    pub at_unit: u64,
    /// Units observed while it drifted.
    pub units: u64,
    /// Whether it settled before the run ended.
    pub settled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Named(&'static str);

    struct Waits;

    impl SchedulingPolicy for Waits {
        fn id(&self) -> PolicyId {
            PolicyId {
                name: "waits",
                version: 1,
            }
        }
        fn decide(&mut self, _state: &SchedulerState<'_>, actions: &mut Vec<Action>) {
            actions.push(Action::Wait);
        }
    }

    impl PolicyAdapter for Named {
        fn name(&self) -> &str {
            self.0
        }
        fn make(&self) -> Box<dyn SchedulingPolicy> {
            Box::new(Waits)
        }
    }

    /// CT-T30 policies by name: a name is a lowercase word, taken once, and an unknown one is
    /// refused naming the registered ones.
    #[test]
    fn ct_t30_policies_by_name() {
        let mut registry = SchedulingPolicies::empty();
        assert!(registry.names().is_empty());
        registry.register(Arc::new(Named("waits"))).expect("a name");
        registry.register(Arc::new(Named("b_2"))).expect("a name");
        assert!(registry.register(Arc::new(Named("waits"))).is_err());
        for bad in ["", "Waits", "2b", "a-b"] {
            assert!(registry.register(Arc::new(Named(bad))).is_err(), "{bad:?}");
        }
        assert_eq!(registry.names(), vec!["b_2", "waits"]);
        let made = registry.make("waits").expect("registered");
        assert_eq!(made.id().name, "waits");
        let refused = match registry.make("nope") {
            Ok(_) => panic!("not registered"),
            Err(e) => e.to_string(),
        };
        assert!(refused.contains("b_2, waits"), "{refused}");
        assert_eq!(format!("{registry:?}"), "[\"b_2\", \"waits\"]");
        assert_eq!(PolicyStats::default().decisions, 0);
        assert_eq!(made.counters(), PolicyCounters::default());
        assert_eq!(made.reads(), Reads::HEADS);
        let mut actions = Vec::new();
        let mut made = made;
        let fake = Topology::chain(1);
        struct Nothing;
        impl Residency for Nothing {
            fn snapshot(&self) -> crate::catalog::CatalogSnapshot {
                crate::catalog::CatalogSnapshot::default()
            }
            fn head(&self, _stage: StageId) -> Option<Head> {
                None
            }
            fn reserve(&self, _tier: crate::Tier, _bytes: u64) -> crate::Result<crate::Reserve> {
                Err(crate::MorunaError::Plan("no".into()))
            }
            fn push_reserved(
                &self,
                _stages: &[StageId],
                _morsel: crate::Morsel,
                _claim: crate::Reservation,
            ) -> crate::Result<()> {
                Ok(())
            }
            fn replace_reserved(
                &self,
                _stage: StageId,
                _morsel: crate::Morsel,
                _claim: crate::Reservation,
            ) -> crate::Result<()> {
                Ok(())
            }
            fn push_spilled(
                &self,
                _stages: &[StageId],
                _morsel: crate::Morsel,
            ) -> crate::Result<()> {
                Ok(())
            }
            fn shed(&self, _tier: crate::TierKind, _bytes: u64) -> bool {
                false
            }
            fn spill(&self, _batch: BufferId) -> crate::Result<()> {
                Ok(())
            }
            fn reload(&self, _batch: BufferId) -> crate::Result<()> {
                Ok(())
            }
            fn observe(&self, _observer: Arc<dyn crate::catalog::CatalogObserver>) {}
            fn floor(&self, _tier: crate::Tier, batch: u64) -> crate::catalog::CatalogFloor {
                crate::catalog::CatalogFloor::of(0, 0, batch, 0)
            }
        }
        made.decide(
            &SchedulerState {
                worker: 0,
                ready: &[],
                topology: &fake,
                residency: &Nothing,
                budget_ns: 1,
                machine: Machine::default(),
                stages: &[],
            },
            &mut actions,
        );
        assert_eq!(actions, vec![Action::Wait]);
    }

    /// CT-T31 a prediction claims its 95th percentile: a spread holds its p95 at its median or
    /// above, an exact one is a point, and a measured ratio is the default source.
    #[test]
    fn ct_t31_a_prediction_claims_its_p95() {
        assert_eq!(Spread::new(10, 4), Spread::exact(10));
        assert_eq!(Spread::new(4, 10), Spread { p50: 4, p95: 10 });
        let p = Prediction {
            out_bytes: Spread::new(100, 250),
            ..Prediction::default()
        };
        assert_eq!(p.claim(), 250);
        assert_eq!(Prediction::default().by, PredictedBy::MeasuredRatio);
        assert_eq!(LearningStats::default().drifts, Vec::new());
        assert!(!DriftStat::default().settled);
        assert_eq!(StageView::default().ports, Vec::new());
        assert_eq!(Machine::default().workers, 0);
        assert_eq!(PortView::default().batches, 0);
        let mut o = Overhead::default();
        assert_eq!(o.share(), 0.0);
        o.unit_ns = 1_000;
        o.gather_ns = 5;
        o.decide_ns = 3;
        o.learn_ns = 2;
        o.store_ns = 1_000_000;
        assert!(
            (o.share() - 0.01).abs() < 1e-12,
            "the store is not in the share"
        );
    }
}
