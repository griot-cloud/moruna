//! The on-demand checkpoint (MH 4.3 `checkpoint`, 4.7).
//!
//! A host that is about to destroy the machine a run is in asks for a manifest now rather than
//! at the end of the checkpoint interval. `Runtime::run` blocks its caller, so the request
//! comes from another thread through a handle the caller made before the run and handed the
//! facade in `Components::checkpoint`. The facade attaches the run's scheduler to it once the
//! scheduler exists and detaches it when the run ends, however it ends; between those two a
//! call writes a manifest on the scheduler's checkpoint thread (SC f.12) and returns its path.
//!
//! This is the seam the host protocol's `checkpoint` message (F8.1, 13-host) calls. The handle
//! holds the scheduler weakly, so a handle that outlives its run keeps nothing alive.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use moruna_kernel::MorunaError;
use moruna_scheduler::Scheduler;

/// How long a host's `checkpoint` waits by default: two fsyncs and a rename, with room for a
/// slow disk. A host with a deadline of its own passes it to [`CheckpointHandle::checkpoint_within`].
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// A way to ask a run for a manifest now, from outside the thread that is running it.
#[derive(Clone, Default)]
pub struct CheckpointHandle {
    scheduler: Arc<Mutex<Option<Weak<Scheduler>>>>,
}

impl std::fmt::Debug for CheckpointHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CheckpointHandle")
            .field("attached", &self.is_attached())
            .finish()
    }
}

impl CheckpointHandle {
    /// A handle attached to nothing; give a clone to `Components::checkpoint`.
    pub fn new() -> CheckpointHandle {
        CheckpointHandle::default()
    }

    /// True while a run is attached, which is from the scheduler's construction to the end of
    /// the run.
    pub fn is_attached(&self) -> bool {
        self.current().is_some()
    }

    /// Write a manifest now and return its path, waiting up to [`DEFAULT_TIMEOUT`].
    pub fn checkpoint_now(&self) -> moruna_kernel::Result<PathBuf> {
        self.checkpoint_within(DEFAULT_TIMEOUT)
    }

    /// Write a manifest now and return its path, waiting up to `timeout`. `Resume` when no run
    /// is attached, when the run writes no manifests (no staging directory, a non-repeatable
    /// source or a sink that cannot say what it committed), or when none was written in time.
    pub fn checkpoint_within(&self, timeout: Duration) -> moruna_kernel::Result<PathBuf> {
        match self.current() {
            Some(scheduler) => scheduler.checkpoint_now(timeout),
            None => Err(MorunaError::Resume(
                "no run is attached to this checkpoint handle".into(),
            )),
        }
    }

    /// Called by the facade once the scheduler exists.
    pub(crate) fn attach(&self, scheduler: &Arc<Scheduler>) {
        *self.lock() = Some(Arc::downgrade(scheduler));
    }

    /// Called by the facade when the run ends, by any path.
    pub(crate) fn detach(&self) {
        *self.lock() = None;
    }

    fn current(&self) -> Option<Arc<Scheduler>> {
        self.lock().as_ref().and_then(Weak::upgrade)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Weak<Scheduler>>> {
        self.scheduler.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Detaches a handle when the lifecycle function returns, by any path.
pub(crate) struct DetachOnDrop(pub(crate) Option<CheckpointHandle>);

impl Drop for DetachOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = &self.0 {
            handle.detach();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handle_with_no_run_says_so() {
        let handle = CheckpointHandle::new();
        assert!(!handle.is_attached());
        assert!(format!("{handle:?}").contains("attached: false"));
        match handle.checkpoint_now() {
            Err(MorunaError::Resume(msg)) => assert!(msg.contains("no run"), "{msg}"),
            other => panic!("expected Resume, got {other:?}"),
        }
        let guard = DetachOnDrop(Some(handle.clone()));
        drop(guard);
        assert!(!handle.is_attached());
    }
}
