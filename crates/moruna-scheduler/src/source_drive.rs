//! The source drives (f.5), the drive-side helper (f.9) and the recompute pass of a resumed run
//! (f.13).
//!
//! One drive per source, and the drives are with the sink drives the only threads in the process
//! that may wait on a `Completion` (CT-I7, SC-I1): each issues its source's `Source::read` up to
//! `read_ahead` in flight, polls the futures with a waker over its own parker, and pushes each
//! result into every queue its source gives to, the batch shared (contracts d.16). It holds no
//! scheduler lock while it waits.
//!
//! The arena bounds how far reading runs ahead, never how many workers decode (system design
//! 3A, item 6.3a): a read that does not fit beside those in flight, though it fits the arena as
//! the arena stands, is cut to the room left, from a split whose ranges read only their own rows
//! (`cut_to_room`), rather than refused while workers idle. One kept out by what the queues hold
//! waits for them to drain, as it did. Each read is charged twice its decoded estimate, so the
//! reads in flight never hold more than half the arena's bytes; and what a read holds outside the
//! arena while a worker decodes it never exceeds its own decoded bytes (Parquet's batch decode,
//! `append` in moruna-sources), which is the arena sizing's assumption for every byte in flight.
//! So however many workers decode at once, the process stays within the arena and the headroom
//! above it.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use crossbeam::channel::{Receiver, Sender};
use crossbeam::sync::{Parker, Unparker};
use moruna_kernel::{
    Allocator, BoxFuture, Morsel, MorunaError, Origin, Payload, Result, RowRange, Seq, Split,
    StageId, Tally, Tier,
};

use crate::shared::{Cursor, DRIVE_DRIVING, DRIVE_STOP, HelperRequest, Shared, SourceEnd};

/// Wakes the drive thread when a completion resolves; the drive owns no runtime of its own (l).
struct DriveWake(Unparker);

impl std::task::Wake for DriveWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Why a read was issued, which decides what happens to its payload.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum ReadKind {
    /// A new range: pushed into the source's queues.
    Fresh,
    /// An entry the engine evicted from this queue: put back in its original position with
    /// `replace`.
    Replacement(StageId),
    /// A morsel the manifest could not point to on disk: pushed into the source's queues with
    /// its original seq.
    Recompute,
}

struct InFlight<'a> {
    future: BoxFuture<'a, Result<Payload>>,
    seq: Seq,
    origin: Origin,
    split: u32,
    rows: RowRange,
    kind: ReadKind,
    /// What the arena may charge for this read, as admission counted it (f.5).
    charge: u64,
    /// What this read has taken from the arena so far: its fetch and the buffers its decode
    /// reserved, which the arena's in-use already counts (6.3c).
    taken: Arc<Tally>,
    /// How many times this read has already met a full arena (RB5).
    attempts: u32,
}

/// RB5 (2026-11-13): a read whose arena allocation failed is a wall, not an error. It keeps its
/// sequence number and its rows and is issued again once memory has come back. A read at the
/// cursor takes no room from the batches queued before it, which are needed sooner: it waits for
/// the consumers (`wall::Room::Wait`), and ends the run only when nothing in the run can free
/// memory (amended 2026-11-14; it was a fixed 24 timed attempts).
/// The reads that met a full arena, in order, and the wall they stand at (RB5).
#[derive(Default)]
struct Waits {
    retry: Vec<Retry>,
    wall: Option<crate::wall::Wall>,
}

struct Retry {
    seq: Seq,
    split: u32,
    rows: RowRange,
    kind: ReadKind,
    charge: u64,
    attempts: u32,
    not_before: std::time::Instant,
    error: MorunaError,
}

/// The tier a read lands in: the run's one host tier (contracts e.1, f.5).
fn tier0(shared: &Shared) -> Tier {
    if shared.alloc.is_pinned() {
        Tier::PinnedHost
    } else {
        Tier::Host
    }
}

/// Where a range that starts at `start` in `split` ends, for a read of about `bytes` (the
/// morsel target of the source's first consumer when `None`, f.5, h), clamped to the morsel
/// range (f.5).
fn range_end(
    shared: &Shared,
    end: &SourceEnd,
    split: &Split,
    start: u64,
    bytes: Option<u64>,
) -> u64 {
    if !split.sub_splittable {
        return split.rows;
    }
    // The range is the first consumer's: below what one of its instances can take, where its
    // kernel says that (system design 3A).
    let (min, max) = crate::knobs::morsel_range(
        shared,
        end.target_stage,
        shared.cfg.morsel_min,
        shared.cfg.morsel_max,
    );
    let target = bytes
        .unwrap_or_else(|| shared.knobs.morsel_target(end.target_stage))
        .clamp(min, max);
    // RB3: the target is bytes in memory and the split's estimate is bytes on disk, so the rows
    // are sized by what reads have actually decoded to (f.5).
    let ratio = end.decoded.ratio();
    let decoded_target = ((target as f64) / ratio).max(1.0) as u64;
    let rows = rows_for(split, decoded_target);
    let end = (start + rows).min(split.rows);
    // A remainder under half a read is taken with this one, not read on its own: a read pays
    // for its split's fetch and its buffers' fixed cost whatever its rows (RB3).
    // But not past the largest morsel the consumer may have: what one of its instances can
    // take, where its kernel says it (system design 3A).
    let most = rows_for(split, ((max as f64) / ratio).max(1.0) as u64);
    if split.rows - end < rows / 2 && split.rows - start <= most {
        split.rows
    } else {
        end
    }
}

/// Step the cursor past every split it has no rows left in: a split it has read to the end,
/// and a split that holds no rows at all (f.5). Every reader of the cursor settles it first, so
/// "the plan is exhausted" and "the cursor is past the last split" are one fact: a plan whose
/// remaining splits hold no rows is exhausted, and its queue closes (SC-T24).
fn settle(cursor: &mut Cursor, plan: &[Split]) {
    while let Some(split) = plan.get(cursor.split_index as usize)
        && cursor.row_offset >= split.rows
    {
        cursor.split_index += 1;
        cursor.row_offset = 0;
    }
}

