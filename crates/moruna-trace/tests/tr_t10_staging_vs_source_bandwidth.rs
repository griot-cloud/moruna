//! TR-T10 staging_vs_source_bandwidth. A synthetic trace over a 10 s run with 2 GiB of
//! stage 1 `bytes_in` and `staging_bytes_delta` summing to +1 GiB and then -1 GiB:
//! `source_bandwidth` is 204.8 MiB/s, `staging_bandwidth` is 204.8 MiB/s, both appear on
//! the staging line of `Display` and in `to_json`. Proves f.2 and e.3.

mod common;

use moruna_kernel::{TraceRecord, TraceSink};
use moruna_trace::{ExitReason, RunReport, TraceWriter};
use common::{TempDir, config, limits, meta, record};

const GIB: u64 = 1024 * 1024 * 1024;
/// 204.8 MiB/s, the figure the SDD names: 2 GiB over 10 s.
const EXPECTED: f64 = (2 * GIB) as f64 / 10.0;
/// A tenth of a GiB, demoted by each of the first ten records and promoted by each of the
/// last ten; ten of them is a GiB to within the four bytes integer division loses.
const SPILL_EACH: u64 = GIB / 10;

fn close(a: f64, b: f64, what: &str) {
    assert!(
        (a - b).abs() <= 1e-6 * b.abs().max(1.0),
        "{what}: {a} vs {b}"
    );
}

/// Twenty stage 1 records, each carrying a twentieth of 2 GiB, the first ten demoting and
/// the last ten promoting a tenth of a GiB each.
fn trace_of(with_staging: bool) -> Vec<TraceRecord> {
    let s = 1_000_000_000u64;
    (0..20u64)
        .map(|i| TraceRecord {
            t_start_ns: i * s / 2,
            t_end_ns: i * s / 2 + s / 4,
            bytes_in: 2 * GIB / 20,
            rows_in: 1_000,
            rows_out: 1_000,
            bytes_out: 2 * GIB / 20,
            mem_anon_before: 0,
            mem_anon_peak: 0,
            placement_miss_wait_us: 0,
            throttled_delta_us: 0,
            staging_bytes_delta: match (with_staging, i < 10) {
                (false, _) => 0,
                (true, true) => SPILL_EACH as i64,
                (true, false) => -(SPILL_EACH as i64),
            },
            ..record(i, 1)
        })
        .collect()
}

fn report(tag: &str, with_staging: bool) -> RunReport {
    let dir = TempDir::new(tag);
    let writer = TraceWriter::start(config(dir.path())).expect("start");
    for r in trace_of(with_staging) {
        writer.record(r);
    }
    let view = writer.finish().expect("finish");
    RunReport::compute(&view, &limits(), &meta(ExitReason::Completed))
}

#[test]
fn tr_t10_staging_vs_source_bandwidth() {
    let r = report("t10", true);
    close(r.wall_s, 10.0, "wall_s");
    close(r.source_bandwidth, EXPECTED, "source_bandwidth");
    close(r.staging_bandwidth, EXPECTED, "staging_bandwidth");
    assert_eq!(r.staging_bytes_written, 10 * SPILL_EACH);
    assert!(r.staging_engaged);

    // Both appear on the staging line, side by side, so a staging directory that shares a
    // device with the source is visible (architecture 7).
    let shown = format!("{r}");
    let line = shown
        .lines()
        .find(|l| l.starts_with("staging:"))
        .expect("the staging line");
    // 204.8 MiB/s to the three significant figures e.3 asks for is 205 MiB/s.
    assert!(line.contains("staging bandwidth 205 MiB/s"), "{line}");
    assert!(line.contains("source bandwidth 205 MiB/s"), "{line}");

    // And in the JSON, as numbers.
    let json = r.to_json();
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
    close(
        parsed["source_bandwidth"]
            .as_f64()
            .expect("source_bandwidth"),
        EXPECTED,
        "source_bandwidth in JSON",
    );
    close(
        parsed["staging_bandwidth"]
            .as_f64()
            .expect("staging_bandwidth"),
        EXPECTED,
        "staging_bandwidth in JSON",
    );
}

