//! Stream P (`architecture/data-os.md` section 5): a kernel that deals its instances has them
//! take its batches in turn, whichever worker brings them, so one worker keeps every instance
//! busy; an image's task processes keep batches in flight of their own. Without it a worker keeps
//! the instance it last held (f.4, SC-I6), and one worker feeds one instance: the NYC gate's
//! native stage of eight tasks ran on one.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use moruna_kernel::{
    CancelToken, Fingerprint, InitCtx, Kernel, KernelHints, KernelKind, KernelState, Payload,
    PayloadSpec, Result, SourceSchema,
};
use moruna_testkit::FakeSource;

use super::common::{RigBuilder, StatefulKernel};

/// `StatefulKernel`, dealing its instances or not.
struct Dealt(Arc<StatefulKernel>, bool);

impl Kernel for Dealt {
    fn fingerprint(&self) -> Fingerprint {
        self.0.fingerprint()
    }
    fn kind(&self) -> KernelKind {
        self.0.kind()
    }
    fn hints(&self) -> KernelHints {
        self.0.hints()
    }
    fn accepts(&self) -> PayloadSpec {
        self.0.accepts()
    }
    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        self.0.output_schema(input)
    }
    fn init(&self, ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        self.0.init(ctx)
    }
    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        self.0.apply(state, input)
    }
    fn deals_instances(&self) -> bool {
        self.1
    }
}

/// Applies per instance, on one worker.
fn applies(deals: bool) -> BTreeMap<usize, usize> {
    let kernel = Arc::new(StatefulKernel::new(4).latency(Duration::from_micros(100)));
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 1;
            cfg.workers_active = 1;
            cfg.initial_morsel_target = 8;
            cfg.read_ahead = 4;
        })
        .source(FakeSource::new().splits(1, 200, 1_600))
        .kernel(Arc::new(Dealt(kernel.clone(), deals)))
        .go();
    rig.scheduler
        .init_instances()
        .expect("the instances are made");
    match rig.scheduler.run(CancelToken::new()) {
        Ok(crate::RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }
    assert!(
        !kernel.overlap.load(std::sync::atomic::Ordering::SeqCst),
        "two applies overlapped on one instance"
    );
    let mut per = BTreeMap::new();
    for (instance, _) in kernel
        .applies
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
    {
        *per.entry(*instance).or_insert(0) += 1;
    }
    per
}

#[test]
fn sc_p_one_worker_deals_batches_to_every_instance_in_turn() {
    let per = applies(true);
    assert_eq!(per.len(), 4, "every instance took batches: {per:?}");
    let (least, most) = (per.values().min().unwrap(), per.values().max().unwrap());
    assert!(most - least <= 1, "in turn, so evenly: {per:?}");
}

#[test]
fn sc_p_without_dealing_one_worker_feeds_the_one_instance_it_holds() {
    let per = applies(false);
    assert_eq!(per.len(), 1, "a worker keeps its instance (f.4): {per:?}");
}
