//! SC-T18: a stateful `init` that fails costs nothing: `init_instances` returns the error, the
//! facade never calls `run`, nothing was read or written, and `shutdown` joins the parked
//! workers cleanly. f.4, h (architecture 7).

use std::sync::Arc;

use moruna_kernel::MorunaError;

use super::common::{RigBuilder, StatefulKernel};

#[test]
fn sc_t18_stateful_init_fails() {
    let kernel = Arc::new(StatefulKernel::new(2).fail_init_for(1));
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 2;
            cfg.workers_active = 2;
        })
        .kernel(kernel.clone())
        .go();
    let outcome = rig.scheduler.init_instances();
    match outcome {
        Err(MorunaError::Kernel { msg, .. }) => assert_eq!(msg, "init failed"),
        other => panic!("expected the kernel's own error, got {other:?}"),
    }
    assert!(rig.source.reads().is_empty(), "nothing was read");
    assert!(rig.sink.written().is_empty(), "nothing was written");
    assert_eq!(rig.sink.finish_calls(), 0, "the sink was not finished");
    // `run` is never called; shutdown joins the parked workers and the two idle drives.
    rig.scheduler.shutdown();
}
