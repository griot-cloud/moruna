//! The sink drive (f.6), the commit watermark (f.11) and the heartbeat check when
//! checkpointing is off (f.14).
//!
//! The second of the two threads that may wait on a `Completion` (CT-I7, SC-I1). It pops the
//! last queue, keeps up to `sink.concurrency` writes in flight, moves the commit watermark as
//! the sink commits, and finishes the sink once the queue is closed and drained.
//!
//! The sink is held in an `RwLock` because `Sink::open`, `finish` and `resume` take `&mut self`
//! while `write`, `skip`, `committed_seq` and `checkpoint` take `&self`. That cell is not one of
//! preamble 4.2's ordered locks: it is an ownership cell, it is only ever acquired by a thread
//! that is about to call the sink, and it is never acquired while a placement lock is held, so
//! no cycle can form through it. The locks that are in the order, the stage table and the
//! placement queues, are never held across a call out of this component.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use amoru_kernel::{AmoruError, BoxFuture, Locality, Result, Seq, Sink, SinkSummary};
use amoru_sinks::SinkHandle;
use crossbeam::sync::{Parker, Unparker};

use crate::shared::{DRIVE_DRIVING, DRIVE_STOP, Exit, RunState, Shared};

struct DriveWake(Unparker);

impl std::task::Wake for DriveWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Why the write loop stopped.
enum Drained {
    /// The last queue is closed and empty and every write completed: finish the sink (f.6).
    Finished,
    /// The run is ending some other way; `finish` is not called (f.10).
    Stopped,
}

/// The drive thread (f.1): idle until `run`, then draining the last queue.
pub(crate) fn drive(shared: Arc<Shared>, parker: Parker) {
    let s: &Shared = &shared;
    let waker = Waker::from(Arc::new(DriveWake(parker.unparker().clone())));
    let mut cx = Context::from_waker(&waker);
    loop {
        let mode = s.sink_mode.get();
        if mode == DRIVE_STOP {
            break;
        }
        if mode != DRIVE_DRIVING {
            parker.park_timeout(Duration::from_millis(1));
            continue;
        }
        match write_loop(s, &parker, &mut cx) {
            Drained::Finished => {
                finish(s);
                break;
            }
            Drained::Stopped => {
                s.sink_mode.set(DRIVE_STOP);
                break;
            }
        }
    }
}

