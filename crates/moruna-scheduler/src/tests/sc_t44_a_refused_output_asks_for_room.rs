//! SC-T44 (RB5, amended 2026-11-14): a unit whose output the arena refuses asks placement for
//! room before it waits: one `shed` of the host tier per refusal, for the bytes the refusal
//! named, and the run completes with every morsel once. A wall that nothing can move (no room,
//! nothing in flight, no progress) is covered by `wall`'s own rule, and the read path asks for
//! no room at all (`wall::Room::Wait`).

use std::sync::Arc;

use moruna_kernel::{CancelToken, TierKind};
use moruna_testkit::{FakeKernel, FakeSource};

use super::common::RigBuilder;

#[test]
fn sc_t44_a_refused_output_asks_for_room_first() {
    let kernel = Arc::new(FakeKernel::new().refuse_on(&[2, 5]));
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 1;
            cfg.workers_active = 1;
            cfg.read_ahead = 1;
            cfg.initial_morsel_target = 8;
        })
        .source(FakeSource::new().splits(1, 20, 160))
        .kernel(Arc::clone(&kernel) as Arc<dyn moruna_kernel::Kernel>)
        .go();
    let outcome = rig.scheduler.run(CancelToken::new()).expect("run");
    assert!(
        matches!(outcome, crate::RunOutcome::Completed { .. }),
        "the refusals were met: {outcome:?}"
    );
    let sheds = rig.fake_placement.sheds();
    assert_eq!(
        sheds.len(),
        2,
        "one request for room per refusal: {sheds:?}"
    );
    assert!(
        sheds
            .iter()
            .all(|(tier, bytes)| *tier == TierKind::Host && *bytes > 0),
        "{sheds:?}"
    );
    let mut written = rig.sink.written();
    written.sort_unstable();
    assert_eq!(written, (0..written.len() as u64).collect::<Vec<_>>());
    assert!(!written.is_empty());
}
