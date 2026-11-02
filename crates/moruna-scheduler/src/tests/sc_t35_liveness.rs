//! SC-T35 liveness (f.20, f.21; adaptive-runtime 12.5; fail loud): a run never hangs quietly.
//! A budget under the run's floor is refused at `new`, naming the floor; a push with no room and
//! no disk to go to waits `PUSH_STALL` by the run's clock and then ends the run naming the
//! morsel, proved here on a clock the test steps; and a run where nothing can move is ended by
//! the stall detector, naming its queues and bytes.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use moruna_kernel::{CancelToken, MorunaError};
use moruna_testkit::{FakeKernel, FakePlacement, FakeSource};

use super::common::RigBuilder;
use crate::decide::{Clock, PUSH_STALL};

/// A clock that moves only when it is waited on: each pause is the whole pause, at once.
#[derive(Debug, Default)]
struct Stepped(AtomicU64);

impl Clock for Stepped {
    fn now(&self) -> Duration {
        Duration::from_nanos(self.0.load(Ordering::SeqCst))
    }

    fn pause(&self, pause: Duration) {
        self.0.fetch_add(pause.as_nanos() as u64, Ordering::SeqCst);
        std::thread::yield_now();
    }
}

fn diagnostic(outcome: crate::Result<crate::RunOutcome>) -> String {
    match outcome {
        Ok(crate::RunOutcome::Terminated { diagnostic, .. }) => diagnostic.to_string(),
        other => panic!("expected the run to end with a diagnostic, got {other:?}"),
    }
}

#[test]
fn sc_t35_a_budget_under_the_floor_is_refused() {
    // Two queues of the default window and one unit at 64-byte morsels need 384 bytes.
    let refused = RigBuilder::new()
        .cfg(|cfg| cfg.morsel_min = 64)
        .placement(FakePlacement::new().with_budget(100))
        .stages(1)
        .build();
    let message = match refused {
        Ok(_) => panic!("a budget under the floor ran"),
        Err(MorunaError::Config { name, msg }) => {
            assert_eq!(name, "budget.host");
            msg
        }
        Err(other) => panic!("refused with {other:?}"),
    };
    assert!(message.contains("below this run's floor"), "{message}");
    assert!(message.contains("promotion windows"), "{message}");
    assert!(message.contains("64 bytes a batch"), "{message}");
}

#[test]
fn sc_t35_a_push_with_no_room_and_no_disk_ends_the_run_naming_it() {
    // Every claim is refused (a budget smaller than one batch) and there is no disk to write
    // to, so the first read waits for room that cannot come. The stepped clock makes the
    // thirty seconds pass at once.
    let clock = Arc::new(Stepped::default());
    let rig = RigBuilder::new()
        .cfg({
            let clock = Arc::clone(&clock);
            move |cfg| cfg.clock = clock
        })
        .source(FakeSource::new().splits(3, 16, 128).sub_splittable(false))
        .placement(FakePlacement::new().with_budget(16).without_disk())
        .kernel(Arc::new(FakeKernel::new()))
        .go();
    let started = Instant::now();
    let message = diagnostic(rig.scheduler.run(CancelToken::new()));
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the clock, not the wall, waited"
    );
    assert!(message.contains("morsel 0"), "names the morsel: {message}");
    assert!(
        message.contains(&format!("waited {} s", PUSH_STALL.as_secs())),
        "{message}"
    );
    assert!(message.contains("no disk tier"), "says why: {message}");
    assert!(clock.now() >= PUSH_STALL, "the run's clock is what waited");
}

#[test]
fn sc_t35_a_run_that_cannot_move_is_ended_by_the_detector() {
    // The placement never makes a head ready: nothing runs, nothing is in flight, nothing is
    // claimed, and nothing changes. The detector ends the run within two checks.
    let rig = RigBuilder::new()
        .cfg(|cfg| cfg.heartbeat_interval_ms = 20)
        .source(FakeSource::new().splits(2, 16, 128).sub_splittable(false))
        .placement(FakePlacement::new().with_delay(Duration::from_secs(3600)))
        .kernel(Arc::new(FakeKernel::new()))
        .go();
    let message = diagnostic(rig.scheduler.run(CancelToken::new()));
    assert!(message.contains("no unit can make progress"), "{message}");
    assert!(
        message.contains("queue 0 holds 2 batches"),
        "names the queues: {message}"
    );
    assert!(
        message.contains("budget bytes resident"),
        "and the bytes: {message}"
    );
}
