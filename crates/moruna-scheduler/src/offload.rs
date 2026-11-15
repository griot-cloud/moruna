//! The offload queue (RB1, `architecture/worker-io.md`): the CPU half of a source's read (its
//! decode) or of a sink's write (its encode), run by whichever worker is free.
//!
//! A drive that has a decode or an encode to do pushes it here and keeps a future for it; a
//! worker takes it at the head of its loop under a slot claim, so offloaded work is bounded by
//! the CPU limit in force like any unit. A job no worker has taken is the drive's own to run: when
//! no worker will take work (the run is not live, is gated, or is stopping), the drive takes its
//! job back and runs it inline, so nothing is ever stranded in the queue.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use crossbeam::queue::SegQueue;
use moruna_kernel::{MorunaError, Result};

/// A unit of offloaded work, already bound to where its result goes.
pub(crate) type Job = Box<dyn FnOnce() + Send + 'static>;

/// A job, held so that exactly one of a worker and the drive that pushed it runs it.
struct Cell {
    job: Mutex<Option<Job>>,
}

impl Cell {
    fn take(&self) -> Option<Job> {
        self.job.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// The run's one offload queue.
#[derive(Default)]
pub(crate) struct Offload {
    queue: SegQueue<Arc<Cell>>,
    queued: AtomicUsize,
}

impl Offload {
    /// Jobs pushed and not yet taken by a worker. A job its drive took back still counts until
    /// a worker pops its empty cell, so this is an upper bound.
    pub(crate) fn queued(&self) -> usize {
        self.queued.load(Ordering::SeqCst)
    }

    /// One job a worker may run now, if there is one.
    pub(crate) fn take(&self) -> Option<Job> {
        while let Some(cell) = self.queue.pop() {
            self.queued.fetch_sub(1, Ordering::SeqCst);
            if let Some(job) = cell.take() {
                return Some(job);
            }
        }
        None
    }

    fn push(&self, cell: Arc<Cell>) {
        self.queued.fetch_add(1, Ordering::SeqCst);
        self.queue.push(cell);
    }
}

/// Where a job's result lands, and who to wake when it does.
struct Slot<T> {
    result: Mutex<Option<T>>,
    waker: Mutex<Option<Waker>>,
}

impl<T> Slot<T> {
    fn set(&self, value: T) {
        *self.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(value);
        if let Some(waker) = self.waker.lock().unwrap_or_else(|e| e.into_inner()).take() {
            waker.wake();
        }
    }

    fn take(&self) -> Option<T> {
        self.result.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// Run `work` on a free worker and resolve with its result; run it inline instead whenever
/// `inline()` says no worker will take it. A panic in `work` resolves as an error rather than
/// leaving the drive waiting.
pub(crate) fn on_worker<'a, R, I, W>(
    offload: &'a Offload,
    inline: I,
    work: W,
) -> impl Future<Output = Result<R>> + Send + 'a
where
    R: Send + 'static,
    I: Fn() -> bool + Send + 'a,
    W: FnOnce() -> Result<R> + Send + 'static,
{
    OnWorker::<R, I, W> {
        offload,
        inline,
        work: Some(work),
        pushed: None,
    }
}

/// A pushed job's cell (so the drive can take it back) and where its result lands.
type Pushed<R> = (Arc<Cell>, Arc<Slot<Result<R>>>);

struct OnWorker<'a, R, I, W> {
    offload: &'a Offload,
    inline: I,
    work: Option<W>,
    pushed: Option<Pushed<R>>,
}

fn guarded<R>(work: impl FnOnce() -> Result<R>) -> Result<R> {
    std::panic::catch_unwind(AssertUnwindSafe(work)).unwrap_or_else(|panic| {
        let msg = panic
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "a decode or encode panicked".to_string());
        Err(MorunaError::Plan(format!("offloaded work panicked: {msg}")))
    })
}

impl<R, I, W> Future for OnWorker<'_, R, I, W>
where
    R: Send + 'static,
    I: Fn() -> bool,
    W: FnOnce() -> Result<R> + Send + 'static,
{
    type Output = Result<R>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<R>> {
        // Nothing in the future is pinned structurally, so it is `Unpin` and moves freely.
        let this = self.get_mut();
        if let Some(work) = this.work.take() {
            if (this.inline)() {
                return Poll::Ready(guarded(work));
            }
            let slot: Arc<Slot<Result<R>>> = Arc::new(Slot {
                result: Mutex::new(None),
                waker: Mutex::new(Some(cx.waker().clone())),
            });
            let landing = Arc::clone(&slot);
            let job: Job = Box::new(move || landing.set(guarded(work)));
            let cell = Arc::new(Cell {
                job: Mutex::new(Some(job)),
            });
            this.offload.push(Arc::clone(&cell));
            this.pushed = Some((cell, slot));
        }
        let Some((cell, slot)) = this.pushed.as_ref() else {
            return Poll::Ready(Err(MorunaError::Plan(
                "an offloaded job was polled after it resolved".into(),
            )));
        };
        if let Some(result) = slot.take() {
            this.pushed = None;
            return Poll::Ready(result);
        }
        if (this.inline)()
            && let Some(job) = cell.take()
        {
            job();
        }
        *slot.waker.lock().unwrap_or_else(|e| e.into_inner()) = Some(cx.waker().clone());
        // A worker may have finished between the check above and the waker going in.
        match slot.take() {
            Some(result) => {
                this.pushed = None;
                Poll::Ready(result)
            }
            None => Poll::Pending,
        }
    }
}

impl<R, I, W> Unpin for OnWorker<'_, R, I, W> {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::task::Wake;

