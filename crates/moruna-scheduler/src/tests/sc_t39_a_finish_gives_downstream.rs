//! SC-T39: what a stateful kernel's `finish` gives goes downstream (contracts d.7, 10 f.6). Each
//! instance's `finish` runs once its stage's input has ended; the batch it gives is a morsel of
//! the stage numbered from the stage's finish base, one past the last number its input carried,
//! and it reaches the sink, in order on an ordered sink, before the sink finishes. A stage after
//! it numbers its own batches after those.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use moruna_kernel::{
    Fingerprint, InitCtx, Kernel, KernelKind, KernelState, MorunaError, Payload, PayloadKind,
    PayloadSpec, Result, SourceSchema, TierPref,
};
use moruna_testkit::{FakeSink, FakeSource};

use super::common::{RigBuilder, run_within};
use crate::RunOutcome;

/// The last batch an instance was given, and how many.
struct Kept {
    last: Option<Payload>,
    seen: usize,
}

impl KernelState for Kept {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

/// A stateful kernel of `instances` instances whose `finish` gives the last batch each instance
/// was given again, or nothing when it was given none.
struct Repeats {
    instances: usize,
    finishes: AtomicUsize,
    seen: AtomicUsize,
}

impl Repeats {
    fn new(instances: usize) -> Arc<Repeats> {
        Arc::new(Repeats {
            instances,
            finishes: AtomicUsize::new(0),
            seen: AtomicUsize::new(0),
        })
    }
}

impl Kernel for Repeats {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint([39; 32])
    }

    fn kind(&self) -> KernelKind {
        KernelKind::Stateful {
            max_instances: core::num::NonZeroUsize::new(self.instances).expect("one or more"),
        }
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        Ok(input.clone())
    }

    fn init(&self, _ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        Ok(Box::new(Kept {
            last: None,
            seen: 0,
        }))
    }

    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        let kept = state
            .as_any_mut()
            .downcast_mut::<Kept>()
            .ok_or_else(|| MorunaError::Plan("a state this kernel did not make".into()))?;
        kept.last = Some(input.share());
        kept.seen += 1;
        self.seen.fetch_add(1, Ordering::SeqCst);
        Ok(input)
    }

    fn finish(&self, state: &mut dyn KernelState) -> Result<Option<Payload>> {
        self.finishes.fetch_add(1, Ordering::SeqCst);
        let kept = state
            .as_any_mut()
            .downcast_mut::<Kept>()
            .ok_or_else(|| MorunaError::Plan("a state this kernel did not make".into()))?;
        Ok(if kept.seen == 0 {
            None
        } else {
            kept.last.take()
        })
    }
}

/// How many morsels an instance that was given any gives at its end, and the run's sink.
fn run(kernels: Vec<Arc<Repeats>>, ordered: bool, morsels: u32) -> (RunOutcome, FakeSink) {
    let sink = FakeSink::new().requires_order(ordered);
    let mut builder = RigBuilder::new()
        .source(FakeSource::new().splits(morsels, 8, 64))
        .sink(sink.clone())
        .ordered(ordered)
        .cfg(|c| c.initial_morsel_target = 1 << 20);
    for kernel in &kernels {
        builder = builder.kernel(kernel.clone());
    }
    let rig = builder.go();
    rig.scheduler
        .init_instances()
        .expect("the instances are made");
    (run_within(&rig, Duration::from_secs(30), "SC-T39"), sink)
}

#[test]
fn sc_t39_one_instance_gives_one_batch_after_every_number() {
    for ordered in [false, true] {
        let kernel = Repeats::new(1);
        let (outcome, sink) = run(vec![kernel.clone()], ordered, 6);
        assert!(
            matches!(outcome, RunOutcome::Completed { .. }),
            "{outcome:?}"
        );
        assert_eq!(kernel.finishes.load(Ordering::SeqCst), 1);
        let seen = kernel.seen.load(Ordering::SeqCst);
        let mut written = sink.written();
        if !ordered {
            written.sort_unstable();
        }
        let expected: Vec<u64> = (0..=seen as u64).collect();
        assert_eq!(
            written, expected,
            "ordered={ordered}: the finish's batch is number {seen}"
        );
        assert_eq!(sink.finish_calls(), 1);
    }
}

#[test]
fn sc_t39_every_instance_finishes_and_a_later_stage_numbers_after_them() {
    let first = Repeats::new(3);
    let second = Repeats::new(1);
    let (outcome, sink) = run(vec![first.clone(), second.clone()], true, 12);
    assert!(
        matches!(outcome, RunOutcome::Completed { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        first.finishes.load(Ordering::SeqCst),
        3,
        "each instance's finish ran"
    );
    assert_eq!(second.finishes.load(Ordering::SeqCst), 1);
    let read = first.seen.load(Ordering::SeqCst) as u64;
    // The first stage's instances that were given morsels each gave one more, and the second
    // stage, which saw those too, gave one after them.
    let gave = second.seen.load(Ordering::SeqCst) as u64 - read;
    assert!((1..=3).contains(&gave), "{gave}");
    let expected: Vec<u64> = (0..=read + gave).collect();
    assert_eq!(sink.written(), expected, "every number once, in order");
}
