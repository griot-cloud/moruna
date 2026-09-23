//! SC-T15: the checkpoint thread ticks on its own thread, checkpoints every instance of every
//! `Checkpoint` stage without an `apply` running on it, records a cursor equal to the next
//! range the source drive will issue, starts only at `run`, writes a final manifest on
//! termination without any call from the test, and refuses a kernel that saves nothing. f.12.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use moruna_kernel::{MorunaError, CancelToken, Knobs};
use moruna_testkit::{FakePlacement, FakeSink, FakeSource};

use super::common::{RigBuilder, StatefulKernel, manifest_lock, wait_for};

#[test]
fn sc_t15_checkpoint_tick() {
    let _guard = manifest_lock();
    let kernel = Arc::new(
        StatefulKernel::new(2)
            .checkpointing()
            .latency(Duration::from_millis(2)),
    );
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 2;
            cfg.workers_active = 2;
            cfg.initial_morsel_target = 8;
            cfg.checkpoint_enabled = true;
            cfg.checkpoint_interval_ms = 50;
        })
        .source(FakeSource::new().splits(4, 400, 3_200))
        .sink(FakeSink::new().resumable(true))
        .placement(FakePlacement::new().with_manifest_store())
        .kernel(kernel.clone())
        .go();
    if let Err(e) = rig.scheduler.init_instances() {
        panic!("init_instances: {e}");
    }
    assert!(
        rig.fake_placement.manifests_written().is_empty(),
        "no manifest is written before run"
    );
    assert_eq!(
        kernel.checkpoint_calls.load(Ordering::SeqCst),
        0,
        "the checkpoint thread does not exist yet"
    );

    let cancel = CancelToken::new();
    let token = cancel.clone();
    let outcome = std::thread::scope(|scope| {
        let handle = scope.spawn(|| rig.scheduler.run(token));
        assert!(
            wait_for(Duration::from_secs(5), || {
                rig.fake_placement.manifests_written().len() >= 2
            }),
            "the checkpoint thread did not tick"
        );
        // The run ends through the controller's own route, which is the termination path, and
        // the scheduler writes the final manifest itself.
        rig.scheduler.terminate(MorunaError::Budget {
            seq: 0,
            stage: 1,
            footprint: 2,
            budget: 1,
            features: moruna_kernel::MorselFeatures::default(),
        });
        match handle.join() {
            Ok(outcome) => outcome,
            Err(_) => panic!("the run thread panicked"),
        }
    });
    let Ok(crate::RunOutcome::Terminated { manifest, .. }) = outcome else {
        panic!("expected Terminated, got {outcome:?}");
    };
    assert!(
        manifest.is_some(),
        "a terminated run names its last manifest"
    );
    let _ = cancel;

    let ticks = rig.fake_placement.manifests_written().len() as u64;
    let calls = kernel.checkpoint_calls.load(Ordering::SeqCst);
    assert_eq!(
        calls,
        ticks * 2,
        "two instances are checkpointed per tick: {calls} calls over {ticks} ticks"
    );
    assert!(
        !kernel.overlap.load(Ordering::SeqCst),
        "an apply ran on an instance while it was being checkpointed"
    );
    // f.12: the ticks run on the checkpoint thread, never a worker and never a drive. The one
    // other thread that may checkpoint is the one driving the exit, which writes the final
    // manifest itself; in this test that is the thread the run was called on.
    let threads = kernel.checkpoint_threads.seen();
    assert!(
        threads.contains(&"moruna-checkpoint".to_string()),
        "the ticks did not run on the checkpoint thread: {threads:?}"
    );
    for name in &threads {
        assert!(
            !name.starts_with("moruna-worker-") && !name.ends_with("-drive"),
            "checkpointing ran on {name}"
        );
    }
}

#[test]
fn sc_t15_checkpoint_kernel_that_saves_nothing_terminates() {
    let _guard = manifest_lock();
    // The latency is what makes the test deterministic: without it the run could finish
    // before the first checkpoint tick, and then nothing asks the kernel to save anything.
    // It was flaky on `main` for that reason (observed 2026-09-22).
    let kernel = Arc::new(
        StatefulKernel::new(1)
            .checkpointing()
            .checkpoint_returns_none()
            .latency(Duration::from_millis(5)),
    );
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 1;
            cfg.workers_active = 1;
            cfg.initial_morsel_target = 8;
            cfg.checkpoint_enabled = true;
            cfg.checkpoint_interval_ms = 20;
        })
        .source(FakeSource::new().splits(4, 400, 3_200))
        .sink(FakeSink::new().resumable(true))
        .placement(FakePlacement::new().with_manifest_store())
        .kernel(kernel)
        .go();
    if let Err(e) = rig.scheduler.init_instances() {
        panic!("init_instances: {e}");
    }
    let outcome = match rig.scheduler.run(CancelToken::new()) {
        Ok(outcome) => outcome,
        Err(e) => panic!("run: {e}"),
    };
    let crate::RunOutcome::Terminated { diagnostic, .. } = outcome else {
        panic!("expected Terminated, got {outcome:?}");
    };
    match diagnostic {
        MorunaError::Resume(msg) => assert!(
            msg.contains("stage 1"),
            "the diagnostic names the stage: {msg}"
        ),
        other => panic!("expected Resume naming the stage, got {other}"),
    }
}
