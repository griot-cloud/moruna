//! SC-T14: the commit watermark moves with the sink and never backwards, and a skipped
//! sequence number does not hold it back. f.11, f.8.
//!
//! `FakeKernel::fail_on` counts applies (contracts d.15), so the test runs one worker and one
//! stage and asserts on what the scheduler recorded: the sequence numbers the trace marks
//! `Skipped` and the ones the sink was told to skip must be the same list, in the same order.

use std::sync::Arc;

use moruna_kernel::{CancelToken, ErrorPolicy, Outcome, Seq};
use moruna_testkit::{FakeKernel, FakeSink, FakeSource};

use super::common::RigBuilder;

#[test]
fn sc_t14_watermark_and_skips() {
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 1;
            cfg.workers_active = 1;
            cfg.read_ahead = 1;
            cfg.sink_concurrency = 1;
            cfg.initial_morsel_target = 8;
            cfg.error_policy = ErrorPolicy::Skip;
        })
        .source(FakeSource::new().splits(1, 40, 320))
        .sink(FakeSink::new().commit_every(10))
        .recording_placement()
        .kernel(Arc::new(FakeKernel::new().fail_on(&[7, 23, 24])))
        .go();
    match rig.scheduler.run(CancelToken::new()) {
        Ok(crate::RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }

    let marked: Vec<Seq> = rig
        .trace
        .records()
        .into_iter()
        .filter(|r| r.outcome == Outcome::Skipped)
        .map(|r| r.seq)
        .collect();
    assert_eq!(marked, vec![7, 23, 24], "three morsels were skipped");
    assert_eq!(
        rig.sink.skipped(),
        marked,
        "the sink's skipped() equals, exactly and in order, the sequence numbers the trace marks Skipped"
    );

    let recorder = match rig.recorder.as_ref() {
        Some(recorder) => recorder,
        None => panic!("the rig was built without the recording placement"),
    };
    let committed = recorder
        .committed
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert!(!committed.is_empty(), "the watermark moved");
    let mut previous = 0u64;
    for seq in &committed {
        assert!(
            *seq >= previous,
            "the watermark went backwards: {committed:?}"
        );
        previous = *seq;
    }
    // Skips do not hold the watermark back: with 40 morsels, three of them skipped and a sink
    // committing in blocks of ten, the last watermark is the last sequence number of the run.
    assert_eq!(
        rig.fake_placement.committed(),
        Some(39),
        "the final watermark is the last sequence number the run issued"
    );
}