/// The next range to issue, and the cursor advanced past it (f.5). `None` when the plan is
/// exhausted. The range is recorded as issued under the same lock that advances the cursor, so
/// a checkpoint never sees the cursor past a read it cannot find (MH 4.7). `most_rows`, when
/// given, cuts the range to at most that many rows (3A, `cut_to_room`); it is at least one.
fn take_next_range(
    shared: &Shared,
    source: &SourceEnd,
    bytes: Option<u64>,
    most_rows: Option<u64>,
) -> Option<(usize, RowRange, Seq, bool)> {
    let mut cursor = source.cursor.lock().unwrap_or_else(|e| e.into_inner());
    settle(&mut cursor, &source.plan);
    let index = cursor.split_index as usize;
    let split = source.plan.get(index)?;
    let start = cursor.row_offset;
    let whole = !split.sub_splittable;
    let mut end = range_end(shared, source, split, start, bytes);
    if let Some(most) = most_rows
        && split.sub_splittable
    {
        end = end.min(start.saturating_add(most.max(1)));
    }
    let seq = cursor.next_seq;
    cursor.next_seq += 1;
    cursor.row_offset = end;
    settle(&mut cursor, &source.plan);
    let origin = Origin {
        split: split.id,
        row_start: start,
        row_end: end,
        node: shared.cfg.node,
    };
    cursor.issued.insert(seq, origin);
    Some((index, RowRange { start, end }, seq, whole))
}

/// Whether every source has nothing left to read: a finite plan whose cursor is past its last
/// row, or a streaming source that has ended (f.5, f.9). A probe of a stage no source feeds asks
/// this when no morsel is waiting for it.
pub(crate) fn nothing_left(shared: &Shared) -> bool {
    shared.sources.iter().all(|source| {
        source.exhausted.load(Ordering::SeqCst)
            || (!source.source.streaming() && peek_next_range(shared, source).is_none())
    })
}

/// The range `take_next_range(shared, None, None)` would issue next, without taking it: its split's
/// index and its rows. `None` when the plan is exhausted.
fn peek_next_range(shared: &Shared, source: &SourceEnd) -> Option<(usize, RowRange)> {
    let mut cursor = source.cursor.lock().unwrap_or_else(|e| e.into_inner());
    settle(&mut cursor, &source.plan);
    let index = cursor.split_index as usize;
    let split = source.plan.get(index)?;
    let end = range_end(shared, source, split, cursor.row_offset, None);
    Some((
        index,
        RowRange {
            start: cursor.row_offset,
            end,
        },
    ))
}

/// The split's estimate of the bytes `rows` of it decode to: its share of the split's
/// uncompressed bytes, by rows (f.5).
fn estimated_bytes(split: &Split, rows: RowRange) -> u64 {
    if split.rows == 0 {
        return 0;
    }
    let share = u128::from(split.uncompressed_bytes) * u128::from(rows.end - rows.start)
        / u128::from(split.rows);
    u64::try_from(share).unwrap_or(u64::MAX)
}

/// What the arena may charge for a read of `rows` of `split` (f.5): the estimate, scaled by
/// the most any read so far has come to over its estimate (`learned`, at least one), and
/// doubled, because the arena charges each buffer the power-of-two class it rounds up to and
/// that is at most twice the buffer (02 AR-I2, e.2).
fn read_charge(split: &Split, rows: RowRange, learned: f64) -> u64 {
    let estimate = estimated_bytes(split, rows) as f64;
    (estimate * learned.max(1.0) * ARENA_ROUNDING) as u64
}

/// The most a size class rounds a buffer up by (02 e.2): the next power of two.
const ARENA_ROUNDING: f64 = 2.0;