/// f.6's loop. The read guard on the sink lives for the whole loop, because every write future
/// borrows the sink; `finish` takes the write guard afterwards, once the guard is gone.
fn write_loop(shared: &Shared, parker: &Parker, cx: &mut Context<'_>) -> Drained {
    let guard = shared.sink.read().unwrap_or_else(|e| e.into_inner());
    let sink: &SinkHandle = &guard;
    let queue = shared.last_queue();
    let blocking = shared.checkpoint_enabled();
    let mut inflight: Vec<(Seq, BoxFuture<'_, Result<()>>)> = Vec::new();
    let mut last_check = Instant::now();
    let interval = Duration::from_millis(shared.cfg.heartbeat_interval_ms);

    loop {
        let mut worked = poll_writes(shared, sink, &mut inflight, cx);
        let ending =
            shared.has_exit() || shared.is_cancelled() || shared.stopping.load(Ordering::SeqCst);
        if ending {
            // f.10: in-flight writes complete, then the drive stops without finishing.
            if inflight.is_empty() {
                return Drained::Stopped;
            }
            parker.park_timeout(Duration::from_millis(1));
            continue;
        }
        // f.6 keeps up to `sink.concurrency` writes in flight. An `Ordered` handle needs one
        // qualification: a `write` it is holding out of order has not resolved (08 f.4, SI-I1),
        // so counting it against the concurrency limit would stop the drive from ever popping
        // the sequence the buffer is waiting for, and the run would wedge. For an ordered
        // handle the limit that applies is the buffer's own byte bound, which it reports
        // through `is_stalled`; reported to the PM as a finding on f.6.
        let ordered = sink.is_ordered();
        let room = inflight.len() < shared.cfg.sink_concurrency as usize
            || (ordered && !sink.is_stalled());
        if room {
            let popped = if blocking && inflight.is_empty() {
                shared
                    .placement
                    .pop_blocking(queue, shared.sink_spec, Locality::Any)
                    .map(|found| found.map(|(morsel, _)| morsel))
            } else {
                shared.placement.pop(queue, shared.sink_spec, Locality::Any)
            };
            match popped {
                Ok(Some(morsel)) => {
                    let future = sink.write(morsel.seq, morsel.payload);
                    shared.writes_in_flight.fetch_add(1, Ordering::SeqCst);
                    inflight.push((morsel.seq, future));
                    worked = true;
                }
                Ok(None) => {
                    if inflight.is_empty()
                        && shared.is_closed(queue)
                        && shared.queue_count(queue) == 0
                    {
                        return Drained::Finished;
                    }
                }
                Err(AmoruError::Cancelled) => return Drained::Stopped,
                Err(e) => {
                    crate::policy::terminate(shared, e);
                    return Drained::Stopped;
                }
            }
        }
        shared.advance_closes();
        // f.14: with checkpointing off, the heartbeat check runs here, once per bounded park.
        if !blocking && last_check.elapsed() >= interval {
            last_check = Instant::now();
            crate::heartbeat::check(shared);
        }
        if !worked {
            parker.park_timeout(if blocking {
                Duration::from_millis(1)
            } else {
                Duration::from_millis(1).min(interval)
            });
        }
    }
}

/// Poll every write once; each completion moves the watermark (f.11).
fn poll_writes(
    shared: &Shared,
    sink: &SinkHandle,
    inflight: &mut Vec<(Seq, BoxFuture<'_, Result<()>>)>,
    cx: &mut Context<'_>,
) -> bool {
    let mut progressed = false;
    let mut index = 0;
    while index < inflight.len() {
        let poll = inflight[index].1.as_mut().poll(cx);
        match poll {
            Poll::Pending => index += 1,
            Poll::Ready(result) => {
                drop(inflight.remove(index));
                shared.writes_in_flight.fetch_sub(1, Ordering::SeqCst);
                progressed = true;
                match result {
                    Ok(()) => advance(shared, sink),
                    Err(e) => {
                        // h: a sink write failure ends the run; `finish` is not called.
                        crate::policy::terminate(shared, e);
                    }
                }
            }
        }
    }
    progressed
}

/// f.11: after each completed write and after each `skip`, read the sink's watermark and tell
/// the placement engine when it moved. It never moves backwards.
fn advance(shared: &Shared, sink: &SinkHandle) {
    let Some(next) = sink.committed_seq() else {
        return;
    };
    let mut held = shared.committed.lock().unwrap_or_else(|e| e.into_inner());
    if held.is_some_and(|current| next <= current) {
        return;
    }
    *held = Some(next);
    drop(held);
    shared.placement.set_committed(next);
}

/// The same from a worker, which calls `Sink::skip` under the error policy (f.8, f.11).
pub(crate) fn update_watermark(shared: &Shared) {
    let guard = shared.sink.read().unwrap_or_else(|e| e.into_inner());
    advance(shared, &guard);
}

/// f.6, f.11: `finish` once the last queue is closed and drained, then the watermark moves to
/// the last sequence number the run issued and the run is complete.
fn finish(shared: &Shared) {
    // f.10: `finish` runs on completion only. The claim is what makes that true: a cancel or a
    // terminate racing this point wins or loses once, here, rather than after the sink has been
    // finished (SC-T11 asserts `finish_calls == 0` on a cancelled run).
    if !shared.claim_exit() {
        return;
    }
    shared.set_run_state(RunState::Finishing);
    let summary: Result<SinkSummary> = {
        let mut guard = shared.sink.write().unwrap_or_else(|e| e.into_inner());
        guard.finish()
    };
    match summary {
        Ok(summary) => {
            let next_seq = crate::source_drive::cursor(shared).next_seq;
            if next_seq > 0 {
                let last = next_seq - 1;
                let mut held = shared.committed.lock().unwrap_or_else(|e| e.into_inner());
                if held.is_none_or(|current| last > current) {
                    *held = Some(last);
                    drop(held);
                    shared.placement.set_committed(last);
                }
            }
            shared.publish_exit(Exit::Completed(summary));
        }
        // This thread already holds the exit claim, so `policy::terminate` would find it taken
        // and publish nothing: the run would wait for ever on a sink that has already failed.
        // The claim holder publishes its own exit, exactly as the `Ok` arm does.
        Err(e) => {
            shared.set_run_state(RunState::Terminating);
            shared.publish_exit(Exit::Terminated(e));
        }
    }
    shared.unpark_all();
}
