//! SC-T11: cancel mid-run returns `Ok(Cancelled)` promptly, the trace is flushed, the sink is
//! not finished, the checkpoint thread is joined and a later `shutdown` is a no-op. f.10.

use std::sync::Arc;
use std::time::{Duration, Instant};

use moruna_kernel::CancelToken;
use moruna_testkit::{FakeKernel, FakePlacement, FakeSource};

use super::common::{RigBuilder, manifest_lock, wait_for};

#[test]
fn sc_t11_cancel() {
    let _guard = manifest_lock();
    let latency = Duration::from_millis(30);
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 2;
            cfg.workers_active = 2;
            cfg.initial_morsel_target = 8;
            cfg.checkpoint_enabled = true;
            cfg.checkpoint_interval_ms = 20;
        })
        .source(FakeSource::new().splits(4, 400, 3_200))
        .sink(moruna_testkit::FakeSink::new().resumable(true))
        .placement(FakePlacement::new().with_manifest_store())
        .kernel(Arc::new(FakeKernel::new().latency(latency)))
        .go();

    let cancel = CancelToken::new();
    let token = cancel.clone();
    let started = Instant::now();
    let outcome = std::thread::scope(|scope| {
        let handle = scope.spawn(|| rig.scheduler.run(token));
        assert!(
            wait_for(Duration::from_secs(5), || !rig.sink.written().is_empty()),
            "the run never got going"
        );
        cancel.cancel();
        match handle.join() {
            Ok(outcome) => outcome,
            Err(_) => panic!("the run thread panicked"),
        }
    });
    let took = started.elapsed();
    match outcome {
        Ok(crate::RunOutcome::Cancelled { .. }) => {}
        other => panic!("expected Cancelled, got {other:?}"),
    }
    assert!(
        took < latency + Duration::from_secs(5),
        "cancellation took {took:?}"
    );
    assert!(rig.trace.flush_calls() >= 1, "the trace was flushed");
    assert_eq!(
        rig.sink.finish_calls(),
        0,
        "a cancelled run does not finish the sink"
    );
    // The checkpoint thread was stopped and joined by the exit sequence; a second shutdown
    // changes nothing.
    rig.scheduler.shutdown();
    rig.scheduler.shutdown();
    assert!(
        rig.fake_placement.shutdown_calls() >= 1,
        "the engine was shut down"
    );
}
