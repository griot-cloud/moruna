//! Single-owner ring lifecycle (06 l, KS1). The owner of every request includes `Done`,
//! which retains its arena buffer/view. Published pointers never outlive that owner.

#![forbid(unsafe_code)]

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::file_blocking::{Done, FileReq, Verb};

pub(crate) const BATCH: usize = 32;
pub(crate) const LINGER: Duration = Duration::from_micros(50);
/// Whole aligned chunks also keep direct IO legal, without truncating usize to u32.
pub(crate) const CHUNK: usize = 1 << 30;

/// A non-SQPOLL ring used by exactly one worker. `unconsumed` is the exact SQ length
/// after enter returns. Without SQPOLL no entry can be consumed outside `submit`.
pub(crate) trait Port: Send + 'static {
    fn push(&mut self, id: u64, request: &FileReq, transferred: usize) -> bool;
    fn submit(&mut self) -> io::Result<usize>;
    fn unconsumed(&mut self) -> usize;
    fn complete(&mut self) -> Option<(u64, i32)>;
}
#[derive(Clone)]
struct Failure {
    kind: io::ErrorKind,
    message: String,
}
impl Failure {
    fn from(error: io::Error) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
    fn error(&self) -> io::Error {
        io::Error::new(self.kind, self.message.clone())
    }
}
struct Request {
    req: FileReq,
    done: Done,
}
#[derive(Default)]
struct Queue {
    requests: VecDeque<Request>,
    stop: bool,
    draining: bool,
    failure: Option<Failure>,
}
/// The producer side holds no kernel-visible pointer after handing its request to the queue.
#[derive(Default)]
pub(crate) struct Shared {
    queue: Mutex<Queue>,
    work: Condvar,
    callback: Mutex<()>,
}
impl Shared {
    pub(crate) fn submit(&self, req: FileReq, done: Done) {
        if req.ptr.checked_add(req.len).is_none()
            || req
                .offset
                .checked_add(req.len as u64)
                .is_none_or(|end| end > i64::MAX as u64)
        {
            self.finish(
                done,
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "file request range overflow",
                )),
            );
            return;
        }
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(failure) = &queue.failure {
            let error = failure.error();
            drop(queue);
            self.finish(done, Err(error));
            return;
        }
        if queue.stop {
            drop(queue);
            self.finish(
                done,
                Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "io_uring engine is stopping",
                )),
            );
            return;
        }
        queue.requests.push_back(Request { req, done });
        self.work.notify_one();
    }
    pub(crate) fn stop(&self) {
        self.queue.lock().unwrap_or_else(|e| e.into_inner()).stop = true;
        self.work.notify_all();
    }
    pub(crate) fn terminal_failure(&self) -> Option<String> {
        // A closing receiver can wake during callback unwinding. Wait for the owner to
        // publish the precise panic diagnostic before observing its terminal state.
        let _callback = self.callback.lock().unwrap_or_else(|e| e.into_inner());
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failure
            .as_ref()
            .map(|e| e.message.clone())
    }
    fn finish(&self, done: Done, result: io::Result<usize>) {
        // Lock order is callback, then queue; no queue lock spans a callback. Product
        // Done callbacks only forward to runtime::once's oneshot, never call this engine.
        let _callback = self.callback.lock().unwrap_or_else(|e| e.into_inner());
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| done(result))).is_err() {
            self.fail(io::Error::other(
                "io_uring completion callback panicked before delivering its result",
            ));
        }
    }
    #[cfg(test)]
    fn wait_draining(&self) {
        let queue = self.queue.lock().unwrap();
        let (_queue, timeout) = self
            .work
            .wait_timeout_while(queue, Duration::from_secs(5), |q| !q.draining)
            .unwrap();
        assert!(
            !timeout.timed_out(),
            "controlled owner did not observe shutdown"
        );
    }
    fn fail(&self, error: io::Error) -> Failure {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if queue.failure.is_none() {
            tracing::error!(target: "reactor", error = %error, "io_uring submission stopped; retaining consumed requests until their original CQEs");
            queue.failure = Some(Failure::from(error));
        }
        queue.stop = true;
        match &queue.failure {
            Some(failure) => failure.clone(),
            None => Failure::from(io::Error::other("missing ring failure")),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    Queued,
    Published,
    Consumed,
}
struct Flight {
    req: FileReq,
    done: Done,
    transferred: usize,
    short_writes: u8,
    state: State,
}

/// Run until normal stop drains all operations, or a submission failure drains every
/// consumed operation and closes the ring before returning unconsumed published owners.
pub(crate) fn run<P: Port>(mut port: P, shared: Arc<Shared>) {
    let mut live: HashMap<u64, Flight> = HashMap::new();
    let mut ready = VecDeque::new();
    let mut published = VecDeque::new();
    let mut next = 1u64;
    let mut failure: Option<Failure> = None;
    loop {
        let (requests, stop) = {
            let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            if queue.stop && !queue.draining {
                queue.draining = true;
                shared.work.notify_all();
            }
            let take = queue.requests.len().min(BATCH);
            (queue.requests.drain(..take).collect::<Vec<_>>(), queue.stop)
        };
        for request in requests {
            if let Some(error) = &failure {
                shared.finish(request.done, Err(error.error()));
                continue;
            }
            let Some(after) = next.checked_add(1) else {
                let error = shared.fail(io::Error::other("io_uring request identity exhausted"));
                shared.finish(request.done, Err(error.error()));
                failure = Some(error);
                continue;
            };
            let id = next;
            next = after;
            live.insert(
                id,
                Flight {
                    req: request.req,
                    done: request.done,
                    transferred: 0,
                    short_writes: 0,
                    state: State::Queued,
                },
            );
            ready.push_back(id);
        }
        let mut progress = false;
        if failure.is_none() {
            for _ in 0..BATCH {
                let Some(id) = ready.pop_front() else { break };
                let Some(op) = live.get_mut(&id) else {
                    continue;
                };
                if !port.push(id, &op.req, op.transferred) {
                    ready.push_front(id);
                    break;
                }
                op.state = State::Published;
                published.push_back(id);
                progress = true;
            }
            if !published.is_empty() {
                let result = port.submit();
                let remaining = port.unconsumed();
                if remaining > published.len() {
                    // An internal port/kernel invariant failed. Treat every published ID
                    // conservatively as consumed; none can be freed on that diagnosis.
                    for id in published.drain(..) {
                        if let Some(op) = live.get_mut(&id) {
                            op.state = State::Consumed;
                        }
                    }
                    failure = Some(shared.fail(io::Error::other(
                        "io_uring SQ accounting inconsistent; original completions required",
                    )));
                } else {
                    let consumed = published.len() - remaining;
                    for id in published.drain(..consumed) {
                        if let Some(op) = live.get_mut(&id) {
                            op.state = State::Consumed;
                        }
                    }
                    match result {
                        Ok(0) if consumed == 0 && remaining > 0 => {
                            failure = Some(shared.fail(io::Error::other(
                                "io_uring made no submission progress with pending entries",
                            )));
                        }
                        Ok(_) => {}
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) => failure = Some(shared.fail(e)),
                    }
                }
            }
        }
        while let Some((id, result)) = port.complete() {
            progress = true;
            let Some(mut op) = live.remove(&id) else {
                failure = Some(shared.fail(io::Error::other("io_uring completed unknown request")));
                continue;
            };
            if op.state != State::Consumed {
                // This violates the port's ordering contract. We have its original CQE,
                // so this owner's release is safe, but the adapter must stop visibly.
                let error = shared.fail(io::Error::other("io_uring CQE before consumed SQ state"));
                shared.finish(op.done, Err(error.error()));
                failure = Some(error);
                continue;
            }
            if let Some(error) = &failure {
                shared.finish(op.done, Err(error.error()));
                continue;
            }
            match outcome(&mut op, result) {
                Outcome::Done(result) => shared.finish(op.done, result),
                Outcome::Again => {
                    op.state = State::Queued;
                    live.insert(id, op);
                    ready.push_back(id);
                }
            }
        }
        if failure.is_none() {
            failure = shared
                .queue
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .failure
                .clone();
        }
        if let Some(error) = &failure {
            for id in ready.drain(..) {
                if let Some(op) = live.remove(&id) {
                    shared.finish(op.done, Err(error.error()));
                }
            }
            if live.values().all(|op| op.state != State::Consumed) {
                // No SQPOLL and no other submitter: closing the ring discards only entries
                // that were never consumed. Close before releasing their pointer owners.
                drop(port);
                for (_, op) in live {
                    shared.finish(op.done, Err(error.error()));
                }
                let requests = {
                    let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
                    queue.requests.drain(..).collect::<Vec<_>>()
                };
                for request in requests {
                    shared.finish(request.done, Err(error.error()));
                }
                return;
            }
        } else if stop && live.is_empty() {
            let queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            if queue.requests.is_empty() {
                return;
            }
        }
        if !progress {
            let queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            let _waited = shared
                .work
                .wait_timeout(queue, LINGER)
                .unwrap_or_else(|e| e.into_inner());
        }
    }
}
enum Outcome {
    Done(io::Result<usize>),
    Again,
}
fn outcome(op: &mut Flight, result: i32) -> Outcome {
    if result < 0 {
        return Outcome::Done(Err(match result.checked_neg() {
            Some(errno) => io::Error::from_raw_os_error(errno),
            None => io::Error::other("io_uring returned an invalid negative completion"),
        }));
    }
    let n = result as usize;
    let asked = (op.req.len - op.transferred).min(CHUNK);
    if n > asked {
        return Outcome::Done(Err(io::Error::other(
            "io_uring completion past requested chunk",
        )));
    }
    op.transferred += n;
    if op.transferred == op.req.len {
        return Outcome::Done(Ok(op.transferred));
    }
    match op.req.verb {
        Verb::Read if n == 0 => Outcome::Done(Ok(op.transferred)),
        Verb::Read => Outcome::Again,
        Verb::Write => {
            if n < asked {
                op.short_writes += 1;
            }
            if op.short_writes > 3 {
                Outcome::Done(Err(io::Error::other(
                    "io_uring write stayed short after three retries",
                )))
            } else {
                Outcome::Again
            }
        }
    }
}

#[cfg(test)]
#[path = "file_lifecycle_tests.rs"]
mod tests;
