//! SC-T6: every instance exists before the first morsel, no instance is used by two workers at
//! once, and a worker reacquires the instance it last used. SC-I6.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use amoru_kernel::{CancelToken, KernelHints};
use amoru_testkit::{FakeKernel, FakeSource};

use super::common::{RigBuilder, StatefulKernel};

#[test]
fn sc_t6_instance_affinity() {
    let kernel = Arc::new(StatefulKernel::new(4).latency(Duration::from_micros(200)));
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 8;
            cfg.workers_active = 8;
            cfg.initial_morsel_target = 8;
            cfg.read_ahead = 8;
        })
        .source(FakeSource::new().splits(1, 600, 4_800))
        .kernel(kernel.clone())
        .go();

    if let Err(e) = rig.scheduler.init_instances() {
        panic!("init_instances: {e}");
    }
    assert_eq!(
        kernel.init_calls.load(Ordering::SeqCst),
        4,
        "every instance is built by init_instances, before any morsel"
    );
    assert!(
        rig.source.reads().is_empty(),
        "nothing was read to build the pool"
    );

    match rig.scheduler.run(CancelToken::new()) {
        Ok(crate::RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }
    assert!(
        !kernel.overlap.load(Ordering::SeqCst),
        "two applies overlapped on one instance"
    );
    let affinity = kernel.affinity();
    assert!(
        affinity >= 0.9,
        "workers reacquired their previous instance only {:.1}% of the time",
        affinity * 100.0
    );
    // The default hints of a `FakeKernel` are the ones the pool is sized from.
    let _ = KernelHints::default();
    let _ = FakeKernel::new();
}
