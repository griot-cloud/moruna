//! Item 6.3m (`architecture/work/6.3m-unit-is-not-a-morsel.md`): what the engine learns of a
//! function from one unit is that unit's own morsel.
//!
//! A ring stage posts a morsel's pieces to its task and returns with the answers that have
//! come; the rest come with later units, or at the stage's end
//! (`moruna_micro::task::TaskState::pipelined`). So after the stage's first unit (6.3j) a unit
//! gives other morsels' answers, under its own morsel's number, and everything that reads one
//! unit reads one morsel's input beside another's output:
//!
//! - the stage's learned growth, the largest bytes out over bytes in of any one unit
//!   (`StageEntry::learn_growth`), by which reads are cut (`source_drive`, `grown`);
//! - the trace's record of each morsel, its rows and bytes out beside its rows and bytes in,
//!   and the morsel the stage pushes downstream under that number;
//! - the cost model's observation of each unit (`learning::settle`).
//!
//! The stage here is 3.1's model of the kernel's (`sc_p_native_stage_memory`), one task on one
//! worker, whose function gives back the rows it is given: a byte out for a byte in, a row for
//! a row, whatever the morsel. Its function takes three milliseconds a piece, far longer than
//! the engine takes to bring the next morsel, as a real function does. A morsel is two pieces.
//!
//! Each test says what its reader must see of this function, and fails today.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use moruna_cost::{
    CostKey, CostModel, Defaults, Digest, InUse, Inputs, LocalStore, LogLinear, ObservationSource,
    Online, Target,
};
use moruna_job::Role;
use moruna_kernel::arrow::array::{Array, Int64Array};
use moruna_kernel::{
    BoxFuture, CancelToken, Kernel, Outcome, Payload, PayloadSpec, Result, Seq, Sink, SinkSummary,
    SourceSchema, StatsSource, TraceRecord,
};
use moruna_testkit::{FakeSink, FakeSource};

use super::common::{Rig, RigBuilder};
use super::sc_p_native_stage_memory::NativeStage;

/// A task's piece: 8 KiB.
const PIECE: u64 = 8 * 1024;
/// A morsel's rows, of eight bytes: two pieces (`layout::cut`).
const MORSEL: u64 = 1800;
/// Twenty morsels and a quarter of one.
const ROWS: u64 = 20 * MORSEL + MORSEL / 4;
/// What the function takes a piece.
const WORK: Duration = Duration::from_millis(3);

/// What the stage pushed downstream: each morsel's number and the values it held.
type Given = Arc<Mutex<Vec<(Seq, Vec<i64>)>>>;

/// A sink that keeps what it is given under each number.
struct Keeping {
    inner: FakeSink,
    given: Given,
}

impl Sink for Keeping {
    fn open(&mut self, schema: &SourceSchema) -> Result<()> {
        self.inner.open(schema)
    }

    fn accepts(&self) -> PayloadSpec {
        self.inner.accepts()
    }

    fn write(&self, seq: Seq, payload: Payload) -> BoxFuture<'_, Result<()>> {
        if let Payload::Table(batch, _) = &payload {
            let values = batch
                .column(0)
                .as_any()
                .downcast_ref::<Int64Array>()
                .map(|a| a.values().to_vec())
                .unwrap_or_default();
            self.given
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((seq, values));
        }
        self.inner.write(seq, payload)
    }

    fn finish(&mut self) -> Result<SinkSummary> {
        self.inner.finish()
    }

    fn committed_seq(&self) -> Option<Seq> {
        self.inner.committed_seq()
    }

    fn skip(&self, seq: Seq) {
        self.inner.skip(seq);
    }

    fn checkpoint(&self) -> Result<Option<Vec<u8>>> {
        self.inner.checkpoint()
    }
}

/// The run: the source's rows through the stage, one task on one worker. The rig, and what
/// the stage gave downstream.
fn run(setup: impl FnOnce(&Rig)) -> (Rig, Vec<(Seq, Vec<i64>)>) {
    let stage = Arc::new(NativeStage::of(1, PIECE, WORK));
    let given = Given::default();
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 1;
            cfg.workers_active = 1;
            cfg.initial_morsel_target = MORSEL * 8;
            cfg.morsel_min = 8;
            cfg.read_ahead = 2;
        })
        .source(FakeSource::new().splits(1, ROWS, ROWS * 8))
        .sink_over(Box::new(Keeping {
            inner: FakeSink::new(),
            given: Arc::clone(&given),
        }))
        .kernel(stage as Arc<dyn Kernel>)
        .go();
    setup(&rig);
    rig.scheduler
        .init_instances()
        .expect("the stage's task starts");
    match rig.scheduler.run(CancelToken::new()) {
        Ok(crate::RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }
    let given = std::mem::take(&mut *given.lock().unwrap_or_else(|e| e.into_inner()));
    // Every row once, whatever number it came under: the stage loses and repeats nothing.
    let mut all: Vec<i64> = given.iter().flat_map(|(_, v)| v.iter().copied()).collect();
    all.sort_unstable();
    assert!(
        all == (0..ROWS as i64).collect::<Vec<_>>(),
        "every row once"
    );
    (rig, given)
}

/// The stage's records of the morsels it was given, by their numbers.
fn morsels(rig: &Rig) -> Vec<TraceRecord> {
    let mut records: Vec<TraceRecord> = rig
        .trace
        .records()
        .into_iter()
        .filter(|r| r.stage == 1 && r.rows_in > 0)
        .collect();
    records.sort_by_key(|r| r.seq);
    assert!(
        records.len() >= 16,
        "about twenty morsels: {}",
        records.len()
    );
    records
}

