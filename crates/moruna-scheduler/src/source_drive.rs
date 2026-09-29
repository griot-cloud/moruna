//! The source drive (f.5), the drive-side helper (f.9) and the recompute pass of a resumed run
//! (f.13).
//!
//! One of the two threads in the process that may wait on a `Completion` (CT-I7, SC-I1): it
//! issues `Source::read` up to `read_ahead` in flight, polls the futures with a waker over its
//! own parker, and pushes each result to Q0. It holds no scheduler lock while it waits.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use crossbeam::channel::{Receiver, Sender};
use crossbeam::sync::{Parker, Unparker};
use moruna_kernel::{
    BoxFuture, Morsel, MorunaError, Origin, Payload, Result, RowRange, Seq, Split, Tier,
};

use crate::shared::{DRIVE_DRIVING, DRIVE_STOP, HelperRequest, Shared};

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
    /// A new range: pushed to Q0.
    Fresh,
    /// An entry the engine evicted: put back in its original position with `replace`.
    Replacement,
    /// A morsel the manifest could not point to on disk: pushed to Q0 with its original seq.
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
}

/// The tier a read lands in: the run's one host tier (contracts e.1, f.5).
fn tier0(shared: &Shared) -> Tier {
    if shared.alloc.is_pinned() {
        Tier::PinnedHost
    } else {
        Tier::Host
    }
}

/// The stage whose morsel target sizes a source read: the first kernel's, or the sink's queue
/// when there is no kernel (f.5, h).
fn target_stage(shared: &Shared) -> moruna_kernel::StageId {
    if shared.stages.is_empty() { 0 } else { 1 }
}

/// Where a range that starts at `start` in `split` ends, for a read of about `bytes` (the
/// morsel target when `None`), clamped to the morsel range (f.5).
fn range_end(shared: &Shared, split: &Split, start: u64, bytes: Option<u64>) -> u64 {
    if !split.sub_splittable {
        return split.rows;
    }
    let target = bytes
        .unwrap_or_else(|| shared.knobs.morsel_target(target_stage(shared)))
        .clamp(shared.cfg.morsel_min, shared.cfg.morsel_max);
    (start + rows_for(split, target)).min(split.rows)
}

/// The next range to issue, and the cursor advanced past it (f.5). `None` when the plan is
/// exhausted. The range is recorded as issued under the same lock that advances the cursor, so
/// a checkpoint never sees the cursor past a read it cannot find (MH 4.7).
fn take_next_range(shared: &Shared, bytes: Option<u64>) -> Option<(usize, RowRange, Seq, bool)> {
    let mut cursor = shared.cursor.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        let index = cursor.split_index as usize;
        let split = shared.plan.get(index)?;
        if cursor.row_offset >= split.rows {
            cursor.split_index += 1;
            cursor.row_offset = 0;
            continue;
        }
        let start = cursor.row_offset;
        let whole = !split.sub_splittable;
        let end = range_end(shared, split, start, bytes);
        let seq = cursor.next_seq;
        cursor.next_seq += 1;
        cursor.row_offset = end;
        if end >= split.rows {
            cursor.split_index += 1;
            cursor.row_offset = 0;
        }
        let origin = Origin {
            split: split.id,
            row_start: start,
            row_end: end,
            node: shared.cfg.node,
        };
        cursor.issued.insert(seq, origin);
        return Some((index, RowRange { start, end }, seq, whole));
    }
}

/// The range `take_next_range(shared, None)` would issue next, without taking it: its split's
/// index and its rows. `None` when the plan is exhausted.
fn peek_next_range(shared: &Shared) -> Option<(usize, RowRange)> {
    let cursor = shared.cursor.lock().unwrap_or_else(|e| e.into_inner());
    let (mut index, mut start) = (cursor.split_index as usize, cursor.row_offset);
    loop {
        let split = shared.plan.get(index)?;
        if start >= split.rows {
            index += 1;
            start = 0;
            continue;
        }
        let end = range_end(shared, split, start, None);
        return Some((index, RowRange { start, end }));
    }
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
    let Some(available) = shared.alloc.available(tier0(shared)) else {
        return true;
    };
    let pending: u64 = inflight.iter().map(|entry| entry.charge).sum();
    if pending.saturating_add(charge) <= available {
        return true;
    }
    inflight.is_empty()
        && shared.writes_in_flight.load(Ordering::SeqCst) == 0
        && shared.workers_busy.load(Ordering::SeqCst) == 0
        && (0..=shared.last_queue()).all(|stage| shared.queue_count(stage) == 0)
}

