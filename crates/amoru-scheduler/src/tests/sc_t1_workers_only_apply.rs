//! SC-T1: no worker thread ever executes reactor code or waits on a completion; the two drive
//! threads are the only ones that do. SC-I1, G-I9.
//!
//! The instrumentation is the thread name: the scheduler names its threads `amoru-worker-N`,
//! `amoru-source-drive` and `amoru-sink-drive`, and the test records the name of the thread
//! every `Source::read`, every `Sink::write` and every `Kernel::apply` ran on. Waiting on a
//! `Completion` happens only inside those two drive functions, so an IO call landing on a
//! worker is the observable form of the invariant breaking.

use std::sync::Arc;

use amoru_kernel::CancelToken;
use amoru_testkit::FakeSource;

use super::common::{NamingKernel, NamingSink, NamingSource, RigBuilder};

#[test]
fn sc_t1_workers_only_apply() {
    let source = NamingSource::new(FakeSource::new().splits(3, 16, 128));
    let read_names = Arc::clone(&source.names);
    let sink = NamingSink::new(amoru_testkit::FakeSink::new());
    let write_names = Arc::clone(&sink.names);
    let kernel = NamingKernel::new();
    let apply_names = Arc::clone(&kernel.names);

    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 4;
            cfg.workers_active = 4;
        })
        .source_over(Arc::new(source))
        .sink_over(Box::new(sink))
        .kernel(Arc::new(kernel))
        .go();
    match rig.scheduler.run(CancelToken::new()) {
        Ok(crate::RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }

    let reads = read_names.seen();
    let writes = write_names.seen();
    let applies = apply_names.seen();
    assert_eq!(
        reads,
        vec!["amoru-source-drive".to_string()],
        "reads: {reads:?}"
    );
    assert_eq!(
        writes,
        vec!["amoru-sink-drive".to_string()],
        "writes: {writes:?}"
    );
    assert!(!applies.is_empty(), "the kernel ran");
    for name in &applies {
        assert!(
            name.starts_with("amoru-worker-"),
            "apply ran on {name}, which is not a worker"
        );
    }
    for name in reads.iter().chain(writes.iter()) {
        assert!(
            !applies.contains(name),
            "{name} both ran a kernel and issued IO"
        );
    }
}
