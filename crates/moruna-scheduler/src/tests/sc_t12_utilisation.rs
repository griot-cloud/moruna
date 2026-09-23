//! SC-T12 (reference host, E1): a compute-bound kernel keeps the pool busy at least 85% of the
//! time over a minute. S4.
//!
//! Tagged for the reference host in section k, so it exists, is marked ignored with the tag's
//! reason, and is listed in the report. Run it with `cargo test -- --ignored` on the Griot
//! bare-metal server in Nairobi.

use std::sync::Arc;
use std::time::{Duration, Instant};

use moruna_kernel::{CancelToken, StatsSource};
use moruna_testkit::{FakeKernel, FakeSource};

use super::common::RigBuilder;

#[test]
#[ignore = "reference host, E1: a busy-fraction figure is only comparable on the named host"]
fn sc_t12_utilisation() {
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 8;
            cfg.workers_active = 8;
            cfg.initial_morsel_target = 8;
            cfg.read_ahead = 16;
        })
        .source(FakeSource::new().splits(1, 100_000, 800_000))
        .kernel(Arc::new(
            FakeKernel::new().latency(Duration::from_millis(50)),
        ))
        .go();
    let cancel = CancelToken::new();
    let token = cancel.clone();
    let (busy, samples) = std::thread::scope(|scope| {
        let handle = scope.spawn(|| rig.scheduler.run(token));
        let deadline = Instant::now() + Duration::from_secs(60);
        let (mut busy, mut samples) = (0u64, 0u64);
        while Instant::now() < deadline {
            busy += rig.scheduler.scheduler_stats().workers_busy as u64;
            samples += 8;
            std::thread::sleep(Duration::from_millis(50));
        }
        cancel.cancel();
        let _ = handle.join();
        (busy, samples)
    });
    let fraction = busy as f64 / samples as f64;
    assert!(
        fraction >= 0.85,
        "busy fraction {fraction:.3} is below 0.85"
    );
}
