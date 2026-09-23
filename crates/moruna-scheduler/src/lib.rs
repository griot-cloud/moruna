//! Moruna component 10, the scheduler: workers, drives, the admission rule and the trace.
//!
//! Design: `architecture/sdd/10-scheduler.md`. The scheduler turns a linear chain of stages
//! into work for a pool of role-free threads. It owns the workers, the stage table, the
//! admission rule (f.3), the stateful instance pools (f.4), the source and sink drives (f.5,
//! f.6), completion detection (f.7), the error policy (f.8), the probe protocol (f.9), the
//! checkpoint thread (f.12), the resume path (f.13), the worker heartbeat (f.14) and the
//! emission of every trace record (f.2).
//!
//! It implements [`Knobs`], [`StatsSource`] and [`Prober`] (contracts d.11) so the controller,
//! the only writer of a knob (G-I5), can move its parameters, read its counters and run the
//! probe protocol without knowing its internals.
//!
//! Two properties hold by construction rather than by care. A worker only ever runs
//! `Kernel::apply`: it never issues an IO operation and never waits on a `Completion`, because
//! the only code that does either lives in the two drive threads (SC-I1, `source_drive` and
//! `sink_drive`). And every morsel leaves exactly one trace record per stage, because the only
//! two places that build one are `worker::build_record`, called once at the end of a task, and
//! the probe (SC-I4, G-I4).

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
// Every fallible function here returns the contract's `MorunaError` (contracts d.14), whose
// size is fixed by that crate and is above clippy's 128 byte threshold. This crate may not
// box it: the error type crosses every component boundary and is the contract's to change.
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use moruna_kernel::{
    Allocator, CancelToken, Kernel, MorunaError, NodeId, Placement, RecordHook, Result,
    ResumePoint, Sampler, TraceSink,
};

mod checkpoint;
mod cputime;
mod heartbeat;
mod instances;
mod knobs;
mod lifecycle;
mod pick;
mod pipeline;
mod policy;
mod probe;
mod shared;
mod sink_drive;
mod source_drive;
mod stats;
mod worker;

pub use pipeline::{Pipeline, SchedulerConfig};

use shared::Shared;

/// How a run ended (d.1).
#[derive(Debug)]
pub enum RunOutcome {
    /// The source was exhausted, every queue drained and the sink finished.
    Completed {
        /// What the sink wrote.
        sink: moruna_kernel::SinkSummary,
    },
    /// The runtime ended the run itself with a diagnostic (G-I8).
    ///
    /// `manifest` is the path of the last manifest written, when checkpointing was on, so the
    /// surface can tell the user the run is resumable. The facade maps this variant to an
    /// error with the partial report attached (12 f.1).
    Terminated {
        /// Why the run ended.
        diagnostic: MorunaError,
        /// The last manifest written, when checkpointing was on.
        manifest: Option<std::path::PathBuf>,
    },
    /// The surface cancelled the run (SIGINT, `KeyboardInterrupt`).
    Cancelled {
        /// The last manifest written, when checkpointing was on.
        manifest: Option<std::path::PathBuf>,
    },
}

/// The workers, the drives, the stage table and the admission rule (a).
pub struct Scheduler {
    shared: Arc<Shared>,
}

impl Scheduler {
    /// Validates the chain and the plan, opens the sink on a fresh run, spawns the workers
    /// parked (f.1). The drive threads exist but drive nothing until `run` (the source drive
    /// only services probe requests, f.9); the checkpoint thread is not started here.
    pub fn new(
        cfg: SchedulerConfig,
        pipeline: Pipeline,
        placement: Arc<dyn Placement>,
        alloc: Arc<dyn Allocator>,
        trace: Arc<dyn TraceSink>,
        sampler: Arc<dyn Sampler>,
    ) -> Result<Scheduler> {
        let shared = Shared::build(cfg, pipeline, placement, alloc, trace, sampler)?;
        shared::spawn_all(&shared)?;
        Ok(Scheduler { shared })
    }