/// f.5: whether the arena has room for a read that may be charged `charge`, beside what the
/// reads in flight may still be charged. An allocator with no budget for the tier bounds
/// nothing. When the arena does not have the room and nothing the run holds is on its way to
/// being released (no read in flight, no write in flight, no worker busy and every queue empty),
/// the read is admitted anyway and the arena decides: a read that cannot fit even an arena
/// holding nothing but what the run keeps for itself ends the run naming its split and rows
/// (h), rather than the drive waiting for bytes nobody will free.
fn arena_admits(shared: &Shared, inflight: &[InFlight<'_>], charge: u64) -> bool {
    room(shared, inflight).is_none_or(|room| charge <= room) || holds_nothing(shared, inflight)
}

/// What the arena has room to charge a read beside what the reads in flight may still be
/// charged (f.5); `None` when the allocator has no budget for the tier, which bounds nothing.
///
/// A read in flight counts at the larger of its charge and what it has taken (6.3c): the
/// arena's free room already lacks what it has taken, so only the rest of its charge is still
/// to come. Counting both, a whole row group's read was charged its estimate twice over beside
/// the buffers its decode had reserved, and a second read found no room until the first landed.
fn room(shared: &Shared, inflight: &[InFlight<'_>]) -> Option<u64> {
    let available = shared.alloc.available(tier0(shared))?;
    let pending: u64 = inflight.iter().map(still_to_take).sum();
    Some(available.saturating_sub(pending))
}

/// What a read in flight may still take from the arena: its charge less what it has taken.
fn still_to_take(entry: &InFlight<'_>) -> u64 {
    entry.charge.saturating_sub(entry.taken.held())
}

/// Nothing the run holds is on its way to being released: no read in flight, no write in
/// flight, no worker busy and every queue empty (`arena_admits`).
fn holds_nothing(shared: &Shared, inflight: &[InFlight<'_>]) -> bool {
    inflight.is_empty()
        && shared.reads_in_flight.load(Ordering::SeqCst) == 0
        && shared.writes_in_flight.load(Ordering::SeqCst) == 0
        && shared.knobs.slots.busy() == 0
        && shared.queues_empty()
}

/// 3A (6.3a): the most rows from `rows.start` a read of `split` may take to be charged no more
/// than `room`, when the read at the cursor does not fit: the arena bounds how far reading runs
/// ahead, never how many workers decode, so a read that does not fit beside those in flight is
/// cut to the room left rather than refused while workers idle. `None` keeps the read whole:
/// a split whose ranges do not read only their own rows (`Source::range_reads_own_rows`, a
/// Parquet row group whose pages cannot be located), or room for less than `morsel_min` bytes.
fn cut_to_room(
    shared: &Shared,
    source: &SourceEnd,
    split: &Split,
    rows: RowRange,
    room: u64,
    learned: f64,
) -> Option<u64> {
    if !split.sub_splittable || !source.source.range_reads_own_rows(split) {
        return None;
    }
    let fits = rows_within(split, rows, room, learned)?;
    let cut = RowRange {
        start: rows.start,
        end: rows.start + fits,
    };
    let decoded = estimated_bytes(split, cut) as f64 * source.decoded.ratio();
    (fits > 0 && decoded >= shared.cfg.morsel_min as f64).then_some(fits)
}

/// The most rows from `rows.start`, within `rows`, a read of `split` may take to be charged no
/// more than `cap`; `None` when all of `rows` fits, and 0 when not even one row does. The charge
/// only grows with the rows, so it is found by halving.
fn rows_within(split: &Split, rows: RowRange, cap: u64, learned: f64) -> Option<u64> {
    let charge = |most: u64| {
        read_charge(
            split,
            RowRange {
                start: rows.start,
                end: rows.start + most,
            },
            learned,
        )
    };
    let (mut fits, mut over) = (0u64, rows.end.saturating_sub(rows.start));
    if over == 0 || charge(over) <= cap {
        return None;
    }
    while over - fits > 1 {
        let mid = fits + (over - fits) / 2;
        if charge(mid) <= cap {
            fits = mid;
        } else {
            over = mid;
        }
    }
    Some(fits)
}

/// 3A (6.3b): the most rows from `rows.start` a read of `split` may take so that no morsel made
/// of it is larger than the budget of the tier it is placed in: its charge, grown by the most
/// any path of stages from the source grows a morsel (`grown`), fits that budget. At least one
/// row. `None` when the read fits as it is, or its split cannot be cut, which a morsel larger
/// than its tier then answers by being admitted alone (`decide`).
fn tier_cap(
    shared: &Shared,
    source: &SourceEnd,
    split: &Split,
    rows: RowRange,
    learned: f64,
) -> Option<u64> {
    if !split.sub_splittable {
        return None;
    }
    let budget = shared.placement.floor(tier0(shared), 0).budget;
    let cap = (budget as f64 / grown(shared, &source.outputs)) as u64;
    rows_within(split, rows, cap, learned).map(|rows| rows.max(1))
}

/// What a morsel read into `queues` comes to at the most, over its own bytes, along every path
/// of stages it feeds: each stage's growth (`StageEntry::growth`) multiplied along the way, and
/// never under 1, its size in the queue it is read into.
fn grown(shared: &Shared, queues: &[StageId]) -> f64 {
    fn walk(shared: &Shared, queue: StageId, at: f64, depth: usize) -> f64 {
        // A queue a sink takes ends the path; a path is never longer than there are stages.
        if depth > shared.stages.len() {
            return at;
        }
        let Some(moruna_kernel::Consumer::Kernel { stage, .. }) =
            shared.topology.link(queue).map(|link| link.to)
        else {
            return at;
        };
        let Some(entry) = shared.stages.iter().find(|entry| entry.stage == stage) else {
            return at;
        };
        let here = at * entry.growth();
        entry
            .outputs
            .iter()
            .map(|&next| walk(shared, next, here, depth + 1))
            .fold(at.max(here), f64::max)
    }
    queues
        .iter()
        .map(|&queue| walk(shared, queue, 1.0, 0))
        .fold(1.0, f64::max)
}

/// A read recorded by `take_next_range` (or by a resume's recompute list) has reached the
/// source's queues, so the lineage holds it from here on and the manifest no longer needs to
/// name it as issued. Called after the push, never before: between the two the morsel may be
/// named twice, which `restore` resolves, and never zero times.
fn pushed(source: &SourceEnd, seq: Seq) {
    source
        .cursor
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .issued
        .remove(&seq);
}

/// Rows that hold about `target` bytes, never fewer than one: a single row larger than the
/// maximum morsel passes at its natural size (f.5, h, architecture 7).
fn rows_for(split: &Split, target: u64) -> u64 {
    if split.rows == 0 {
        return 0;
    }
    let bytes_per_row = (split.uncompressed_bytes / split.rows).max(1);
    (target / bytes_per_row).max(1)
}

/// Source `index`'s drive thread (f.1): idle until `run`, answering helper requests throughout.
pub(crate) fn drive(
    shared: Arc<Shared>,
    index: usize,
    helper_rx: Receiver<HelperRequest>,
    parker: Parker,
) {
    let Some(source) = shared.sources.get(index) else {
        return;
    };
    let waker = Waker::from(Arc::new(DriveWake(parker.unparker().clone())));
    let mut cx = Context::from_waker(&waker);
    let mut inflight: Vec<InFlight<'_>> = Vec::new();
    let mut replacing: HashSet<(StageId, Seq)> = HashSet::new();
    let mut stream_read: Option<BoxFuture<'_, Result<Option<Payload>>>> = None;
    let mut stream_charge = source
        .plan
        .first()
        .map_or(0, |split| split.uncompressed_bytes.saturating_mul(2));
    let mut recomputed = false;
    // The most a read has come to over its split's estimate, which scales the next read's
    // admission (f.5).
    let mut learned = 1.0f64;
    // Reads that met a full arena, to issue again once memory has come back (RB5), and the wall
    // they stand at.
    let mut waits = Waits::default();

    loop {
        let mode = shared.source_mode.get();
        let mut worked = poll_inflight(
            &shared,
            source,
            &mut inflight,
            &mut replacing,
            &mut learned,
            &mut waits,
            &mut cx,
        );
        if let Some(future) = stream_read.as_mut()
            && let Poll::Ready(result) = future.as_mut().poll(&mut cx)
        {
            stream_read = None;
            shared.reads_in_flight.fetch_sub(1, Ordering::SeqCst);
            if let Ok(Some(payload)) = &result {
                stream_charge = payload.bytes().saturating_mul(2);
            }
            match publish_stream(&shared, source, result) {
                Ok(()) | Err(MorunaError::Cancelled) => {}
                Err(error) => crate::policy::terminate(&shared, error),
            }
            worked = true;
        }
        // The owner of the futures keeps polling their original completions. Only after
        // the final result has reached placement can it acknowledge a stable source cursor.
        if shared.checkpoint_pause.load(Ordering::SeqCst) && mode != DRIVE_STOP {
            source.checkpoint_paused.store(
                inflight.is_empty() && stream_read.is_none(),
                Ordering::SeqCst,
            );
            parker.park_timeout(Duration::from_millis(1));
            continue;
        }
        source.checkpoint_paused.store(false, Ordering::SeqCst);
        while stream_read.is_none()
            && let Ok(request) = helper_rx.try_recv()
        {
            service_helper(&shared, source, request, &parker, &mut cx);
            worked = true;
        }
        if mode == DRIVE_STOP {
            if inflight.is_empty() && stream_read.is_none() {
                break;
            }
        } else if mode == DRIVE_DRIVING
            && shared.run_state().is_live()
            && !shared.is_cancelled()
            && !shared.has_exit()
        {
            if source.source.streaming() {
                // One original pull at a time, only after its previous output has left Q0.
                // Unknown batch sizes remain subject to the real arena allocator's limit.
                if stream_read.is_none()
                    && !source.exhausted.load(Ordering::SeqCst)
                    && source
                        .outputs
                        .iter()
                        .all(|&q| shared.queue_count(q) == 0 && !shared.placement.is_full(q))
                    && !shared.sinks.iter().any(|end| {
                        end.sink
                            .read()
                            .unwrap_or_else(|e| e.into_inner())
                            .is_stalled()
                    })
                    && arena_admits(&shared, &[], stream_charge)
                {
                    shared.source_reading(source);
                    shared.reads_in_flight.fetch_add(1, Ordering::SeqCst);
                    stream_read = Some(
                        source
                            .source
                            .read_next(shared.alloc.as_ref(), tier0(&shared)),
                    );
                    worked = true;
                }
            } else {
                if !recomputed {
                    recomputed = true;
                    recompute(&shared, source, &parker, &mut cx);
                }
                worked |=
                    issue_replacements(&shared, source, &mut inflight, &mut replacing, learned);
                worked |= issue_retries(&shared, source, &mut inflight, &mut waits);
                // A read waiting for memory goes before any new one, so the cursor does not
                // run ahead of what the arena can hold.
                if waits.retry.is_empty() {
                    worked |= issue_reads(&shared, source, &mut inflight, learned);
                }
                close_when_exhausted(&shared, source, &inflight, &waits);
            }
        }
        // A read waiting out its backoff is work in flight for the stall detector (RB5): it will
        // be issued again, and the run is not stuck while it waits.
        source
            .waiting_reads
            .store(waits.retry.len(), Ordering::SeqCst);
        if !worked {
            parker.park_timeout(Duration::from_millis(1));
        }
    }
}

/// f.5: while there is room, a read at the cursor. `read_ahead = 0` keeps a floor of one read,
/// issued only when Q0 is empty.
///
/// Room includes the arena's, because `is_full(0)` does not keep reads inside it: a queue over
/// its high water with a candidate to demote or evict is draining, not full (09 f.14), and a
/// queue whose head is evicted is not planned at all until the head is back (09 f.2 step 1).
/// A peQL copy at 256 MiB behind a slow sink (2026-09-29) evicted its seven
/// youngest entries, waited on the head's re-read, and meanwhile read on until Q0 held 27
/// entries, 83.8 MB of a 92.8 MB arena, and the next read's `Alloc` ended the run. A fresh read
/// is now issued only while no evicted entry waits for its re-read and the arena has room for
/// it beside what the reads already in flight may still take (`arena_admits`), so the queue
/// waits on the sink instead.
fn issue_reads<'a>(
    shared: &'a Shared,
    source: &'a SourceEnd,
    inflight: &mut Vec<InFlight<'a>>,
    learned: f64,
) -> bool {
    let depth = shared.knobs.read_ahead();
    let mut issued = false;
    loop {
        let fresh = inflight
            .iter()
            .filter(|entry| entry.kind == ReadKind::Fresh)
            .count();
        if depth == 0 {
            if fresh >= 1 || source.outputs.iter().any(|&q| shared.queue_count(q) != 0) {
                break;
            }
        } else if fresh >= depth as usize {
            break;
        }
        if source.outputs.iter().any(|&q| shared.placement.is_full(q)) {
            break;
        }
        // f.5: an ordered sink that is holding bytes out of order stops admission until the
        // sequence it is waiting for arrives (08 f.4).
        if shared.sinks.iter().any(|end| {
            end.sink
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .is_stalled()
        }) {
            break;
        }
        // f.5: an evicted Q0 entry is one the planner took out because Q0 was over its high
        // water, and its re-read needs back the room its eviction gave. A fresh read lands at
        // the tail, which is where the planner evicts from next, and when the evicted entry is
        // the head nothing leaves Q0 until it is back (09 f.2 step 1). Fresh reads wait for the
        // replacements.
        if source
            .outputs
            .iter()
            .any(|&q| !shared.placement.evicted(q).is_empty())
        {
            break;
        }
        // 6.3b: a morsel larger than its tier's whole budget waits while another holds the tier
        // over its budget; a fresh read now would only add to what it waits on.
        if shared.oversize_waiting.load(Ordering::SeqCst) > 0 {
            break;
        }
        let Some((index, mut rows)) = peek_next_range(shared, source) else {
            break;
        };
        let split = &source.plan[index];
        // 3A (6.3b): no read comes to more than the budget of the tier it is placed in.
        let tier_rows = tier_cap(shared, source, split, rows, learned);
        if let Some(most) = tier_rows {
            rows.end = rows.start + most;
        }
        let whole_charge = read_charge(split, rows, learned);
        // 3A (6.3a): a read that does not fit beside those in flight, though it fits the arena as
        // the arena stands, is cut to the room left, from a split that can be cut: it is the
        // reads in flight that keep it out, and cutting it lets a second worker decode. One kept
        // out by what the queues hold waits for them to drain, as before; cut, it would only
        // trickle small morsels into the room behind a slow consumer (SC-T22). Any other waits,
        // or is admitted for the arena to decide when the run holds nothing (`arena_admits`).
        let mut most_rows = None;
        if let Some(room) = room(shared, inflight)
            && whole_charge > room
        {
            // What the reads in flight hold is theirs, not the queues' (6.3c): the read fits the
            // arena as it stands when it fits beside the queues' bytes alone.
            let held: u64 = inflight.iter().map(|entry| entry.taken.held()).sum();
            let kept_out_by_reads = shared
                .alloc
                .available(tier0(shared))
                .is_some_and(|available| whole_charge <= available.saturating_add(held));
            most_rows = kept_out_by_reads
                .then(|| cut_to_room(shared, source, split, rows, room, learned))
                .flatten();
            if most_rows.is_none() && !holds_nothing(shared, inflight) {
                break;
            }
        }
        let most_rows = match (tier_rows, most_rows) {
            (Some(tier), Some(room)) => Some(tier.min(room)),
            (tier, room) => tier.or(room),
        };
        let Some((index, rows, seq, whole)) = take_next_range(shared, source, None, most_rows)
        else {
            break;
        };
        let charge = read_charge(&source.plan[index], rows, learned);
        submit(
            shared,
            source,
            inflight,
            index,
            rows,
            seq,
            whole,
            ReadKind::Fresh,
            charge,
        );
        issued = true;
    }
    issued
}

/// f.5: service `placement.evicted(q)` of each queue the source gives by re-reading each entry
/// and calling `replace`, the entries nearest the head first and no more of them at once than
/// the read-ahead. An entry was
/// evicted because Q0 was over its high water, so re-reading every evicted entry as soon as it was
/// evicted put them all back over it: the placement evicted others, which were re-read in turn,
/// and the replacements, which no admission check stops, filled the arena (a peQL copy at
/// 256 MiB behind a slow sink held 27 morsels, twice Q0's share, F8.9). The rest stay evicted
/// until the consumer reaches them.
fn issue_replacements<'a>(
    shared: &'a Shared,
    source: &'a SourceEnd,
    inflight: &mut Vec<InFlight<'a>>,
    replacing: &mut HashSet<(StageId, Seq)>,
    learned: f64,
) -> bool {
    let mut issued = false;
    let window = usize::from(shared.knobs.read_ahead().max(1));
    let evicted: Vec<(StageId, Seq, Origin)> = source
        .outputs
        .iter()
        .flat_map(|&q| {
            shared
                .placement
                .evicted(q)
                .into_iter()
                .take(window)
                .map(move |(seq, origin)| (q, seq, origin))
        })
        .collect();
    for (queue, seq, origin) in evicted {
        if !replacing.insert((queue, seq)) {
            continue;
        }
        let Some(index) = source
            .plan
            .iter()
            .position(|split| split.id == origin.split)
        else {
            continue;
        };
        let rows = RowRange {
            start: origin.row_start,
            end: origin.row_end,
        };
        let whole = !source.plan[index].sub_splittable;
        // Counted beside the fresh reads' charges, so a fresh read is not admitted into bytes
        // a replacement is about to take.
        let charge = read_charge(&source.plan[index], rows, learned);
        submit(
            shared,
            source,
            inflight,
            index,
            rows,
            seq,
            whole,
            ReadKind::Replacement(queue),
            charge,
        );
        issued = true;
    }
    issued
}

#[allow(clippy::too_many_arguments)]
fn submit<'a>(
    shared: &'a Shared,
    source: &'a SourceEnd,
    inflight: &mut Vec<InFlight<'a>>,
    index: usize,
    rows: RowRange,
    seq: Seq,
    whole: bool,
    kind: ReadKind,
    charge: u64,
) {
    let split = &source.plan[index];
    let origin = Origin {
        split: split.id,
        row_start: rows.start,
        row_end: rows.end,
        node: shared.cfg.node,
    };
    let range = if whole { None } else { Some(rows) };
    shared.source_reading(source);
    let tier = tier0(shared);
    let times = Arc::clone(&source.out.times);
    let issued = Instant::now();
    // 6.3c: a read that decodes on workers allocates through a tally of its own, its fetch on
    // the drive and its decode on a worker, so admission knows what it has taken.
    let taken = Arc::new(Tally::new(Arc::clone(&shared.alloc)));
    let tally = Arc::clone(&taken);
    let future: BoxFuture<'a, Result<Payload>> = if source.source.decodes_on_workers() {
        // RB1: the drive fetches the bytes and any worker decodes them; each half is timed.
        let decoder = Arc::clone(&source.source);
        let alloc: Arc<dyn Allocator> = Arc::clone(&tally) as Arc<dyn Allocator>;
        let owned = split.clone();
        let worker_times = Arc::clone(&shared.times);
        Box::pin(async move {
            let fetched = source
                .source
                .fetch(split, range, tally.as_ref(), tier)
                .await;
            times.io(issued);
            let fetched = match fetched {
                Ok(f) => f,
                Err(e) => {
                    times.done();
                    return Err(e);
                }
            };
            let decoding = Arc::clone(&times);
            let decoded = crate::offload::on_worker(
                &shared.offload,
                || shared.offload_inline(),
                move || {
                    let started = std::time::Instant::now();
                    let decoded = decoder.decode(&owned, fetched, alloc.as_ref(), tier);
                    worker_times
                        .here()
                        .decode
                        .fetch_add(crate::times::since(started), Ordering::Relaxed);
                    decoding.codec(started);
                    decoded
                },
            )
            .await;
            times.done();
            decoded
        })
    } else {
        // A source that reads on the drive is called as the read is issued, which a borrowed
        // tally cannot outlive; its read is counted at its charge, as it was.
        let read = source
            .source
            .read(split, range, shared.alloc.as_ref(), tier);
        Box::pin(async move {
            let read = read.await;
            times.io(issued);
            times.done();
            read
        })
    };
    shared.reads_in_flight.fetch_add(1, Ordering::SeqCst);
    inflight.push(InFlight {
        future,
        seq,
        origin,
        split: split.id,
        rows,
        kind,
        charge,
        taken,
        attempts: 0,
    });
    let charged = inflight
        .iter()
        .fold(0u64, |sum, entry| sum.saturating_add(entry.charge));
    source.charged_peak.fetch_max(charged, Ordering::SeqCst);
}

