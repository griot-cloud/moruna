//! SC-T47 (item 6.3a, system design 3A): a budget bounds how far reading runs ahead, not how
//! many workers decode.
//!
//! A source that decodes on workers (RB1) gives a worker work only through a read in flight:
//! the drive fetches, and the decode is the worker's. So the reads the drive keeps in flight are
//! the workers that can decode at once. Here two workers and a read-ahead of two copy six splits
//! with no kernel stage, each read a whole split (as the no-kernel path's target made it), into
//! an arena with room for three splits' decoded bytes and a half. Two reads, their fetched bytes
//! and a morsel on its way to the sink fit in it: a second worker decoding beside the first is
//! what the budget allows.
//!
//! The defect (6.3a): the drive admitted a read only while twice its decoded estimate fit beside
//! twice the estimate of every read in flight, and refused it otherwise, so a read larger than
//! about a quarter of the arena was seldom in flight beside another and every worker but one
//! idled. Found on NYC 2024 into a `count` sink: row groups of 1 Mi rows, 155 MB decoded and
//! 310 MB charged, two workers, each idle half the run at 672 MiB and at 1 GiB, and busy from
//! 1152 MiB up. The drive now cuts such a read to the room left (`source_drive::cut_to_room`),
//! from a split whose ranges read only their own rows; the charge stays twice the estimate, so
//! the reads in flight are never charged more than the arena, and a split that cannot be cut is
//! read whole, one at a time when only one fits, as before.

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
/// One split's decoded bytes, which is also its estimate, so what a read learns is one.
const SPLIT: u64 = ROWS * 8;
/// The splits the run copies.
const SPLITS: u32 = 6;
/// What a split's fetch holds before its decode: an eighth of what it decodes to, about what a
/// zstd Parquet column chunk is to its Arrow arrays.
const FETCHED: u64 = SPLIT / 8;
/// How long a decode waits for a second one to start beside it before going on alone.
const OVERLAP_WAIT: Duration = Duration::from_millis(200);

/// The decodes running now, every decode, those that ran with no other beside them, and the
/// ranges decoded, by split.
#[derive(Default)]
struct Overlap {
    now: AtomicUsize,
    decodes: AtomicUsize,
    alone: AtomicUsize,
    ranges: Mutex<Vec<(u32, RowRange)>>,
}

/// A test-local `Source` (k allows one for a behaviour no fake knob provides): the fake source,
/// fetching on the drive into `FETCHED` bytes of the arena per split and decoding on a worker
/// into the split's rows. A decode waits up to `wait` for another decode to run beside it, so
/// two reads in flight are two decodes at once whatever the threads' timing, and a decode that
/// waited it out ran alone. `own_rows`, when given, is its answer to `range_reads_own_rows`
/// (a Parquet group whose pages cannot be located answers false).
struct Decoding {
    inner: FakeSource,
    overlap: Arc<Overlap>,
    wait: Duration,
    own_rows: Option<bool>,
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

    fn range_reads_own_rows(&self, split: &Split) -> bool {
        self.own_rows.unwrap_or(split.sub_splittable)
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
        let deadline = Instant::now() + self.wait;
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
        if decoded.is_ok() {
            self.overlap
                .ranges
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((split.id, rows));
        }
        self.overlap.now.fetch_sub(1, Ordering::SeqCst);
        decoded
    }
}

/// What a run of `copy` saw.
struct Copied {
    decodes: usize,
    alone: usize,
    /// The ranges decoded, sorted by split and start.
    ranges: Vec<(u32, RowRange)>,
    /// The most the drive charged its reads in flight at once.
    charged_peak: u64,
}

/// Copies the splits with two workers and a read-ahead of two into `alloc`, each read a whole
/// split unless the drive cuts it, and checks every row reached the sink once.
fn copy(
    alloc: FakeAllocator,
    source: FakeSource,
    wait: Duration,
    own_rows: Option<bool>,
) -> Copied {
    let overlap = Arc::new(Overlap::default());
    let source = Decoding {
        inner: source,
        overlap: Arc::clone(&overlap),
        wait,
        own_rows,
    };
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 2;
            cfg.workers_active = 2;
            cfg.read_ahead = 2;
            // Larger than a split, as the no-kernel path's target was larger than a row group:
            // a read is a whole split unless the drive cuts it.
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
    let mut ranges = overlap
        .ranges
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    ranges.sort_by_key(|(split, rows)| (*split, rows.start));
    Copied {
        decodes: overlap.decodes.load(Ordering::SeqCst),
        alone: overlap.alone.load(Ordering::SeqCst),
        ranges,
        charged_peak: rig.scheduler.shared().sources[0]
            .charged_peak
            .load(Ordering::SeqCst),
    }
}

