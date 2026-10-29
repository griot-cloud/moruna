//! SC-T24: a plan whose splits hold no rows, alone, first, between non-empty ones or last,
//! completes, the sink receives exactly the rows of the non-empty splits, and no read names a
//! split with no rows. f.5, SC-I9.

use std::sync::Arc;
use std::time::Duration;

use moruna_kernel::Source;

use super::common::{RigBuilder, RowsSource, run_within};
use crate::RunOutcome;

#[test]
fn sc_t24_zero_row_splits() {
    let plans: [&[u64]; 6] = [
        &[0],
        &[0, 0, 0],
        &[5, 0, 7],
        &[5, 7, 0],
        &[0, 5],
        &[0, 3, 0],
    ];
    for rows in plans {
        for stages in [0usize, 1] {
            let what = format!("plan {rows:?} through {stages} kernel stages");
            let source = Arc::new(RowsSource::new(rows, true));
            let rig = RigBuilder::new()
                .source_over(source.clone() as Arc<dyn Source>)
                .stages(stages)
                .go();
            let outcome = run_within(&rig, Duration::from_secs(10), &what);
            let RunOutcome::Completed { sinks } = outcome else {
                panic!("{what}: expected Completed, got {outcome:?}");
            };
            let sink = &sinks[0];
            assert_eq!(
                sink.rows,
                rows.iter().sum::<u64>(),
                "{what}: the sink holds exactly the rows of the non-empty splits"
            );
            assert_eq!(rig.sink.finish_calls(), 1, "{what}: finish runs once");
            for split in source.read_splits() {
                assert_ne!(
                    rows[split as usize], 0,
                    "{what}: split {split} holds no rows and was read"
                );
            }
        }
    }
}
