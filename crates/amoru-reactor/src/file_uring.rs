//! File IO through io_uring (06 f.1): a submission thread that batches, and a completion
//! thread that reaps.
//!
//! The ring is this crate's, not `tokio-uring`'s, because the same tokio runtime has to serve
//! object storage as well (the decision in the SDD's header) and `tokio-uring` wants a runtime
//! of its own. Two threads own the two halves of one ring: the submission thread drains the
//! request queue and pushes SQEs, submitting every 32 entries or every 50 microseconds,
//! whichever comes first; the completion thread reaps CQEs and resolves the operations. A
//! short read or a short write resubmits the remainder from the completion thread, so the
//! loop of f.1 and f.2 is the same loop in both engines.
//!
//! `unsafe` is permitted here (section l): pushing an SQE is unsafe because the kernel will
//! read or write the request's buffer after the push, which is exactly what RE-I1 guarantees
//! (the reactor holds the `Buffer` or the `BufferView`, through `FileReq`, until the
//! completion resolves), and the two `*_shared` accessors are unsafe because they hand out the
//! ring's halves without a borrow, which is sound here because exactly one thread touches each
//! half.
//!
//! This module compiles and runs only on Linux with the `uring` feature. It is checked for the
//! Linux target and exercised in the weekly container job; the developer host this component
//! was written on is macOS arm64, where io_uring does not exist.

#![cfg(all(target_os = "linux", feature = "uring"))]

use std::collections::HashMap;
use std::io;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use io_uring::{IoUring, opcode, types};

use crate::file_blocking::{Done, FileEngine, FileReq, Verb};

/// How many entries the submission thread gathers before it submits (f.1).
const BATCH: usize = 32;
/// How long it waits for a fuller batch (f.1).
const LINGER: Duration = Duration::from_micros(50);

/// One operation the ring is carrying, keyed by the SQE's user data.
struct InFlight {
    req: FileReq,
    done: Done,
    /// Bytes already transferred by earlier partial completions of this operation.
    transferred: usize,
    /// Short writes so far; the fourth is an error (f.1).
    short_writes: u8,
}

#[derive(Default)]
struct Queue {
    pending: Vec<(u64, FileReq)>,
    stop: bool,
}

/// The io_uring path of e.2.
pub(crate) struct UringEngine {
    inner: Arc<Inner>,
    submitter: Option<std::thread::JoinHandle<()>>,
    reaper: Option<std::thread::JoinHandle<()>>,
}

struct Inner {
    ring: IoUring,
    queue: Mutex<Queue>,
    work: Condvar,
    in_flight: Mutex<HashMap<u64, InFlight>>,
    next_id: AtomicU64,
    stopping: AtomicBool,
}

// SAFETY: `IoUring` is `Send`; the two halves this module reaches through `submission_shared`
// and `completion_shared` are each touched by exactly one thread (the submission thread and
// the completion thread respectively), and every other field is guarded by its own lock.
unsafe impl Sync for Inner {}

impl UringEngine {
    /// Build the ring and start its two threads. `Err` means the host refused
    /// `io_uring_setup`, which the caller turns into a fallback or a `Config` error per RE-I3.
    pub(crate) fn new(depth: u32) -> io::Result<UringEngine> {
        let ring = IoUring::new(depth.max(4))?;
        let inner = Arc::new(Inner {
            ring,
            queue: Mutex::new(Queue::default()),
            work: Condvar::new(),
            in_flight: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            stopping: AtomicBool::new(false),
        });
        let submitter = {
            let inner = Arc::clone(&inner);
            std::thread::Builder::new()
                .name("amoru-uring-submit".into())
                .spawn(move || submit_loop(&inner))?
        };
        let reaper = {
            let inner = Arc::clone(&inner);
            std::thread::Builder::new()
                .name("amoru-uring-reap".into())
                .spawn(move || reap_loop(&inner))?
        };
        Ok(UringEngine {
            inner,
            submitter: Some(submitter),
            reaper: Some(reaper),
        })
    }
}

impl FileEngine for UringEngine {
    fn submit(&self, req: FileReq, done: Done) {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        {
            let mut in_flight = self
                .inner
                .in_flight
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            in_flight.insert(
                id,
                InFlight {
                    req: FileReq {
                        fd: Arc::clone(&req.fd),
                        verb: req.verb,
                        ptr: req.ptr,
                        len: req.len,
                        offset: req.offset,
                    },
                    done,
                    transferred: 0,
                    short_writes: 0,
                },
            );
        }
        let mut queue = self.inner.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.pending.push((id, req));
        self.inner.work.notify_one();
    }

    fn name(&self) -> &'static str {
        "uring"
    }
}

