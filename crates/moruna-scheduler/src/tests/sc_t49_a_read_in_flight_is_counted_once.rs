//! SC-T49 (item 6.3c, system design 3A): a read in flight is counted once by the arena's
//! admission, at the larger of its charge and what it has taken, never at their sum.
//!
//! Found by 6.3a's measurement: at 672 MiB a whole row group's read in flight was charged its
//! 312 MB beside the 172 MB the arena already counted for it (its fetch and the buffers its
//! decode had reserved), so a second read found no room until the first landed, and one of two
//! workers idled. Here a read takes its whole decoded size as soon as it is fetched, as a batch
//! decode reserves its buffers, and is charged twice that, as every read is. An arena of four
//! splits and a half holds two such reads counted once (two charges, of which one split is
//! already taken) and not counted twice (a split taken beside two charges, and a third).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
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
/// One split's decoded bytes, and its estimate.
const SPLIT: u64 = ROWS * 8;
/// The splits the run copies.
const SPLITS: u32 = 6;
/// The arena: two reads counted once fit, counted twice they do not.
const ARENA: u64 = 4 * SPLIT + SPLIT / 2;
/// How long a decode waits for a second one to start beside it before going on alone.
const OVERLAP_WAIT: Duration = Duration::from_millis(200);

#[derive(Default)]
struct Seen {
    now: AtomicUsize,
    decodes: AtomicUsize,
    alone: AtomicUsize,
    ranges: Mutex<Vec<(u32, RowRange)>>,
}

/// A test-local `Source` (k): its fetch takes the range's whole decoded size from the arena, as
/// a batch decode reserves its buffers, and its decode, on a worker, waits for a second decode
/// beside it before it gives that back and decodes the rows.
struct Reserving {
    inner: FakeSource,
    seen: Arc<Seen>,
}

impl Source for Reserving {
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
        let bytes = ((rows.end - rows.start) * 8).max(1) as usize;
        let reserved = alloc
            .alloc(bytes, tier)
            .map(|buffer| Fetched(Box::new((buffer, rows))));
        Box::pin(async move { reserved })
    }

    fn decode(
        &self,
        split: &Split,
        fetched: Fetched,
        alloc: &dyn Allocator,
        tier: Tier,
    ) -> Result<Payload> {
        let (reserved, rows) = *fetched
            .0
            .downcast::<(Buffer, RowRange)>()
            .map_err(|_| MorunaError::Plan("not this source's fetch".into()))?;
        self.seen.now.fetch_add(1, Ordering::SeqCst);
        self.seen.decodes.fetch_add(1, Ordering::SeqCst);
        let deadline = Instant::now() + OVERLAP_WAIT;
        while self.seen.now.load(Ordering::SeqCst) < 2 {
            if Instant::now() >= deadline {
                self.seen.alone.fetch_add(1, Ordering::SeqCst);
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(reserved);
        let whole = rows.start == 0 && rows.end == split.rows;
        let mut read = self
            .inner
            .read(split, (!whole).then_some(rows), alloc, tier);
        let decoded = match read.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(decoded) => decoded,
            Poll::Pending => Err(MorunaError::Plan("the fake source's read is ready".into())),
        };
        if decoded.is_ok() {
            self.seen
                .ranges
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((split.id, rows));
        }
        self.seen.now.fetch_sub(1, Ordering::SeqCst);
        decoded
    }
}

/// Copies the splits with two workers and a read-ahead of two into an arena of `arena` bytes,
/// each read a whole split unless the drive cuts it; the decodes, those that ran alone, and the
/// ranges read.
fn copy(source: FakeSource, arena: u64) -> (usize, usize, Vec<(u32, RowRange)>) {
    let seen = Arc::new(Seen::default());
    let alloc = FakeAllocator::new().with_limit(Tier::Host, arena);
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 2;
            cfg.workers_active = 2;
            cfg.read_ahead = 2;
            cfg.initial_morsel_target = 1 << 30;
            cfg.morsel_min = 8;
            cfg.morsel_max = 1 << 30;
        })
        .source_over(Arc::new(Reserving {
            inner: source,
            seen: Arc::clone(&seen),
        }))
        .alloc(alloc.clone())
        .go();
    let rows = match rig.scheduler.run(CancelToken::new()) {
        Ok(crate::RunOutcome::Completed { sinks }) => sinks[0].rows,
        other => panic!("expected Completed, got {other:?}"),
    };
    assert_eq!(rows, u64::from(SPLITS) * ROWS, "every row reached the sink");
    assert_eq!(alloc.in_use(Tier::Host), 0, "every read was released");
    let ranges = seen
        .ranges
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    (
        seen.decodes.load(Ordering::SeqCst),
        seen.alone.load(Ordering::SeqCst),
        ranges,
    )
}

#[test]
fn sc_t49_a_split_that_cannot_be_cut_is_counted_once_while_in_flight() {
    // A split that cannot be cut is never cut to fit: the second whole read is admitted only
    // when the first is counted once.
    let (decodes, alone, ranges) = copy(
        FakeSource::new()
            .splits(SPLITS, ROWS, SPLIT)
            .sub_splittable(false),
        ARENA,
    );
    assert_eq!(ranges.len(), SPLITS as usize);
    assert!(
        alone <= 1,
        "{alone} of {decodes} decodes ran alone in an arena of {ARENA} bytes: a read in flight \
         was counted at its charge of {} bytes beside the {SPLIT} it had taken, so the second \
         whole read never fitted (6.3c)",
        2 * SPLIT
    );
}

#[test]
fn sc_t49_what_a_read_in_flight_holds_is_not_taken_for_the_queues() {
    // An arena of 2.8 splits: with one read in flight holding its split, the arena's free room
    // (1.8 splits) is under a whole read's charge (2), so the next read seems kept out by what
    // the queues hold, and would wait. It is the read in flight that holds those bytes: beside
    // it, the next read is cut to the room left (0.8 of a split's charge) and decoded at once.
    let arena = 2 * SPLIT + SPLIT * 4 / 5;
    let (_, _, mut ranges) = copy(FakeSource::new().splits(SPLITS, ROWS, SPLIT), arena);
    assert!(
        ranges.len() > SPLITS as usize,
        "no read was cut beside a read in flight in an arena of {arena} bytes: {ranges:?}"
    );
    ranges.sort_by_key(|(split, rows)| (*split, rows.start));
    for split in 0..SPLITS {
        let mut at = 0;
        for (_, rows) in ranges.iter().filter(|(id, _)| *id == split) {
            assert_eq!(rows.start, at, "split {split}: every row once");
            at = rows.end;
        }
        assert_eq!(at, ROWS, "split {split}: every row read");
    }
}
