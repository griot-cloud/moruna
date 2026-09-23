//! The chain the scheduler runs and its validation (d.1, f.1).

use std::sync::Arc;

use moruna_kernel::{
    MorunaError, ErrorPolicy, Kernel, NodeId, PayloadSpec, Result, Sink, Source, SourceSchema,
};
use moruna_sinks::SinkHandle;

/// The linear chain: one source, zero or more kernels, one sink (d.1).
pub struct Pipeline {
    /// Where the run's input comes from.
    pub source: Arc<dyn Source>,
    /// Stages 1..=n; may be empty (h, zero kernels).
    pub kernels: Vec<Arc<dyn Kernel>>,
    /// `Plain` or `Ordered` (08 d.1); the scheduler never names the inner type.
    pub sink: SinkHandle,
}

/// Everything the scheduler is configured with at `new` (d.1).
#[derive(Clone, Debug)]
pub struct SchedulerConfig {
    /// The thread pool size (`workers.max`).
    pub workers_max: u16,
    /// Workers allowed to take tasks at start (`workers.active`).
    pub workers_active: u16,
    /// Source splits in flight (`readahead.splits`).
    pub read_ahead: u16,
    /// Sink writes in flight (`sink.concurrency`).
    pub sink_concurrency: u16,
    /// What happens after a kernel error (`errors.policy`).
    pub error_policy: ErrorPolicy,
    /// Per stage, before the controller sets knobs (`morsel.probe_bytes`).
    pub initial_morsel_target: u64,
    /// Lower clamp for `MorselTarget` and for the source drive's row range (f.5, f.15).
    pub morsel_min: u64,
    /// Upper clamp for `MorselTarget` and for the source drive's row range (f.5, f.15).
    pub morsel_max: u64,
    /// `checkpoint.enabled` and the placement engine has a staging directory.
    pub checkpoint_enabled: bool,
    /// `checkpoint.interval_ms`.
    pub checkpoint_interval_ms: u64,
    /// `checkpoint.keep`: the run directory and the final manifest survive a completed run, so
    /// the scheduler writes a final manifest on completion as well (f.12).
    pub checkpoint_keep: bool,
    /// How often the heartbeat table is checked (f.14).
    pub heartbeat_interval_ms: u64,
    /// True when the facade will call `apply_resume_point`; `new` then skips `open`.
    pub resuming: bool,
    /// `LOCAL_NODE` in v1; passed to `Origin`.
    pub node: NodeId,
}

impl Default for SchedulerConfig {
    fn default() -> SchedulerConfig {
        SchedulerConfig {
            workers_max: 1,
            workers_active: 1,
            read_ahead: 2,
            sink_concurrency: 2,
            error_policy: ErrorPolicy::Terminate,
            initial_morsel_target: 16 * 1024 * 1024,
            morsel_min: 4 * 1024 * 1024,
            morsel_max: 512 * 1024 * 1024,
            checkpoint_enabled: false,
            checkpoint_interval_ms: 5000,
            checkpoint_keep: false,
            heartbeat_interval_ms: 1000,
            resuming: false,
            node: moruna_kernel::LOCAL_NODE,
        }
    }
}

impl SchedulerConfig {
    /// The configuration rows this component owns, checked once at `new` (i).
    pub(crate) fn validate(&self) -> Result<()> {
        if self.workers_max == 0 {
            return Err(MorunaError::Config {
                name: "workers.max",
                msg: "the pool must hold at least one worker".into(),
            });
        }
        if self.sink_concurrency == 0 {
            return Err(MorunaError::Config {
                name: "sink.concurrency",
                msg: "at least one write must be allowed in flight".into(),
            });
        }
        if self.morsel_min == 0 || self.morsel_max < self.morsel_min {
            return Err(MorunaError::Config {
                name: "morsel.min_bytes",
                msg: "the morsel range is empty".into(),
            });
        }
        if self.heartbeat_interval_ms == 0 {
            return Err(MorunaError::Config {
                name: "heartbeat_interval_ms",
                msg: "the heartbeat interval must be positive".into(),
            });
        }
        if self.checkpoint_interval_ms == 0 {
            return Err(MorunaError::Config {
                name: "checkpoint.interval_ms",
                msg: "the checkpoint interval must be positive".into(),
            });
        }
        Ok(())
    }
}

/// The schema each stage produces, stage 0 first, checked with `PayloadSpec::check` as it goes
/// (f.1, CT-I5). The last entry is what the sink receives.
pub(crate) fn validate_chain(
    source: &dyn Source,
    kernels: &[Arc<dyn Kernel>],
    sink: &SinkHandle,
) -> Result<Vec<SourceSchema>> {
    let mut schemas = Vec::with_capacity(kernels.len() + 1);
    let mut schema = source.schema();
    schemas.push(schema.clone());
    for (index, kernel) in kernels.iter().enumerate() {
        let spec: PayloadSpec = kernel.accepts();
        spec.check(&schema)
            .map_err(|e| MorunaError::Plan(format!("stage {}: {e}", index as u16 + 1)))?;
        schema = kernel.output_schema(&schema)?;
        schemas.push(schema.clone());
    }
    sink.accepts()
        .check(&schema)
        .map_err(|e| MorunaError::Plan(format!("sink: {e}")))?;
    Ok(schemas)
}
