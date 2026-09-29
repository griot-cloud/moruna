//! limits_timeline. A trace told of two limits changes: the report carries the starting
//! limits and the timeline in milliseconds from the start of the run, `peak_fraction_of_ceiling`
//! is taken against the ceiling in force at the peak and names it, the drains the meta carries are
//! reported, `Display` has the timeline line, and a change after `finish` is late. TR-I3.

mod common;

use common::{TempDir, config, limits, meta, record};
use moruna_kernel::{Limits, LimitsChangeReason, LimitsChanged, TraceSink};
use moruna_trace::{DrainSummary, ExitReason, RunReport, TraceWriter};

const GIB: u64 = 1024 * 1024 * 1024;

fn change(at_ns: u64, old: &Limits, ceiling: u64, cpu: f64) -> LimitsChanged {
    LimitsChanged {
        at_ns,
        old: old.clone(),
        new: Limits {
            memory_ceiling: ceiling,
            cpu_quota: cpu,
            observed_at: at_ns,
            ..old.clone()
        },
        reason: LimitsChangeReason::MemoryAndCpu,
    }
}

#[test]
fn limits_timeline() {
    let dir = TempDir::new("t12");
    let writer = TraceWriter::start(config(dir.path())).expect("start");
    let initial = limits();
    let start_ns = meta(ExitReason::Completed).start_ns;

    // The peak (seq 9) ends 9.5 us after the start, which is after the first change (5 us) and
    // before the second (3 s); the report must measure it against the first change's ceiling.
    for seq in 0..10 {
        writer.record(record(seq, 1));
    }
    let first = change(start_ns + 5_000, &initial, 2 * GIB, 16.0);
    let second = change(start_ns + 3_000_000_000, &first.new, 4 * GIB, 2.0);
    // Told out of order: the report sorts by time.
    writer.limits_changed(second.clone());
    writer.limits_changed(first.clone());
    let view = writer.finish().expect("finish");
    assert_eq!(view.limits_changes().len(), 2);

    let mut run_meta = meta(ExitReason::Completed);
    run_meta.drains = vec![
        DrainSummary {
            at_ms: 3_000,
            bytes: 2 * GIB,
            drain_ms: Some(750),
        },
        DrainSummary {
            at_ms: 9_000,
            bytes: GIB,
            drain_ms: None,
        },
    ];
    let report = RunReport::compute(&view, &initial, &run_meta);

    assert_eq!(report.limits_initial.memory_ceiling, initial.memory_ceiling);
    assert_eq!(report.limits_timeline.len(), 2);
    assert_eq!(
        report.limits_timeline[0].0, 0,
        "milliseconds from the start"
    );
    assert_eq!(report.limits_timeline[0].1.memory_ceiling, 2 * GIB);
    assert_eq!(report.limits_timeline[1].0, 3_000);
    assert_eq!(report.limits_timeline[1].1.memory_ceiling, 4 * GIB);
    assert!((report.limits_timeline[1].1.cpu_quota - 2.0).abs() < f64::EPSILON);

    let peak = 10_000 + 2 * (1_000 + 9);
    assert_eq!(report.peak_anon_bytes, peak);
    assert_eq!(
        report.peak_ceiling_bytes,
        2 * GIB,
        "the ceiling in force at the peak, not the one the run began or ended with"
    );
    assert!((report.peak_fraction_of_ceiling - peak as f64 / (2 * GIB) as f64).abs() < 1e-15);
    assert_eq!(report.drains, run_meta.drains);

    let text = report.to_string();
    assert!(text.contains("limits moved 2 times"), "{text}");
    assert!(text.contains("a drain still running at the end"), "{text}");
    let json = report.to_json();
    assert!(json.contains("\"limits_initial\""));
    assert!(json.contains("\"limits_timeline\""));
    assert!(json.contains("\"peak_ceiling_bytes\""));

    // Pure: the same inputs give the same report (TR-I3).
    let again = RunReport::compute(&view, &initial, &run_meta);
    assert_eq!(again.to_json(), json);

    // A completed drain is shown with its duration.
    let mut done = run_meta.clone();
    done.drains.pop();
    let text = RunReport::compute(&view, &initial, &done).to_string();
    assert!(text.contains("last drain 0.750 s"), "{text}");

    // After `finish` a change is late, like a record.
    let late_before = writer.snapshot().late_records();
    writer.limits_changed(first);
    assert_eq!(writer.snapshot().late_records(), late_before + 1);
    assert_eq!(writer.snapshot().limits_changes().len(), 2);
}

/// A run whose machine never moved: an empty timeline, the fraction against the starting
/// ceiling, and no timeline line.
#[test]
fn no_change_is_no_timeline() {
    let dir = TempDir::new("t12-none");
    let writer = TraceWriter::start(config(dir.path())).expect("start");
    writer.record(record(1, 1));
    let view = writer.finish().expect("finish");
    let report = RunReport::compute(&view, &limits(), &meta(ExitReason::Completed));
    assert!(report.limits_timeline.is_empty());
    assert!(report.drains.is_empty());
    assert_eq!(report.peak_ceiling_bytes, limits().memory_ceiling);
    assert!(!report.to_string().contains("limits moved"));
}

/// A peak reached while the arena was still giving memory back after a lowered ceiling was
/// memory taken under the ceiling before the change, and is judged against that one (MH 4.4,
/// 7.1): Moruna never allocates past the ceiling in force and returns what it holds eventually.
/// A peak outside any drain is judged against the ceiling in force, lowered or not, so a real
/// breach after a shrink still shows. Before 2026-09-29 the first case read as over the ceiling
/// whenever the peak sample landed just after the change.
#[test]
fn a_peak_during_a_drain_is_judged_against_the_ceiling_it_was_taken_under() {
    let dir = TempDir::new("t12-drain");
    let writer = TraceWriter::start(config(dir.path())).expect("start");
    let initial = limits();
    let start_ns = meta(ExitReason::Completed).start_ns;
    for seq in 0..10 {
        writer.record(record(seq, 1));
    }
    // Lowered to half 5 us in, before the peak at 9.5 us.
    let lowered = change(start_ns + 5_000, &initial, initial.memory_ceiling / 2, 16.0);
    writer.limits_changed(lowered);
    let view = writer.finish().expect("finish");

    // The drain began at the change and was still running at the peak.
    let mut draining = meta(ExitReason::Completed);
    draining.drains = vec![DrainSummary {
        at_ms: 0,
        bytes: GIB,
        drain_ms: Some(10),
    }];
    let report = RunReport::compute(&view, &initial, &draining);
    assert_eq!(
        report.peak_ceiling_bytes, initial.memory_ceiling,
        "a peak while draining is judged against the ceiling it was taken under"
    );

    // No drain under way at the peak: the lowered ceiling is the one in force.
    let mut settled = meta(ExitReason::Completed);
    settled.drains = vec![DrainSummary {
        at_ms: 1,
        bytes: GIB,
        drain_ms: Some(10),
    }];
    let report = RunReport::compute(&view, &initial, &settled);
    assert_eq!(
        report.peak_ceiling_bytes,
        initial.memory_ceiling / 2,
        "outside a drain the lowered ceiling stands, so a real breach still shows"
    );
}