    /// Eagerly runs `Kernel::init` for every instance up to `max_instances` of every stateful
    /// stage, on the worker that will own the instance (f.4). The facade calls it before the
    /// controller's `prepare`, so the baseline it samples includes every loaded model.
    pub fn init_instances(&self) -> Result<()> {
        instances::init_instances(&self.shared)
    }

    /// Resume, step one (f.13): refuses when `!checkpoint_enabled`, resumes the sink instead of
    /// opening it, sets the source cursor and sequence counter, restores `Checkpoint` instances
    /// and re-inits `Reinit` ones, sets the commit watermark. Called by the facade before the
    /// controller's `probe_missing`. Nothing is read or written.
    pub fn apply_resume_point(&self, point: ResumePoint) -> Result<()> {
        lifecycle::apply_resume_point(&self.shared, point)
    }

    /// Starts the drives and the checkpoint thread, enters `Running`, runs to completion,
    /// termination or cancellation. Blocks the caller. `Ok(RunOutcome)` for every run that
    /// entered `Running`; `Err` only for a failure before that.
    pub fn run(&self, cancel: CancelToken) -> Result<RunOutcome> {
        lifecycle::run(&self.shared, cancel, false)
    }

    /// Resume, step two (f.13): re-reads `to_recompute` through the source path, then continues
    /// as `run`.
    pub fn run_resumed(&self, cancel: CancelToken) -> Result<RunOutcome> {
        lifecycle::run(&self.shared, cancel, true)
    }

    /// Installed by the facade after the controller exists (contracts d.11 `RecordHook`);
    /// called on the recording thread after every `TraceSink::record`.
    pub fn set_record_hook(&self, hook: RecordHook) {
        self.shared.set_record_hook(hook);
    }

    /// Sets cancel, unparks and joins the workers, stops the drives and the checkpoint thread
    /// (f.10). Idempotent; also called from `Drop`.
    pub fn shutdown(&self) {
        lifecycle::shutdown(&self.shared);
    }

    /// The kernels of the chain, in stage order, for the facade's fingerprint list.
    pub fn kernels(&self) -> Vec<Arc<dyn Kernel>> {
        self.shared
            .stages
            .iter()
            .map(|s| s.kernel.clone())
            .collect()
    }

    /// The node this run reads on; `LOCAL_NODE` in v1 (d.1).
    pub fn node(&self) -> NodeId {
        self.shared.cfg.node
    }

    /// True when the run may write manifests: `checkpoint.enabled` and neither a non-resumable
    /// sink nor a non-repeatable source forced it off at `new` (f.1).
    pub fn checkpoint_enabled(&self) -> bool {
        self.shared.checkpoint_enabled()
    }

    /// The shared state, for the tests that assert on the stage table and the pick rule.
    #[cfg(test)]
    pub(crate) fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    /// The seam SC-T19 uses: make worker `w` leave its loop without reporting, so the heartbeat
    /// checker has a dead worker to find (f.14).
    #[cfg(test)]
    pub(crate) fn test_kill_worker(&self, w: u16) {
        let _ = self.shared.send_job(w, shared::WorkerJob::Die);
    }
}

impl moruna_kernel::Knobs for Scheduler {
    fn set(&self, knob: moruna_kernel::Knob) {
        knobs::set(&self.shared, knob);
    }

    fn snapshot(&self) -> moruna_kernel::KnobSnapshot {
        self.shared.knobs.snapshot()
    }

    fn terminate(&self, diagnostic: MorunaError) {
        policy::terminate(&self.shared, diagnostic);
    }
}

impl moruna_kernel::StatsSource for Scheduler {
    fn scheduler_stats(&self) -> moruna_kernel::SchedulerStats {
        stats::scheduler_stats(&self.shared)
    }
}

impl moruna_kernel::Prober for Scheduler {
    fn probe(
        &self,
        stage: moruna_kernel::StageId,
        bytes: u64,
    ) -> Result<moruna_kernel::ProbeResult> {
        probe::probe(&self.shared, stage, bytes)
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests;
