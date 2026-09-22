//! SC-T13: an entry the engine evicted under pressure is re-read from its origin and put back
//! with `replace`, and the run's output is the same as one without pressure. PL-I6 interplay.

use amoru_kernel::CancelToken;
use amoru_testkit::{FakePlacement, FakeSource};

use super::common::RigBuilder;

/// What one run of this test observed: what the sink wrote, what the source read, and which
/// sequence numbers were put back with `replace`.
type Observed = (
    Vec<u64>,
    Vec<(u32, Option<amoru_kernel::RowRange>)>,
    Vec<u64>,
);

fn run(pressure: Option<u64>) -> Observed {
    let placement = match pressure {
        Some(bytes) => FakePlacement::new().with_pressure(0, bytes),
        None => FakePlacement::new(),
    };
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 2;
            cfg.workers_active = 2;
            cfg.initial_morsel_target = 8;
            cfg.read_ahead = 4;
        })
        .source(FakeSource::new().splits(2, 20, 160))
        .placement(placement)
        .recording_placement()
        .stages(1)
        .go();
    match rig.scheduler.run(CancelToken::new()) {
        Ok(crate::RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }
    let replaced = match rig.recorder.as_ref() {
        Some(recorder) => recorder
            .replaced
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
        None => Vec::new(),
    };
    let mut written = rig.sink.written();
    written.sort_unstable();
    (written, rig.source.reads(), replaced)
}

#[test]
fn sc_t13_evicted_replay() {
    let (clean, clean_reads, clean_replaced) = run(None);
    assert!(
        clean_replaced.is_empty(),
        "nothing is evicted without pressure"
    );
    let (pressed, pressed_reads, replaced) = run(Some(64));
    assert!(
        !replaced.is_empty(),
        "the pressure knob evicted nothing, so the path was not exercised"
    );
    assert_eq!(
        pressed, clean,
        "the output is the same with and without pressure"
    );
    assert!(
        pressed_reads.len() > clean_reads.len(),
        "an evicted entry costs a second read of the same origin"
    );
    // Every replacement read names an origin the run had already read.
    for seq in &replaced {
        assert!(
            clean.contains(seq),
            "sequence {seq} was replaced but never written"
        );
    }
}
