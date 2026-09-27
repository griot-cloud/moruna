//! process_peak (F8.9). The report's peak is the whole process's, as the sampler measured it,
//! for every shape of run: a run with no kernel writes no record and its peak is still the
//! process's, a record that saw more raises it, the fraction is taken against the ceiling in
//! force, and the report says whether the figure is the operating system's own mark. TR-I3.

mod common;

use common::{TempDir, config, limits, meta, record};
use moruna_kernel::{ProcessPeak, TraceSink};
use moruna_trace::{ExitReason, RunReport, TraceWriter};

#[test]
fn the_peak_is_the_processes_whatever_the_records_say() {
    // No record at all: a copy with no kernel.
    let dir = TempDir::new("process-peak");
    let view = TraceWriter::start(config(dir.path()))
        .expect("start")
        .finish()
        .expect("finish");
    let mut run_meta = meta(ExitReason::Completed);
    let ceiling = limits().memory_ceiling;
    run_meta.process_peak = ProcessPeak {
        bytes: ceiling / 2,
        at_ns: run_meta.start_ns + 1_000,
        exact: true,
    };
    let report = RunReport::compute(&view, &limits(), &run_meta);
    assert_eq!(report.peak_anon_bytes, ceiling / 2);
    assert!((report.peak_fraction_of_ceiling - 0.5).abs() < 1e-12);
    assert_eq!(report.peak_ceiling_bytes, ceiling);
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.contains("operating system's own high-water mark")),
        "{:?}",
        report.notes
    );

    // A peak only sampled says so.
    run_meta.process_peak.exact = false;
    let sampled = RunReport::compute(&view, &limits(), &run_meta);
    assert!(
        sampled
            .notes
            .iter()
            .any(|n| n.contains("highest of the samples")),
        "{:?}",
        sampled.notes
    );

    // A record that saw more than the process peak raises it.
    let dir = TempDir::new("process-peak-records");
    let writer = TraceWriter::start(config(dir.path())).expect("start");
    let mut high = record(0, 1);
    high.mem_anon_peak = ceiling;
    writer.record(high);
    let view = writer.finish().expect("finish");
    let report = RunReport::compute(&view, &limits(), &run_meta);
    assert_eq!(report.peak_anon_bytes, ceiling);
}
