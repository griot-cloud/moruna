//! SC-T10: the probe protocol. One worker active, one read for stage 1 at the cursor with the
//! cursor advanced, no read for a later stage, a `ProbeResult` whose peak comes from the
//! sampler, the output pushed downstream and one peak reset per probe. f.9.
//!
//! `FakeKnobs` has no `probes()` observable and its `terminated()` returns `Option<String>`
//! rather than `Option<AmoruError>`, so this test asserts on the scheduler's own output rather
//! than on the fake, exactly as the controller agent's tests do. That is a gap in the fake,
//! reported rather than worked around in the testkit.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::time::Duration;

use amoru_kernel::{Prober, Sample, StatsSource};
use amoru_testkit::FakeSource;

use super::common::{RigBuilder, scripted_sampler};

fn sample(anon: u64, peak: u64) -> Sample {
    Sample {
        anon_bytes: anon,
        peak_anon_bytes: peak,
        ..Sample::default()
    }
}

#[test]
fn sc_t10_probe_protocol() {
    let sampler = scripted_sampler(vec![
        sample(100, 100),
        sample(900, 900),
        sample(200, 200),
        sample(700, 700),
    ]);
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 4;
            cfg.workers_active = 4;
            cfg.initial_morsel_target = 16;
        })
        .source(FakeSource::new().splits(1, 64, 512))
        .sampler(sampler)
        .stages(2)
        .go();

    // Watch the count the controller would read while the probe runs.
    let stop = Arc::new(AtomicBool::new(false));
    let seen = Arc::new(AtomicU16::new(u16::MAX));
    let first = amoru_kernel::StatsSource::scheduler_stats(&rig.scheduler).workers_active;
    assert_eq!(first, 4, "four workers are active before the probe");

    let one = {
        let stop = Arc::clone(&stop);
        let seen = Arc::clone(&seen);
        let scheduler = &rig.scheduler;
        std::thread::scope(|scope| {
            scope.spawn(|| {
                while !stop.load(Ordering::SeqCst) {
                    let active = scheduler.scheduler_stats().workers_active;
                    seen.fetch_min(active, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_micros(200));
                }
            });
            let result = scheduler.probe(1, 16);
            stop.store(true, Ordering::SeqCst);
            result
        })
    };
    let one = match one {
        Ok(result) => result,
        Err(e) => panic!("probe(1): {e}"),
    };
    assert_eq!(
        seen.load(Ordering::SeqCst),
        1,
        "the probe runs with exactly one worker active"
    );
    assert_eq!(
        rig.source.reads().len(),
        1,
        "stage 1 costs exactly one read"
    );
    let reads = rig.source.reads();
    let range = match reads[0].1 {
        Some(range) => range,
        None => panic!("the probe read the whole split rather than a range at the cursor"),
    };
    assert_eq!(range.start, 0, "the probe reads at the cursor");
    assert_eq!(
        one.peak_delta, 800,
        "peak_delta is the scripted peak above the sample before"
    );
    assert!(one.rows_in > 0 && one.bytes_in > 0);

    // The probe's output went downstream as a normal morsel, so Q1 holds it.
    assert_eq!(
        rig.fake_placement.pushed(1).len(),
        1,
        "the probe output is in Q1"
    );

    let two = match rig.scheduler.probe(2, 16) {
        Ok(result) => result,
        Err(e) => panic!("probe(2): {e}"),
    };
    assert_eq!(rig.source.reads().len(), 1, "a later stage costs no read");
    assert_eq!(rig.fake_placement.popped(1).len(), 1, "it popped Q1's head");
    assert_eq!(
        rig.fake_placement.pushed(2).len(),
        1,
        "and pushed its output to Q2"
    );
    assert!(two.wall_ns > 0 || two.cpu_ns == two.cpu_ns);
    assert_eq!(rig.sampler.peak_resets(), 2, "one peak reset per probe");

    // h: a chain has no stage 0 and no stage beyond its last kernel.
    assert!(rig.scheduler.probe(0, 16).is_err());
    assert!(rig.scheduler.probe(3, 16).is_err());
}
