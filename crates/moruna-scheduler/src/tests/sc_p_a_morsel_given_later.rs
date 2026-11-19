//! Item 6.3m (`architecture/work/6.3m-unit-is-not-a-morsel.md`, Decided): a stage gives a
//! morsel's output under that morsel's number, once, and the scheduler records it then.
//!
//! The stage here gives each morsel one unit late, as a pipelined kernel does: a unit gives the
//! morsel before its own, and the stage's last morsel comes from `finish_morsels`, beside one
//! batch of the stage's own numbered after every input number. Its even morsels come timed, its
//! odd ones untimed. The scheduler pushes each under its own number with its own record and
//! growth, gives the cost model an observation only of the timed, and refuses a number the
//! stage was not given.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use moruna_cost::{
    CostKey, CostModel, Defaults, Digest, InUse, LocalStore, LogLinear, ObservationSource, Online,
    Target,
};
use moruna_job::Role;
use moruna_kernel::{
    CancelToken, Completed, Fingerprint, InitCtx, Kernel, KernelKind, KernelState, MorselTime,
    MorunaError, Payload, PayloadKind, PayloadSpec, Result, Seq, SourceSchema, StatsSource,
    TierPref, TraceRecord,
};
use moruna_testkit::FakeSource;

use super::common::{Rig, RigBuilder};

/// Ten morsels of a thousand rows of eight bytes.
const ROWS: u64 = 10_000;
const MORSEL: u64 = 1_000;

/// How the stage misbehaves, if it does.
#[derive(Clone, Copy, PartialEq)]
enum Fault {
    None,
    /// A unit gives a number the stage was never given.
    Unknown,
    /// The stage's end gives again a morsel it gave already.
    Again,
}

/// The morsel a unit holds back for the next.
struct Held(Option<(Seq, Payload)>);

impl KernelState for Held {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

fn held(state: &mut dyn KernelState) -> Result<&mut Held> {
    state
        .as_any_mut()
        .downcast_mut::<Held>()
        .ok_or_else(|| MorunaError::Plan("a state this kernel did not make".into()))
}

/// One instance; each morsel given one unit late, its rows as they came.
struct Late {
    fault: Fault,
    applied: AtomicU64,
    /// The `after` its end was given.
    after: Mutex<Option<Seq>>,
}

impl Late {
    fn new(fault: Fault) -> Arc<Late> {
        Arc::new(Late {
            fault,
            applied: AtomicU64::new(0),
            after: Mutex::new(None),
        })
    }

    /// Timed when its number is even.
    fn time(seq: Seq) -> MorselTime {
        if seq.is_multiple_of(2) {
            MorselTime::Given {
                wall_ns: 2_000_000,
                cpu_ns: 1_000_000,
            }
        } else {
            MorselTime::Untimed
        }
    }
}

impl Kernel for Late {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint([63; 32])
    }

    fn kind(&self) -> KernelKind {
        KernelKind::Stateful {
            max_instances: core::num::NonZeroUsize::MIN,
        }
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        Ok(input.clone())
    }

    fn init(&self, _ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        Ok(Box::new(Held(None)))
    }

    fn apply(&self, _state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        Ok(input)
    }

    fn apply_morsels(
        &self,
        state: &mut dyn KernelState,
        input: Payload,
        seq: Seq,
    ) -> Result<Vec<Completed>> {
        self.applied.fetch_add(1, Ordering::SeqCst);
        if self.fault == Fault::Unknown && seq == 3 {
            return Ok(vec![Completed::unit(seq + 1_000, input)]);
        }
        let st = held(state)?;
        Ok(st
            .0
            .replace((seq, input))
            .map(|(seq, output)| Completed {
                seq,
                output,
                time: Late::time(seq),
            })
            .into_iter()
            .collect())
    }

    fn finish_morsels(&self, state: &mut dyn KernelState, after: Seq) -> Result<Vec<Completed>> {
        *self.after.lock().unwrap_or_else(|e| e.into_inner()) = Some(after);
        let st = held(state)?;
        let Some((seq, last)) = st.0.take() else {
            return Ok(Vec::new());
        };
        let own = match &last {
            Payload::Table(batch, tier) => Payload::Table(batch.slice(0, 1), *tier),
            Payload::Tensor(..) => return Err(MorunaError::Plan("a table".into())),
        };
        let again = if self.fault == Fault::Again { 0 } else { seq };
        Ok(vec![
            Completed::unit(again, last),
            Completed::unit(after, own),
        ])
    }
}

