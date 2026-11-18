//! SC-T47 (item 6.3a): a budget bounds how far reading runs ahead, not how many workers decode.
//!
//! A source that decodes on workers (RB1) gives a worker work only through a read in flight:
//! the drive fetches, and the decode is the worker's. So the reads the drive keeps in flight are
//! the workers that can decode at once. Here two workers and a read-ahead of two copy six splits
//! with no kernel stage, each read a whole split (as the no-kernel path's target makes it), into
//! an arena with room for three splits' decoded bytes and a half. Two reads, their fetched bytes
//! and a morsel on its way to the sink fit in it: a second worker decoding beside the first is
//! what the budget allows.
//!
//! The defect (6.3a): the drive admits a read only while twice its decoded estimate fits beside
//! twice the estimate of every read in flight (`source_drive::issue_reads`, `arena_admits` over
//! `read_charge`), so a read larger than about a quarter of the arena is seldom in flight
//! beside another, and every worker but one idles. Found on NYC 2024 into a `count` sink:
//! row groups of 1 Mi rows, 155 MB decoded and 310 MB charged, two workers, each idle half the
//! run at 672 MiB and at 1 GiB, and busy from 1152 MiB up.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use moruna_kernel::{
    Allocator, BoxFuture, Buffer, CancelToken, Fetched, MorunaError, Payload, Result, RowRange,
    Source, SourceSchema, Split, Tier,
};
use moruna_testkit::{FakeAllocator, FakeSource};

use super::common::RigBuilder;

/// Rows of a split: eight bytes each once decoded.
const ROWS: u64 = 4096;
/// One split's decoded bytes, which is also its estimate, so what a read learns is one.
const SPLIT: u64 = ROWS * 8;
/// The splits the run copies.
const SPLITS: u32 = 6;
/// What a split's fetch holds before its decode: an eighth of what it decodes to, about what a
/// zstd Parquet column chunk is to its Arrow arrays.
const FETCHED: u64 = SPLIT / 8;
/// How long a decode waits for a second one to start beside it before going on alone.
const OVERLAP_WAIT: Duration = Duration::from_millis(200);

/// The decodes running now, every decode, and those that ran with no other beside them.
#[derive(Default)]
struct Overlap {
    now: AtomicUsize,
    decodes: AtomicUsize,
    alone: AtomicUsize,
}

/// A test-local `Source` (k allows one for a behaviour no fake knob provides): the fake source,
/// fetching on the drive into `FETCHED` bytes of the arena per split and decoding on a worker
/// into the split's rows. A decode waits up to `OVERLAP_WAIT` for another decode to run beside
/// it, so two reads in flight are two decodes at once whatever the threads' timing, and a
/// decode that waited it out ran alone.
struct Decoding {
    inner: FakeSource,
    overlap: Arc<Overlap>,
}

impl Source for Decoding {
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
        self.inner.read(split, rows, alloc, tier)
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
        let rows = rows.unwrap_or(RowRange {
            start: 0,
            end: split.rows,
        });
        let span = rows.end.saturating_sub(rows.start);
        let bytes = (FETCHED * span / split.rows.max(1)).max(1) as usize;
        let fetched = alloc
            .alloc(bytes, tier)
            .map(|buffer| Fetched(Box::new((buffer, rows))));
        Box::pin(async move { fetched })
    }

    fn decode(
        &self,
        split: &Split,
        fetched: Fetched,
        alloc: &dyn Allocator,
        tier: Tier,
    ) -> Result<Payload> {
        let (buffer, rows) = *fetched
            .0
            .downcast::<(Buffer, RowRange)>()
            .map_err(|_| MorunaError::Plan("not this source's fetch".into()))?;
        self.overlap.now.fetch_add(1, Ordering::SeqCst);
        self.overlap.decodes.fetch_add(1, Ordering::SeqCst);
        let deadline = Instant::now() + OVERLAP_WAIT;
        while self.overlap.now.load(Ordering::SeqCst) < 2 {
            if Instant::now() >= deadline {
                self.overlap.alone.fetch_add(1, Ordering::SeqCst);
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let whole = rows.start == 0 && rows.end == split.rows;
        let mut read = self
            .inner
            .read(split, (!whole).then_some(rows), alloc, tier);
        let decoded = match read.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(decoded) => decoded,
            Poll::Pending => Err(MorunaError::Plan("the fake source's read is ready".into())),
        };
        drop(buffer);
        self.overlap.now.fetch_sub(1, Ordering::SeqCst);
        decoded
    }
}

/// Copies the splits with two workers and a read-ahead of two into `alloc`, each read a whole
/// split, and returns the decodes and those of them that ran alone.
fn decodes(alloc: FakeAllocator) -> (usize, usize) {
    let overlap = Arc::new(Overlap::default());
    let source = Decoding {
        inner: FakeSource::new().splits(SPLITS, ROWS, SPLIT),
        overlap: Arc::clone(&overlap),
    };
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 2;
            cfg.workers_active = 2;
            cfg.read_ahead = 2;
            // Larger than a split, as the no-kernel path's target is larger than a row group:
            // every read is a whole split.
            cfg.initial_morsel_target = 1 << 30;
            cfg.morsel_min = 8;
            cfg.morsel_max = 1 << 30;
        })
        .source_over(Arc::new(source))
        .alloc(alloc.clone())
        .go();
    let rows = match rig.scheduler.run(CancelToken::new()) {
        Ok(crate::RunOutcome::Completed { sinks }) => sinks.first().map_or(0, |sink| sink.rows),
        other => panic!("expected Completed, got {other:?}"),
    };
    assert_eq!(
        rows,
        u64::from(SPLITS) * ROWS,
        "every row reached the sink once"
    );
    assert_eq!(alloc.in_use(Tier::Host), 0, "every read was released");
    (
        overlap.decodes.load(Ordering::SeqCst),
        overlap.alone.load(Ordering::SeqCst),
    )
}

#[test]
fn sc_t47_with_no_budget_two_workers_decode_side_by_side() {
    // The rig itself: nothing bounds the reads, and every decode but perhaps the last has a
    // second beside it.
    let (decodes, alone) = decodes(FakeAllocator::new());
    assert!(decodes >= SPLITS as usize);
    assert!(alone <= 1, "{alone} of {decodes} decodes ran alone");
}

#[test]
fn sc_t47_a_budget_with_room_for_three_reads_keeps_two_workers_decoding() {
    // Room for three splits' decoded bytes and a half: two reads decoding, their fetched bytes
    // and a morsel on its way to the sink all fit.
    let arena = 3 * SPLIT + SPLIT / 2;
    let (decodes, alone) = decodes(FakeAllocator::new().with_limit(Tier::Host, arena));
    assert!(
        alone <= 1,
        "an arena of {arena} bytes, room for three reads of {SPLIT} decoded bytes, ran {alone} \
         of its {decodes} decodes alone: the second worker idled while the budget had room for \
         its read. The drive admits a read only while twice its estimate fits beside twice the \
         estimate of every read in flight, so the budget bounded the decoders, not how far \
         reading ran ahead (6.3a)"
    );
}
