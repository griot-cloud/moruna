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

/// The next range to issue, and the cursor advanced past it (f.5). `None` when the plan is
/// exhausted.
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
        let target = bytes
            .unwrap_or_else(|| shared.knobs.morsel_target(target_stage(shared)))
            .clamp(shared.cfg.morsel_min, shared.cfg.morsel_max);
        let rows = rows_for(split, target);
        let start = cursor.row_offset;
        let whole = !split.sub_splittable;
        let end = if whole {
            split.rows
        } else {
            (start + rows).min(split.rows)
        };
        let seq = cursor.next_seq;
        cursor.next_seq += 1;
        cursor.row_offset = end;
        if end >= split.rows {
            cursor.split_index += 1;
            cursor.row_offset = 0;
        }
        return Some((index, RowRange { start, end }, seq, whole));
    }
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

    loop {
        let mode = shared.source_mode.get();
        let mut worked = poll_inflight(&shared, &mut inflight, &mut replacing, &mut cx);
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
            worked |= issue_replacements(&shared, &mut inflight, &mut replacing);
            worked |= issue_reads(&shared, &mut inflight);
            close_when_exhausted(&shared, &inflight);
        }
        if !worked {
            parker.park_timeout(Duration::from_millis(1));
        }
    }
}

/// f.5: while there is room, a read at the cursor. `read_ahead = 0` keeps a floor of one read,
/// issued only when Q0 is empty.
fn issue_reads<'a>(shared: &'a Shared, inflight: &mut Vec<InFlight<'a>>) -> bool {
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
        let Some((index, rows, seq, whole)) = take_next_range(shared, None) else {
            break;
        };
        submit(shared, inflight, index, rows, seq, whole, ReadKind::Fresh);
        issued = true;
    }
    issued
}

/// f.5: service `placement.evicted(0)` by re-reading each entry and calling `replace`.
fn issue_replacements<'a>(
    shared: &'a Shared,
    inflight: &mut Vec<InFlight<'a>>,
    replacing: &mut HashSet<Seq>,
) -> bool {
    let mut issued = false;
    for (seq, origin) in shared.placement.evicted(0) {
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
        submit(
            shared,
            inflight,
            index,
            rows,
            seq,
            whole,
            ReadKind::Replacement,
        );
        issued = true;
    }
    issued
}

fn submit<'a>(
    shared: &'a Shared,
    inflight: &mut Vec<InFlight<'a>>,
    index: usize,
    rows: RowRange,
    seq: Seq,
    whole: bool,
    kind: ReadKind,
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
    });
}

/// Poll every read once; a resolved one is pushed, replaced or fails the run (f.5, h).
fn poll_inflight(
    shared: &Shared,
    inflight: &mut Vec<InFlight<'_>>,
    replacing: &mut HashSet<Seq>,
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
                        if let Err(e) = outcome
                            && !matches!(e, MorunaError::Cancelled)
                        {
                            crate::policy::terminate(shared, e);
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
        Ok(payload) => shared
            .placement
            .push(0, Morsel::new(seq, 0, payload, origin)),
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

/// f.13: the cursor a resumed run starts from, and the sequence counter that goes with it.
pub(crate) fn set_cursor(shared: &Shared, cursor: moruna_kernel::SourceCursor) {
    let mut slot = shared.cursor.lock().unwrap_or_else(|e| e.into_inner());
    slot.split_index = cursor.split_index;
    slot.row_offset = cursor.row_offset;
    slot.next_seq = cursor.next_seq;
}

/// The cursor as f.12 records it: the next range to issue, never the last completed one.
pub(crate) fn cursor(shared: &Shared) -> moruna_kernel::SourceCursor {
    let slot = shared.cursor.lock().unwrap_or_else(|e| e.into_inner());
    moruna_kernel::SourceCursor {
        split_index: slot.split_index,
        row_offset: slot.row_offset,
        next_seq: slot.next_seq,
    }
}
