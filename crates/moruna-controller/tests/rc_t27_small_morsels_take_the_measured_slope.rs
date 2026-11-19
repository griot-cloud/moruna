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
//!
//! The line is the one the run was given at the start (the job's, the registry's or its store's,
//! in the stage's hints), or the one its warm-up measured, the moment the stage's counters say it
//! (PM, 2026-11-19): mid-run, or by the end of the probe when the probe's morsel completed the
//! warm-up. The probe's slope is the line's too: one reading has no spread either.

mod common;

use common::{GIB, MIB, active_workers, config, record, stats};
use moruna_controller::Controller;
use moruna_kernel::{KernelHints, MeasuredLine, ProbeResult, SchedulerStats, TraceRecord};
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
/// IM-T32's small batches, and the allocator's chunk one of them carries.
const SMALL: u64 = 5_000;
const CHUNK: u64 = 1_070_000;

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

/// IM-T32's 5 KB morsels: two carry an allocator's chunk of about a megabyte and half of one.
fn small_morsels() -> Vec<Reading> {
    readings(SMALL, 12, &[(2, CHUNK), (7, 570_000)])
}

/// The scheduler's counters for the one stage on four busy workers, its kernel saying `line`.
fn counters(line: Option<MeasuredLine>) -> SchedulerStats {
    let mut stats = stats(WORKERS, WORKERS);
    stats.per_stage[0].measured_line = line;
    stats
}

/// A probe of one morsel of `bytes_in` bytes that grew the process by `growth`.
fn probe(bytes_in: u64, growth: u64) -> ProbeResult {
    ProbeResult {
        bytes_in,
        rows_in: bytes_in / 16,
        bytes_out: bytes_in,
        peak_delta: growth,
        dev_peak_delta: 0,
        wall_ns: 1_000_000,
        cpu_ns: 1_000_000,
    }
}

/// A run at 1 GiB on 4 workers whose stage was given `line` at the start and whose probe is
/// `probed`; when `warmed` is given, the probe's morsel completes the stage's warm-up and the
/// counters say that line from then on. Run up: prepared, probed and started.
fn started(
    line: Option<MeasuredLine>,
    probed: ProbeResult,
    warmed: Option<MeasuredLine>,
) -> common::Rig {
    let cfg = config(GIB, WORKERS);
    let resting = cfg.arena_bytes;
    let hints = KernelHints {
        measured_line: line,
        ..KernelHints::default()
    };
    let knobs = FakeKnobs::new()
        .probe_result(1, probed)
        .stats(counters(None));
    let prober = match warmed {
        Some(line) => common::RecordingProber::then_stats(knobs.clone(), counters(Some(line))),
        None => common::RecordingProber::new(knobs.clone()),
    };
    let rig = common::Rig::with_prober(
        cfg,
        vec![common::kernel(1, hints)],
        knobs,
        FakeSampler::new().scripted(common::steady(resting + HELD, 4096)),
        prober,
    );
    rig.run_up();
    rig
}

/// Feed `readings` a tick at a time.
fn feed(rig: &common::Rig, readings: &[Reading]) {
    let resting = config(GIB, WORKERS).arena_bytes;
    for (at, r) in readings.iter().enumerate() {
        rig.feed(&reading(at as u64 + 1, resting, r));
        rig.controller.tick_once();
    }
}

/// What a run ended on: the worker count it last wrote, and its fit as the report says it.
struct Ended {
    workers: u16,
    c: u64,
    a: f64,
    note: String,
}

/// The warm-up's end hands room back to the arena and the plan is solved again (`set_arena`);
/// then what the run ended on.
fn hand_back(rig: common::Rig) -> Ended {
    let resting = config(GIB, WORKERS).arena_bytes;
    rig.controller.set_arena(resting + HANDED_BACK, 0);
    let ended = now(&rig);
    rig.controller.stop();
    ended
}

