//! TR-T7 formulas. A synthetic trace with known values: every report field equals the value
//! hand computed in this test. Proves f.2.

mod common;

use amoru_kernel::{Outcome, TraceRecord, TraceSink};
use amoru_trace::{ExitReason, RunReport, TraceWriter};
use common::{TempDir, config, limits, meta, record, synthetic_trace};

fn close(a: f64, b: f64, what: &str) {
    assert!(
        (a - b).abs() <= 1e-9 * b.abs().max(1.0),
        "{what}: got {a}, hand computed {b}"
    );
}

fn report(dir: &TempDir) -> RunReport {
    let writer = TraceWriter::start(config(dir.path())).expect("start");
    for r in synthetic_trace() {
        writer.record(r);
    }
    let view = writer.finish().expect("finish");
    RunReport::compute(&view, &limits(), &meta(ExitReason::Completed))
}

#[test]
fn tr_t7_formulas() {
    let dir = TempDir::new("t7");
    let r = report(&dir);

    // The run: meta.end - meta.start is 10 s (common::meta).
    close(r.wall_s, 10.0, "wall_s");
    assert_eq!(r.run_id, "ab".repeat(16));
    assert_eq!(r.exit, ExitReason::Completed);
    assert!(!r.resumed);

    // peak_anon_bytes = max(mem_anon_peak); the ceiling is 8 GiB (common::limits).
    assert_eq!(r.peak_anon_bytes, 9_100);
    close(
        r.peak_fraction_of_ceiling,
        9_100.0 / (8.0 * 1024.0 * 1024.0 * 1024.0),
        "peak_fraction_of_ceiling",
    );

    // worker_busy_fraction = sum of kernel_busy_s over (W times mean N).
    // Stage 1 contributes 1 + 2 + 1 = 4 s, stage 2 nothing (Error and Skipped), stage 3
    // two halves and two 0.4 s applies = 1.8 s. Every record ran with two active workers.
    close(
        r.worker_busy_fraction,
        5.8 / (10.0 * 2.0),
        "worker_busy_fraction",
    );

    // cpu_throttled_fraction = 2e6 us over (10 s times 8 CPUs times 1e6).
    close(
        r.cpu_throttled_fraction,
        2_000_000.0 / (10.0 * 8.0 * 1e6),
        "cpu_throttled_fraction",
    );

    // Stage 1 read 6,000 bytes over its own 4 s of wall time, and over the run's 10 s.
    close(r.source_bytes_per_s, 6_000.0 / 4.0, "source_bytes_per_s");
    close(r.source_bandwidth, 6_000.0 / 10.0, "source_bandwidth");

    // staging: +500 then -500, so 500 written and 1,000 moved in total.
    assert_eq!(r.staging_bytes_written, 500);
    assert!(r.staging_engaged);
    close(r.staging_bandwidth, 1_000.0 / 10.0, "staging_bandwidth");

    // The meta half of the report.
    assert!(!r.gil_serialised);
    assert_eq!(r.sizer_used, "rule");
    assert_eq!(r.sizer_fallback_at, None);
    assert_eq!(r.bottleneck_timeline.len(), 2);
    assert_eq!(
        r.notes,
        vec![
            "discovery: cgroup v2 ceiling".to_string(),
            "small dataset: no adaptation".to_string(),
        ],
        "notes are meta.notes followed by meta.controller_notes"
    );
    assert!(!r.overflow_failed);
    assert_eq!(r.late_records, 0);

    assert_eq!(r.stages.len(), 3);
    let s1 = &r.stages[0];
    assert_eq!(s1.stage, 1);
    assert_eq!(s1.morsels, 3);
    assert_eq!((s1.rows_in, s1.rows_out), (600, 480));
    assert_eq!((s1.bytes_in, s1.bytes_out), (6_000, 4_800));
    close(s1.wall_s, 4.0, "stage 1 wall_s");
    close(s1.kernel_busy_s, 4.0, "stage 1 kernel_busy_s");
    close(s1.rows_per_s, 480.0 / 4.0, "stage 1 rows_per_s");
    close(s1.bytes_per_s, 4_800.0 / 4.0, "stage 1 bytes_per_s");
    // Amplifications 1, 2 and 3: nearest rank puts p50 at the second and p95 at the third.
    close(s1.amplification_p50, 2.0, "stage 1 amplification_p50");
    close(s1.amplification_p95, 3.0, "stage 1 amplification_p95");
    close(
        s1.placement_miss_wait_s,
        1.0,
        "stage 1 placement_miss_wait_s",
    );
    assert_eq!((s1.errors, s1.skipped), (0, 0));
    assert_eq!((s1.state_bytes_max, s1.state_growth), (0, 0));

    let s2 = &r.stages[1];
    assert_eq!(s2.stage, 2);
    assert_eq!(s2.morsels, 2);
    close(s2.wall_s, 2.0, "stage 2 wall_s");
    close(s2.kernel_busy_s, 0.0, "stage 2 kernel_busy_s");
    close(s2.rows_per_s, 40.0, "stage 2 rows_per_s");
    close(s2.bytes_per_s, 400.0, "stage 2 bytes_per_s");
    close(s2.amplification_p50, 0.5, "stage 2 amplification_p50");
    close(s2.amplification_p95, 0.5, "stage 2 amplification_p95");
    assert_eq!((s2.errors, s2.skipped), (1, 1));

    let s3 = &r.stages[2];
    assert_eq!(s3.stage, 3);
    assert_eq!(s3.morsels, 4);
    close(s3.wall_s, 1.6, "stage 3 wall_s");
    close(s3.kernel_busy_s, 1.8, "stage 3 kernel_busy_s");
    close(s3.amplification_p50, 1.0, "stage 3 amplification_p50");
    close(s3.amplification_p95, 2.0, "stage 3 amplification_p95");
    // Grouped by instance: instance 0 grew from 1,000 to 3,000, instance 1 from 500 to 600,
    // so the larger growth is the stage's and a fresh instance hides nothing.
    assert_eq!(s3.state_bytes_max, 3_000);
    assert_eq!(s3.state_growth, 2_000);
}

