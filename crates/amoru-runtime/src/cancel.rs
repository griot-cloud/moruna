//! Cancellation and the reverse shutdown (preamble 4.3, 12 f.1).
//!
//! "Shutdown is the reverse of what was started, always executed, including on error"
//! (PY-I1). The facade records each component as it starts it and unwinds the record in
//! reverse when the run ends or a step fails. Nothing here retries and nothing swallows: a
//! failure during shutdown becomes a note, and the first error is the one returned (12 l).

use std::sync::Arc;

use amoru_controller::Controller;
use amoru_kernel::{CancelToken, Placement, Reactor};
use amoru_scheduler::Scheduler;
use amoru_trace::{TraceView, TraceWriter};

/// A cancel token that is cancelled when this guard drops unless it is disarmed. The surface
/// holds the real token (12 f.5); this is what makes a panic between `start` and `run` stop
/// the threads the facade started rather than leave them running.
pub struct CancelOnDrop {
    token: CancelToken,
    armed: bool,
}

impl CancelOnDrop {
    /// Arm a guard over `token`.
    pub fn new(token: CancelToken) -> CancelOnDrop {
        CancelOnDrop { token, armed: true }
    }

    /// The token itself, for the call that is about to block on it.
    pub fn token(&self) -> CancelToken {
        self.token.clone()
    }

    /// Stop cancelling on drop: the run reached its own end.
    pub fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.token.cancel();
        }
    }
}

/// What the facade has started so far, in start order.
#[derive(Default)]
pub struct Started {
    /// The trace writer; `finish` produces the view the report is computed from.
    pub trace: Option<Arc<TraceWriter>>,
    /// The reactor.
    pub reactor: Option<Arc<dyn Reactor>>,
    /// The placement engine.
    pub placement: Option<Arc<dyn Placement>>,
    /// The scheduler: workers, drives and the checkpoint thread.
    pub scheduler: Option<Arc<Scheduler>>,
    /// The controller: the tick thread.
    pub controller: Option<Arc<Controller>>,
}

impl Started {
    /// Stop everything that was started, in the reverse of 12 f.1's order: controller, then
    /// scheduler (which joins the workers and the drives), then placement, then the reactor,
    /// then the trace writer. The arena is released when the last `Arc` to it drops, which is
    /// after this returns.
    ///
    /// Returns the view `TraceWriter::finish` produced, when a writer was started and
    /// finished cleanly, and the notes collected on the way.
    pub fn unwind(&mut self) -> (Option<TraceView>, Vec<String>) {
        let mut notes = Vec::new();
        if let Some(controller) = self.controller.take() {
            let summary = controller.stop();
            notes.extend(summary.notes);
        }
        if let Some(scheduler) = self.scheduler.take() {
            scheduler.shutdown();
        }
        if let Some(placement) = self.placement.take() {
            placement.shutdown();
        }
        if let Some(reactor) = self.reactor.take() {
            reactor.shutdown();
        }
        let view = match self.trace.take() {
            Some(trace) => match trace.finish() {
                Ok(view) => Some(view),
                Err(error) => {
                    notes.push(format!("the trace writer did not finish cleanly: {error}"));
                    None
                }
            },
            None => None,
        };
        (view, notes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A guard cancels its token when it drops, and does not once it is disarmed.
    #[test]
    fn the_guard_cancels_unless_disarmed() {
        let token = CancelToken::new();
        {
            let guard = CancelOnDrop::new(token.clone());
            assert!(!guard.token().is_cancelled());
        }
        assert!(token.is_cancelled(), "a dropped guard cancels");

        let second = CancelToken::new();
        {
            let mut guard = CancelOnDrop::new(second.clone());
            guard.disarm();
        }
        assert!(!second.is_cancelled(), "a disarmed guard does not");
    }

    /// Unwinding nothing produces no view and no notes, and is safe to repeat.
    #[test]
    fn unwinding_nothing_is_nothing() {
        let mut started = Started::default();
        let (view, notes) = started.unwind();
        assert!(view.is_none());
        assert!(notes.is_empty());
        let (view, notes) = started.unwind();
        assert!(view.is_none());
        assert!(notes.is_empty());
    }
}