    struct Flag(AtomicBool);
    impl Wake for Flag {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn context(flag: &Arc<Flag>) -> Waker {
        Waker::from(Arc::clone(flag))
    }

    #[test]
    fn a_worker_runs_the_job_and_wakes_the_drive() {
        let offload = Offload::default();
        let flag = Arc::new(Flag(AtomicBool::new(false)));
        let waker = context(&flag);
        let mut cx = Context::from_waker(&waker);
        let mut future = Box::pin(on_worker(&offload, || false, || Ok(41 + 1)));
        assert!(future.as_mut().poll(&mut cx).is_pending());
        assert_eq!(offload.queued(), 1);
        let job = offload.take().expect("the job is queued");
        assert_eq!(offload.queued(), 0);
        job();
        assert!(flag.0.load(Ordering::SeqCst), "the worker woke the drive");
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(Ok(value)) => assert_eq!(value, 42),
            other => panic!("expected the worker's result, got {other:?}"),
        }
    }

    #[test]
    fn with_no_worker_the_drive_runs_it_inline() {
        let offload = Offload::default();
        let flag = Arc::new(Flag(AtomicBool::new(false)));
        let waker = context(&flag);
        let mut cx = Context::from_waker(&waker);
        let mut future = Box::pin(on_worker(&offload, || true, || Ok("inline")));
        assert!(matches!(
            future.as_mut().poll(&mut cx),
            Poll::Ready(Ok("inline"))
        ));
        assert_eq!(offload.queued(), 0, "nothing was pushed");
    }

    #[test]
    fn a_job_no_worker_took_is_taken_back_when_workers_stop() {
        let offload = Offload::default();
        let stopping = Arc::new(AtomicBool::new(false));
        let flag = Arc::new(Flag(AtomicBool::new(false)));
        let waker = context(&flag);
        let mut cx = Context::from_waker(&waker);
        let watch = Arc::clone(&stopping);
        let mut future = Box::pin(on_worker(
            &offload,
            move || watch.load(Ordering::SeqCst),
            || Ok(7),
        ));
        assert!(future.as_mut().poll(&mut cx).is_pending());
        stopping.store(true, Ordering::SeqCst);
        assert!(matches!(future.as_mut().poll(&mut cx), Poll::Ready(Ok(7))));
        assert!(
            offload.take().is_none(),
            "the cell was emptied by the drive"
        );
    }

    #[test]
    fn a_panic_resolves_as_an_error() {
        let offload = Offload::default();
        let flag = Arc::new(Flag(AtomicBool::new(false)));
        let waker = context(&flag);
        let mut cx = Context::from_waker(&waker);
        let mut future = Box::pin(on_worker(
            &offload,
            || false,
            || -> Result<u8> { panic!("boom") },
        ));
        assert!(future.as_mut().poll(&mut cx).is_pending());
        offload.take().expect("queued")();
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(Err(e)) => assert!(e.to_string().contains("boom"), "{e}"),
            other => panic!("expected an error, got {other:?}"),
        }
    }
}
