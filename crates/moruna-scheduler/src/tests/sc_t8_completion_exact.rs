//! SC-T8: random pipeline sizes and speeds; `run` returns `Ok(Completed)` with every queue
//! empty and the sink's row count equal to the source's. SC-I9.

use moruna_kernel::CancelToken;

use super::common::RigBuilder;
use crate::RunOutcome;

#[test]
fn sc_t8_completion_exact() {
    for (splits, rows, stages, workers) in
        [(1u32, 8u64, 0usize, 1u16), (3, 16, 1, 2), (2, 12, 3, 4)]
    {
        let rig = RigBuilder::new()
            .cfg(|cfg| {
                cfg.workers_max = workers;
                cfg.workers_active = workers;
            })
            .source(moruna_testkit::FakeSource::new().splits(splits, rows, rows * 8))
            .stages(stages)
            .go();
        let outcome = match rig.scheduler.run(CancelToken::new()) {
            Ok(outcome) => outcome,
            Err(e) => panic!("run failed: {e}"),
        };
        let RunOutcome::Completed { sink } = outcome else {
            panic!("expected Completed, got {outcome:?}");
        };
        assert_eq!(
            sink.rows,
            splits as u64 * rows,
            "every source row reached the sink"
        );
        for queue in rig.placement.stats().queues {
            assert_eq!(queue.count, 0, "queue {} was not empty", queue.stage);
        }
        assert_eq!(rig.sink.finish_calls(), 1, "finish runs exactly once");
    }
}
