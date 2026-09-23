//! SC-T20: a run that completes before one `checkpoint_interval_ms` has elapsed writes a final
//! manifest when `checkpoint.keep` is set, and none when it is not. f.12.
//!
//! Without this, `checkpoint.keep` kept an empty run directory on any run shorter than the
//! checkpoint interval, and 12 f.7's promise that the report names a manifest was false.

use moruna_kernel::CancelToken;
use moruna_testkit::{FakePlacement, FakeSink, FakeSource};

use super::common::{RigBuilder, manifest_lock};

/// A run far shorter than this, so the checkpoint thread never ticks.
const NEVER: u64 = 60_000;

fn run_and_count(keep: bool) -> usize {
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 2;
            cfg.workers_active = 2;
            cfg.initial_morsel_target = 8;
            cfg.checkpoint_enabled = true;
            cfg.checkpoint_interval_ms = NEVER;
            cfg.checkpoint_keep = keep;
        })
        .source(FakeSource::new().splits(2, 40, 320))
        .sink(FakeSink::new().resumable(true))
        .placement(FakePlacement::new().with_manifest_store())
        .go();
    if let Err(e) = rig.scheduler.init_instances() {
        panic!("init_instances: {e}");
    }
    match rig.scheduler.run(CancelToken::new()) {
        Ok(crate::RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }
    rig.fake_placement.manifests_written().len()
}

#[test]
fn sc_t20_manifest_on_completion() {
    let _guard = manifest_lock();
    assert_eq!(
        run_and_count(false),
        0,
        "f.12: a completed run writes no final manifest unless checkpoint.keep is set"
    );
    assert_eq!(
        run_and_count(true),
        1,
        "f.12: checkpoint.keep makes the scheduler write a final manifest on completion, so a \
         run shorter than checkpoint.interval_ms keeps something"
    );
}
