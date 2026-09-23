//! SC-T9: queues close in stage order, and a stage never closes while its producer has a task
//! running. f.7.

use moruna_kernel::CancelToken;
use moruna_testkit::FakeSource;

use super::common::RigBuilder;

#[test]
fn sc_t9_draining_order() {
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 4;
            cfg.workers_active = 4;
            cfg.initial_morsel_target = 8;
        })
        .source(FakeSource::new().splits(2, 40, 320))
        .recording_placement()
        .stages(3)
        .go();
    match rig.scheduler.run(CancelToken::new()) {
        Ok(crate::RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }
    let recorder = match rig.recorder.as_ref() {
        Some(recorder) => recorder,
        None => panic!("the rig was built without the recording placement"),
    };
    let closes = recorder
        .closes
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert_eq!(
        closes,
        vec![0, 1, 2, 3],
        "queues close once each, in stage order"
    );
    // A stage closes only when its input queue is closed and empty and its producer has no
    // task running, so by the time the chain is closed every queue is empty.
    for queue in rig.placement.stats().queues {
        assert_eq!(queue.count, 0, "queue {} still held morsels", queue.stage);
    }
}
