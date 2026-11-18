//! System design 3A (6.3g phase 3): a function measured by nothing yet is warmed up on its first
//! instance, and its other instances wait for what that finds. They wait without a worker: the
//! kernel says how many of its instances are ready, the scheduler starts and deals only those,
//! and a worker that finds none free takes other stages' work. Only that stage waits (PM,
//! 2026-11-18).

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::Duration;

use moruna_kernel::{
    CancelToken, Fingerprint, InitCtx, Kernel, KernelHints, KernelKind, KernelState, NoState,
    Payload, PayloadKind, PayloadSpec, Result, SourceSchema, TierPref,
};
use moruna_testkit::FakeSource;

use super::common::{RigBuilder, StatefulKernel};

/// `StatefulKernel` dealing its instances, one of them ready until its first `warm` applies
/// have run, then all: a stage warming up on its first instance.
struct Warming {
    inner: Arc<StatefulKernel>,
    warm: usize,
    applied: AtomicUsize,
    /// The instances made, in order, and how many applies had run when each was.
    made: Mutex<Vec<(usize, usize)>>,
}

impl Kernel for Warming {
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
        self.made
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((ctx.instance, self.applied.load(Ordering::SeqCst)));
        self.inner.init(ctx)
    }
    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        let out = self.inner.apply(state, input);
        self.applied.fetch_add(1, Ordering::SeqCst);
        out
    }
    fn deals_instances(&self) -> bool {
        true
    }
    fn instances_ready(&self) -> usize {
        if self.applied.load(Ordering::SeqCst) < self.warm {
            1
        } else {
            usize::MAX
        }
    }
}

/// A stage after it that any worker runs: which workers ran it, and how many times while the
/// stage before was warming.
struct After {
    warming: Arc<Warming>,
    during: AtomicUsize,
    threads: Mutex<HashSet<ThreadId>>,
}

impl Kernel for After {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint::compute("moruna-scheduler::tests::warm_up::After", b"v1")
    }
    fn kind(&self) -> KernelKind {
        KernelKind::Stateless
    }
    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Either,
            tier: TierPref::Any,
        }
    }
    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        Ok(input.clone())
    }
    fn init(&self, _ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }
    fn apply(&self, _state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        if self.warming.instances_ready() == 1 {
            self.during.fetch_add(1, Ordering::SeqCst);
            self.threads
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(std::thread::current().id());
        }
        std::thread::sleep(Duration::from_millis(2));
        Ok(input)
    }
}

#[test]
fn sc_p_a_stage_s_instances_wait_for_its_warm_up_and_its_workers_take_other_work() {
    let inner = Arc::new(StatefulKernel::new(4).latency(Duration::from_millis(15)));
    let warming = Arc::new(Warming {
        inner: Arc::clone(&inner),
        warm: 5,
        applied: AtomicUsize::new(0),
        made: Mutex::new(Vec::new()),
    });
    let after = Arc::new(After {
        warming: Arc::clone(&warming),
        during: AtomicUsize::new(0),
        threads: Mutex::new(HashSet::new()),
    });
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 4;
            cfg.workers_active = 4;
            cfg.initial_morsel_target = 8;
            cfg.read_ahead = 8;
        })
        .source(FakeSource::new().splits(1, 64, 64 * 8))
        .kernel(warming.clone())
        .kernel(after.clone())
        .go();
    rig.scheduler
        .init_instances()
        .expect("the ready instance is made");
    // Only the instance that warms up is made before the run.
    assert_eq!(
        warming
            .made
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
        vec![(0, 0)]
    );
    match rig.scheduler.run(CancelToken::new()) {
        Ok(crate::RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }
    assert!(
        !inner.overlap.load(Ordering::SeqCst),
        "two applies overlapped on one instance"
    );
    // More morsels than the warm-up took.
    assert!(warming.applied.load(Ordering::SeqCst) > 5);
    let applies = inner
        .applies
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    // The warm-up's applies all on the first instance; the others only after it.
    assert!(
        applies.iter().take(5).all(|(instance, _)| *instance == 0),
        "{applies:?}"
    );
    let made = warming
        .made
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert!(
        made.iter().skip(1).all(|(_, applied)| *applied >= 5),
        "an instance not ready was made only after the warm-up: {made:?}"
    );
    assert!(
        made.len() > 1,
        "the other instances were made once ready: {made:?}"
    );
    // While the stage warmed, the workers it did not hold ran the stage after it: more than
    // one worker, so no worker sat waiting for an instance that was not ready.
    assert!(after.during.load(Ordering::SeqCst) >= 1);
    assert!(
        after
            .threads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
            >= 2,
        "the stage after was run by several workers while the first warmed"
    );
}
