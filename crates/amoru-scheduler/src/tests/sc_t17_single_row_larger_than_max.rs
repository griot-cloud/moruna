//! SC-T17: a single row whose estimated bytes exceed the maximum morsel is read one row at a
//! time and passes at its natural size; a row larger than the budget fails the read with
//! `Alloc` and ends the run naming the split and the row range. f.5, h (architecture 7).

use amoru_kernel::{AmoruError, CancelToken, Tier};
use amoru_testkit::{FakeAllocator, FakeSource};

use super::common::RigBuilder;

const GIB: u64 = 1024 * 1024 * 1024;

#[test]
fn sc_t17_single_row_larger_than_max() {
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 1;
            cfg.workers_active = 1;
            cfg.read_ahead = 1;
            cfg.morsel_min = 4 * 1024 * 1024;
            cfg.morsel_max = 512 * 1024 * 1024;
            cfg.initial_morsel_target = 512 * 1024 * 1024;
        })
        // One split of four rows and four GiB: one GiB a row, twice the maximum morsel.
        .source(FakeSource::new().splits(1, 4, 4 * GIB).sub_splittable(true))
        .stages(1)
        .go();
    match rig.scheduler.run(CancelToken::new()) {
        Ok(crate::RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }
    let ranges: Vec<(u64, u64)> = rig
        .source
        .reads()
        .into_iter()
        .filter_map(|(_, range)| range.map(|r| (r.start, r.end)))
        .collect();
    assert_eq!(
        ranges,
        vec![(0, 1), (1, 2), (2, 3), (3, 4)],
        "the drive issues four one-row reads"
    );
    assert_eq!(rig.sink.written().len(), 4, "every row reached the sink");
    // The fake source materialises eight bytes a row rather than a gigabyte, so the trace
    // cannot show a `bytes_in` of one GiB; what it does show is the one row per morsel that
    // the estimate above `morsel_max` forced.
    for record in rig.trace.records() {
        assert_eq!(record.rows_in, 1, "each morsel carries exactly one row");
    }
}

#[test]
fn sc_t17_single_row_larger_than_the_budget_terminates() {
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 1;
            cfg.workers_active = 1;
            cfg.read_ahead = 1;
            cfg.morsel_min = 4 * 1024 * 1024;
            cfg.morsel_max = 512 * 1024 * 1024;
            cfg.initial_morsel_target = 512 * 1024 * 1024;
        })
        .source(FakeSource::new().splits(1, 4, 4 * GIB).sub_splittable(true))
        // A host tier too small for even one row's buffer.
        .alloc(FakeAllocator::new().with_limit(Tier::Host, 4))
        .stages(1)
        .go();
    let outcome = match rig.scheduler.run(CancelToken::new()) {
        Ok(outcome) => outcome,
        Err(e) => panic!("run: {e}"),
    };
    let crate::RunOutcome::Terminated { diagnostic, .. } = outcome else {
        panic!("expected Terminated, got {outcome:?}");
    };
    match diagnostic {
        AmoruError::Source { split, msg } => {
            assert_eq!(split, 0, "the diagnostic names the split");
            assert!(msg.contains("rows 0..1"), "it names the row range: {msg}");
            assert!(
                msg.contains("alloc"),
                "it names the allocation failure: {msg}"
            );
        }
        other => panic!("expected a Source diagnostic naming the split, got {other}"),
    }
    assert!(rig.sink.written().is_empty(), "nothing was written");
}
