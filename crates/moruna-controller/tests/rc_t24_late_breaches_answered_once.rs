//! RC-T24 late_breaches_answered_once (RB5). Eight workers cross the line together, which is one
//! event: the first record halves the target and sheds workers, and the other seven, whose
//! morsels started before that shrink, arrive afterwards. They are the breach the shrink already
//! answered, so they neither halve the target again nor count against the floor, and the run is
//! not terminated on them; a morsel picked up after the shrink that breaches again is a new
//! event and is acted on.

mod common;

use common::{GIB, MIB, kernel, morsel_targets, probe, record, started_now, steady};
use moruna_kernel::KernelHints;
use moruna_testkit::{FakeKnobs, FakeSampler};

#[test]
fn rc_t24_late_breaches_answered_once() {
    let cfg = common::config_with_baseline(8 * GIB, 8, 400 * MIB);
    let probe_bytes = cfg.probe_bytes;
    let morsel_min = cfg.morsel_min;
    let ceiling = cfg.limits.memory_ceiling;
    let resting = cfg.baseline_bytes + cfg.arena_bytes;
    let line = (resting + (ceiling - resting) / 2).min(ceiling);
    let rig = common::Rig::new(
        cfg,
        vec![kernel(1, KernelHints::default())],
        FakeKnobs::new().probe_result(1, probe(probe_bytes, 4.0)),
        FakeSampler::new().scripted(steady(400 * MIB, 4)),
    );
    rig.run_up();
    let target = |rig: &common::Rig| {
        morsel_targets(&rig.writes())
            .last()
            .map(|(_, bytes)| *bytes)
            .expect("a target")
    };
    let initial = target(&rig);

    // Eight morsels picked up together at the initial target, all crossing the line.
    let burst: Vec<_> = (1..=8u64)
        .map(|seq| {
            let mut r = started_now(record(seq, 1, initial, 0));
            r.knob_active_workers = 8;
            r.mem_anon_peak = line + MIB;
            r
        })
        .collect();
    rig.feed(&burst[0]);
    let halved = target(&rig);
    assert_eq!(halved, initial / 2, "the first record halves the target");
    for late in &burst[1..] {
        rig.feed(late);
    }
    assert_eq!(
        target(&rig),
        halved,
        "the seven that started before the shrink do not halve it again"
    );
    assert!(
        rig.knobs.terminated().is_none(),
        "nor end the run: {:?}",
        rig.knobs.terminated()
    );

    // A morsel picked up after the shrink, at the new target, that still breaches is a new event.
    let mut again = started_now(record(9, 1, halved, 0));
    again.mem_anon_peak = line + MIB;
    rig.feed(&again);
    assert_eq!(
        target(&rig),
        (halved / 2).max(morsel_min),
        "a breach after the shrink is acted on"
    );
    let summary = rig.controller.stop();
    assert_eq!(summary.breaches, 9, "every breach is still counted");
}

/// RC-T24 (b). Breaches at the floor on one worker that leave the process short of the stop line
/// (a quarter of the reserve under the ceiling) are answered by shrinking and waiting, however
/// many there are and whatever the model says the floor costs: the floor's own measurement says
/// it fits (f.7, RB5).
#[test]
fn rc_t24_floor_breaches_short_of_the_stop_line_never_end_the_run() {
    let cfg = common::config_with_baseline(8 * GIB, 1, 400 * MIB);
    let probe_bytes = cfg.probe_bytes;
    let ceiling = cfg.limits.memory_ceiling;
    let resting = cfg.baseline_bytes + cfg.arena_bytes;
    let line = (resting + (ceiling - resting) / 2).min(ceiling);
    let rig = common::Rig::new(
        cfg,
        vec![kernel(1, KernelHints::default())],
        FakeKnobs::new().probe_result(1, probe(probe_bytes, 4.0)),
        FakeSampler::new().scripted(steady(400 * MIB, 4)),
    );
    rig.run_up();
    for seq in 1..=64u64 {
        let target = morsel_targets(&rig.writes())
            .last()
            .map(|(_, bytes)| *bytes)
            .expect("a target");
        let mut breach = started_now(record(seq, 1, target, 0));
        breach.mem_anon_peak = line + MIB;
        rig.feed(&breach);
    }
    assert!(
        rig.knobs.terminated().is_none(),
        "sixty-four floor breaches short of the stop line: {:?}",
        rig.knobs.terminated()
    );
    rig.controller.stop();
}