#[test]
fn tr_t10_no_staging_reports_zero() {
    let r = report("t10-none", false);
    close(r.staging_bandwidth, 0.0, "staging_bandwidth");
    assert!(!r.staging_engaged);
    assert_eq!(r.staging_bytes_written, 0);
    close(r.source_bandwidth, EXPECTED, "source_bandwidth");
    let shown = format!("{r}");
    assert!(shown.contains("staging: not engaged"), "{shown}");
}

/// e.3: the whole layout, on a report that exercises every optional line. The count stays
/// inside forty lines however long the chain and however many notes the run collected,
/// which is what the Python surface asserts in wave 5 (12 PY-T5).
#[test]
fn tr_t10_layout_stays_inside_forty_lines() {
    use moruna_kernel::GilState;

    let dir = TempDir::new("t10-layout");
    let writer = TraceWriter::start(config(dir.path())).expect("start");
    // Thirty stages, more than the layout prints one line each for.
    for stage in 1..=30u16 {
        for seq in 0..3u64 {
            writer.record(TraceRecord {
                t_start_ns: seq * 1_000_000_000,
                t_end_ns: seq * 1_000_000_000 + 500_000_000,
                bytes_in: 1_024 * 1_024,
                mem_anon_before: 0,
                mem_anon_peak: 2 * 1_024 * 1_024,
                state_bytes: 1_000 * (seq + 1),
                instance: 0,
                ..record(seq, stage)
            });
        }
    }
    let view = writer.finish().expect("finish");

    let mut m = meta(moruna_trace::ExitReason::Terminated {
        diagnostic: "budget: morsel 3 stage 7 footprint 9 exceeds budget 8".to_string(),
    });
    m.resumed = true;
    m.manifest = Some(std::path::PathBuf::from("/tmp/moruna/run/manifest.json"));
    m.gil = vec![(1, GilState::Serialised), (2, GilState::FreeThreaded)];
    m.sizer = "learned";
    m.sizer_fallback_at = Some(1_234);
    m.notes = (0..12).map(|i| format!("note number {i}")).collect();
    m.bottleneck_timeline = (0..20).map(|i| (i as f64, format!("class {i}"))).collect();

    let mut lim = limits();
    lim.memory_kill = None;
    lim.devices.clear();

    let r = RunReport::compute(&view, &lim, &m);
    let shown = format!("{r}");
    let lines: Vec<&str> = shown.lines().collect();
    assert!(
        lines.len() <= 40,
        "the layout must stay inside forty lines, got {}:\n{shown}",
        lines.len()
    );
    assert!(shown.contains("(resumed)"), "{shown}");
    assert!(shown.contains("terminated: budget:"), "{shown}");
    assert!(
        shown.contains("manifest: /tmp/moruna/run/manifest.json"),
        "{shown}"
    );
    assert!(shown.contains("kill none"), "{shown}");
    assert!(shown.contains("sizer: learned"), "{shown}");
    assert!(
        shown.contains("fell back to the rule sizer at morsel 1234"),
        "{shown}"
    );
    assert!(
        shown.contains("serialised on at least one stage"),
        "{shown}"
    );
    assert!(shown.contains("1:Serialised"), "{shown}");
    assert!(shown.contains("bottlenecks:"), "{shown}");
    assert!(shown.contains("stages: 10 more not shown"), "{shown}");
    assert!(shown.contains("note: 7 more not shown"), "{shown}");
    assert!(
        shown.contains("state "),
        "a stage that holds state says so: {shown}"
    );
    assert!(r.gil_serialised);

    // A report with no devices and no optional lines is shorter and still complete.
    let plain = RunReport::compute(&view, &limits(), &meta(moruna_trace::ExitReason::Completed));
    let plain_shown = format!("{plain}");
    assert!(plain_shown.lines().count() <= 40);
    assert!(!plain_shown.contains("manifest:"));
    assert!(plain_shown.contains("devices 1"), "{plain_shown}");
    assert!(plain_shown.contains("free threaded"), "{plain_shown}");
}
