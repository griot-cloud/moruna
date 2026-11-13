//! SC-T43 (RB5): a unit whose output the arena refuses (`MorunaError::Alloc` out of `apply`)
//! is a wall, not an error. The worker keeps a share of its input, helps drain, waits and
//! applies again, so the run completes with every morsel once and no outcome is `Error`,
//! whatever the error policy, which a kernel's own error would still end the run under.

use std::sync::Arc;

use moruna_kernel::{CancelToken, ErrorPolicy, Outcome};
use moruna_testkit::{FakeKernel, FakeSource};

use super::common::RigBuilder;

#[test]
fn sc_t43_a_refused_output_waits_and_is_applied_again() {
    let kernel = Arc::new(FakeKernel::new().refuse_on(&[3, 4, 9]));
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 1;
            cfg.workers_active = 1;
            cfg.read_ahead = 1;
            cfg.initial_morsel_target = 8;
            cfg.error_policy = ErrorPolicy::Terminate;
        })
        .source(FakeSource::new().splits(1, 30, 240))
        .kernel(Arc::clone(&kernel) as Arc<dyn moruna_kernel::Kernel>)
        .go();
    let outcome = rig.scheduler.run(CancelToken::new()).expect("run");
    assert!(
        matches!(outcome, crate::RunOutcome::Completed { .. }),
        "the refusals were waited out: {outcome:?}"
    );
    let records = rig.trace.records();
    assert!(
        records.iter().all(|r| r.outcome != Outcome::Error),
        "no unit failed"
    );
    let mut written = rig.sink.written();
    written.sort_unstable();
    assert_eq!(
        written,
        (0..written.len() as u64).collect::<Vec<_>>(),
        "every morsel reached the sink once"
    );
    assert!(!written.is_empty());
    assert_eq!(
        kernel.applies().len(),
        written.len() + 3,
        "each refusal cost one more apply"
    );
}
