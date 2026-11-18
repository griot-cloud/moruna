//! SC-T51 (item 6.3n, system design 3A; 10 f.5, RB5): a source's queues close only when none
//! of its reads is in flight, none waits to be issued again, and its cursor is past its plan.
//!
//! A read the arena refuses is a wall, not an error (RB5): it keeps its number and its rows and
//! waits to be issued again once memory has come back. While it waits it is neither a read in
//! flight nor a range ahead of the cursor, and the drive closed its source's queues on those
//! two alone. So when the read the arena refused was the plan's last, its queue closed under
//! it, and what followed depended on which came first:
//!
//! - the read came back, was pushed into the closed queue, and the run ended
//!   `staging: push: stage 0 is closed` (6.3j's host test, 2026-11-18: 3 of 12 runs of a
//!   160 MiB input that was short of arena);
//! - or the stages and the sinks downstream drained what the queue held, the sinks were
//!   finished, and the run completed without that read's rows.
//!
//! Both are run here, through the scheduler and the real placement engine, which refuses a
//! push into a closed queue (the testkit's placement takes one). The source's read of its last
//! split is refused, as an arena that is full refuses it (`MorunaError::Alloc`), the first
//! times it is issued; the cursor is past the plan from the first of them. Each run must
//! complete with every row.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use moruna_kernel::{
    Allocator, BoxFuture, CancelToken, Kernel, MorunaError, Payload, Placement, Result, RowRange,
    Sink, Source, SourceSchema, Split, SplitId, Tier, TierBudgets, Topology,
};
use moruna_placement::{PlacementConfig, PlacementEngine};
use moruna_scheduler::{Pipeline, RunOutcome, Scheduler, SchedulerConfig};
use moruna_sinks::SinkHandle;
use moruna_testkit::{
    FakeAllocator, FakeKernel, FakeReactor, FakeSampler, FakeSink, FakeSource, FakeTrace,
};

/// The plan: this many splits, each read whole.
const SPLITS: u32 = 4;
/// Rows of a split, eight bytes each.
const ROWS: u64 = 64;
/// The host tier's budget: room for the whole plan many times over, so the only refusal is the
/// one the source gives.
const BUDGET: u64 = 1 << 20;
/// How long a run may take before it is reported stuck.
const DEADLINE: Duration = Duration::from_secs(60);

/// A test-local `Source` (10 k): `FakeSource`, whose read of its last split is refused with
/// `Alloc`, as a full arena refuses it, the first `refusals` times it is issued.
struct LastReadRefused {
    inner: FakeSource,
    last: SplitId,
    refusals: usize,
    /// Reads of the last split issued so far.
    issued: AtomicUsize,
}

impl Source for LastReadRefused {
    fn schema(&self) -> SourceSchema {
        Source::schema(&self.inner)
    }

    fn plan(&self) -> Result<Vec<Split>> {
        self.inner.plan()
    }

    fn read<'a>(
        &'a self,
        split: &'a Split,
        rows: Option<RowRange>,
        alloc: &'a dyn Allocator,
        tier: Tier,
    ) -> BoxFuture<'a, Result<Payload>> {
        if split.id == self.last && self.issued.fetch_add(1, Ordering::SeqCst) < self.refusals {
            let bytes = split.uncompressed_bytes;
            return Box::pin(async move {
                Err(MorunaError::Alloc {
                    bytes,
                    tier,
                    budget: BUDGET,
                    in_use: BUDGET,
                })
            });
        }
        self.inner.read(split, rows, alloc, tier)
    }
}

/// What a run came to.
struct Ran {
    outcome: RunOutcome,
    /// Reads of the last split the drive issued.
    issued: usize,
    /// The batches the sink was written.
    written: usize,
}