/// The decoded ranges cover every split's rows exactly once: no row read twice, none missed.
fn assert_tiled(ranges: &[(u32, RowRange)]) {
    for split in 0..SPLITS {
        let mut at = 0;
        for (_, rows) in ranges.iter().filter(|(id, _)| *id == split) {
            assert_eq!(
                rows.start, at,
                "split {split}: rows {at}.. read once, in one range"
            );
            assert!(rows.end > rows.start, "split {split}: an empty read");
            at = rows.end;
        }
        assert_eq!(at, ROWS, "split {split}: every row read");
    }
}

fn splits() -> FakeSource {
    FakeSource::new().splits(SPLITS, ROWS, SPLIT)
}

#[test]
fn sc_t47_with_no_budget_two_workers_decode_side_by_side() {
    // The rig itself: nothing bounds the reads, and every decode but perhaps the last has a
    // second beside it.
    let run = copy(FakeAllocator::new(), splits(), OVERLAP_WAIT, None);
    assert!(run.decodes >= SPLITS as usize);
    assert!(
        run.alone <= 1,
        "{} of {} decodes ran alone",
        run.alone,
        run.decodes
    );
    assert_tiled(&run.ranges);
}

#[test]
fn sc_t47_a_budget_with_room_for_three_reads_keeps_two_workers_decoding() {
    // Room for three splits' decoded bytes and a half: two reads decoding, their fetched bytes
    // and a morsel on its way to the sink all fit.
    let arena = 3 * SPLIT + SPLIT / 2;
    let run = copy(
        FakeAllocator::new().with_limit(Tier::Host, arena),
        splits(),
        OVERLAP_WAIT,
        None,
    );
    assert!(
        run.alone <= 1,
        "an arena of {arena} bytes, room for three reads of {SPLIT} decoded bytes, ran {} of \
         its {} decodes alone: the second worker idled while the budget had room for its read, \
         so the budget bounded the decoders, not how far reading ran ahead (6.3a)",
        run.alone,
        run.decodes
    );
    assert_tiled(&run.ranges);
    assert!(run.charged_peak <= arena);
}

#[test]
fn sc_t47_reads_in_flight_are_never_charged_more_than_the_arena() {
    // From an arena in which one whole read fits but a second fits only cut (a whole split is
    // charged twice its bytes) to one in which two do: every read the drive admits or cuts fits
    // beside those in flight, and every row is read once.
    for arena in [2 * SPLIT + SPLIT / 2, 3 * SPLIT + SPLIT / 2, 5 * SPLIT] {
        let run = copy(
            FakeAllocator::new().with_limit(Tier::Host, arena),
            splits(),
            Duration::ZERO,
            None,
        );
        assert!(
            run.charged_peak <= arena,
            "the reads in flight were charged {} bytes at once in an arena of {arena}",
            run.charged_peak
        );
        assert!(run.charged_peak > 0);
        assert_tiled(&run.ranges);
        if arena < 4 * SPLIT {
            assert!(
                run.ranges.len() > SPLITS as usize,
                "an arena of {arena} holds one whole read of {SPLIT} bytes at its charge and \
                 not two, so the second is cut: {:?}",
                run.ranges
            );
        }
    }
}

#[test]
fn sc_t47_a_split_that_cannot_be_cut_is_read_whole_one_at_a_time() {
    // Today's behaviour, kept: a split that is not sub-splittable, and one whose ranges would
    // not read only their own rows (a Parquet group whose pages cannot be located), are read
    // whole, and while only one whole read fits, one at a time.
    let arena = 3 * SPLIT + SPLIT / 2;
    for (source, own_rows) in [
        (splits().sub_splittable(false), None),
        (splits(), Some(false)),
    ] {
        let run = copy(
            FakeAllocator::new().with_limit(Tier::Host, arena),
            source,
            OVERLAP_WAIT,
            own_rows,
        );
        assert_eq!(run.ranges.len(), SPLITS as usize, "{:?}", run.ranges);
        assert!(
            run.ranges
                .iter()
                .all(|(_, rows)| rows.start == 0 && rows.end == ROWS)
        );
        assert_eq!(
            run.alone, run.decodes,
            "one whole read fits beside another nowhere in {arena} bytes"
        );
        assert!(run.charged_peak <= arena);
    }
}