impl Drop for UringEngine {
    fn drop(&mut self) {
        self.inner.stopping.store(true, Ordering::SeqCst);
        {
            let mut queue = self.inner.queue.lock().unwrap_or_else(|e| e.into_inner());
            queue.stop = true;
        }
        self.inner.work.notify_all();
        if let Some(t) = self.submitter.take() {
            let _ = t.join();
        }
        if let Some(t) = self.reaper.take() {
            let _ = t.join();
        }
        // Whatever the ring never completed resolves `Cancelled` when its `InFlight` drops,
        // because dropping a `CompletionSender` resolves it (contracts d.9, RE-I7).
        let mut in_flight = self
            .inner
            .in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        in_flight.clear();
    }
}

/// Build the SQE for one request, starting at `transferred` bytes in.
fn entry(id: u64, req: &FileReq, transferred: usize) -> io_uring::squeue::Entry {
    let fd = types::Fd(req.fd.as_raw_fd());
    let ptr = (req.ptr + transferred) as *mut u8;
    let len = (req.len - transferred) as u32;
    let offset = req.offset + transferred as u64;
    match req.verb {
        Verb::Read => opcode::Read::new(fd, ptr, len)
            .offset(offset)
            .build()
            .user_data(id),
        Verb::Write => opcode::Write::new(fd, ptr.cast_const(), len)
            .offset(offset)
            .build()
            .user_data(id),
    }
}

/// Push one entry, waiting for room rather than involving the caller (h, `queued_max`).
fn push(inner: &Inner, sqe: &io_uring::squeue::Entry) {
    loop {
        // SAFETY: only this thread touches the submission queue, and the buffer the entry
        // points at is kept alive by the `FileReq` in `in_flight` until the operation resolves
        // (RE-I1).
        let pushed = unsafe { inner.ring.submission_shared().push(sqe) };
        if pushed.is_ok() {
            return;
        }
        let _ = inner.ring.submit();
        std::thread::yield_now();
    }
}

fn submit_loop(inner: &Inner) {
    loop {
        let batch = {
            let mut queue = inner.queue.lock().unwrap_or_else(|e| e.into_inner());
            while queue.pending.is_empty() && !queue.stop {
                let (guard, _) = inner
                    .work
                    .wait_timeout(queue, LINGER)
                    .unwrap_or_else(|e| e.into_inner());
                queue = guard;
                if !queue.pending.is_empty() || queue.stop {
                    break;
                }
            }
            if queue.pending.is_empty() && queue.stop {
                return;
            }
            let take = queue.pending.len().min(BATCH);
            queue.pending.drain(..take).collect::<Vec<_>>()
        };
        for (id, req) in &batch {
            push(inner, &entry(*id, req, 0));
        }
        if !batch.is_empty() {
            let _ = inner.ring.submit();
        }
    }
}

fn reap_loop(inner: &Inner) {
    while !inner.stopping.load(Ordering::SeqCst) {
        // SAFETY: only this thread touches the completion queue.
        let cqe = unsafe { inner.ring.completion_shared().next() };
        let Some(cqe) = cqe else {
            std::thread::sleep(LINGER);
            continue;
        };
        let id = cqe.user_data();
        let result = cqe.result();
        let Some(mut op) = inner
            .in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)
        else {
            continue;
        };
        match outcome(&mut op, result) {
            Outcome::Done(r) => (op.done)(r),
            Outcome::Again => {
                let sqe = entry(id, &op.req, op.transferred);
                inner
                    .in_flight
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id, op);
                push(inner, &sqe);
                let _ = inner.ring.submit();
            }
        }
    }
}

enum Outcome {
    Done(io::Result<usize>),
    Again,
}

/// The loop of f.1, as a pure function of one completion result so it can be tested without a
/// ring: continue a partial transfer, stop a read at end of file, fail a write that stays
/// short after three retries.
fn outcome(op: &mut InFlight, result: i32) -> Outcome {
    if result < 0 {
        return Outcome::Done(Err(io::Error::from_raw_os_error(-result)));
    }
    let n = result as usize;
    op.transferred += n;
    if op.transferred >= op.req.len {
        return Outcome::Done(Ok(op.transferred));
    }
    match op.req.verb {
        Verb::Read => {
            if n == 0 {
                Outcome::Done(Ok(op.transferred))
            } else {
                Outcome::Again
            }
        }
        Verb::Write => {
            op.short_writes += 1;
            if op.short_writes > 3 {
                Outcome::Done(Err(io::Error::other(format!(
                    "short write: {} of {} bytes after 3 retries",
                    op.transferred, op.req.len
                ))))
            } else {
                Outcome::Again
            }
        }
    }
}
