//! RC-T4 breach_immediate. A record whose peak crossed the line halves its stage's target
//! inside `on_record`, on the worker that recorded it, without waiting for a tick; and a stage
//! that is already at the floor with one worker left produces the diagnostic the runtime
//! terminates itself with rather than waiting to be killed. Proves RC-I4 and G-I8.
//!
//! Two figures here changed on 2026-09-23 and both were wrong in the same direction: they let a
//! run finish over budget. The line was `baseline + arena + reserve`, which is the ceiling, so
//! the first signal the controller got was S1 already broken; it now sits at half the headroom
//! the arena left, where a halving still has somewhere to land. And termination was three floor
//! breaches with one worker, a count that needs `workers_max + 3` breaching records to reach:
//! the first program measured had ten morsels and ten workers and completed at 1.11 x its
//! ceiling. It is now the model's verdict -- the floor on one worker does not satisfy the anon
//! inequality of f.3 -- so a ten-morsel run ends on the same evidence as a ten-million one.

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
    // f.7: the process's resting anonymous memory is the baseline plus the arena, every page of
    // which the arena touches at `new`; the line sits half of what is left above it, and the
    // other half is the cushion the reaction works in.
    let resting = cfg.baseline_bytes + cfg.arena_bytes;
    let headroom = ceiling - resting;
    let line = (resting + headroom / 2).min(ceiling);
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

    // Keep breaching. The refit each breach performs drives `a_anon` up, so once the target is at
    // the floor the model can say that the floor on one worker does not fit, and that is the
    // record the run ends on (f.7).
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
    // The two figures the message compares are the same kind of number: the out-of-arena bytes
    // the smallest set of knobs would cost, against the bytes the process has above the arena.
    // A per-morsel delta against an absolute line, which is what it used to carry, read as
    // "footprint 835584 exceeds budget 536818484".
    assert!(
        diagnostic.contains(&format!("exceeds budget {headroom}")),
        "the diagnostic carries the headroom the footprint did not fit: {diagnostic}"
    );
    let footprint: u64 = diagnostic
        .split("footprint ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|bytes| bytes.parse().ok())
        .expect("a footprint in the diagnostic");
    assert!(
        footprint > headroom,
        "the footprint is the larger of the two, or the sentence does not mean anything: \
         {diagnostic}"
    );
    let first_floor = at_floor.expect("the target reached the floor");
    assert_eq!(
        seq - first_floor,
        1,
        "f.7: the first breach at the floor with one worker ends it, because the model can \
         already say the floor does not fit and no further record will change that"
    );
    let summary = rig.controller.stop();
    assert!(
        summary.breaches >= 2,
        "every breach is counted: {}",
        summary.breaches
    );
}
