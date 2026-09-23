//! The behaviours section h fixes and section k gives no id of its own: the two configuration
//! errors `prepare` raises, the zero-kernel path of f.3, the sampler that stops moving, and the
//! row that is wider than the morsel target. Each is a sentence in the document that would
//! otherwise be held up by nothing.

mod common;

use common::{
    GIB, MIB, active_workers, config, kernel, limits, morsel_targets, probe, read_aheads, record,
    sample, steady,
};
use moruna_kernel::{KernelHints, Knob};
use moruna_testkit::{FakeKnobs, FakeSampler};

/// h, failures: a ceiling that leaves less than two morsels after the baseline and the reserve
/// is a configuration error at `prepare`, not a run that fails on morsel forty thousand.
#[test]
fn rc_h_budget_too_small_is_a_config_error() {
    // A baseline that takes almost the whole ceiling, so the arena the facade could size is
    // smaller than two morsels (11 f.1).
    let mut cfg = common::config_with_baseline(256 * MIB, 4, 240 * MIB);
    cfg.limits = limits(256 * MIB);
    let rig = common::Rig::new(
        cfg,
        vec![kernel(1, KernelHints::default())],
        FakeKnobs::new(),
        FakeSampler::new().scripted(steady(240 * MIB, 2)),
    );
    let error = rig
        .controller
        .prepare()
        .expect_err("the budget does not fit");
    let message = error.to_string();
    assert!(
        message.starts_with("config budget.host:"),
        "f.1: the error names the row it is about: {message}"
    );
}

/// f.3: a kernel the probe measured as too expensive for the ceiling is refused at `start`,
/// before a morsel of it has run.
///
/// The refusal used to wait for f.7: the run began, one morsel ran, the record came back over the
/// breach line and the diagnostic named a footprint the process was already holding. The figures
/// are all known at `start`, and a diagnostic that arrives after the ceiling has been passed is a
/// post-mortem rather than a guard, which is what took a Linux run to 1.005 of its budget while
/// every inequality in the model held (2026-09-23). What is refused on is the probe's slope, not
/// its fixed reading: one probe cannot tell a kernel that holds a fixed cost from one whose cost
/// scales, and the second reading is the one that cannot be argued with.
#[test]
fn rc_f3_a_floor_morsel_that_cannot_fit_is_refused_at_start() {
    let mut cfg = common::config_with_baseline(GIB, 8, 400 * MIB);
    // The floor is the whole of what the ceiling leaves above the arena, several times over: at
    // this amplification one morsel of `morsel_min` on one worker costs more than the headroom.
    cfg.morsel_min = 64 * MIB;
    let probe_bytes = cfg.probe_bytes;
    let rig = common::Rig::new(
        cfg,
        vec![kernel(1, KernelHints::default())],
        FakeKnobs::new().probe_result(1, probe(probe_bytes, 40.0)),
        FakeSampler::new().scripted(steady(400 * MIB, 8)),
    );
    rig.controller.prepare().expect("prepare");
    rig.controller.probe_all().expect("probe_all");
    let error = rig
        .controller
        .start()
        .expect_err("a floor morsel that does not fit is not a run");
    let message = error.to_string();
    assert!(
        message.starts_with("config budget:"),
        "f.3: the refusal names the budget: {message}"
    );
    for figure in ["ceiling is", "before the arena existed", "past the ceiling"] {
        assert!(
            message.contains(figure),
            "f.3: the refusal names the arithmetic it refused on ({figure}): {message}"
        );
    }
    assert!(
        morsel_targets(&rig.writes()).is_empty(),
        "f.3: a run that was refused wrote no knobs, so nothing ran"
    );
}

/// h, edge cases: a kernel that allocates device memory on a host with no device cannot be
/// sized, and that is knowable at `prepare`.
#[test]
fn rc_h_device_kernel_without_a_device_is_a_config_error() {
    let rig = common::Rig::new(
        config(8 * GIB, 4),
        vec![kernel(
            1,
            KernelHints {
                uses_device_memory: true,
                ..KernelHints::default()
            },
        )],
        FakeKnobs::new(),
        FakeSampler::new().scripted(steady(400 * MIB, 2)),
    );
    let error = rig
        .controller
        .prepare()
        .expect_err("there is no device to size for");
    let message = error.to_string();
    assert!(
        message.starts_with("config budget.device:"),
        "f.1: the error names the row it is about: {message}"
    );
}

