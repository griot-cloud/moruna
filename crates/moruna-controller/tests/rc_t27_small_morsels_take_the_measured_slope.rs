//! RC-T27 (6.3w; 11 f.3, system design 3A): on small morsels of one size, the anon fit takes the
//! stage's measured line's slope, not the steepest growth over a morsel.
//!
//! IM-T32 and IM-T33 (6.3f, 1 GiB, an image function echoing its batches on 4 CPUs) found it.
//! A run's morsels all come at one size, so the window never has the spread a slope needs, and
//! the growth across one `apply` holds what does not scale with the morsel: an allocator's chunk,
//! first touches, a new instance's own few MB. On 5 KB batches the fit read 115 to 216 bytes a
//! morsel byte and the run kept 1 or 2 workers of 4; on 1 MiB batches it read 12.9 where the
//! truth is about 2, and the workers dropped to 1 at the warm-up's end, when the room the
//! instances did not need went back to the arena and the plan was solved again. With the stage's
//! measured line (about 4 MB fixed and 2 bytes a byte here), the window's no-spread fallback
//! takes the line's slope; the growth that does not scale goes into the fixed term, once; every
//! reading in the window is still bounded (RC-I1); and the workers stay at what the arena and
//! the CPU allow.

mod common;

use common::{GIB, MIB, active_workers, config, record, stats};
use moruna_controller::Controller;
use moruna_kernel::{KernelHints, MeasuredLine, ProbeResult, TraceRecord};
use moruna_testkit::{FakeKnobs, FakeSampler};

/// The stage's measured line: about 4 MB whatever the batch, 2 bytes a byte of input.
const LINE: MeasuredLine = MeasuredLine {
    fixed_bytes: 4_000_000.0,
    per_input_byte: 2.0,
};
/// What the process holds before each `apply` above the resting figure: the instances' own.
const HELD: u64 = 4_000_000;
/// The CPUs of the job, and its workers.
const WORKERS: u16 = 4;
/// The room the instances did not need, handed back to the arena at the warm-up's end.
const HANDED_BACK: u64 = 4 * MIB;

/// One reading as the test feeds it: the morsel, and what the process grew by across `apply`.
#[derive(Clone, Copy, Debug)]
struct Reading {
    bytes_in: u64,
    growth: u64,
}

/// `count` morsels of `bytes_in` bytes, each growing the process by two bytes a byte, and the
/// ones `chunks` names by that much more, which does not scale with the morsel.
fn readings(bytes_in: u64, count: usize, chunks: &[(usize, u64)]) -> Vec<Reading> {
    (0..count)
        .map(|at| Reading {
            bytes_in,
            growth: 2 * bytes_in
                + chunks
                    .iter()
                    .filter(|(on, _)| *on == at)
                    .map(|(_, chunk)| chunk)
                    .sum::<u64>(),
        })
        .collect()
}

/// What a run ended on: the worker count it last wrote, and its fit as the report says it.
struct Ended {
    workers: u16,
    c: u64,
    a: f64,
    note: String,
}

/// A run at 1 GiB on 4 workers whose stage carries `line`, probed on a morsel of the run's own
/// size at two bytes a byte, fed `readings` a tick at a time; then the warm-up's end hands room
/// back to the arena and the plan is solved again (`set_arena`).
fn run(line: Option<MeasuredLine>, readings: &[Reading]) -> Ended {
    let cfg = config(GIB, WORKERS);
    let resting = cfg.arena_bytes;
    let first = readings[0].bytes_in;
    let probe = ProbeResult {
        bytes_in: first,
        rows_in: first / 16,
        bytes_out: first,
        peak_delta: 2 * first,
        dev_peak_delta: 0,
        wall_ns: 1_000_000,
        cpu_ns: 1_000_000,
    };
    let hints = KernelHints {
        measured_line: line,
        ..KernelHints::default()
    };
    let rig = common::Rig::new(
        cfg,
        vec![common::kernel(1, hints)],
        FakeKnobs::new()
            .probe_result(1, probe)
            .stats(stats(WORKERS, WORKERS)),
        FakeSampler::new().scripted(common::steady(resting + HELD, 4096)),
    );
    rig.run_up();
    for (at, r) in readings.iter().enumerate() {
        rig.feed(&reading(at as u64 + 1, resting, r));
        rig.controller.tick_once();
    }
    rig.controller.set_arena(resting + HANDED_BACK, 0);
    let workers = *active_workers(&rig.writes())
        .last()
        .expect("a worker count");
    let (c, a, note) = fit(&rig.controller);
    rig.controller.stop();
    Ended {
        workers,
        c,
        a,
        note,
    }
}