/// RB5: issue again, in order, every read that met a full arena and whose wait is over, while
/// admission says it fits. One that does not fit counts as the wall standing; the wall ends the
/// run only when nothing in the run can free memory (`wall`).
fn issue_retries<'a>(
    shared: &'a Shared,
    source: &'a SourceEnd,
    inflight: &mut Vec<InFlight<'a>>,
    waits: &mut Waits,
) -> bool {
    let Waits { retry, wall } = waits;
    let mut issued = false;
    while let Some(next) = retry.first() {
        let now = std::time::Instant::now();
        if now < next.not_before {
            break;
        }
        if !arena_admits(shared, inflight, next.charge) {
            let charge = next.charge;
            let at = wall.get_or_insert_with(|| crate::wall::Wall::new(shared));
            match at.refused(shared, charge, crate::wall::Room::Wait, 0) {
                crate::wall::Next::Retry(wait) => retry[0].not_before = now + wait,
                crate::wall::Next::GiveUp(why) => {
                    let failed = retry.remove(0);
                    tracing::warn!(target: "sched.wall", split = failed.split, %why, "memory wall");
                    crate::policy::terminate(
                        shared,
                        source_diagnostic(failed.error, failed.split, failed.rows),
                    );
                    retry.clear();
                    return true;
                }
            }
            break;
        }
        let again = retry.remove(0);
        let Some(index) = source.plan.iter().position(|split| split.id == again.split) else {
            crate::policy::terminate(shared, again.error);
            return true;
        };
        let whole = !source.plan[index].sub_splittable;
        submit(
            shared,
            source,
            inflight,
            index,
            again.rows,
            again.seq,
            whole,
            again.kind,
            again.charge,
        );
        // Should it meet a full arena again, it comes back with one more attempt counted.
        if let Some(entry) = inflight.last_mut() {
            entry.attempts = again.attempts;
        }
        issued = true;
    }
    issued
}