#[test]
fn sc_p_a_ring_stage_s_learned_growth_is_its_function_s() {
    let (rig, _) = run(|_| {});
    let records = morsels(&rig);
    let stage = rig
        .scheduler
        .shared
        .stages
        .iter()
        .find(|entry| entry.stage == 1)
        .expect("the stage");
    let growth = stage.growth();
    assert!(
        (growth - 1.0).abs() < 0.05,
        "the stage's learned growth is {growth:.2}, where its function gives a byte out for a \
         byte in; its units gave, of the bytes they took: {:?}",
        records
            .iter()
            .map(|r| format!("{:.2}", r.bytes_out as f64 / r.bytes_in as f64))
            .collect::<Vec<_>>()
    );
}

#[test]
fn sc_p_a_ring_stage_s_record_of_a_morsel_is_that_morsel_s() {
    let (rig, given) = run(|_| {});
    let records = morsels(&rig);
    let given: HashMap<Seq, &Vec<i64>> = given.iter().map(|(seq, v)| (*seq, v)).collect();
    // A morsel's rows are the source's next (`FakeSource::value_at`: row `i` holds `i`).
    let mut next = 0i64;
    let mut wrong = Vec::new();
    for r in records.iter().filter(|r| r.outcome == Outcome::Ok) {
        let own: Vec<i64> = (next..next + r.rows_in as i64).collect();
        next += r.rows_in as i64;
        let under_its_number = given.get(&r.seq).map(|v| v.as_slice()).unwrap_or(&[]);
        if r.rows_out != r.rows_in || under_its_number != own.as_slice() {
            wrong.push(format!(
                "morsel {}: rows {}..{} in, {} rows out by its record, rows {:?}..{:?} given \
                 under its number",
                r.seq,
                own[0],
                own[own.len() - 1] + 1,
                r.rows_out,
                under_its_number.first(),
                under_its_number.last().map(|v| v + 1)
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of the stage's {} morsels have a record, and an output under their number, that is \
         not their own, where the function gives back the rows it is given: {wrong:#?}",
        wrong.len(),
        records.len()
    );
}

fn key() -> CostKey {
    CostKey {
        role: Role::Kernel,
        kind: "ring".into(),
        fingerprint: Digest([1; 32]),
        settings: Digest([0; 32]),
        schema: Digest([0; 32]),
    }
}

/// A store directory only this test uses, removed when it goes.
struct Dir(std::path::PathBuf);

impl Dir {
    fn new() -> Dir {
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "moruna-sc-p-6.3m-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        Dir(path)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn sc_p_a_ring_stage_s_unit_is_observed_with_its_own_output() {
    let dir = Dir::new();
    let store = Arc::new(LocalStore::open(&dir.0).expect("opens"));
    let model: Box<dyn CostModel> = Box::new(LogLinear::new(Defaults::standard()));
    let online = Arc::new(Online::new(InUse::new(model, None), store.clone()));
    let (rig, _) = run(|rig| {
        rig.scheduler
            .set_cost(Arc::clone(&online), vec![key()], None)
            .expect("set before the run");
    });
    let learning = rig
        .scheduler
        .scheduler_stats()
        .policy
        .learning
        .expect("a run with a model reports learning");
    assert_eq!(learning.unwritten, 0, "{learning:?}");
    let mut kept = Vec::new();
    store
        .scan_all(&mut |h| {
            kept.extend(h.observations);
            Ok(())
        })
        .expect("scans");
    assert!(kept.len() >= 16, "a unit a morsel: {}", kept.len());
    // What the model is given of each unit: its bytes and rows in, and what it gave.
    let wrong: Vec<String> = kept
        .iter()
        .filter_map(|o| {
            let (bytes, rows) = (o.inputs.data.bytes? as f64, o.inputs.data.rows? as f64);
            let (out, rows_out) = (
                o.outcome.get(Target::BytesOut)?,
                o.outcome.get(Target::RowsOut)?,
            );
            ((out / bytes - 1.0).abs() > 0.05 || rows_out != rows)
                .then(|| format!("{rows} rows in, {rows_out} out"))
        })
        .collect();
    // Every row is in some unit's observation: none is given outside one, at the stage's end.
    let observed: f64 = kept
        .iter()
        .filter_map(|o| o.outcome.get(Target::RowsOut))
        .sum();
    assert!(
        wrong.is_empty() && observed == ROWS as f64,
        "{} of the {} units the cost model learned from carry an output that is not their own, \
         where the function gives a row for a row: {wrong:?}; they account for {observed} of the \
         stage's {ROWS} rows out",
        wrong.len(),
        kept.len()
    );
    // Item 6.3z: each morsel's time is what its task measured of its pieces and gave on their
    // releases, through the plane's path as the kernel's task takes it: its wall and CPU time
    // alike, and at least the function's work on one piece.
    let untimed: Vec<String> = kept
        .iter()
        .filter_map(|o| {
            let (wall, cpu) = (
                o.outcome.get(Target::Duration)?,
                o.outcome.get(Target::WorkerTime)?,
            );
            (wall != cpu || wall < WORK.as_secs_f64())
                .then(|| format!("wall {wall:.6} s, cpu {cpu:.6} s"))
        })
        .collect();
    assert!(
        untimed.is_empty(),
        "every morsel is timed by its task's pieces, at least {WORK:?} each: {untimed:?}"
    );
    // What it then says of a morsel of the stage.
    let bytes = MORSEL * 8;
    let predicted = online
        .predict(&key(), &Inputs::batch(MORSEL, bytes))
        .deciding
        .cost
        .bytes_out
        .p50;
    assert!(
        (predicted / bytes as f64 - 1.0).abs() < 0.05,
        "the model predicts {predicted:.0} bytes out for a morsel of {bytes}"
    );
}
