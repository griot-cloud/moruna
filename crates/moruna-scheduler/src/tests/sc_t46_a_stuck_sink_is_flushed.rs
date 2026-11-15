//! SC-T46: a sink whose watermark has not moved since the last checkpoint, while its queue
//! delivered past it and it committed past it (`Sink::lagging`), is flushed at the next
//! checkpoint, so the manifest says what it holds; a sink whose output commits in order is not.
//! f.12 (amended 2026-11-15).

use std::sync::Arc;
use std::time::Duration;

use moruna_kernel::CancelToken;
use moruna_testkit::{FakeKernel, FakePlacement, FakeSink, FakeSource};

use super::common::{RigBuilder, manifest_lock};
use crate::checkpoint::stuck;

/// The decision itself: stuck only once a checkpoint has seen the same watermark, and only
/// with morsels delivered past it.
#[test]
fn sc_t46_the_decision() {
    // No checkpoint has read the watermark yet: never stuck.
    assert!(!stuck(None, None, Some(4)));
    assert!(!stuck(None, Some(2), Some(4)));
    // The same watermark as at the last checkpoint, with morsels past it: stuck.
    assert!(stuck(Some(None), None, Some(0)));
    assert!(stuck(Some(Some(2)), Some(2), Some(4)));
    // It moved since: not stuck.
    assert!(!stuck(Some(Some(1)), Some(2), Some(4)));
    assert!(!stuck(Some(None), Some(0), Some(4)));
    // Nothing delivered past it: nothing to flush.
    assert!(!stuck(Some(Some(4)), Some(4), Some(4)));
    assert!(!stuck(Some(None), None, None));
}

/// A run whose sink commits nothing by itself (its files never fill) is flushed by a plain
/// checkpoint, and its watermark then covers what it was given before the run finishes it.
#[test]
fn sc_t46_a_stuck_sink_is_flushed_at_a_checkpoint() {
    let _guard = manifest_lock();
    let sink = FakeSink::new()
        .resumable(true)
        .commit_every(1_000_000)
        .commits_on_flush(true)
        .lags(true)
        .latency(Duration::from_millis(2));
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 2;
            cfg.workers_active = 2;
            cfg.initial_morsel_target = 8;
            cfg.checkpoint_enabled = true;
            cfg.checkpoint_interval_ms = 20;
        })
        .source(FakeSource::new().splits(4, 200, 1_600))
        .sink(sink)
        .placement(FakePlacement::new().with_manifest_store())
        .kernel(Arc::new(FakeKernel::new()))
        .go();
    let outcome = rig.scheduler.run(CancelToken::new());
    assert!(
        matches!(outcome, Ok(crate::RunOutcome::Completed { .. })),
        "{outcome:?}"
    );
    assert!(
        rig.sink.checkpoint_flush_calls() >= 1,
        "a sink whose watermark never moved was never flushed"
    );
}

/// A sink whose watermark stands still but whose output commits in order, one writer filling
/// one file, is not behind: a plain checkpoint never flushes it, so where its files end does not
/// depend on when checkpoints fell (PL-T17 resumes a run byte for byte).
#[test]
fn sc_t46_a_sink_that_does_not_lag_is_not_flushed() {
    let _guard = manifest_lock();
    let sink = FakeSink::new()
        .resumable(true)
        .commit_every(1_000_000)
        .commits_on_flush(true)
        .latency(Duration::from_millis(2));
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 2;
            cfg.workers_active = 2;
            cfg.initial_morsel_target = 8;
            cfg.checkpoint_enabled = true;
            cfg.checkpoint_interval_ms = 20;
        })
        .source(FakeSource::new().splits(4, 200, 1_600))
        .sink(sink)
        .placement(FakePlacement::new().with_manifest_store())
        .kernel(Arc::new(FakeKernel::new()))
        .go();
    let outcome = rig.scheduler.run(CancelToken::new());
    assert!(
        matches!(outcome, Ok(crate::RunOutcome::Completed { .. })),
        "{outcome:?}"
    );
    assert_eq!(
        rig.sink.checkpoint_flush_calls(),
        0,
        "a sink that commits in order was flushed"
    );
}