/// Poll every read once; a resolved one is pushed, replaced or fails the run (f.5, h).
fn poll_inflight(
    shared: &Shared,
    source: &SourceEnd,
    inflight: &mut Vec<InFlight<'_>>,
    replacing: &mut HashSet<(StageId, Seq)>,
    learned: &mut f64,
    waits: &mut Waits,
    cx: &mut Context<'_>,
) -> bool {
    let Waits { retry, wall } = waits;
    let mut progressed = false;
    let mut index = 0;
    while index < inflight.len() {
        let poll = inflight[index].future.as_mut().poll(cx);
        match poll {
            Poll::Pending => index += 1,
            Poll::Ready(result) => {
                let entry = inflight.remove(index);
                shared.reads_in_flight.fetch_sub(1, Ordering::SeqCst);
                progressed = true;
                // A replacement stays in `replacing` while it waits to be issued again (RB5):
                // let go here, the entry still evicted would draw a second read, and whichever
                // landed second would find the entry already back and end the run ("replace: no
                // evicted entry", Enrich at 2 GiB).
                let retried = matches!(result, Err(MorunaError::Alloc { .. }));
                if let ReadKind::Replacement(queue) = entry.kind
                    && !retried
                {
                    replacing.remove(&(queue, entry.seq));
                }
                match result {
                    Ok(payload) => {
                        // A read that lands is the wall moving.
                        *wall = None;
                        learn(source, &entry, &payload, learned);
                        let (rows, bytes) = (payload.rows(), payload.bytes());
                        let morsel = Morsel::new(entry.seq, 0, payload, entry.origin.clone());
                        let outcome = match entry.kind {
                            ReadKind::Replacement(queue) => {
                                crate::decide::admit_replace(shared, queue, morsel)
                            }
                            ReadKind::Fresh | ReadKind::Recompute => {
                                if entry.kind == ReadKind::Recompute {
                                    shared.recomputed.fetch_add(1, Ordering::SeqCst);
                                }
                                crate::decide::admit_push(shared, &source.outputs, morsel, None)
                            }
                        };
                        match outcome {
                            Ok(()) => {
                                if !matches!(entry.kind, ReadKind::Replacement(_)) {
                                    pushed(source, entry.seq);
                                    shared.source_pushed(source, rows, bytes);
                                }
                            }
                            Err(MorunaError::Cancelled) => {}
                            Err(e) => crate::policy::terminate(shared, e),
                        }
                    }
                    Err(e @ MorunaError::Alloc { .. }) => {
                        // RB5: the arena was full when this read landed; it waits for memory
                        // to come back and is issued again (`issue_retries`), unless nothing in
                        // the run can free memory.
                        let at = wall.get_or_insert_with(|| crate::wall::Wall::new(shared));
                        match at.refused(shared, entry.charge, crate::wall::Room::Wait, 0) {
                            crate::wall::Next::Retry(wait) => retry.push(Retry {
                                seq: entry.seq,
                                split: entry.split,
                                rows: entry.rows,
                                kind: entry.kind,
                                charge: entry.charge,
                                attempts: entry.attempts + 1,
                                not_before: std::time::Instant::now() + wait,
                                error: e,
                            }),
                            crate::wall::Next::GiveUp(why) => {
                                tracing::warn!(target: "sched.wall", split = entry.split, %why, "memory wall");
                                crate::policy::terminate(
                                    shared,
                                    source_diagnostic(e, entry.split, entry.rows),
                                );
                            }
                        }
                        progressed = true;
                    }
                    Err(e) => {
                        // h: there is no skip for a source error; the diagnostic names the
                        // split and the row range.
                        crate::policy::terminate(
                            shared,
                            source_diagnostic(e, entry.split, entry.rows),
                        );
                    }
                }
            }
        }
    }
    progressed
}

