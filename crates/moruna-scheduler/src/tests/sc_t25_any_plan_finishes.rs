//! SC-T25: any plan, of any number of splits with any row counts, zeros included, through any
//! number of kernel stages, completes, and the sink receives exactly the sum of the splits'
//! rows. A property over seeded cases. f.5, SC-I9.

use std::sync::Arc;
use std::time::Duration;

use moruna_kernel::Source;

use super::common::{Cases, RigBuilder, RowsSource, run_within};
use crate::RunOutcome;

/// How many plans the property is checked on.
const CASES: usize = 200;

#[test]
fn sc_t25_any_plan_finishes() {
    let mut cases = Cases::seeded(0x5c_7250);
    for case in 0..CASES {
        let splits = cases.below(7) as usize;
        // Half the splits hold no rows, so plans of zeros only, zeros first, between and last
        // all come up.
        let rows: Vec<u64> = (0..splits)
            .map(|_| match cases.below(2) {
                0 => 0,
                _ => 1 + cases.below(40),
            })
            .collect();
        let stages = cases.below(3) as usize;
        let workers = 1 + cases.below(3) as u16;
        let read_ahead = cases.below(4) as u16;
        let sub_splittable = cases.below(2) == 1;
        let what = format!(
            "case {case}: rows {rows:?}, {stages} stages, {workers} workers, read_ahead \
             {read_ahead}, sub_splittable {sub_splittable}"
        );
        let source = Arc::new(RowsSource::new(&rows, sub_splittable));
        let rig = RigBuilder::new()
            .cfg(|cfg| {
                cfg.workers_max = workers;
                cfg.workers_active = workers;
                cfg.read_ahead = read_ahead;
            })
            .source_over(source as Arc<dyn Source>)
            .stages(stages)
            .go();
        let outcome = run_within(&rig, Duration::from_secs(10), &what);
        let RunOutcome::Completed { sink } = outcome else {
            panic!("{what}: expected Completed, got {outcome:?}");
        };
        assert_eq!(
            sink.rows,
            rows.iter().sum::<u64>(),
            "{what}: the sink holds the sum of the splits' rows"
        );
        for queue in rig.placement.stats().queues {
            assert_eq!(
                queue.count, 0,
                "{what}: queue {} was not empty",
                queue.stage
            );
        }
    }
}
