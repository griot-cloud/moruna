//! The run the scheduler drives and its validation (d.1, f.1): sources, kernel stages and sinks
//! joined by queues, the run's DAG (contracts d.16). A linear chain is [`Pipeline::chain`].

use std::sync::Arc;

use moruna_kernel::{
    ErrorPolicy, Kernel, MorunaError, NodeId, PayloadSpec, Producer, Result, Sink, Source,
    SourceSchema, Topology,
};
use moruna_sinks::SinkHandle;

/// The run: its sources, its kernel stages, its sinks and the queues between them (d.1).
pub struct Pipeline {
    /// Where the run's input comes from, source 0 first.
    pub sources: Vec<Arc<dyn Source>>,
    /// Stages 1..=n, in the order the topology numbers them; may be empty (h, zero kernels).
    pub kernels: Vec<Arc<dyn Kernel>>,
    /// Where the run's output goes, sink 0 first; `Plain` or `Ordered` (08 d.1).
    pub sinks: Vec<SinkHandle>,
    /// How they are joined: one queue per edge (contracts d.16).
    pub topology: Topology,
}

impl Pipeline {
    /// The linear chain: one source, `kernels` in stage order, one sink.
    pub fn chain(
        source: Arc<dyn Source>,
        kernels: Vec<Arc<dyn Kernel>>,
        sink: SinkHandle,
    ) -> Pipeline {
        let topology = Topology::chain(kernels.len() as u16);
        Pipeline {
            sources: vec![source],
            kernels,
            sinks: vec![sink],
            topology,
        }
    }
}

/// Everything the scheduler is configured with at `new` (d.1).
#[derive(Clone, Debug)]
pub struct SchedulerConfig {
    /// The thread pool size (`workers.max`).
    pub workers_max: u16,
    /// Workers allowed to take tasks at start (`workers.active`).
    pub workers_active: u16,
    /// Source splits in flight (`readahead.splits`), per source.
    pub read_ahead: u16,
    /// Sink writes in flight (`sink.concurrency`), per sink.
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

/// The schema each queue carries, by queue id, each checked with `PayloadSpec::check` against
/// its consumer as it goes (f.1, CT-I5): a source's queues carry its schema, a stage's what
/// `Kernel::output_schema_of` gives for its inputs' schemas in port order. The stages are taken
/// in their numbered order, which the topology guarantees is one in which every queue goes
/// forward, so each stage's inputs are known before it is.
pub fn validate_graph(pipeline: &Pipeline) -> Result<Vec<SourceSchema>> {
    let topology = &pipeline.topology;
    if pipeline.sources.len() != usize::from(topology.sources())
        || pipeline.kernels.len() != usize::from(topology.stages())
        || pipeline.sinks.len() != usize::from(topology.sinks())
    {
        return Err(MorunaError::Plan(format!(
            "the topology has {} sources, {} stages and {} sinks, and the run {}, {} and {}",
            topology.sources(),
            topology.stages(),
            topology.sinks(),
            pipeline.sources.len(),
            pipeline.kernels.len(),
            pipeline.sinks.len()
        )));
    }
    let mut schemas: Vec<Option<SourceSchema>> = vec![None; topology.links().len()];
    for (q, link) in topology.links().iter().enumerate() {
        if let Producer::Source(s) = link.from {
            schemas[q] = Some(pipeline.sources[usize::from(s)].schema());
        }
    }
    for (index, kernel) in pipeline.kernels.iter().enumerate() {
        let stage = index as u16 + 1;
        let spec: PayloadSpec = kernel.accepts();
        let inputs = topology.inputs(stage);
        if inputs.len() > usize::from(kernel.max_inputs()) {
            return Err(MorunaError::Plan(format!(
                "stage {stage} is given {} inputs and its kernel takes at most {}",
                inputs.len(),
                kernel.max_inputs()
            )));
        }
        let mut given = Vec::with_capacity(inputs.len());
        for (port, &q) in inputs.iter().enumerate() {
            let Some(schema) = schemas[usize::from(q)].clone() else {
                return Err(MorunaError::Plan(format!(
                    "stage {stage}'s port {port} is fed before its producer is known"
                )));
            };
            spec.check(&schema).map_err(|e| {
                MorunaError::Plan(if inputs.len() == 1 {
                    format!("stage {stage}: {e}")
                } else {
                    format!("stage {stage} port {port}: {e}")
                })
            })?;
            given.push(schema);
        }
        let out = kernel.output_schema_of(&given)?;
        for &q in topology.outputs(stage) {
            schemas[usize::from(q)] = Some(out.clone());
        }
    }
    let mut out = Vec::with_capacity(schemas.len());
    for (q, schema) in schemas.into_iter().enumerate() {
        match schema {
            Some(schema) => out.push(schema),
            None => {
                return Err(MorunaError::Plan(format!("queue {q} carries no schema")));
            }
        }
    }
    for (index, sink) in pipeline.sinks.iter().enumerate() {
        let Some(q) = topology.sink_input(index as u16) else {
            continue;
        };
        sink.accepts().check(&out[usize::from(q)]).map_err(|e| {
            MorunaError::Plan(if pipeline.sinks.len() == 1 {
                format!("sink: {e}")
            } else {
                format!("sink {index}: {e}")
            })
        })?;
    }
    Ok(out)
}