/// f.3, the zero-kernel path: a run with no kernels has nothing to probe and nothing to size
/// for, so the source drive gets a target of its own and one worker moves the morsels through.
#[test]
fn rc_f3_zero_kernels() {
    let cfg = config(8 * GIB, 8);
    let morsel_max = cfg.morsel_max;
    let rig = common::Rig::new(
        cfg,
        Vec::new(),
        FakeKnobs::new(),
        FakeSampler::new().scripted(steady(400 * MIB, 8)),
    );
    rig.run_up();

    assert!(rig.prober.calls().is_empty(), "f.3: nothing to probe");
    let writes = rig.writes();
    let targets = morsel_targets(&writes);
    assert_eq!(targets.len(), 1, "one target, for the source's own queue");
    assert_eq!(targets[0].0, 0, "f.3: and it is stage 0");
    assert!(
        targets[0].1 <= morsel_max,
        "f.3: bounded by morsel.max_bytes"
    );
    assert_eq!(active_workers(&writes), vec![1], "f.3: one worker");
    assert_eq!(read_aheads(&writes), vec![2], "f.3: the default read-ahead");
    assert!(
        !writes
            .iter()
            .any(|knob| matches!(knob, Knob::HighWater { .. })),
        "f.3: there are no kernel queues to water"
    );

    // Ticking changes nothing: there is no stage to size.
    let mark = rig.writes().len();
    rig.controller.tick_once();
    rig.controller.tick_once();
    assert_eq!(rig.writes().len(), mark, "f.3: nothing for a tick to move");
    rig.controller.stop();
}

/// h, failures: a sampler whose clock has stopped is not a slow sampler, it is a blind
/// controller, and a blind controller must not keep sizing.
#[test]
fn rc_h_sampler_stall_terminates() {
    let cfg = config(8 * GIB, 4);
    let probe_bytes = cfg.probe_bytes;
    // One sample, repeated for ever: `at_ns` never moves again.
    let rig = common::Rig::new(
        cfg,
        vec![kernel(1, KernelHints::default())],
        FakeKnobs::new().probe_result(1, probe(probe_bytes, 3.0)),
        FakeSampler::new().scripted(vec![sample(400 * MIB, 1_000)]),
    );
    rig.run_up();
    for _ in 0..101 {
        rig.controller.tick_once();
    }
    let diagnostic = rig
        .knobs
        .terminated()
        .expect("h: a stalled sampler ends the run");
    assert!(
        diagnostic.starts_with("config sampler:"),
        "h: and says which thing stopped: {diagnostic}"
    );
    rig.controller.stop();
}

/// h, edge cases: a single row wider than the morsel target is a fact about the data, not a
/// sizing failure, and the report says so once.
#[test]
fn rc_h_row_larger_than_target_notes_once() {
    let cfg = config(8 * GIB, 4);
    let probe_bytes = cfg.probe_bytes;
    let rig = common::Rig::new(
        cfg,
        vec![kernel(1, KernelHints::default())],
        FakeKnobs::new().probe_result(1, probe(probe_bytes, 3.0)),
        FakeSampler::new().scripted(steady(400 * MIB, 8)),
    );
    rig.run_up();
    let target = morsel_targets(&rig.writes())
        .last()
        .map(|(_, bytes)| *bytes)
        .expect("a target");

    for seq in 1..=3u64 {
        rig.feed(&record(seq, 1, target * 2, target * 2));
    }
    rig.controller.tick_once();

    let summary = rig.controller.stop();
    let notes: Vec<&String> = summary
        .notes
        .iter()
        .filter(|note| note.as_str() == "row larger than morsel target on stage 1")
        .collect();
    assert_eq!(
        notes.len(),
        1,
        "h: once, not once per morsel: {:?}",
        summary.notes
    );
}
