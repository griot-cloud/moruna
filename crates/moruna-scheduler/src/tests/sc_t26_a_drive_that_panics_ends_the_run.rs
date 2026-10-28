//! SC-T26: a source or a sink that panics on the drive serving it ends the run: the run returns
//! `Ok(Terminated)` within a deadline, with a diagnostic that names the source's split or the
//! sink and says what the panic said, at every read, every write and in `finish`; before, the
//! drive thread died and the run waited for ever. f.5, f.6, h, SC-I9.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use moruna_kernel::{
    Allocator, BoxFuture, MorunaError, Payload, PayloadSpec, Result, RowRange, Seq, Sink,
    SinkSummary, Source, SourceSchema, Split,
};
use moruna_testkit::{FakeSink, FakeSource};

use super::common::{RigBuilder, run_within};
use crate::RunOutcome;

/// Splits of the test's plan, one read each.
const SPLITS: u64 = 6;

/// A test-local `Source` (k): `FakeSource`'s reads, except the `at`-th, which panics, in the
/// call itself when `in_call` and otherwise when its future is polled.
struct PanickingSource {
    inner: FakeSource,
    reads: AtomicU64,
    at: u64,
    in_call: bool,
}

impl Source for PanickingSource {
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
        tier: moruna_kernel::Tier,
    ) -> BoxFuture<'a, Result<Payload>> {
        let n = self.reads.fetch_add(1, Ordering::SeqCst) + 1;
        if n == self.at {
            if self.in_call {
                panic!("the source broke on read {n}");
            }
            return Box::pin(async move { panic!("the source's read {n} broke when polled") });
        }
        self.inner.read(split, rows, alloc, tier)
    }

    fn repeatable(&self) -> bool {
        true
    }
}

/// Where a test-local sink panics.
#[derive(Clone, Copy, Debug)]
enum Breaks {
    /// The `n`-th write, when it is polled.
    Write(u64),
    /// `finish`.
    Finish,
}

/// A test-local `Sink` (k): `FakeSink`, except where it `Breaks`.
struct PanickingSink {
    inner: FakeSink,
    writes: AtomicU64,
    breaks: Breaks,
}

impl Sink for PanickingSink {
    fn open(&mut self, schema: &SourceSchema) -> Result<()> {
        self.inner.open(schema)
    }

    fn accepts(&self) -> PayloadSpec {
        self.inner.accepts()
    }

    fn write(&self, seq: Seq, payload: Payload) -> BoxFuture<'_, Result<()>> {
        let n = self.writes.fetch_add(1, Ordering::SeqCst) + 1;
        if let Breaks::Write(at) = self.breaks
            && n == at
        {
            return Box::pin(async move { panic!("the sink's write {n} broke") });
        }
        self.inner.write(seq, payload)
    }

    fn finish(&mut self) -> Result<SinkSummary> {
        if let Breaks::Finish = self.breaks {
            panic!("the sink broke in finish");
        }
        self.inner.finish()
    }
}

fn terminated(outcome: RunOutcome, what: &str) -> MorunaError {
    match outcome {
        RunOutcome::Terminated { diagnostic, .. } => diagnostic,
        other => panic!("{what}: expected Terminated, got {other:?}"),
    }
}

#[test]
fn sc_t26_a_source_that_panics_ends_the_run() {
    for at in 1..=SPLITS {
        for in_call in [true, false] {
            for stages in [0usize, 1] {
                let what = format!("read {at}, in the call {in_call}, {stages} stages");
                let source = PanickingSource {
                    inner: FakeSource::new().splits(SPLITS as u32, 8, 64),
                    reads: AtomicU64::new(0),
                    at,
                    in_call,
                };
                let rig = RigBuilder::new()
                    .cfg(|cfg| cfg.initial_morsel_target = 64)
                    .source_over(Arc::new(source))
                    .stages(stages)
                    .go();
                let outcome = run_within(&rig, Duration::from_secs(10), &what);
                match terminated(outcome, &what) {
                    MorunaError::Source { split, msg } => {
                        assert!(
                            msg.contains("panicked") && msg.contains("broke"),
                            "{what}: the diagnostic says what the panic said: {msg}"
                        );
                        assert!(u64::from(split) < SPLITS, "{what}: split {split}");
                    }
                    other => panic!("{what}: expected a source diagnostic, got {other}"),
                }
                assert_eq!(rig.sink.finish_calls(), 0, "{what}: finish never ran");
            }
        }
    }
}

#[test]
fn sc_t26_a_sink_that_panics_ends_the_run() {
    let mut cases: Vec<Breaks> = (1..=SPLITS).map(Breaks::Write).collect();
    cases.push(Breaks::Finish);
    for breaks in cases {
        for stages in [0usize, 1] {
            let what = format!("{breaks:?}, {stages} stages");
            let sink = PanickingSink {
                inner: FakeSink::new(),
                writes: AtomicU64::new(0),
                breaks,
            };
            let rig = RigBuilder::new()
                .cfg(|cfg| cfg.initial_morsel_target = 64)
                .source(FakeSource::new().splits(SPLITS as u32, 8, 64))
                .sink_over(Box::new(sink))
                .stages(stages)
                .go();
            let outcome = run_within(&rig, Duration::from_secs(10), &what);
            match terminated(outcome, &what) {
                MorunaError::Sink(msg) => assert!(
                    msg.contains("panicked") && msg.contains("broke"),
                    "{what}: the diagnostic says what the panic said: {msg}"
                ),
                other => panic!("{what}: expected a sink diagnostic, got {other}"),
            }
        }
    }
}
