//! A worker keeps one Python thread state for its life (05 f.12, 2026-11-14).
//!
//! `Python::attach` on a thread Python did not make goes through `PyGILState_Ensure`, which makes
//! a thread state when the thread has none, and `PyGILState_Release`, which deletes it when its
//! count falls back to zero. A worker attaches for each morsel and detaches after it, so every
//! morsel made a thread state and deleted it: in the free-threaded build that is the thread's
//! own allocator heaps, abandoned at each delete and started cold at the next, its free lists,
//! and every `threading.local` value a function kept, gone before its next batch. Holding one
//! count for the thread's life keeps the one thread state; the hold is given back when the
//! thread exits, which deletes it then.

use std::cell::RefCell;

use pyo3::ffi;

/// The hold: the count `PyGILState_Ensure` gave, and the thread state detached after it.
struct Held {
    gstate: ffi::PyGILState_STATE,
    saved: *mut ffi::PyThreadState,
}

impl Drop for Held {
    fn drop(&mut self) {
        // SAFETY: `saved` is this thread's own state, detached by `keep` and not attached since
        // (every attach after it is balanced by a detach before the thread can exit), so
        // restoring it attaches the thread, and releasing the count `keep` took brings it to
        // zero, which deletes the state. Nothing is done while the interpreter is going away:
        // a thread that attaches then is stopped where it stands.
        unsafe {
            if ffi::Py_IsInitialized() == 0 || ffi::Py_IsFinalizing() != 0 {
                return;
            }
            ffi::PyEval_RestoreThread(self.saved);
            ffi::PyGILState_Release(self.gstate);
        }
    }
}

thread_local! {
    static HELD: RefCell<Option<Held>> = const { RefCell::new(None) };
}

/// Keep this thread's Python thread state until the thread exits (see the module header). A
/// thread that already has one, a thread Python made or one attached now, is left as it is, so
/// this must be called detached, as a kernel's entry points are. Cheap after the first call.
pub(crate) fn keep() {
    let _ = HELD.try_with(|held| {
        if held.borrow().is_some() {
            return;
        }
        // SAFETY: the interpreter is up (checked), the thread has no thread state (checked), so
        // `PyGILState_Ensure` makes one and attaches it, and `PyEval_SaveThread` detaches the
        // thread again with the state kept for it; `Held::drop` undoes both.
        unsafe {
            if ffi::Py_IsInitialized() == 0 || !ffi::PyGILState_GetThisThreadState().is_null() {
                return;
            }
            let gstate = ffi::PyGILState_Ensure();
            let saved = ffi::PyEval_SaveThread();
            *held.borrow_mut() = Some(Held { gstate, saved });
        }
    });
}
