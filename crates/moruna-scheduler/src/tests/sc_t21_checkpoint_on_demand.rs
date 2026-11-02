//! SC-T21 (MH 4.3 `checkpoint`, 4.7, added by F8.4): a manifest asked for mid-run is written on
//! the checkpoint thread at once, not at the end of the interval, and the path comes back to
//! the caller; a run that writes no manifests, or is not running, says so rather than waiting.

use std::sync::Arc;
use std::time::{Duration, Instant};

use moruna_kernel::{CancelToken, MorunaError};
use moruna_testkit::{FakePlacement, FakeSink, FakeSource};

use super::common::{HeldSource, Latch, Release, RigBuilder, StatefulKernel, manifest_lock};

#[test]
fn sc_t21_checkpoint_on_demand() {
    let _guard = manifest_lock();
    let gate = Latch::after(4);
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 2;
            cfg.workers_active = 2;
            cfg.initial_morsel_target = 8;
            cfg.checkpoint_enabled = true;
            // Far longer than the test: every manifest it sees was asked for.
            cfg.checkpoint_interval_ms = 600_000;
        })
        .source(FakeSource::new().splits(4, 60, 480))
        .sink(FakeSink::new().resumable(true).commit_every(5))
        .placement(FakePlacement::new().with_manifest_store())
        .kernel(Arc::new(StatefulKernel::new(0).gated(gate.clone())))
        .go();

    // Before `run` there is no checkpoint thread to serve a request: refused at once, naming
    // the state, and not left counted for the thread to serve once the run starts.
    match rig.scheduler.checkpoint_now(Duration::from_secs(30)) {
        Err(MorunaError::Resume(msg)) => {
            assert!(msg.contains("no manifest") && msg.contains("Init"), "{msg}")
        }
        other => panic!("expected a refusal naming the state, got {other:?}"),
    }

    let cancel = CancelToken::new();
    let token = cancel.clone();
    std::thread::scope(|scope| {
        let handle = scope.spawn(|| rig.scheduler.run(token));
        let _release = Release(|| {
            cancel.cancel();
            gate.open();
        });
        assert!(
            gate.wait_holding(Duration::from_secs(30)),
            "the run never reached the latch"
        );
        let before = rig.fake_placement.manifests_written().len();
        assert_eq!(
            before, 0,
            "the request refused before the run left nothing for the checkpoint thread to serve"
        );
        let asked = Instant::now();
        let path = match rig.scheduler.checkpoint_now(Duration::from_secs(10)) {
            Ok(path) => path,
            Err(e) => panic!("checkpoint_now: {e}"),
        };
        assert!(
            asked.elapsed() < Duration::from_secs(5),
            "written at once, not at the end of the interval"
        );
        let written = rig.fake_placement.manifests_written();
        assert_eq!(
            written.len(),
            before + 1,
            "exactly one manifest per request"
        );
        assert_eq!(written.last(), Some(&path), "the path of that manifest");
        assert!(rig.scheduler.request_checkpoint(), "a request is accepted");
        cancel.cancel();
        gate.open();
        let _ = handle.join();
    });
}

#[test]
fn sc_t21_checkpoint_on_demand_refused_without_manifests() {
    let _guard = manifest_lock();
    let rig = RigBuilder::new()
        .cfg(|cfg| cfg.checkpoint_enabled = false)
        .sink(FakeSink::new().resumable(true))
        .stages(1)
        .go();
    assert!(!rig.scheduler.request_checkpoint());
    match rig.scheduler.checkpoint_now(Duration::from_secs(1)) {
        Err(MorunaError::Resume(msg)) => assert!(msg.contains("off"), "{msg}"),
        other => panic!("expected Resume, got {other:?}"),
    }
}

/// MH 4.7, the in-flight read gap (10 l): a manifest written while reads are outstanding names
/// them as issued, and a restore hands each back to be read again.
#[test]
fn sc_t21_a_read_in_flight_is_named_in_the_manifest() {
    use std::sync::atomic::Ordering;
    let _guard = manifest_lock();
    let source = HeldSource::new(FakeSource::new().splits(3, 20, 160), 1);
    let release = source.release.clone();
    let pending = source.pending.clone();
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 2;
            cfg.workers_active = 2;
            cfg.initial_morsel_target = 64;
            cfg.checkpoint_enabled = true;
            cfg.checkpoint_interval_ms = 600_000;
        })
        .source_over(Arc::new(source))
        .sink(FakeSink::new().resumable(true).commit_every(2))
        .placement(FakePlacement::new().with_manifest_store())
        .stages(1)
        .go();
    let cancel = CancelToken::new();
    let token = cancel.clone();
    std::thread::scope(|scope| {
        let handle = scope.spawn(|| rig.scheduler.run(token));
        let _release = Release(|| {
            release.store(true, Ordering::SeqCst);
            cancel.cancel();
        });
        assert!(
            super::common::wait_for(Duration::from_secs(10), || pending.load(Ordering::SeqCst)
                > 0),
            "a read of the held split was issued"
        );
        let path = match rig.scheduler.checkpoint_now(Duration::from_secs(10)) {
            Ok(path) => path,
            Err(e) => panic!("checkpoint_now: {e}"),
        };
        let plan = match moruna_kernel::Source::plan(&FakeSource::new().splits(3, 20, 160)) {
            Ok(plan) => plan,
            Err(e) => panic!("plan: {e}"),
        };
        let restored = FakePlacement::new().with_manifest_store();
        let point = match moruna_kernel::Placement::restore(&restored, &path, &[plan], &[]) {
            Ok(point) => point,
            Err(e) => panic!("restore: {e}"),
        };
        assert!(
            !point.extras.issued[0].is_empty(),
            "the held reads are in the manifest"
        );
        for (seq, origin) in &point.extras.issued[0] {
            assert!(
                *seq < point.extras.source_cursors[0].next_seq,
                "behind the cursor"
            );
            if origin.split >= 1 {
                assert!(
                    point.to_recompute[0].iter().any(|(s, _)| s == seq),
                    "held read {seq} comes back to be read again"
                );
            }
        }
        release.store(true, Ordering::SeqCst);
        cancel.cancel();
        let _ = handle.join();
    });
}
