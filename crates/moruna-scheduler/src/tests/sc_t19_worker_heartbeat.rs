//! SC-T19: a worker that leaves its loop without reporting is noticed within two heartbeat
//! intervals and ends the run with an internal diagnostic, whether the checkpoint thread or
//! the sink drive's bounded park is the one checking; a long kernel is never called dead.
//! SC-I11, f.14.

use std::sync::Arc;
use std::time::{Duration, Instant};

use moruna_kernel::{CancelToken, MorunaError};
use moruna_testkit::{FakeKernel, FakePlacement, FakeSink, FakeSource};

use super::common::{RigBuilder, manifest_lock, wait_for};

fn kill_and_wait(checkpointing: bool) -> crate::RunOutcome {
    let guard = if checkpointing {
        Some(manifest_lock())
    } else {
        None
    };
    let placement = if checkpointing {
        FakePlacement::new().with_manifest_store()
    } else {
        FakePlacement::new()
    };
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 2;
            cfg.workers_active = 2;
            cfg.initial_morsel_target = 8;
            cfg.heartbeat_interval_ms = 1000;
            cfg.checkpoint_enabled = checkpointing;
            cfg.checkpoint_interval_ms = 50;
        })
        .source(FakeSource::new().splits(4, 400, 3_200))
        .sink(FakeSink::new().resumable(checkpointing))
        .placement(placement)
        .kernel(Arc::new(
            FakeKernel::new().latency(Duration::from_millis(1)),
        ))
        .go();

    let cancel = CancelToken::new();
    let started = Instant::now();
    let outcome = std::thread::scope(|scope| {
        let handle = scope.spawn(|| rig.scheduler.run(cancel));
        assert!(
            wait_for(Duration::from_secs(5), || !rig.sink.written().is_empty()),
            "the run never got going"
        );
        rig.scheduler.test_kill_worker(1);
        match handle.join() {
            Ok(outcome) => outcome,
            Err(_) => panic!("the run thread panicked"),
        }
    });
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(10),
        "the death took {took:?} to notice"
    );
    drop(guard);
    match outcome {
        Ok(outcome) => outcome,
        Err(e) => panic!("run: {e}"),
    }
}

#[test]
fn sc_t19_worker_heartbeat_with_the_checkpoint_thread_checking() {
    let outcome = kill_and_wait(true);
    let crate::RunOutcome::Terminated { diagnostic, .. } = outcome else {
        panic!("a dead worker must end the run, got {outcome:?}");
    };
    match diagnostic {
        MorunaError::Kernel { msg, .. } => assert!(
            msg.contains("worker 1 died outside apply"),
            "the diagnostic names the worker: {msg}"
        ),
        other => panic!("expected the internal Kernel diagnostic, got {other}"),
    }
}

#[test]
fn sc_t19_worker_heartbeat_with_the_sink_drive_checking() {
    let outcome = kill_and_wait(false);
    let crate::RunOutcome::Terminated { diagnostic, .. } = outcome else {
        panic!("a dead worker must end the run, got {outcome:?}");
    };
    assert!(
        diagnostic
            .to_string()
            .contains("worker 1 died outside apply"),
        "the diagnostic names the worker: {diagnostic}"
    );
}

#[test]
fn sc_t19_a_long_kernel_is_not_a_dead_worker() {
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 2;
            cfg.workers_active = 2;
            cfg.initial_morsel_target = 8;
            cfg.heartbeat_interval_ms = 50;
        })
        .source(FakeSource::new().splits(1, 2, 16))
        .kernel(Arc::new(
            FakeKernel::new().latency(Duration::from_millis(300)),
        ))
        .go();
    match rig.scheduler.run(CancelToken::new()) {
        Ok(crate::RunOutcome::Completed { .. }) => {}
        other => panic!("a kernel slower than the heartbeat interval is slow, not dead: {other:?}"),
    }
}