/// The worker count the run last wrote, and its fit as the report says it now.
fn now(rig: &common::Rig) -> Ended {
    let workers = *active_workers(&rig.writes())
        .last()
        .expect("a worker count");
    let (c, a, note) = fit(&rig.controller);
    Ended {
        workers,
        c,
        a,
        note,
    }
}

/// A run whose stage was given `line` at the start, probed on a morsel of the run's own size at
/// two bytes a byte, fed `readings` a tick at a time; then the warm-up's hand-back.
fn run(line: Option<MeasuredLine>, readings: &[Reading]) -> Ended {
    let first = readings[0].bytes_in;
    let rig = started(line, probe(first, 2 * first), None);
    feed(&rig, readings);
    hand_back(rig)
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

/// The fixed term 5 KB morsels leave on four workers by the line: the chunk and the morsel's own
/// growth, less what the line charges the four in flight.
const SMALL_C: u64 = HELD + CHUNK + 2 * SMALL - 2 * 4 * SMALL;

#[test]
fn rc_t27_five_kilobyte_morsels_take_the_measured_slope() {
    let fed = small_morsels();
    let ended = run(Some(LINE), &fed);
    assert_took_the_line(&ended);
    assert_eq!(
        ended.c, SMALL_C,
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
    let fed = small_morsels();
    let ended = run(None, &fed);
    let steepest = (CHUNK + 2 * SMALL) as f64 / SMALL as f64;
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

#[test]
fn rc_t27_the_warm_up_s_line_arriving_mid_run_is_taken_at_once() {
    // IM-T32 as it runs: nothing measured the function, so its stage warms up on the job's first
    // batches and the controller has no line until the warm-up has fitted one.
    let fed = small_morsels();
    let rig = started(None, probe(SMALL, 2 * SMALL), None);
    feed(&rig, &fed);
    let before = now(&rig);
    assert!(
        before.a > 200.0 && before.note.contains("(steepest reading)"),
        "{}",
        before.note
    );
    // The warm-up has fitted its line: the stage's counters say it, and the next tick refits.
    rig.knobs.set_stats(counters(Some(LINE)));
    rig.controller.tick_once();
    let ended = hand_back(rig);
    assert_took_the_line(&ended);
    assert_eq!(ended.c, SMALL_C, "{}", ended.note);
    assert_bounded(&ended, &fed);
}

#[test]
fn rc_t27_the_probe_takes_the_line() {
    // A 5 KB probe that caught the allocator's chunk: alone, its reading is 216 bytes a byte,
    // and the solve at `start` runs one worker of four.
    let steep = probe(SMALL, CHUNK + 2 * SMALL);
    let alone = started(None, steep.clone(), None);
    let without = now(&alone);
    assert_eq!(without.workers, 1, "{}", without.note);
    assert!((without.a - 216.0).abs() < 1e-9, "{}", without.note);
    alone.controller.stop();

    // With a line given at the start, the probe's slope is the line's: one reading has no
    // spread either. Its growth is still the fixed term, until a record separates the two.
    let given = started(Some(LINE), steep.clone(), None);
    let with = now(&given);
    assert_eq!(
        (with.workers, with.a, with.c),
        (WORKERS, LINE.per_input_byte, CHUNK + 2 * SMALL),
        "{}",
        with.note
    );
    given.controller.stop();

    // And a warm-up the probe's morsel completed says its line by the probe's end.
    let warmed = started(None, steep.clone(), Some(LINE));
    let with = now(&warmed);
    assert_eq!(
        (with.workers, with.a),
        (WORKERS, LINE.per_input_byte),
        "{}",
        with.note
    );
    warmed.controller.stop();

    // A line that arrives before any record of the run is the slope at once, the probe's fixed
    // term kept.
    let later = started(None, steep, None);
    later.knobs.set_stats(counters(Some(LINE)));
    later.controller.tick_once();
    let (c, a, note) = fit(&later.controller);
    assert_eq!((a, c), (LINE.per_input_byte, CHUNK + 2 * SMALL), "{note}");
    later.controller.stop();
}
