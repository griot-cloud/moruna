//! Item 6.3ac (`architecture/work/6.3ac-morsel-search-counts-the-whole-stage.md`, PM 2026-11-19):
//! a unit's CPU is counted once.
//!
//! The stage here gives its morsels in pairs, as a pipelined kernel can: a unit of an even morsel
//! holds it and gives nothing, the next unit gives the held morsel and its own. Each of a pair's
//! records carries half the unit's CPU, so the records sum to the giving units' CPU; the holding
//! units' CPU is in the stage's counter only, which holds every unit's. Each morsel comes with
//! its task's time (`MorselTime::Given`), which the stage's task counter sums.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use moruna_kernel::{
    CancelToken, Completed, Fingerprint, InitCtx, Kernel, KernelKind, KernelState, MorselTime,
    MorunaError, Payload, PayloadKind, PayloadSpec, Result, Seq, SourceSchema, StatsSource,
    TierPref, TraceRecord,
};
use moruna_testkit::FakeSource;

use super::common::RigBuilder;

/// Six morsels of a thousand rows of eight bytes.
const ROWS: u64 = 6_000;
const MORSEL: u64 = 1_000;
/// The CPU a unit that holds its morsel spends, and one that gives a pair.
const HOLD_NS: u64 = 30_000_000;
const GIVE_NS: u64 = 20_000_000;
/// The task's time each morsel comes with.
const TASK_NS: u64 = 7_000_000;
/// What a unit spends beyond its burn, at the most, summed over the run's units.
const SLACK_NS: u64 = 5_000_000;

/// Spend `ns` of this thread's CPU, and say how much was spent.
fn burn(ns: u64) -> u64 {
    let start = crate::cputime::thread_cpu_ns();
    let mut x = 0u64;
    while crate::cputime::thread_cpu_ns().saturating_sub(start) < ns {
        x = std::hint::black_box(x.wrapping_mul(31).wrapping_add(7));
    }
    crate::cputime::thread_cpu_ns().saturating_sub(start)
}

/// The morsel a unit holds for the next.
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

/// One instance; morsels given in pairs.
#[derive(Default)]
struct Pairs {
    /// The CPU each unit burned: (it gave morsels, nanoseconds).
    units: Mutex<Vec<(bool, u64)>>,
    applied: AtomicU64,
}

impl Kernel for Pairs {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint([64; 32])
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
        let st = held(state)?;
        let time = MorselTime::Given {
            wall_ns: TASK_NS,
            cpu_ns: TASK_NS,
        };
        match st.0.take() {
            None => {
                st.0 = Some((seq, input));
                let spent = burn(HOLD_NS);
                self.units
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((false, spent));
                Ok(Vec::new())
            }
            Some((before, output)) => {
                let spent = burn(GIVE_NS);
                self.units
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((true, spent));
                Ok(vec![
                    Completed {
                        seq: before,
                        output,
                        time,
                    },
                    Completed {
                        seq,
                        output: input,
                        time,
                    },
                ])
            }
        }
    }

    fn finish_morsels(&self, state: &mut dyn KernelState, _after: Seq) -> Result<Vec<Completed>> {
        Ok(held(state)?
            .0
            .take()
            .map(|(seq, output)| Completed::unit(seq, output))
            .into_iter()
            .collect())
    }
}

#[test]
fn sc_p_a_unit_s_cpu_is_split_among_its_morsels_and_counted_once() {
    let stage = Arc::new(Pairs::default());
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 1;
            cfg.workers_active = 1;
            cfg.initial_morsel_target = MORSEL * 8;
            cfg.morsel_min = 8;
        })
        .source(FakeSource::new().splits(1, ROWS, ROWS * 8))
        .kernel(Arc::clone(&stage) as Arc<dyn Kernel>)
        .go();
    rig.scheduler
        .init_instances()
        .expect("the stage's instance starts");
    let outcome = rig.scheduler.run(CancelToken::new()).expect("the run ends");
    assert!(
        matches!(outcome, crate::RunOutcome::Completed { .. }),
        "{outcome:?}"
    );
    assert_eq!(stage.applied.load(Ordering::SeqCst), ROWS / MORSEL);

    let units = stage
        .units
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let giving: u64 = units.iter().filter(|u| u.0).map(|u| u.1).sum();
    let holding: u64 = units.iter().filter(|u| !u.0).map(|u| u.1).sum();
    assert_eq!(units.iter().filter(|u| u.0).count(), 3, "{units:?}");

    let mut records: Vec<TraceRecord> = rig
        .trace
        .records()
        .into_iter()
        .filter(|r| r.stage == 1)
        .collect();
    records.sort_by_key(|r| r.seq);
    assert_eq!(records.len() as u64, ROWS / MORSEL, "{records:?}");
    // A pair's records are one unit's, its CPU in halves.
    for pair in records.chunks(2) {
        assert_eq!(pair[0].t_start_ns, pair[1].t_start_ns, "one unit gave both");
        assert!(
            pair[0].cpu_time_us.abs_diff(pair[1].cpu_time_us) <= 1,
            "halves: {} and {} us",
            pair[0].cpu_time_us,
            pair[1].cpu_time_us
        );
    }
    let in_records: u64 = records.iter().map(|r| r.cpu_time_us * 1_000).sum();
    let stats = rig.scheduler.scheduler_stats();
    let counted = &stats.per_stage[0];
    eprintln!(
        "records {:.1} ms, giving units {:.1} ms, holding units {:.1} ms, stage counter {:.1} ms",
        in_records as f64 / 1e6,
        giving as f64 / 1e6,
        holding as f64 / 1e6,
        counted.worker_cpu_ns as f64 / 1e6
    );
    // The records sum to the giving units' CPU, and the holding units' is not in them.
    assert!(
        in_records + SLACK_NS >= giving && in_records <= giving + SLACK_NS,
        "records {in_records} ns, the giving units {giving} ns"
    );
    // The stage's counter holds every unit's.
    assert!(
        counted.worker_cpu_ns >= giving + holding
            && counted.worker_cpu_ns <= giving + holding + SLACK_NS,
        "counter {} ns, the units {} ns",
        counted.worker_cpu_ns,
        giving + holding
    );
    assert!(counted.worker_cpu_ns - in_records + SLACK_NS >= holding);
    // The task's time of every morsel, and every byte taken.
    assert_eq!(counted.task_cpu_ns, ROWS / MORSEL * TASK_NS);
    assert_eq!(
        counted.bytes_in,
        records.iter().map(|r| r.bytes_in).sum::<u64>()
    );
}
