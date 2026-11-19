//! The gate's split (`arrow-core-0.7.md` §9): a source's reads are timed apart from its decodes
//! and a sink's encodes apart from its writes, when they decode or encode on workers (RB1), and
//! as one I/O time when they do both in one call.

use std::sync::Arc;
use std::time::Duration;

use moruna_kernel::{
    Allocator, BoxFuture, CancelToken, Encoded, Fetched, MorunaError, Payload, PayloadSpec, Result,
    RowRange, Seq, Sink, SinkSummary, Source, SourceSchema, Split, Tier,
};
use moruna_sinks::SinkHandle;
use moruna_testkit::{FakeKernel, FakePlacement, FakeSink, FakeSource};

use super::common::cfg;
use crate::{Pipeline, RunOutcome, Scheduler, SchedulerConfig};

const FETCH: Duration = Duration::from_millis(2);
const DECODE: Duration = Duration::from_millis(3);
const ENCODE: Duration = Duration::from_millis(3);
const PUT: Duration = Duration::from_millis(1);
/// The CPU each decode and each encode spends besides its sleep (item 6.3ac).
const BURN_NS: u64 = 1_000_000;

/// Spend `ns` of this thread's CPU.
fn burn(ns: u64) {
    let start = crate::cputime::thread_cpu_ns();
    let mut x = 0u64;
    while crate::cputime::thread_cpu_ns().saturating_sub(start) < ns {
        x = std::hint::black_box(x.wrapping_mul(31).wrapping_add(7));
    }
}

/// A fake source that fetches on the drive and decodes on workers.
struct Split2(FakeSource);

impl Source for Split2 {
    fn schema(&self) -> SourceSchema {
        Source::schema(&self.0)
    }
    fn plan(&self) -> Result<Vec<Split>> {
        Source::plan(&self.0)
    }
    fn read<'a>(
        &'a self,
        split: &'a Split,
        rows: Option<RowRange>,
        alloc: &'a dyn Allocator,
        tier: Tier,
    ) -> BoxFuture<'a, Result<Payload>> {
        Source::read(&self.0, split, rows, alloc, tier)
    }
    fn repeatable(&self) -> bool {
        Source::repeatable(&self.0)
    }
    fn decodes_on_workers(&self) -> bool {
        true
    }
    fn fetch<'a>(
        &'a self,
        split: &'a Split,
        rows: Option<RowRange>,
        alloc: &'a dyn Allocator,
        tier: Tier,
    ) -> BoxFuture<'a, Result<Fetched>> {
        Box::pin(async move {
            std::thread::sleep(FETCH);
            let payload = Source::read(&self.0, split, rows, alloc, tier).await?;
            Ok(Fetched(Box::new(payload)))
        })
    }
    fn decode(
        &self,
        _split: &Split,
        fetched: Fetched,
        _alloc: &dyn Allocator,
        _tier: Tier,
    ) -> Result<Payload> {
        std::thread::sleep(DECODE);
        burn(BURN_NS);
        fetched
            .0
            .downcast::<Payload>()
            .map(|p| *p)
            .map_err(|_| MorunaError::Plan("not a payload".into()))
    }
}

/// A fake sink that encodes on workers and puts on the drive.
struct Encode2(FakeSink);

impl Sink for Encode2 {
    fn open(&mut self, schema: &SourceSchema) -> Result<()> {
        self.0.open(schema)
    }
    fn accepts(&self) -> PayloadSpec {
        self.0.accepts()
    }
    fn write(&self, seq: Seq, payload: Payload) -> BoxFuture<'_, Result<()>> {
        self.0.write(seq, payload)
    }
    fn finish(&mut self) -> Result<SinkSummary> {
        self.0.finish()
    }
    fn encodes_on_workers(&self) -> bool {
        true
    }
    fn encode(&self, seq: Seq, payload: Payload) -> Result<Encoded> {
        std::thread::sleep(ENCODE);
        burn(BURN_NS);
        Ok(Encoded(Box::new((seq, payload))))
    }
    fn put(&self, encoded: Encoded) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            std::thread::sleep(PUT);
            let (seq, payload) = *encoded
                .0
                .downcast::<(Seq, Payload)>()
                .map_err(|_| MorunaError::Sink("not a morsel".into()))?;
            self.0.write(seq, payload).await
        })
    }
}

fn run(source: Arc<dyn Source>, sink: Box<dyn Sink>) -> Scheduler {
    run_chain(source, 1, sink)
}