/// h, edge cases: an empty trace divides by nothing and says there are no kernel stages.
#[test]
fn tr_t7_formulas_empty_trace() {
    let dir = TempDir::new("t7-empty");
    let writer = TraceWriter::start(config(dir.path())).expect("start");
    let view = writer.finish().expect("finish");
    let r = RunReport::compute(&view, &limits(), &meta(ExitReason::Completed));
    assert_eq!(view.len(), 0);
    assert!(view.is_empty());
    assert!(view.tail(0, 4).is_empty());
    assert!(view.records().is_empty());
    assert!(r.stages.is_empty());
    close(r.wall_s, 10.0, "wall_s from meta");
    close(r.source_bytes_per_s, 0.0, "source_bytes_per_s");
    close(r.source_bandwidth, 0.0, "source_bandwidth");
    close(r.staging_bandwidth, 0.0, "staging_bandwidth");
    close(r.worker_busy_fraction, 0.0, "worker_busy_fraction");
    close(r.peak_fraction_of_ceiling, 0.0, "peak_fraction_of_ceiling");
    assert!(!r.staging_engaged);
    assert!(format!("{r}").contains("no kernel stages"));
}

/// h, edge cases: a stage whose only record is a probe has one morsel and takes its
/// amplification from the probe alone.
#[test]
fn tr_t7_formulas_probe_only() {
    let dir = TempDir::new("t7-probe");
    let writer = TraceWriter::start(config(dir.path())).expect("start");
    writer.record(TraceRecord {
        t_start_ns: 0,
        t_end_ns: 1_000_000_000,
        bytes_in: 1_000,
        mem_anon_before: 0,
        mem_anon_peak: 4_000,
        outcome: Outcome::Probe,
        ..record(1, 1)
    });
    let view = writer.finish().expect("finish");
    let r = RunReport::compute(&view, &limits(), &meta(ExitReason::Completed));
    assert_eq!(r.stages.len(), 1);
    assert_eq!(r.stages[0].morsels, 1);
    close(
        r.stages[0].amplification_p50,
        4.0,
        "probe amplification_p50",
    );
    close(
        r.stages[0].amplification_p95,
        4.0,
        "probe amplification_p95",
    );
    close(r.stages[0].kernel_busy_s, 1.0, "a probe is busy time");
}

/// h, edge cases: nothing in f.2 divides by zero. A run with no measurable duration, no
/// discovered ceiling and no CPU quota still produces a report, with rates of 0.0.
#[test]
fn tr_t7_formulas_divide_by_nothing() {
    let dir = TempDir::new("t7-zero");
    let writer = TraceWriter::start(config(dir.path())).expect("start");
    // Two records that begin and end at the same instant, so every span is zero.
    for seq in 0..2 {
        writer.record(TraceRecord {
            t_start_ns: 5_000,
            t_end_ns: 5_000,
            rows_in: 10,
            bytes_in: 0,
            rows_out: 10,
            bytes_out: 100,
            mem_anon_before: 0,
            mem_anon_peak: 0,
            knob_active_workers: 3,
            staging_bytes_delta: 0,
            ..record(seq, 1)
        });
    }
    let view = writer.finish().expect("finish");

    let mut lim = limits();
    lim.memory_ceiling = 0;
    lim.cpu_quota = 0.0;
    lim.memory_kill = None;
    let mut m = meta(ExitReason::Completed);
    m.start_ns = 1_000;
    m.end_ns = 1_000;

    let r = RunReport::compute(&view, &lim, &m);
    close(r.wall_s, 0.0, "wall_s");
    close(r.peak_fraction_of_ceiling, 0.0, "peak_fraction_of_ceiling");
    close(r.cpu_throttled_fraction, 0.0, "cpu_throttled_fraction");
    close(r.worker_busy_fraction, 0.0, "worker_busy_fraction");
    close(r.source_bytes_per_s, 0.0, "source_bytes_per_s");
    close(r.source_bandwidth, 0.0, "source_bandwidth");
    close(r.staging_bandwidth, 0.0, "staging_bandwidth");
    assert_eq!(r.stages.len(), 1);
    close(r.stages[0].wall_s, 0.0, "stage wall_s");
    close(r.stages[0].rows_per_s, 0.0, "stage rows_per_s");
    close(r.stages[0].bytes_per_s, 0.0, "stage bytes_per_s");
    // No record had input bytes, so there is no amplification sample to take a percentile of.
    close(r.stages[0].amplification_p50, 0.0, "amplification_p50");
    close(r.stages[0].amplification_p95, 0.0, "amplification_p95");
    // Every figure is a real number, so the rendering and the JSON are usable.
    let shown = format!("{r}");
    assert!(!shown.contains("NaN"), "{shown}");
    assert!(!shown.contains("inf"), "{shown}");
    assert!(serde_json::from_str::<serde_json::Value>(&r.to_json()).is_ok());
}