/// A read recorded by `take_next_range` (or by a resume's recompute list) has reached Q0, so
/// the lineage holds it from here on and the manifest no longer needs to name it as issued.
/// Called after the push, never before: between the two the morsel may be named twice, which
/// `restore` resolves, and never zero times.
fn pushed(shared: &Shared, seq: Seq) {
    shared
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

/// The drive thread (f.1): idle until `run`, answering helper requests throughout.
pub(crate) fn drive(shared: Arc<Shared>, helper_rx: Receiver<HelperRequest>, parker: Parker) {
    let waker = Waker::from(Arc::new(DriveWake(parker.unparker().clone())));
    let mut cx = Context::from_waker(&waker);
    let mut inflight: Vec<InFlight<'_>> = Vec::new();
    let mut replacing: HashSet<Seq> = HashSet::new();
    let mut recomputed = false;
    // The most a read has come to over its split's estimate, which scales the next read's
    // admission (f.5).
    let mut learned = 1.0f64;

    loop {
        let mode = shared.source_mode.get();
        let mut worked = poll_inflight(
            &shared,
            &mut inflight,
            &mut replacing,
            &mut learned,
            &mut cx,
        );
        while let Ok(request) = helper_rx.try_recv() {
            service_helper(&shared, request, &parker, &mut cx);
            worked = true;
        }
        if mode == DRIVE_STOP {
            if inflight.is_empty() {
                break;
            }
        } else if mode == DRIVE_DRIVING
            && shared.run_state().is_live()
            && !shared.is_cancelled()
            && !shared.has_exit()
        {
            if !recomputed {
                recomputed = true;
                recompute(&shared, &parker, &mut cx);
            }
            worked |= issue_replacements(&shared, &mut inflight, &mut replacing, learned);
            worked |= issue_reads(&shared, &mut inflight, learned);
            close_when_exhausted(&shared, &inflight);
        }
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
fn issue_reads<'a>(shared: &'a Shared, inflight: &mut Vec<InFlight<'a>>, learned: f64) -> bool {
    let depth = shared.knobs.read_ahead();
    let mut issued = false;
    loop {
        let fresh = inflight
            .iter()
            .filter(|entry| entry.kind == ReadKind::Fresh)
            .count();
        if depth == 0 {
            if fresh >= 1 || shared.queue_count(0) != 0 {
                break;
            }
        } else if fresh >= depth as usize {
            break;
        }
        if shared.placement.is_full(0) {
            break;
        }
        // f.5: an ordered sink that is holding bytes out of order stops admission until the
        // sequence it is waiting for arrives (08 f.4).
        if shared
            .sink
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_stalled()
        {
            break;
        }
        // f.5: an evicted Q0 entry is one the planner took out because Q0 was over its high
        // water, and its re-read needs back the room its eviction gave. A fresh read lands at
        // the tail, which is where the planner evicts from next, and when the evicted entry is
        // the head nothing leaves Q0 until it is back (09 f.2 step 1). Fresh reads wait for the
        // replacements.
        if !shared.placement.evicted(0).is_empty() {
            break;
        }
        let Some((index, rows)) = peek_next_range(shared) else {
            break;
        };
        if !arena_admits(
            shared,
            inflight,
            read_charge(&shared.plan[index], rows, learned),
        ) {
            break;
        }
        let Some((index, rows, seq, whole)) = take_next_range(shared, None) else {
            break;
        };
        let charge = read_charge(&shared.plan[index], rows, learned);
        submit(
            shared,
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

/// f.5: service `placement.evicted(0)` by re-reading each entry and calling `replace`, the
/// entries nearest the head first and no more of them at once than the read-ahead. An entry was
/// evicted because Q0 was over its high water, so re-reading every evicted entry as soon as it was
/// evicted put them all back over it: the placement evicted others, which were re-read in turn,
/// and the replacements, which no admission check stops, filled the arena (a peQL copy at
/// 256 MiB behind a slow sink held 27 morsels, twice Q0's share, F8.9). The rest stay evicted
/// until the consumer reaches them.
fn issue_replacements<'a>(
    shared: &'a Shared,
    inflight: &mut Vec<InFlight<'a>>,
    replacing: &mut HashSet<Seq>,
    learned: f64,
) -> bool {
    let mut issued = false;
    let window = usize::from(shared.knobs.read_ahead().max(1));
    for (seq, origin) in shared.placement.evicted(0).into_iter().take(window) {
        if !replacing.insert(seq) {
            continue;
        }
        let Some(index) = shared
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
        let whole = !shared.plan[index].sub_splittable;
        // Counted beside the fresh reads' charges, so a fresh read is not admitted into bytes
        // a replacement is about to take.
        let charge = read_charge(&shared.plan[index], rows, learned);
        submit(
            shared,
            inflight,
            index,
            rows,
            seq,
            whole,
            ReadKind::Replacement,
            charge,
        );
        issued = true;
    }
    issued
}

#[allow(clippy::too_many_arguments)]
fn submit<'a>(
    shared: &'a Shared,
    inflight: &mut Vec<InFlight<'a>>,
    index: usize,
    rows: RowRange,
    seq: Seq,
    whole: bool,
    kind: ReadKind,
    charge: u64,
) {
    let split = &shared.plan[index];
    let origin = Origin {
        split: split.id,
        row_start: rows.start,
        row_end: rows.end,
        node: shared.cfg.node,
    };
    let range = if whole { None } else { Some(rows) };
    let future = shared
        .source
        .read(split, range, shared.alloc.as_ref(), tier0(shared));
    shared.reads_in_flight.fetch_add(1, Ordering::SeqCst);
    inflight.push(InFlight {
        future,
        seq,
        origin,
        split: split.id,
        rows,
        kind,
        charge,
    });
}

/// Poll every read once; a resolved one is pushed, replaced or fails the run (f.5, h).
fn poll_inflight(
    shared: &Shared,
    inflight: &mut Vec<InFlight<'_>>,
    replacing: &mut HashSet<Seq>,
    learned: &mut f64,
    cx: &mut Context<'_>,
) -> bool {
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
                replacing.remove(&entry.seq);
                match result {
                    Ok(payload) => {
                        learn(shared, &entry, &payload, learned);
                        let morsel = Morsel::new(entry.seq, 0, payload, entry.origin.clone());
                        let outcome = match entry.kind {
                            ReadKind::Replacement => shared.placement.replace(0, morsel),
                            ReadKind::Fresh | ReadKind::Recompute => {
                                if entry.kind == ReadKind::Recompute {
                                    shared.recomputed.fetch_add(1, Ordering::SeqCst);
                                }
                                shared.placement.push(0, morsel)
                            }
                        };
                        match outcome {
                            Ok(()) => {
                                if entry.kind != ReadKind::Replacement {
                                    pushed(shared, entry.seq);
                                }
                            }
                            Err(MorunaError::Cancelled) => {}
                            Err(e) => crate::policy::terminate(shared, e),
                        }
                    }
                    Err(e) => {
                        // h: there is no skip for a source error; the diagnostic names the
                        // split and the row range, including the `Alloc` of a single row
                        // larger than the budget.
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

/// f.5: a split's bytes are an estimate (`Split::estimated`), so the drive keeps the most any
/// read has come to over its estimate and scales the next read's admission by it. It never
/// falls, so one read that came to more than its estimate keeps the bound honest for the rest
/// of the run.
fn learn(shared: &Shared, entry: &InFlight<'_>, payload: &Payload, learned: &mut f64) {
    let Some(split) = shared.plan.iter().find(|split| split.id == entry.split) else {
        return;
    };
    let estimate = estimated_bytes(split, entry.rows);
    if estimate > 0 {
        *learned = learned.max(payload.bytes() as f64 / estimate as f64);
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

/// f.5: when the plan is exhausted and no read is in flight, close Q0 and say so.
fn close_when_exhausted(shared: &Shared, inflight: &[InFlight<'_>]) {
    if !inflight.is_empty() {
        return;
    }
    {
        let cursor = shared.cursor.lock().unwrap_or_else(|e| e.into_inner());
        if (cursor.split_index as usize) < shared.plan.len() {
            return;
        }
    }
    shared.source_exhausted.store(true, Ordering::SeqCst);
    shared.close_queue(0);
    shared.advance_closes();
}

/// The drive-side helper of f.9: one read of about `bytes` at the cursor, issued and waited on
/// here so the probing thread never waits on a completion itself.
fn service_helper(shared: &Shared, request: HelperRequest, parker: &Parker, cx: &mut Context<'_>) {
    let reply: Sender<Result<()>> = request.reply;
    let Some((index, rows, seq, whole)) = take_next_range(shared, Some(request.bytes)) else {
        let _ = reply.send(Err(MorunaError::Plan(
            "the source plan is exhausted; there is nothing to probe with".into(),
        )));
        return;
    };
    let split = &shared.plan[index];
    let origin = Origin {
        split: split.id,
        row_start: rows.start,
        row_end: rows.end,
        node: shared.cfg.node,
    };
    let range = if whole { None } else { Some(rows) };
    let mut future = shared
        .source
        .read(split, range, shared.alloc.as_ref(), tier0(shared));
    shared.reads_in_flight.fetch_add(1, Ordering::SeqCst);
    let result = block_on(&mut future, parker, cx);
    shared.reads_in_flight.fetch_sub(1, Ordering::SeqCst);
    let answer = match result {
        Ok(payload) => {
            let pushed_ok = shared
                .placement
                .push(0, Morsel::new(seq, 0, payload, origin));
            if pushed_ok.is_ok() {
                pushed(shared, seq);
            }
            pushed_ok
        }
        Err(e) => Err(source_diagnostic(e, split.id, rows)),
    };
    let _ = reply.send(answer);
}

/// f.13: before the source drive issues a new read, every `(seq, origin)` the manifest could
/// not point to on disk is re-read in order through the normal source path and pushed to Q0
/// with its original sequence number. The re-reads obey `read_ahead` and `is_full(0)` like any
/// other read, so a large recompute list does not blow the budget.
fn recompute(shared: &Shared, parker: &Parker, cx: &mut Context<'_>) {
    let pending: Vec<(Seq, Origin)> = std::mem::take(
        &mut *shared
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
        while shared.placement.is_full(0) && !shared.is_cancelled() && !shared.has_exit() {
            parker.park_timeout(Duration::from_millis(1));
        }
        if shared.is_cancelled() || shared.has_exit() {
            return;
        }
        let Some(index) = shared
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
        let split = &shared.plan[index];
        let rows = RowRange {
            start: origin.row_start,
            end: origin.row_end,
        };
        let range = if split.sub_splittable {
            Some(rows)
        } else {
            None
        };
        let mut future = shared
            .source
            .read(split, range, shared.alloc.as_ref(), tier0(shared));
        shared.reads_in_flight.fetch_add(1, Ordering::SeqCst);
        let result = block_on(&mut future, parker, cx);
        shared.reads_in_flight.fetch_sub(1, Ordering::SeqCst);
        match result {
            Ok(payload) => {
                let morsel = Morsel::new(seq, 0, payload, origin);
                if let Err(e) = shared.placement.push(0, morsel) {
                    crate::policy::terminate(shared, e);
                    return;
                }
                pushed(shared, seq);
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

/// f.13: the cursor a resumed run starts from, and the sequence counter that goes with it. The
/// morsels the resume will re-read first are recorded as issued, so a manifest written while
/// the recompute pass is still reading them names them rather than losing them.
pub(crate) fn set_cursor(
    shared: &Shared,
    cursor: moruna_kernel::SourceCursor,
    recompute: &[(Seq, Origin)],
) {
    let mut slot = shared.cursor.lock().unwrap_or_else(|e| e.into_inner());
    slot.split_index = cursor.split_index;
    slot.row_offset = cursor.row_offset;
    slot.next_seq = cursor.next_seq;
    slot.issued = recompute.iter().cloned().collect();
}

/// The cursor as f.12 records it, the next range to issue, never the last completed one; and,
/// in the same snapshot, every read issued behind it that has not reached Q0 (MH 4.7).
pub(crate) fn cursor(shared: &Shared) -> (moruna_kernel::SourceCursor, Vec<(Seq, Origin)>) {
    let slot = shared.cursor.lock().unwrap_or_else(|e| e.into_inner());
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