/// The run of `stages` stages of the fake kernel between the source and the sink.
fn run_chain(source: Arc<dyn Source>, stages: usize, sink: Box<dyn Sink>) -> Scheduler {
    let scheduler = Scheduler::new(
        SchedulerConfig {
            initial_morsel_target: 8,
            morsel_min: 8,
            morsel_max: 1 << 20,
            ..cfg()
        },
        Pipeline::chain(
            source,
            (0..stages)
                .map(|_| Arc::new(FakeKernel::new()) as Arc<dyn moruna_kernel::Kernel>)
                .collect(),
            SinkHandle::wrap(sink, false, 1 << 20),
        ),
        Arc::new(FakePlacement::new()),
        Arc::new(moruna_testkit::FakeAllocator::new()),
        Arc::new(moruna_testkit::FakeTrace::new()),
        Arc::new(moruna_testkit::FakeSampler::new()),
    )
    .expect("new");
    match scheduler.run(CancelToken::new()) {
        Ok(RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }
    scheduler
}

fn ns(d: Duration) -> u64 {
    d.as_nanos() as u64
}

#[test]
fn reads_are_timed_apart_from_decodes_and_encodes_apart_from_writes() {
    let sink = FakeSink::new();
    let scheduler = run(
        Arc::new(Split2(FakeSource::new().splits(1, 8, 64))),
        Box::new(Encode2(sink.clone())),
    );
    assert_eq!(sink.written().len(), 8);
    let times = scheduler.io_times();
    let (source, sink) = (times.sources[0], times.sinks[0]);
    assert_eq!(source.operations, 8);
    assert!(source.io_ns >= 8 * ns(FETCH), "{source:?}");
    assert!(
        source.codec_ns.expect("timed apart") >= 8 * ns(DECODE),
        "{source:?}"
    );
    assert_eq!(sink.operations, 8);
    assert!(
        sink.codec_ns.expect("timed apart") >= 8 * ns(ENCODE),
        "{sink:?}"
    );
    assert!(sink.io_ns >= 8 * ns(PUT), "{sink:?}");
}

/// Item 6.3ac: a decode's and an encode's CPU is counted as the thread's, as a worker's unit is,
/// not as the wall time they took, and charged to the stages beside them: the source's to the
/// stage it feeds, the sink's to the stage that feeds it. Each stage counts the bytes it took.
#[test]
fn decode_and_encode_cpu_are_charged_to_the_stages_beside_them() {
    use moruna_kernel::StatsSource;
    let sink = FakeSink::new();
    let scheduler = run_chain(
        Arc::new(Split2(FakeSource::new().splits(1, 8, 64))),
        2,
        Box::new(Encode2(sink.clone())),
    );
    assert_eq!(sink.written().len(), 8);
    let stats = scheduler.scheduler_stats();
    let times = scheduler.io_times();
    let (decode, encode) = (&stats.decodes[0], &stats.encodes[0]);
    assert_eq!((stats.decodes.len(), stats.encodes.len()), (1, 1));
    assert_eq!(decode.stages, vec![1], "the source feeds stage 1");
    assert_eq!(encode.stages, vec![2], "stage 2 feeds the sink");
    for (cpu, wall) in [
        (
            decode.cpu_ns,
            times.sources[0].codec_ns.expect("timed apart"),
        ),
        (encode.cpu_ns, times.sinks[0].codec_ns.expect("timed apart")),
    ] {
        assert!(cpu >= 8 * BURN_NS, "{cpu} ns of CPU");
        assert!(
            cpu + 8 * ns(DECODE) / 2 <= wall,
            "{cpu} ns of CPU in {wall} ns"
        );
    }
    for stage in &stats.per_stage {
        assert!(stage.bytes_in > 0, "{stage:?}");
        assert_eq!(stage.task_cpu_ns, 0, "no ring task");
    }
}

#[test]
fn an_end_that_does_both_in_one_call_is_all_io() {
    let sink = FakeSink::new();
    let scheduler = run(
        Arc::new(FakeSource::new().splits(1, 8, 64)),
        Box::new(sink.clone()),
    );
    let times = scheduler.io_times();
    assert_eq!(times.sources.len(), 1);
    assert_eq!(times.sinks.len(), 1);
    assert_eq!(times.sources[0].operations, 8);
    assert_eq!(times.sources[0].codec_ns, None);
    assert_eq!(times.sinks[0].operations, 8);
    assert_eq!(times.sinks[0].codec_ns, None);
}

#[test]
fn a_failed_fetch_or_encode_is_counted_and_ends_the_run() {
    /// A source whose fetch fails.
    struct Broken(FakeSource);
    impl Source for Broken {
        fn schema(&self) -> SourceSchema {
            Source::schema(&self.0)
        }
        fn plan(&self) -> Result<Vec<Split>> {
            Source::plan(&self.0)
        }
        fn read<'a>(
            &'a self,
            split: &'a Split,
            rows: Option<RowRange>,
            alloc: &'a dyn Allocator,
            tier: Tier,
        ) -> BoxFuture<'a, Result<Payload>> {
            Source::read(&self.0, split, rows, alloc, tier)
        }
        fn decodes_on_workers(&self) -> bool {
            true
        }
    }
    let scheduler = Scheduler::new(
        SchedulerConfig {
            initial_morsel_target: 8,
            morsel_min: 8,
            morsel_max: 1 << 20,
            ..cfg()
        },
        Pipeline::chain(
            Arc::new(Broken(FakeSource::new().splits(1, 8, 64))),
            vec![Arc::new(FakeKernel::new())],
            SinkHandle::wrap(Box::new(FakeSink::new()), false, 1 << 20),
        ),
        Arc::new(FakePlacement::new()),
        Arc::new(moruna_testkit::FakeAllocator::new()),
        Arc::new(moruna_testkit::FakeTrace::new()),
        Arc::new(moruna_testkit::FakeSampler::new()),
    )
    .expect("new");
    let outcome = scheduler.run(CancelToken::new());
    assert!(
        !matches!(outcome, Ok(RunOutcome::Completed { .. })),
        "{outcome:?}"
    );
    let source = scheduler.io_times().sources[0];
    assert!(source.operations >= 1);
    assert_eq!(source.codec_ns, None, "nothing was decoded");
}
