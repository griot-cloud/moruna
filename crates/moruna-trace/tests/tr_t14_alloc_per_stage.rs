//! TR-T14 alloc_per_stage (E13). A synthetic trace with a Python stage whose records carry
//! known `alloc` counts and a Rust stage with none: the Python stage's `alloc` is the
//! hand-computed sums, maxima and outside-Arrow part of the largest call; unmeasured fields are
//! `None` and `null` in JSON; the Rust stage has none; `Display` shows one line; the columns
//! round-trip through the trace file. d.1, f.2, e.3.

mod common;

use common::{TempDir, config, limits, meta, record};
use moruna_kernel::{AllocCounts, KernelAlloc, TraceRecord, TraceSink};
use moruna_trace::{ExitReason, RunReport, StageAlloc, TraceWriter};

const MIB: u64 = 1024 * 1024;

fn counts(bytes: u64, requests: u64, largest: u64, peak: u64, refused: u64) -> AllocCounts {
    AllocCounts {
        bytes,
        requests,
        largest,
        peak,
        refused,
    }
}

fn trace() -> Vec<TraceRecord> {
    let python = |seq, alloc, rise: u64| TraceRecord {
        mem_anon_before: 100 * MIB,
        mem_anon_peak: 100 * MIB + rise,
        alloc,
        ..record(seq, 1)
    };
    vec![
        // Hooked peaks 2 + 6 MiB; the process rose 4 MiB: the call total is 8 MiB, 6 outside Arrow.
        python(
            0,
            KernelAlloc {
                measured: true,
                refusal_on: true,
                python: counts(MIB, 100, 64 * 1024, 0, 0),
                numpy: counts(6 * MIB, 2, 4 * MIB, 6 * MIB, 0),
                arrow: counts(2 * MIB, 3, 0, 2 * MIB, 0),
            },
            4 * MIB,
        ),
        // A refusal, and a process rise of 20 MiB larger than the hooks saw: the largest call.
        python(
            1,
            KernelAlloc {
                measured: true,
                refusal_on: true,
                python: counts(2 * MIB, 50, 2 * MIB, 0, 1),
                numpy: counts(3 * MIB, 1, 3 * MIB, 3 * MIB, 1),
                arrow: counts(5 * MIB, 1, 0, 5 * MIB, 0),
            },
            20 * MIB,
        ),
        // A Rust stage: nothing measured.
        record(0, 2),
    ]
}

fn report(tag: &str) -> RunReport {
    let dir = TempDir::new(tag);
    let writer = TraceWriter::start(config(dir.path())).expect("start");
    for r in trace() {
        writer.record(r);
    }
    let view = writer.finish().expect("finish");
    let records = view.records();
    let originals = trace();
    for r in &originals {
        let back = records
            .iter()
            .find(|b| (b.stage, b.seq) == (r.stage, r.seq))
            .expect("the record came back");
        assert_eq!(back.alloc, r.alloc, "alloc round-trips through the file");
    }
    RunReport::compute(&view, &limits(), &meta(ExitReason::Completed))
}

#[test]
fn tr_t14_alloc_per_stage() {
    let report = report("tr14");
    let python = report.stages[0]
        .alloc
        .clone()
        .expect("the Python stage is measured");
    assert!(
        report.stages[1].alloc.is_none(),
        "the Rust stage has no alloc"
    );
    assert!(python.refusal_on);
    assert_eq!(python.python.requested_bytes, 3 * MIB);
    assert_eq!(python.python.requests, 150);
    assert_eq!(python.python.largest_request_bytes, Some(2 * MIB));
    assert_eq!(
        python.python.peak_bytes, None,
        "Python objects' peak is unmeasured"
    );
    assert_eq!(python.python.refused, Some(1));
    assert_eq!(python.numpy.requested_bytes, 9 * MIB);
    assert_eq!(python.numpy.requests, 3);
    assert_eq!(python.numpy.largest_request_bytes, Some(4 * MIB));
    assert_eq!(python.numpy.peak_bytes, Some(6 * MIB));
    assert_eq!(python.numpy.refused, Some(1));
    assert_eq!(python.arrow.requested_bytes, 7 * MIB);
    assert_eq!(python.arrow.requests, 4);
    assert_eq!(python.arrow.largest_request_bytes, None);
    assert_eq!(python.arrow.peak_bytes, Some(5 * MIB));
    assert_eq!(python.arrow.refused, None);
    // The largest call is the second: total 20 MiB (the process's rise), 5 MiB of it Arrow.
    assert_eq!(python.peak_bytes, 20 * MIB);
    assert_eq!(python.outside_arrow_peak_bytes, 15 * MIB);
    assert_eq!(python.outside_arrow_fraction, Some(0.75));

    let json = report.to_json();
    assert!(json.contains("\"outside_arrow_fraction\": 0.75"), "{json}");
    assert!(json.contains("\"largest_request_bytes\": null"), "{json}");
    assert!(
        json.contains("\"alloc\": null"),
        "the Rust stage's alloc is null"
    );

    let text = report.to_string();
    let lines: Vec<&str> = text.lines().filter(|l| l.starts_with("alloc ")).collect();
    assert_eq!(lines.len(), 1, "{text}");
    assert!(lines[0].contains("refusal on"), "{}", lines[0]);
    assert!(lines[0].contains("75.0% outside Arrow"), "{}", lines[0]);
    assert!(lines[0].contains("refused 2"), "{}", lines[0]);
    assert!(text.lines().count() <= 40);
}

#[test]
fn tr_t14_nothing_measured_is_none() {
    assert_eq!(StageAlloc::of(&[record(0, 1)]), None);
    let zero = TraceRecord {
        alloc: KernelAlloc {
            measured: true,
            ..KernelAlloc::default()
        },
        mem_anon_before: 0,
        mem_anon_peak: 0,
        ..record(0, 1)
    };
    let a = StageAlloc::of(&[zero]).expect("measured");
    assert_eq!(a.peak_bytes, 0);
    assert_eq!(a.outside_arrow_fraction, None);
    assert!(!a.refusal_on);
}

#[test]
fn tr_t14_ties_choose_the_earlier_call() {
    let at = |seq, t, arrow| TraceRecord {
        t_start_ns: t,
        alloc: KernelAlloc {
            measured: true,
            numpy: counts(0, 0, 0, MIB - arrow, 0),
            arrow: counts(0, 0, 0, arrow, 0),
            ..KernelAlloc::default()
        },
        mem_anon_before: 0,
        mem_anon_peak: 0,
        ..record(seq, 1)
    };
    let a = StageAlloc::of(&[at(1, 20, MIB / 2), at(0, 10, MIB / 4)]).expect("measured");
    assert_eq!(
        a.outside_arrow_peak_bytes,
        MIB - MIB / 4,
        "the earlier call of two equals"
    );
}