/// One run of the plan through one stage, whose every apply takes `apply`, into one sink: the
/// last split's read is refused `refusals` times and then read.
fn run(refusals: usize, apply: Duration) -> Ran {
    let alloc = FakeAllocator::new();
    let source = Arc::new(LastReadRefused {
        inner: FakeSource::new()
            .splits(SPLITS, ROWS, ROWS * 8)
            .sub_splittable(false),
        last: SPLITS - 1,
        refusals,
        issued: AtomicUsize::new(0),
    });
    let kernel: Arc<dyn Kernel> = Arc::new(FakeKernel::new().latency(apply));
    let sink = FakeSink::new();
    let topology = Topology::chain(1);
    let engine = match PlacementEngine::new(
        PlacementConfig {
            topology: topology.clone(),
            budgets: TierBudgets {
                host: BUDGET,
                pinned_host: BUDGET,
                ..TierBudgets::default()
            },
            fingerprints: vec![kernel.fingerprint()],
            resume_policy: vec![kernel.hints().resume],
            checkpoint_enabled: false,
            ..PlacementConfig::default()
        },
        Arc::new(alloc.clone()),
        Arc::new(FakeReactor::new()),
    ) {
        Ok(engine) => engine,
        Err(e) => panic!("the placement engine: {e}"),
    };
    for queue in 0..topology.queue_count() {
        engine.set_promotion_window(queue, 1);
    }
    let placement: Arc<dyn Placement> = engine;
    let pipeline = Pipeline::chain(
        Arc::clone(&source) as Arc<dyn Source>,
        vec![kernel],
        SinkHandle::wrap(Box::new(sink.clone()) as Box<dyn Sink>, false, 1 << 28),
    );
    let scheduler = match Scheduler::new(
        SchedulerConfig {
            workers_max: 1,
            workers_active: 1,
            read_ahead: 2,
            // Whole splits: each read is one split, so the plan's last read is its last split's.
            initial_morsel_target: ROWS * 8,
            morsel_min: ROWS * 8,
            morsel_max: 1 << 30,
            ..SchedulerConfig::default()
        },
        pipeline,
        placement,
        Arc::new(alloc),
        Arc::new(FakeTrace::new().capacity(1 << 12)),
        Arc::new(FakeSampler::new()),
    ) {
        Ok(scheduler) => scheduler,
        Err(e) => panic!("the scheduler: {e}"),
    };
    // A run that never ends is a defect too: it is cancelled and the test fails, not hangs.
    let cancel = CancelToken::new();
    let outcome = std::thread::scope(|scope| {
        // The error is only ever printed, so the thread hands back its words.
        let handle = scope.spawn(|| scheduler.run(cancel.clone()).map_err(|e| e.to_string()));
        let started = Instant::now();
        while !handle.is_finished() && started.elapsed() < DEADLINE {
            std::thread::sleep(Duration::from_millis(1));
        }
        let finished = handle.is_finished();
        if !finished {
            cancel.cancel();
        }
        let outcome = match handle.join() {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(e)) => panic!("the run failed before Running: {e}"),
            Err(_) => panic!("the run's thread panicked"),
        };
        assert!(finished, "the run had not ended after {DEADLINE:?}");
        outcome
    });
    Ran {
        outcome,
        issued: source.issued.load(Ordering::SeqCst),
        written: sink.written().len(),
    }
}

/// The run completed, the sink got every batch and every row, and the last split's read was
/// issued once for each refusal and once more.
fn every_row_is_written(ran: &Ran, refusals: usize) {
    let rows = match &ran.outcome {
        RunOutcome::Completed { sinks } => sinks[0].rows,
        other => panic!(
            "a run whose last read the arena refused {refusals} times and then took did not \
             complete: {other:?}"
        ),
    };
    assert_eq!(
        (ran.written as u64, rows),
        (u64::from(SPLITS), u64::from(SPLITS) * ROWS),
        "the run completed without the batch of the read that waited to be issued again: \
         batches and rows written, of those the plan holds"
    );
    assert_eq!(
        ran.issued,
        refusals + 1,
        "the last split's read was refused {refusals} times and issued once more"
    );
}

/// The read comes back while the stage is still at work on the batches before it: 14 ms of
/// waits (2, 4 and 8) against 20 ms an apply. It was pushed into a closed queue, and the run
/// ended `staging: push: stage 0 is closed`.
#[test]
fn sc_t51_a_last_read_the_arena_refused_is_pushed_when_it_comes_back() {
    let refusals = 3;
    let ran = run(refusals, Duration::from_millis(20));
    every_row_is_written(&ran, refusals);
}

/// The read is still waiting when everything before it has been written: 212 ms of waits
/// behind a stage that takes no time. The sink was finished without it, and the run completed
/// three splits of four.
#[test]
fn sc_t51_a_run_does_not_complete_while_a_read_waits_to_be_issued_again() {
    let refusals = 8;
    let ran = run(refusals, Duration::ZERO);
    every_row_is_written(&ran, refusals);
}
