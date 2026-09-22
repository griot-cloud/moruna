//! `Completion`, the handle for a reactor operation (contracts d.9), over `std` only: a
//! `Mutex` plus a `Condvar` slot with a stored waker, so the reactor, the fakes and this crate
//! agree on one type and no crate needs an async runtime to name it.

use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Waker};

use crate::error::AmoruError;

type Callback<T> = Box<dyn FnOnce(crate::Result<T>) + Send + 'static>;

struct Slot<T> {
    value: Option<crate::Result<T>>,
    waker: Option<Waker>,
    callback: Option<Callback<T>>,
    /// Set when the receiving half is gone, so `resolve` drops the value instead of storing it.
    receiver_gone: bool,
}

struct Shared<T> {
    slot: Mutex<Slot<T>>,
    ready: Condvar,
}

/// Resolves exactly once. `Completion<Buffer>` returns the same buffer the operation was given.
pub struct Completion<T> {
    shared: Arc<Shared<T>>,
}

/// The sending half of a `Completion`: the reactor (or a fake) keeps it and resolves it once.
/// Dropping it unresolved resolves the completion with `AmoruError::Cancelled`.
pub struct CompletionSender<T> {
    shared: Arc<Shared<T>>,
    resolved: bool,
}

impl<T: Send + 'static> Completion<T> {
    /// A linked pair. The reactor (or a fake) keeps the sender and resolves it once.
    pub fn channel() -> (CompletionSender<T>, Completion<T>) {
        let shared = Arc::new(Shared {
            slot: Mutex::new(Slot {
                value: None,
                waker: None,
                callback: None,
                receiver_gone: false,
            }),
            ready: Condvar::new(),
        });
        (
            CompletionSender {
                shared: Arc::clone(&shared),
                resolved: false,
            },
            Completion { shared },
        )
    }

    /// A completion that is already resolved; for a fake or an operation that failed at
    /// submission.
    pub fn resolved(value: crate::Result<T>) -> Completion<T> {
        let (sender, completion) = Completion::channel();
        sender.resolve(value);
        completion
    }

    /// Blocking wait; only the scheduler's source and sink drives may call it (CT-I7, RE-I2).
    pub fn wait(self) -> crate::Result<T> {
        let mut slot = self.shared.slot.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(value) = slot.value.take() {
                return value;
            }
            slot = self
                .shared
                .ready
                .wait(slot)
                .unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Run `f` on the thread that resolves the completion, at resolution (or at once if
    /// already resolved). This is how the placement engine observes move completions
    /// without a thread of its own (placement g); `f` must be short and must not block.
    pub fn then(self, f: Callback<T>) {
        let mut pending = Some(f);
        let ready = {
            let mut slot = self.shared.slot.lock().unwrap_or_else(|e| e.into_inner());
            match slot.value.take() {
                Some(value) => Some(value),
                None => {
                    slot.callback = pending.take();
                    None
                }
            }
        };
        // The callback runs outside the lock, because it may take locks of its own
        // (placement g).
        if let (Some(value), Some(f)) = (ready, pending.take()) {
            f(value);
        }
    }

    /// True when the operation has already resolved (the value is still there to take).
    pub fn is_ready(&self) -> bool {
        let slot = self.shared.slot.lock().unwrap_or_else(|e| e.into_inner());
        slot.value.is_some()
    }
}

impl<T> Drop for Completion<T> {
    fn drop(&mut self) {
        let mut slot = self.shared.slot.lock().unwrap_or_else(|e| e.into_inner());
        // A completion dropped with a `then` callback registered keeps that callback: `then`
        // consumes the handle, and the callback is the caller's way of observing the result.
        if slot.callback.is_none() {
            slot.receiver_gone = true;
            slot.value = None;
        }
        slot.waker = None;
    }
}

impl<T> core::future::Future for Completion<T> {
    type Output = crate::Result<T>;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut slot = self.shared.slot.lock().unwrap_or_else(|e| e.into_inner());
        match slot.value.take() {
            Some(value) => Poll::Ready(value),
            None => {
                slot.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

impl<T> CompletionSender<T> {
    /// Resolve the completion. Exactly one `resolve` per completion takes effect; a `then`
    /// callback registered earlier runs on this thread, and a `wait` or `.await` is woken.
    pub fn resolve(mut self, r: crate::Result<T>) {
        self.resolve_inner(r);
    }

    fn resolve_inner(&mut self, r: crate::Result<T>) {
        if self.resolved {
            return;
        }
        self.resolved = true;
        let (callback, waker) = {
            let mut slot = self.shared.slot.lock().unwrap_or_else(|e| e.into_inner());
            if slot.receiver_gone {
                (None, None)
            } else {
                match slot.callback.take() {
                    Some(cb) => (Some((cb, r)), None),
                    None => {
                        slot.value = Some(r);
                        (None, slot.waker.take())
                    }
                }
            }
        };
        self.shared.ready.notify_all();
        if let Some(waker) = waker {
            waker.wake();
        }
        // The callback runs outside the lock (placement g: it may take placement locks).
        if let Some((cb, value)) = callback {
            cb(value);
        }
    }
}

impl<T> Drop for CompletionSender<T> {
    fn drop(&mut self) {
        // A sender dropped without resolving cancels the operation rather than leaving a
        // completion that never resolves (CT-T16).
        self.resolve_inner(Err(AmoruError::Cancelled));
    }
}
