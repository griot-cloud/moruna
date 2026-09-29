//! CT-T16 completion_channel: `Completion::channel`; `resolve` on another thread wakes a
//! `wait`, an `.await` and a `then` callback, each exactly once; a `then` registered after
//! resolution runs at once; a dropped sender resolves with `Cancelled`. Proves d.9.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::thread;
use std::time::Duration;

use moruna_kernel::{Completion, MorunaError};

/// A waker that counts how many times the future asked to be woken.
struct CountingWaker(AtomicUsize);

impl Wake for CountingWaker {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn ct_t16_completion_channel() {
    // `resolve` on another thread wakes a blocking `wait`, exactly once.
    let (sender, completion) = Completion::<u64>::channel();
    let worker = thread::spawn(move || {
        thread::sleep(Duration::from_millis(10));
        sender.resolve(Ok(41));
    });
    assert_eq!(completion.wait().expect("the operation resolved"), 41);
    worker.join().expect("the resolving thread finished");

    // `resolve` wakes an `.await`: the future is polled by hand, so the waking is visible.
    let (sender, mut completion) = Completion::<u64>::channel();
    let waker = Arc::new(CountingWaker(AtomicUsize::new(0)));
    let context_waker = Waker::from(Arc::clone(&waker));
    let mut context = Context::from_waker(&context_waker);
    let pinned = std::pin::Pin::new(&mut completion);
    assert!(matches!(
        std::future::Future::poll(pinned, &mut context),
        Poll::Pending
    ));
    assert_eq!(waker.0.load(Ordering::SeqCst), 0);
    let worker = thread::spawn(move || sender.resolve(Ok(7)));
    worker.join().expect("the resolving thread finished");
    assert_eq!(
        waker.0.load(Ordering::SeqCst),
        1,
        "the waker ran exactly once"
    );
    let pinned = std::pin::Pin::new(&mut completion);
    match std::future::Future::poll(pinned, &mut context) {
        Poll::Ready(Ok(value)) => assert_eq!(value, 7),
        other => panic!("expected Ready(Ok(7)), got {}", ready_label(&other)),
    }

    // `resolve` runs a `then` callback, on the resolving thread, exactly once.
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(AtomicUsize::new(0));
    let (sender, completion) = Completion::<u64>::channel();
    let counter = Arc::clone(&calls);
    let value_seen = Arc::clone(&seen);
    completion.then(Box::new(move |result| {
        counter.fetch_add(1, Ordering::SeqCst);
        value_seen.store(
            result.expect("the operation resolved") as usize,
            Ordering::SeqCst,
        );
    }));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the callback waits for the resolution"
    );
    let worker = thread::spawn(move || sender.resolve(Ok(9)));
    worker.join().expect("the resolving thread finished");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(seen.load(Ordering::SeqCst), 9);

    // A `then` registered after the resolution runs at once.
    let (sender, completion) = Completion::<u64>::channel();
    sender.resolve(Ok(5));
    assert!(completion.is_ready());
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    completion.then(Box::new(move |result| {
        counter.fetch_add(1, Ordering::SeqCst);
        assert_eq!(result.expect("the operation resolved"), 5);
    }));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // A dropped sender resolves the completion with `Cancelled`, for a blocking waiter,
    let (sender, completion) = Completion::<u64>::channel();
    drop(sender);
    assert!(matches!(completion.wait(), Err(MorunaError::Cancelled)));
    // for a `then` callback,
    let (sender, completion) = Completion::<u64>::channel();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    completion.then(Box::new(move |result| {
        counter.fetch_add(1, Ordering::SeqCst);
        assert!(matches!(result, Err(MorunaError::Cancelled)));
    }));
    drop(sender);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // and for a poller.
    let (sender, mut completion) = Completion::<u64>::channel();
    let waker = Arc::new(CountingWaker(AtomicUsize::new(0)));
    let context_waker = Waker::from(Arc::clone(&waker));
    let mut context = Context::from_waker(&context_waker);
    let pinned = std::pin::Pin::new(&mut completion);
    assert!(matches!(
        std::future::Future::poll(pinned, &mut context),
        Poll::Pending
    ));
    drop(sender);
    let pinned = std::pin::Pin::new(&mut completion);
    match std::future::Future::poll(pinned, &mut context) {
        Poll::Ready(Err(MorunaError::Cancelled)) => {}
        other => panic!("expected Ready(Cancelled), got {}", ready_label(&other)),
    }

    // An already resolved completion, for a reactor that failed at submission.
    let ready = Completion::<u64>::resolved(Err(MorunaError::Unsupported("rdma")));
    assert!(matches!(
        ready.wait(),
        Err(MorunaError::Unsupported("rdma"))
    ));

    // A completion that is dropped before the sender resolves it costs nothing.
    let (sender, completion) = Completion::<u64>::channel();
    drop(completion);
    sender.resolve(Ok(1));
}

fn ready_label(poll: &Poll<moruna_kernel::Result<u64>>) -> String {
    match poll {
        Poll::Pending => "Pending".to_string(),
        Poll::Ready(Ok(v)) => format!("Ready(Ok({v}))"),
        Poll::Ready(Err(e)) => format!("Ready(Err({e}))"),
    }
}

/// A completion made with a `Dispatch` runs a callback registered after it resolved through that
/// dispatch, not on the registering thread; one made without keeps the rule above and runs it at
/// once. This is how the reactor keeps a late callback off a worker (RE-I2, RE-T14).
#[test]
fn ct_t16_a_late_callback_runs_through_the_dispatch() {
    use std::sync::Mutex;
    use std::sync::mpsc;

    type Jobs = Arc<Mutex<Vec<Box<dyn FnOnce() + Send>>>>;
    let queued: Jobs = Arc::new(Mutex::new(Vec::new()));
    let queue = Arc::clone(&queued);
    let dispatch: moruna_kernel::Dispatch = Arc::new(move |job| {
        queue.lock().unwrap_or_else(|e| e.into_inner()).push(job);
    });
    let (sender, completion) = Completion::<u64>::channel_on(dispatch);
    sender.resolve(Ok(7));
    let (tx, rx) = mpsc::channel();
    completion.then(Box::new(move |r| {
        tx.send((r.ok(), thread::current().id())).ok();
    }));
    assert!(
        rx.try_recv().is_err(),
        "the callback did not run on the registering thread"
    );
    let jobs: Vec<_> = std::mem::take(&mut *queued.lock().unwrap_or_else(|e| e.into_inner()));
    assert_eq!(jobs.len(), 1, "it went to the dispatch, once");
    let runner = thread::spawn(move || {
        for job in jobs {
            job();
        }
        thread::current().id()
    });
    let ran_on = runner.join().expect("the dispatch thread");
    assert_eq!(rx.recv().expect("the callback ran"), (Some(7), ran_on));

    // Without a dispatch: at once, here.
    let (sender, completion) = Completion::<u64>::channel();
    sender.resolve(Ok(8));
    let (tx, rx) = mpsc::channel();
    completion.then(Box::new(move |r| {
        tx.send(r.ok()).ok();
    }));
    assert_eq!(rx.try_recv().ok(), Some(Some(8)));
}
