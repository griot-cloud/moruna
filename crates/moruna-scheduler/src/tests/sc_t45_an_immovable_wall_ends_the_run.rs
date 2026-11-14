//! SC-T45 (RB5, amended 2026-11-14): a wall that nothing in the run can move ends the run. A
//! kernel whose every apply is refused, on one worker, with no room to make (the fake placement
//! frees nothing), no read, write or move in flight and no progress, is ended once the wall has
//! stood for `wall::QUIET_LIMIT`, with the allocation error it met; it neither hangs nor gives up
//! before the limit.

use std::sync::Arc;
use std::time::Instant;

use moruna_kernel::{CancelToken, MorunaError};
use moruna_testkit::{FakeKernel, FakeSource};

use super::common::RigBuilder;

#[test]
fn sc_t45_an_immovable_wall_ends_the_run() {
    let every: Vec<usize> = (0..100_000).collect();
    let kernel = Arc::new(FakeKernel::new().refuse_on(&every));
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 1;
            cfg.workers_active = 1;
            cfg.read_ahead = 1;
            cfg.initial_morsel_target = 8;
        })
        .source(FakeSource::new().splits(1, 8, 64))
        .kernel(Arc::clone(&kernel) as Arc<dyn moruna_kernel::Kernel>)
        .go();
    let started = Instant::now();
    let outcome = rig.scheduler.run(CancelToken::new()).expect("run");
    let took = started.elapsed();
    match outcome {
        crate::RunOutcome::Terminated { diagnostic, .. } => assert!(
            matches!(diagnostic, MorunaError::Alloc { .. })
                || diagnostic.to_string().contains("alloc"),
            "{diagnostic}"
        ),
        other => panic!("an immovable wall ends the run: {other:?}"),
    }
    assert!(
        took >= crate::wall::QUIET_LIMIT,
        "it waited out the limit first: {took:?}"
    );
    assert!(
        took < crate::wall::QUIET_LIMIT * 4,
        "and no longer: {took:?}"
    );
    assert!(
        !rig.fake_placement.sheds().is_empty(),
        "it asked for room first"
    );
}
