//! RC-T4 breach_immediate. A record whose peak crossed the line halves its stage's target
//! inside `on_record`, on the worker that recorded it, without waiting for a tick; and a stage
//! that is already at the floor with one worker left produces the diagnostic the runtime
//! terminates itself with rather than waiting to be killed. Proves RC-I4 and G-I8.

mod common;

use amoru_kernel::KernelHints;
use amoru_testkit::{FakeKnobs, FakeSampler};
use common::{GIB, MIB, kernel, morsel_targets, probe, record, steady};

#[test]
fn rc_t4_breach_immediate() {
    let cfg = common::config_with_baseline(8 * GIB, 1, 400 * MIB);
    let probe_bytes = cfg.probe_bytes;
    let morsel_min = cfg.morsel_min;
    let ceiling = cfg.limits.memory_ceiling;
    let reserve = (ceiling as f64 * f64::from(cfg.reserve_fraction)) as u64;
    // f.7: the breach line is what the arena and the process already hold plus the reserve,
    // capped at the ceiling; the arena itself sits a whole reserve below it.
    let line = (cfg.baseline_bytes + cfg.arena_bytes + reserve).min(ceiling);
    let rig = common::Rig::new(
        cfg,
        vec![kernel(1, KernelHints::default())],
        FakeKnobs::new().probe_result(1, probe(probe_bytes, 4.0)),
        FakeSampler::new().scripted(steady(400 * MIB, 4)),
    );
    rig.run_up();
    let initial = morsel_targets(&rig.writes())
        .last()
        .map(|(_, bytes)| *bytes)
        .expect("a target after start");

    // One breaching record, and nothing else: no tick is run, so a target that moves can only
    // have moved inside `on_record`.
    let before = rig.writes().len();
    let mut breach = record(1, 1, initial, 0);
    breach.mem_anon_before = 400 * MIB;
    breach.mem_anon_peak = line + MIB;
    rig.feed(&breach);
    let after = morsel_targets(&rig.writes());
    assert!(
        rig.writes().len() > before,
        "RC-I4: the breach wrote a knob from inside on_record"
    );
    assert_eq!(
        after.last().map(|(_, bytes)| *bytes),
        Some(initial / 2),
        "RC-I4: the breaching stage is halved at once"
    );

    // Keep breaching. Once the target is at the floor, exactly three more breaches with one
    // worker left produce the diagnostic (f.7).
    let mut seq = 2u64;
    let mut at_floor = None;
    for _ in 0..64 {
        if rig.knobs.terminated().is_some() {
            break;
        }
        let target = morsel_targets(&rig.writes())
            .last()
            .map(|(_, bytes)| *bytes)
            .unwrap_or(initial);
        if target == morsel_min && at_floor.is_none() {
            at_floor = Some(seq);
        }
        let mut breach = record(seq, 1, target, 0);
        breach.mem_anon_peak = line + MIB;
        rig.feed(&breach);
        seq += 1;
    }

    let diagnostic = rig.knobs.terminated().expect("the run is terminated");
    assert!(
        diagnostic.starts_with("budget: morsel "),
        "G-I8: the diagnostic is a Budget error naming the morsel: {diagnostic}"
    );
    assert!(
        diagnostic.contains("stage 1"),
        "the diagnostic names the stage: {diagnostic}"
    );
    assert!(
        diagnostic.contains(&format!("exceeds budget {line}")),
        "the diagnostic carries the budget it exceeded: {diagnostic}"
    );
    let first_floor = at_floor.expect("the target reached the floor");
    assert_eq!(
        seq - first_floor,
        3,
        "f.7: three breaches at the floor with one worker terminate the run"
    );
    let summary = rig.controller.stop();
    assert!(
        summary.breaches >= 3,
        "every breach is counted: {}",
        summary.breaches
    );
}