fn key() -> CostKey {
    CostKey {
        role: Role::Kernel,
        kind: "late".into(),
        fingerprint: Digest([63; 32]),
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
            "moruna-sc-p-6.3m-late-{}-{}",
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

/// The run of `stage` over the source, one worker, with a cost model keeping in `store`.
fn run(stage: &Arc<Late>, store: &Arc<LocalStore>) -> (Rig, crate::RunOutcome) {
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 1;
            cfg.workers_active = 1;
            cfg.initial_morsel_target = MORSEL * 8;
            cfg.morsel_min = 8;
        })
        .source(FakeSource::new().splits(1, ROWS, ROWS * 8))
        .kernel(Arc::clone(stage) as Arc<dyn Kernel>)
        .go();
    let model: Box<dyn CostModel> = Box::new(LogLinear::new(Defaults::standard()));
    let online = Arc::new(Online::new(InUse::new(model, None), store.clone()));
    rig.scheduler
        .set_cost(online, vec![key()], None)
        .expect("set before the run");
    rig.scheduler
        .init_instances()
        .expect("the stage's instance starts");
    let outcome = rig.scheduler.run(CancelToken::new()).expect("the run ends");
    (rig, outcome)
}

fn records(rig: &Rig) -> Vec<TraceRecord> {
    let mut records: Vec<TraceRecord> = rig
        .trace
        .records()
        .into_iter()
        .filter(|r| r.stage == 1)
        .collect();
    records.sort_by_key(|r| r.seq);
    records
}

#[test]
fn sc_p_a_morsel_given_later_is_recorded_pushed_and_learned_under_its_own_number() {
    let dir = Dir::new();
    let store = Arc::new(LocalStore::open(&dir.0).expect("opens"));
    let stage = Late::new(Fault::None);
    let (rig, outcome) = run(&stage, &store);
    assert!(
        matches!(outcome, crate::RunOutcome::Completed { .. }),
        "{outcome:?}"
    );
    let morsels = stage.applied.load(Ordering::SeqCst);
    assert!(morsels >= 8, "about ten morsels: {morsels}");
    let after = stage
        .after
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .expect("the stage ended");
    assert_eq!(after, morsels, "numbered after every input number (d.16)");

    // One record a morsel, under its own number, its rows in and out its own; and the stage's
    // own batch after them, with nothing in.
    let records = records(&rig);
    let numbers: Vec<Seq> = records.iter().map(|r| r.seq).collect();
    assert_eq!(numbers, (0..=morsels).collect::<Vec<_>>(), "{records:?}");
    for r in &records[..records.len() - 1] {
        assert!(
            r.rows_in > 0 && r.rows_out == r.rows_in && r.bytes_out == r.bytes_in,
            "morsel {}: {} rows in, {} out",
            r.seq,
            r.rows_in,
            r.rows_out
        );
    }
    let own = &records[records.len() - 1];
    assert_eq!((own.rows_in, own.rows_out), (0, 1), "the stage's own batch");
    assert_eq!(
        records.iter().map(|r| r.rows_in).sum::<u64>(),
        ROWS,
        "every row in a morsel"
    );
    // Every number reaches the sink once.
    let mut written = rig.sink.written();
    written.sort_unstable();
    assert_eq!(written, numbers);
    // The stage's growth is its morsels': a byte out for a byte in.
    let entry = &rig.scheduler.shared.stages[0];
    assert!((entry.growth() - 1.0).abs() < 1e-9, "{}", entry.growth());
    assert!(
        entry
            .open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    );

    // The cost model learned the timed morsels only: the even, and the last, which its end gave
    // timed as the end's call.
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
    let timed = (0..morsels - 1).filter(|seq| seq.is_multiple_of(2)).count() + 1;
    assert_eq!(kept.len(), timed, "an observation a timed morsel");
    let given = kept
        .iter()
        .filter(|o| o.outcome.get(Target::Duration) == Some(0.002))
        .count();
    assert_eq!(
        given,
        timed - 1,
        "the time a kernel gave is the cost model's"
    );
    for o in &kept {
        assert_eq!(
            o.outcome.get(Target::RowsOut),
            o.inputs.data.rows.map(|r| r as f64)
        );
    }
}

#[test]
fn sc_p_a_number_the_stage_was_not_given_fails_the_unit() {
    let dir = Dir::new();
    let store = Arc::new(LocalStore::open(&dir.0).expect("opens"));
    let (_rig, outcome) = run(&Late::new(Fault::Unknown), &store);
    let crate::RunOutcome::Terminated { diagnostic, .. } = outcome else {
        panic!("expected Terminated, got {outcome:?}");
    };
    let said = diagnostic.to_string();
    assert!(
        matches!(diagnostic, MorunaError::Kernel { seq: 3, .. })
            && said.contains("it gave morsel 1003, which the stage was not given"),
        "{said}"
    );
}

#[test]
fn sc_p_a_morsel_given_twice_at_the_stage_s_end_ends_the_run() {
    let dir = Dir::new();
    let store = Arc::new(LocalStore::open(&dir.0).expect("opens"));
    let (_rig, outcome) = run(&Late::new(Fault::Again), &store);
    let crate::RunOutcome::Terminated { diagnostic, .. } = outcome else {
        panic!("expected Terminated, got {outcome:?}");
    };
    let said = diagnostic.to_string();
    assert!(
        said.contains("its finish gave morsel 0, which the stage was not given or has given"),
        "{said}"
    );
}
