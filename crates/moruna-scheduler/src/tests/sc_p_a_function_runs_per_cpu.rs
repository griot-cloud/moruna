//! Item 6.3e (system design 3E): a function runs one instance per CPU of the job unless it
//! declares fewer. 6.2 run 3 found Score's function on a 4-CPU node as one process, its workers
//! waiting 82.6 s of 93.5 s for its answers (`gate-score: 1 tasks, busy_peak 1`): an image
//! stage ran the `MORUNA_PROCESSES` its image declared, or one, whatever the job had.
//!
//! The stage here is an image function's as the kernel's image task makes it
//! (`moruna_micro::task::ImageTask::new`, which runs on the kernel only): its instance count
//! from its image's environment and the job's CPUs, its instances dealt the batches in turn
//! (`deals_instances`, stream P), each apply holding a batch as a task does. Every instance must
//! take batches, and all of them must hold one at once: `busy_peak` the job's CPUs.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use moruna_kernel::{
    CancelToken, Fingerprint, InitCtx, Kernel, KernelHints, KernelKind, KernelState, Payload,
    PayloadSpec, Result, SourceSchema,
};
use moruna_testkit::FakeSource;

use super::common::{RigBuilder, StatefulKernel};

/// The instances an image function's stage runs: its environment's ceiling over the job's CPUs.
fn instances_of(env: &[String], budget_cpu: Option<f64>, node_cpus: usize) -> usize {
    let cpus = moruna_adapters::instances::cpus(budget_cpu, node_cpus);
    let ceiling = moruna_adapters::instances::declared(env).expect("a valid declaration");
    moruna_adapters::instances::count(cpus, ceiling)
}

/// An image function's stage: `StatefulKernel`'s instances, dealt, with how many hold a batch
/// at once, and the most.
struct ImageFunction {
    inner: Arc<StatefulKernel>,
    now: AtomicUsize,
    peak: AtomicUsize,
}

impl Kernel for ImageFunction {
    fn fingerprint(&self) -> Fingerprint {
        self.inner.fingerprint()
    }
    fn kind(&self) -> KernelKind {
        self.inner.kind()
    }
    fn hints(&self) -> KernelHints {
        self.inner.hints()
    }
    fn accepts(&self) -> PayloadSpec {
        self.inner.accepts()
    }
    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        self.inner.output_schema(input)
    }
    fn init(&self, ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        self.inner.init(ctx)
    }
    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        let out = self.inner.apply(state, input);
        self.now.fetch_sub(1, Ordering::SeqCst);
        out
    }
    fn deals_instances(&self) -> bool {
        true
    }
}

/// Runs the stage of `instances` on `workers` workers over 64 batches: the instances that took
/// batches, and `busy_peak`.
fn run(instances: usize, workers: u16) -> (BTreeSet<usize>, usize) {
    let stage = Arc::new(ImageFunction {
        inner: Arc::new(StatefulKernel::new(instances).latency(Duration::from_millis(5))),
        now: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
    });
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = workers;
            cfg.workers_active = workers;
            cfg.initial_morsel_target = 8;
            cfg.read_ahead = 8;
        })
        .source(FakeSource::new().splits(1, 64, 64 * 8))
        .kernel(stage.clone())
        .go();
    rig.scheduler
        .init_instances()
        .expect("the instances are made");
    match rig.scheduler.run(CancelToken::new()) {
        Ok(crate::RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }
    assert!(
        !stage.inner.overlap.load(Ordering::SeqCst),
        "two applies overlapped on one instance"
    );
    let took: BTreeSet<usize> = stage
        .inner
        .applies
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(instance, _)| *instance)
        .collect();
    (took, stage.peak.load(Ordering::SeqCst))
}

#[test]
fn sc_p_a_function_that_declares_nothing_runs_one_instance_per_cpu_of_the_job() {
    // An image function's environment as the Python shim's layer and a job's runner give it:
    // no MORUNA_PROCESSES. The job's `budget.cpu` is 4, on a node of 16.
    let env: Vec<String> = [
        "PATH=/usr/bin",
        "MORUNA_PLANE=rings",
        "MORUNA_ROLE=function",
    ]
    .iter()
    .map(|e| e.to_string())
    .collect();
    let instances = instances_of(&env, Some(4.0), 16);
    assert_eq!(
        instances, 4,
        "one instance per CPU of the job: 6.2 run 3's Score ran one"
    );
    let (took, busy_peak) = run(instances, 4);
    assert_eq!(
        took,
        (0..4).collect(),
        "the batches go to every instance: {took:?}"
    );
    assert_eq!(busy_peak, 4, "all four hold a batch at once");
}

#[test]
fn sc_p_a_function_that_declares_one_runs_alone_on_any_job() {
    // A function that keeps state across batches (a running total) declares one.
    let env = vec!["MORUNA_PROCESSES=1".to_string()];
    let instances = instances_of(&env, Some(4.0), 16);
    assert_eq!(instances, 1);
    let (took, busy_peak) = run(instances, 4);
    assert_eq!(took, BTreeSet::from([0]));
    assert_eq!(busy_peak, 1);
    // No budget.cpu: the node's CPUs, under the declared ceiling of two.
    let env = vec!["MORUNA_PROCESSES=2".to_string()];
    assert_eq!(instances_of(&env, None, 8), 2);
    assert_eq!(instances_of(&[], None, 8), 8);
}
