//! SC-T7: the three error policies, and a panic that becomes a `Kernel` error rather than a
//! dead pool. SC-I8.
//!
//! `FakeKernel::fail_on` counts applies, not sequence numbers, because a kernel never learns
//! its position in the run (contracts d.15). With one worker and one stage the n-th apply
//! carries sequence number n, and the assertions are on what the scheduler recorded: the
//! outcomes in the trace, and the sequence numbers the sink was told to skip.

use std::sync::Arc;

use amoru_kernel::{AmoruError, CancelToken, ErrorPolicy, Outcome, Seq};
use amoru_testkit::{FakeKernel, FakeSource};

use super::common::RigBuilder;

fn seqs_with(records: &[amoru_kernel::TraceRecord], outcome: Outcome) -> Vec<Seq> {
    records
        .iter()
        .filter(|r| r.outcome == outcome)
        .map(|r| r.seq)
        .collect()
}

fn run(
    policy: ErrorPolicy,
    fail_on: &[usize],
) -> (Vec<amoru_kernel::TraceRecord>, Vec<Seq>, crate::RunOutcome) {
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 1;
            cfg.workers_active = 1;
            cfg.read_ahead = 1;
            cfg.initial_morsel_target = 8;
            cfg.error_policy = policy;
        })
        .source(FakeSource::new().splits(1, 30, 240))
        .kernel(Arc::new(FakeKernel::new().fail_on(fail_on)))
        .go();
    let outcome = match rig.scheduler.run(CancelToken::new()) {
        Ok(outcome) => outcome,
        Err(e) => panic!("run: {e}"),
    };
    (rig.trace.records(), rig.sink.skipped(), outcome)
}

#[test]
fn sc_t7_error_policies_terminate() {
    let (records, skipped, outcome) = run(ErrorPolicy::Terminate, &[5, 9, 12, 20]);
    let errors = seqs_with(&records, Outcome::Error);
    assert_eq!(errors, vec![5], "the first failure ends the run");
    assert!(
        seqs_with(&records, Outcome::Skipped).is_empty(),
        "terminate never skips"
    );
    assert!(skipped.is_empty(), "the sink was told of no skip");
    let crate::RunOutcome::Terminated { diagnostic, .. } = outcome else {
        panic!("expected Terminated, got {outcome:?}");
    };
    match diagnostic {
        AmoruError::Kernel { stage, seq, .. } => {
            assert_eq!((stage, seq), (1, 5), "the diagnostic names the morsel");
        }
        other => panic!("expected a Kernel diagnostic, got {other}"),
    }
}

#[test]
fn sc_t7_error_policies_skip() {
    let (records, skipped, outcome) = run(ErrorPolicy::Skip, &[5, 9, 12, 20]);
    let marked = seqs_with(&records, Outcome::Skipped);
    assert_eq!(
        marked,
        vec![5, 9, 12, 20],
        "every failing apply was skipped"
    );
    assert_eq!(
        skipped, marked,
        "the sink's skipped() lists exactly the sequence numbers the trace marks Skipped"
    );
    assert!(
        matches!(outcome, crate::RunOutcome::Completed { .. }),
        "{outcome:?}"
    );
}

#[test]
fn sc_t7_error_policies_budget() {
    let (records, skipped, outcome) = run(ErrorPolicy::Budget(3), &[5, 9, 12, 20]);
    assert_eq!(
        seqs_with(&records, Outcome::Skipped),
        vec![5, 9],
        "errors one and two are skipped"
    );
    assert_eq!(
        seqs_with(&records, Outcome::Error),
        vec![12],
        "the third ends the run"
    );
    assert_eq!(skipped, vec![5, 9]);
    assert!(
        matches!(outcome, crate::RunOutcome::Terminated { .. }),
        "{outcome:?}"
    );
}

#[test]
fn sc_t7_error_policies_panic_is_a_kernel_error() {
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 1;
            cfg.workers_active = 1;
            cfg.read_ahead = 1;
            cfg.initial_morsel_target = 8;
            cfg.error_policy = ErrorPolicy::Skip;
        })
        .source(FakeSource::new().splits(1, 20, 160))
        .kernel(Arc::new(FakeKernel::new().panic_on(&[7])))
        .go();
    // The hook is process wide and these tests run in parallel: suppress this kernel's own
    // panic only, so a concurrent test's assertion message is not swallowed with it.
    super::common::quiet_kernel_panics();
    let outcome = rig.scheduler.run(CancelToken::new());
    let Ok(crate::RunOutcome::Completed { .. }) = outcome else {
        panic!("a panicking kernel must not stop the pool: {outcome:?}");
    };
    let records = rig.trace.records();
    let panicked: Vec<_> = records
        .iter()
        .filter(|r| r.outcome == Outcome::Skipped)
        .collect();
    assert_eq!(panicked.len(), 1, "one morsel was lost to the panic");
    let message = panicked[0].error.clone().unwrap_or_default();
    assert!(
        message.contains("panic:"),
        "the error should be a Kernel error naming the panic, got {message:?}"
    );
    assert_eq!(rig.sink.skipped(), vec![panicked[0].seq]);
}