/// The trace record of one reading: the process at `resting` plus what the instances hold
/// before `apply`, and that plus the growth at its peak.
fn reading(seq: u64, resting: u64, r: &Reading) -> TraceRecord {
    let mut record = record(seq, 1, r.bytes_in, r.growth);
    record.mem_anon_before = resting + HELD;
    record.mem_anon_peak = resting + HELD + r.growth;
    record.knob_active_workers = WORKERS;
    record
}

/// The fit the run ended with, from the report's note: its fixed bytes, its bytes a morsel byte,
/// and the note itself.
fn fit(controller: &Controller) -> (u64, f64, String) {
    let notes = controller.summary().notes;
    let note = notes
        .iter()
        .find(|n| n.starts_with("controller stage 1:"))
        .cloned()
        .unwrap_or_else(|| panic!("no fit in {notes:?}"));
    let after = |mark: &str| {
        let at = note.find(mark).unwrap_or_else(|| panic!("{mark}: {note}")) + mark.len();
        note[at..]
            .split_whitespace()
            .next()
            .unwrap_or_else(|| panic!("{mark}: {note}"))
            .to_string()
    };
    let c = after("anon fit ").parse().expect("the fixed bytes");
    let a = after("bytes fixed plus ").parse().expect("the slope");
    (c, a, note)
}

/// Every reading bounded by the fit as `anon_footprint` charges it (RC-I1): `c` once and `a`
/// per byte of each morsel in flight, one a worker.
fn assert_bounded(ended: &Ended, readings: &[Reading]) {
    for r in readings {
        let x = r.bytes_in as f64;
        let others = f64::from(WORKERS - 1) * x;
        let charged = ended.c as f64 + ended.a * (x + others);
        assert!(
            charged >= (HELD + r.growth) as f64,
            "{r:?} is not bounded by {charged:.0}: {}",
            ended.note
        );
    }
}

/// The run took the line's slope and kept every worker the arena and the CPU allow.
fn assert_took_the_line(ended: &Ended) {
    assert!(
        (ended.a - LINE.per_input_byte).abs() < 1e-9 && ended.workers == WORKERS,
        "the slope is the line's and the workers stay at {WORKERS}: {} workers; {}",
        ended.workers,
        ended.note
    );
    assert!(
        ended
            .note
            .contains("a morsel byte (slope from the stage's measured line)"),
        "{}",
        ended.note
    );
}

#[test]
fn rc_t27_five_kilobyte_morsels_take_the_measured_slope() {
    // 5 KB morsels; two of them carry an allocator's chunk of about a megabyte and half of one.
    let fed = readings(5_000, 12, &[(2, 1_070_000), (7, 570_000)]);
    let ended = run(Some(LINE), &fed);
    assert_took_the_line(&ended);
    // The chunk, and the morsel's own growth, less what the line charges the four in flight.
    assert_eq!(
        ended.c,
        HELD + 1_070_000 + 2 * 5_000 - 2 * 4 * 5_000,
        "the growth that does not scale is in the fixed term, once: {}",
        ended.note
    );
    assert_bounded(&ended, &fed);
}

#[test]
fn rc_t27_one_mebibyte_morsels_take_the_measured_slope() {
    // 1 MiB morsels; at the warm-up's end three instances start, about 3.8 MB each, inside one
    // morsel's `apply`.
    let fed = readings(MIB, 12, &[(5, 3 * 3_810_000)]);
    let ended = run(Some(LINE), &fed);
    assert_took_the_line(&ended);
    assert_bounded(&ended, &fed);
}

#[test]
fn rc_t27_without_a_line_the_steepest_reading_stands() {
    // Where no line exists, nothing changes: the steepest growth over a morsel, said so.
    let fed = readings(5_000, 12, &[(2, 1_070_000), (7, 570_000)]);
    let ended = run(None, &fed);
    let steepest = (1_070_000.0 + 10_000.0) / 5_000.0;
    assert!((ended.a - steepest).abs() < 1e-6, "{}", ended.note);
    assert_eq!(ended.workers, 1, "{}", ended.note);
    assert!(
        ended.note.contains("a morsel byte (steepest reading)"),
        "{}",
        ended.note
    );
    assert_bounded(&ended, &fed);
}

#[test]
fn rc_t27_a_window_with_spread_fits_its_own_line() {
    // Morsels of 1, 2 and 4 MiB: the window has the spread, and fits its own slope of 3 whatever
    // the line says (the line seeds it and does not override it).
    let fed: Vec<Reading> = [MIB, 2 * MIB, 4 * MIB, MIB, 2 * MIB, 4 * MIB]
        .iter()
        .map(|&bytes_in| Reading {
            bytes_in,
            growth: 3 * bytes_in,
        })
        .collect();
    let ended = run(Some(LINE), &fed);
    assert!((ended.a - 3.0).abs() < 1e-6, "{}", ended.note);
    assert!(
        ended.note.contains("a morsel byte (fitted)"),
        "{}",
        ended.note
    );
    assert_bounded(&ended, &fed);
}
