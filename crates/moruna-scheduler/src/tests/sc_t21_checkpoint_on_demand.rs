//! SC-T21 (MH 4.3 `checkpoint`, 4.7, added by F8.4): a manifest asked for mid-run is written on
//! the checkpoint thread at once, not at the end of the interval, and the path comes back to
//! the caller; a run that writes no manifests, or is not running, says so rather than waiting.

use std::sync::Arc;
use std::time::{Duration, Instant};

use moruna_kernel::{CancelToken, MorunaError};
use moruna_testkit::{FakePlacement, FakeSink, FakeSource};

use super::common::{Latch, RigBuilder, StatefulKernel, manifest_lock};

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

    // Before `run` there is no checkpoint thread to serve a request.
    match rig.scheduler.checkpoint_now(Duration::from_millis(30)) {
        Err(MorunaError::Resume(msg)) => assert!(msg.contains("no manifest"), "{msg}"),
        other => panic!("expected a timeout naming the state, got {other:?}"),
    }

    let cancel = CancelToken::new();
    let token = cancel.clone();
    std::thread::scope(|scope| {
        let handle = scope.spawn(|| rig.scheduler.run(token));
        assert!(
            gate.wait_holding(Duration::from_secs(30)),
            "the run never reached the latch"
        );
        let before = rig.fake_placement.manifests_written().len();
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

/// A source whose reads of one split stay pending until the test lets them go, so a manifest
/// is written with reads demonstrably in flight.
struct HeldSource {
    inner: FakeSource,
    held_split: u32,
    release: Arc<std::sync::atomic::AtomicBool>,
    pending: Arc<std::sync::atomic::AtomicUsize>,
}

impl moruna_kernel::Source for HeldSource {
    fn schema(&self) -> moruna_kernel::SourceSchema {
        moruna_kernel::Source::schema(&self.inner)
    }

    fn plan(&self) -> moruna_kernel::Result<Vec<moruna_kernel::Split>> {
        moruna_kernel::Source::plan(&self.inner)
    }

    fn read<'a>(
        &'a self,
        split: &'a moruna_kernel::Split,
        rows: Option<moruna_kernel::RowRange>,
        alloc: &'a dyn moruna_kernel::Allocator,
        tier: moruna_kernel::Tier,
    ) -> moruna_kernel::BoxFuture<'a, moruna_kernel::Result<moruna_kernel::Payload>> {
        use std::sync::atomic::Ordering;
        let held = split.id == self.held_split;
        if held {
            self.pending.fetch_add(1, Ordering::SeqCst);
        }
        let release = Arc::clone(&self.release);
        let mut inner = moruna_kernel::Source::read(&self.inner, split, rows, alloc, tier);
        Box::pin(std::future::poll_fn(move |cx| {
            if held && !release.load(Ordering::SeqCst) {
                cx.waker().wake_by_ref();
                return std::task::Poll::Pending;
            }
            inner.as_mut().poll(cx)
        }))
    }
}

/// MH 4.7, the in-flight read gap (10 l): a manifest written while reads are outstanding names
/// them as issued, and a restore hands each back to be read again.
#[test]
fn sc_t21_a_read_in_flight_is_named_in_the_manifest() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let _guard = manifest_lock();
    let release = Arc::new(AtomicBool::new(false));
    let pending = Arc::new(AtomicUsize::new(0));
    let source = HeldSource {
        inner: FakeSource::new().splits(3, 20, 160),
        held_split: 1,
        release: release.clone(),
        pending: pending.clone(),
    };
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
        let point = match moruna_kernel::Placement::restore(&restored, &path, &plan, &[]) {
            Ok(point) => point,
            Err(e) => panic!("restore: {e}"),
        };
        assert!(
            !point.extras.issued.is_empty(),
            "the held reads are in the manifest"
        );
        for (seq, origin) in &point.extras.issued {
            assert!(
                *seq < point.extras.source_cursor.next_seq,
                "behind the cursor"
            );
            if origin.split == 1 {
                assert!(
                    point.to_recompute.iter().any(|(s, _)| s == seq),
                    "held read {seq} comes back to be read again"
                );
            }
        }
        release.store(true, Ordering::SeqCst);
        cancel.cancel();
        let _ = handle.join();
    });
}