/// f.5: a split's bytes are an estimate (`Split::estimated`), so the drive learns what reads
/// come to over their estimate (`DecodeRatio`) and scales the next read's admission and rows by
/// it, for every reader of this source's cursor, the probe's helper included.
fn learn(source: &SourceEnd, entry: &InFlight<'_>, payload: &Payload, learned: &mut f64) {
    let Some(split) = source.plan.iter().find(|split| split.id == entry.split) else {
        return;
    };
    let estimate = estimated_bytes(split, entry.rows);
    if estimate > 0 {
        *learned = source
            .decoded
            .observe(payload.rows(), payload.bytes() as f64 / estimate as f64);
    }
}

/// h: name the split and the row range on any read failure.
fn source_diagnostic(e: MorunaError, split: u32, rows: RowRange) -> MorunaError {
    match e {
        MorunaError::Alloc {
            bytes,
            tier,
            budget,
            in_use,
        } => MorunaError::Source {
            split,
            msg: format!(
                "rows {}..{}: alloc {bytes} bytes in {tier:?} exceeds budget {budget} with {in_use} in use",
                rows.start, rows.end
            ),
        },
        MorunaError::Source { split: at, msg } => MorunaError::Source {
            split: at,
            msg: format!("rows {}..{}: {msg}", rows.start, rows.end),
        },
        other => other,
    }
}

/// f.5: when the plan is exhausted (no split at or after the cursor has a row left), no read is
/// in flight and none waits to be read again, close the source's queues and say so; the run's
/// sources are exhausted when the last of them is.
///
/// A read that met a full arena (RB5) has left `inflight` but not the plan: the cursor is already
/// past its rows, and it is still to come. Closing on `inflight` alone dropped it: the sink
/// finished without it and the run reported `Completed` with its rows missing (a 768-column copy
/// read back 35,000 of 40,000 rows), or its retry was pushed into the closed queue and ended the
/// run "push: stage 0 is closed" (items 6.3s and 6.3n, SC-T50).
fn close_when_exhausted(
    shared: &Shared,
    source: &SourceEnd,
    inflight: &[InFlight<'_>],
    waits: &Waits,
) {
    if !inflight.is_empty() || !waits.retry.is_empty() || source.exhausted.load(Ordering::SeqCst) {
        return;
    }
    {
        let mut cursor = source.cursor.lock().unwrap_or_else(|e| e.into_inner());
        settle(&mut cursor, &source.plan);
        // Every read taken from the cursor is pushed before the queues close: a read the cursor
        // passed and nobody pushed is rows the sink would never see.
        if (cursor.split_index as usize) < source.plan.len() || !cursor.issued.is_empty() {
            return;
        }
    }
    close_source(shared, source);
}

/// A streaming cursor counts actual delivered batches and rows, never invented plan rows.
fn publish_stream(
    shared: &Shared,
    source: &SourceEnd,
    result: Result<Option<Payload>>,
) -> Result<()> {
    match result {
        Ok(None) => {
            close_source(shared, source);
            Ok(())
        }
        Ok(Some(payload)) => {
            let (rows, bytes) = (payload.rows(), payload.bytes());
            let (seq, origin) = {
                let mut cursor = source.cursor.lock().unwrap_or_else(|e| e.into_inner());
                let seq = cursor.next_seq;
                cursor.next_seq += 1;
                let start = cursor.row_offset;
                cursor.row_offset = start.saturating_add(rows);
                (
                    seq,
                    Origin {
                        split: 0,
                        row_start: start,
                        row_end: cursor.row_offset,
                        node: shared.cfg.node,
                    },
                )
            };
            crate::decide::admit_push(
                shared,
                &source.outputs,
                Morsel::new(seq, 0, payload, origin),
                None,
            )?;
            shared.source_pushed(source, rows, bytes);
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn close_source(shared: &Shared, source: &SourceEnd) {
    if source.exhausted.swap(true, Ordering::SeqCst) {
        return;
    }
    if shared
        .sources
        .iter()
        .all(|end| end.exhausted.load(Ordering::SeqCst))
    {
        shared.source_exhausted.store(true, Ordering::SeqCst);
    }
    for &q in &source.outputs {
        shared.close_queue(q);
    }
    shared.advance_closes();
    shared.unpark_all();
}

/// How many morsels each source has read, for the sources read to the end; `None` for one still
/// reading.
pub(crate) fn counts(shared: &Shared) -> Vec<Option<Seq>> {
    shared
        .sources
        .iter()
        .map(|end| {
            end.exhausted.load(Ordering::SeqCst).then(|| {
                end.cursor
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .next_seq
            })
        })
        .collect()
}

/// f.5, h: the source drive panicked, in the source's code or in its own. The run ends with a
/// source diagnostic naming the split of the oldest read the drive had issued and not pushed
/// (the read it was most likely serving), or the split at the cursor when none was in flight,
/// and saying what the panic said. Nothing is left waiting for a drive that is gone: without
/// this the run waited for ever, its tasks never ended (MA2).
pub(crate) fn panicked(shared: &Shared, index: usize, panic: &(dyn std::any::Any + Send)) {
    let Some(source) = shared.sources.get(index) else {
        return;
    };
    let split = {
        let cursor = source.cursor.lock().unwrap_or_else(|e| e.into_inner());
        match cursor.issued.values().next() {
            Some(origin) => origin.split,
            None => source
                .plan
                .get(cursor.split_index as usize)
                .map_or(cursor.split_index, |split| split.id),
        }
    };
    let msg = format!(
        "the source drive panicked: {}",
        crate::worker::panic_message(panic)
    );
    tracing::error!(target: "sched.drive_panicked", split, %msg, "the source drive died");
    crate::policy::terminate(shared, MorunaError::Source { split, msg });
}

/// The drive-side helper of f.9: one read of about `bytes` at the cursor, issued and waited on
/// here so the probing thread never waits on a completion itself.
fn service_helper(
    shared: &Shared,
    source: &SourceEnd,
    request: HelperRequest,
    parker: &Parker,
    cx: &mut Context<'_>,
) {
    let reply: Sender<Result<()>> = request.reply;
    if source.source.streaming() {
        if source.exhausted.load(Ordering::SeqCst)
            || source
                .outputs
                .iter()
                .any(|&q| shared.queue_count(q) != 0 || shared.placement.is_full(q))
            || shared.sinks.iter().any(|end| {
                end.sink
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_stalled()
            })
            || !arena_admits(
                shared,
                &[],
                source
                    .plan
                    .first()
                    .map_or(0, |split| split.uncompressed_bytes.saturating_mul(2)),
            )
        {
            let _ = reply.send(Err(MorunaError::Plan(
                "the streaming source cannot admit a probe read now".into(),
            )));
            return;
        }
        let mut future = source
            .source
            .read_next(shared.alloc.as_ref(), tier0(shared));
        shared.source_reading(source);
        shared.reads_in_flight.fetch_add(1, Ordering::SeqCst);
        let result = block_on(&mut future, parker, cx);
        shared.reads_in_flight.fetch_sub(1, Ordering::SeqCst);
        let answer = publish_stream(shared, source, result);
        let _ = reply.send(answer);
        return;
    }

    let Some((index, rows, seq, whole)) =
        take_next_range(shared, source, Some(request.bytes), None)
    else {
        let _ = reply.send(Err(MorunaError::Plan(
            "the source plan is exhausted; there is nothing to probe with".into(),
        )));
        return;
    };
    let split = &source.plan[index];
    let origin = Origin {
        split: split.id,
        row_start: rows.start,
        row_end: rows.end,
        node: shared.cfg.node,
    };
    let range = if whole { None } else { Some(rows) };
    shared.source_reading(source);
    let issued = Instant::now();
    let mut future = source
        .source
        .read(split, range, shared.alloc.as_ref(), tier0(shared));
    shared.reads_in_flight.fetch_add(1, Ordering::SeqCst);
    let result = block_on(&mut future, parker, cx);
    shared.reads_in_flight.fetch_sub(1, Ordering::SeqCst);
    source.out.times.io(issued);
    source.out.times.done();
    let answer = match result {
        Ok(payload) => {
            // The probe's read is the run's first: what it decoded to sizes every read after it
            // (`range_end`), so the first wave behind the probe is not sized from the file's
            // encoded bytes.
            let estimate = estimated_bytes(split, rows);
            if estimate > 0 {
                source
                    .decoded
                    .observe(payload.rows(), payload.bytes() as f64 / estimate as f64);
            }
            let (rows, bytes) = (payload.rows(), payload.bytes());
            let pushed_ok = shared
                .placement
                .push_to(&source.outputs, Morsel::new(seq, 0, payload, origin));
            if pushed_ok.is_ok() {
                pushed(source, seq);
                shared.source_pushed(source, rows, bytes);
            }
            pushed_ok
        }
        Err(e) => Err(source_diagnostic(e, split.id, rows)),
    };
    let _ = reply.send(answer);
}

/// f.13: before a source's drive issues a new read, every `(seq, origin)` of it the manifest
/// could not point to on disk is re-read in order through the normal source path and pushed into
/// its queues with its original sequence number. The re-reads obey `is_full` like any other
/// read, so a large recompute list does not blow the budget.
fn recompute(shared: &Shared, source: &SourceEnd, parker: &Parker, cx: &mut Context<'_>) {
    let pending: Vec<(Seq, Origin)> = std::mem::take(
        &mut *source
            .to_recompute
            .lock()
            .unwrap_or_else(|e| e.into_inner()),
    );
    if pending.is_empty() {
        return;
    }
    tracing::info!(
        target: "sched.resume",
        to_recompute = pending.len(),
        "re-reading the morsels the manifest could not point to"
    );
    for (seq, origin) in pending {
        while source.outputs.iter().any(|&q| shared.placement.is_full(q))
            && !shared.is_cancelled()
            && !shared.has_exit()
        {
            parker.park_timeout(Duration::from_millis(1));
        }
        if shared.is_cancelled() || shared.has_exit() {
            return;
        }
        let Some(index) = source
            .plan
            .iter()
            .position(|split| split.id == origin.split)
        else {
            crate::policy::terminate(
                shared,
                MorunaError::Resume(format!(
                    "the manifest names split {} which the plan does not hold",
                    origin.split
                )),
            );
            return;
        };
        let split = &source.plan[index];
        let rows = RowRange {
            start: origin.row_start,
            end: origin.row_end,
        };
        let range = if split.sub_splittable {
            Some(rows)
        } else {
            None
        };
        shared.source_reading(source);
        let issued = Instant::now();
        let mut future = source
            .source
            .read(split, range, shared.alloc.as_ref(), tier0(shared));
        shared.reads_in_flight.fetch_add(1, Ordering::SeqCst);
        let result = block_on(&mut future, parker, cx);
        shared.reads_in_flight.fetch_sub(1, Ordering::SeqCst);
        source.out.times.io(issued);
        source.out.times.done();
        match result {
            Ok(payload) => {
                let (rows, bytes) = (payload.rows(), payload.bytes());
                let morsel = Morsel::new(seq, 0, payload, origin);
                if let Err(e) = crate::decide::admit_push(shared, &source.outputs, morsel, None) {
                    crate::policy::terminate(shared, e);
                    return;
                }
                pushed(source, seq);
                shared.source_pushed(source, rows, bytes);
                shared.recomputed.fetch_add(1, Ordering::SeqCst);
            }
            Err(e) => {
                crate::policy::terminate(shared, source_diagnostic(e, split.id, rows));
                return;
            }
        }
    }
}

/// Wait for one read on the drive thread, which is where CT-I7 allows a wait.
fn block_on<T>(future: &mut BoxFuture<'_, T>, parker: &Parker, cx: &mut Context<'_>) -> T {
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(cx) {
            return value;
        }
        parker.park_timeout(Duration::from_millis(1));
    }
}

/// f.13: the cursor a resumed source starts from, and the sequence counter that goes with it.
/// The morsels the resume will re-read first are recorded as issued, so a manifest written
/// while the recompute pass is still reading them names them rather than losing them.
pub(crate) fn set_cursor(
    source: &SourceEnd,
    cursor: moruna_kernel::SourceCursor,
    recompute: &[(Seq, Origin)],
) {
    let mut slot = source.cursor.lock().unwrap_or_else(|e| e.into_inner());
    slot.split_index = cursor.split_index;
    slot.row_offset = cursor.row_offset;
    slot.next_seq = cursor.next_seq;
    slot.issued = recompute.iter().cloned().collect();
}

/// A source's cursor as f.12 records it, the next range to issue, never the last completed
/// one; and, in the same snapshot, every read issued behind it that has not reached its queues
/// (MH 4.7).
pub(crate) fn cursor(source: &SourceEnd) -> (moruna_kernel::SourceCursor, Vec<(Seq, Origin)>) {
    let slot = source.cursor.lock().unwrap_or_else(|e| e.into_inner());
    (
        moruna_kernel::SourceCursor {
            split_index: slot.split_index,
            row_offset: slot.row_offset,
            next_seq: slot.next_seq,
        },
        slot.issued
            .iter()
            .map(|(seq, origin)| (*seq, origin.clone()))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(charge: u64, taken: &Arc<Tally>) -> InFlight<'static> {
        InFlight {
            future: Box::pin(async { Err(MorunaError::Cancelled) }),
            seq: 0,
            origin: Origin {
                split: 0,
                row_start: 0,
                row_end: 1,
                node: moruna_kernel::LOCAL_NODE,
            },
            split: 0,
            rows: RowRange { start: 0, end: 1 },
            kind: ReadKind::Fresh,
            charge,
            taken: Arc::clone(taken),
            attempts: 0,
        }
    }

    /// 6.3c: a read in flight counts at max(charge, held). What it may still take is its charge
    /// less what it holds, and nothing, never less, once it holds more than its charge (an
    /// estimate that came in short): the arena's in-use carries that excess.
    #[test]
    fn a_read_is_counted_at_the_larger_of_its_charge_and_what_it_holds() {
        let alloc: Arc<dyn moruna_kernel::Allocator> =
            Arc::new(moruna_testkit::FakeAllocator::new());
        let taken = Arc::new(Tally::new(alloc));
        let read = entry(200, &taken);
        assert_eq!(still_to_take(&read), 200, "nothing taken yet");
        let some = taken.alloc(50, Tier::Host).expect("fifty");
        assert_eq!(still_to_take(&read), 150);
        let more = taken.alloc(250, Tier::Host).expect("two hundred and fifty");
        assert_eq!(taken.held(), 300);
        assert_eq!(still_to_take(&read), 0, "it holds more than its charge");
        drop((some, more));
        assert_eq!(
            still_to_take(&read),
            200,
            "what it gave back it may take again"
        );
    }
}
