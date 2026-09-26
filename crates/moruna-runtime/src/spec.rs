//! What a run is asked for, and what a caller may hand the facade already built (12 d.1).

use std::path::PathBuf;
use std::sync::Arc;

use moruna_discovery::Discovered;
use moruna_kernel::{
    Allocator, CancelToken, ErrorPolicy, HostProfile, Kernel, ObjectMetadata, Placement, Reactor,
    RunId, Sampler, Sink, SizerKind, Source, TraceSink, TraceTail,
};
use moruna_reactor::ObjectStoreConfig;

/// What a source or sink builder is given at the "sources and sinks built" step of the run
/// lifecycle (preamble 4.4, PY-I1).
///
/// The arena and the reactor exist by then and do not exist before it, which is why a source
/// that needs either cannot be constructed by the caller ahead of `run` (see `SourceSpec`).
pub struct BuildCtx {
    /// What discovery found for this run.
    pub discovered: Discovered,
    /// The arena, as the contract sees it.
    pub alloc: Arc<dyn Allocator>,
    /// The reactor, as the contract sees it.
    pub reactor: Arc<dyn Reactor>,
    /// The same reactor as object metadata, when one was available. The facade's own reactor
    /// always supplies it; an injected `Components::reactor` supplies it only when the caller
    /// also injected `Components::object_metadata`.
    pub object_metadata: Option<Arc<dyn ObjectMetadata>>,
    /// The run's identity, minted or read from a manifest.
    pub run_id: RunId,
}

impl BuildCtx {
    /// The object metadata handle, or a `Config` error naming what is missing. A source that
    /// reads object URLs needs it; one that reads local files does not (07 d.1).
    pub fn object_metadata(&self) -> moruna_kernel::Result<Arc<dyn ObjectMetadata>> {
        match &self.object_metadata {
            Some(meta) => Ok(meta.clone()),
            None => Err(moruna_kernel::MorunaError::Config {
                name: "object_metadata",
                msg: "the injected reactor supplies no object metadata".into(),
            }),
        }
    }
}

/// A source builder, run at the lifecycle's "sources built" step.
pub type SourceFactory =
    Box<dyn FnOnce(&BuildCtx) -> moruna_kernel::Result<Arc<dyn Source>> + Send>;
/// A sink builder, run at the lifecycle's "sinks built" step.
pub type SinkFactory = Box<dyn FnOnce(&BuildCtx) -> moruna_kernel::Result<Box<dyn Sink>> + Send>;

/// Where a run's input comes from.
///
/// Preamble 4.4 puts "sources and sinks built" after the reactor, and every file-backed source
/// takes the reactor at construction (07 d.1), so a caller cannot build one first: `Build` is
/// the lifecycle's shape. `Built` carries a source that needs neither arena nor reactor.
pub enum SourceSpec {
    /// Already built by the caller.
    Built(Arc<dyn Source>),
    /// Built by the facade once the arena and the reactor exist.
    Build(SourceFactory),
}

impl SourceSpec {
    /// Run the builder, or hand back what was already built.
    pub fn build(self, ctx: &BuildCtx) -> moruna_kernel::Result<Arc<dyn Source>> {
        match self {
            SourceSpec::Built(source) => Ok(source),
            SourceSpec::Build(factory) => factory(ctx),
        }
    }
}

impl From<Arc<dyn Source>> for SourceSpec {
    fn from(source: Arc<dyn Source>) -> SourceSpec {
        SourceSpec::Built(source)
    }
}

/// Where a run's output goes; the same two shapes as `SourceSpec`, for the same reason.
pub enum SinkSpec {
    /// Already built by the caller.
    Built(Box<dyn Sink>),
    /// Built by the facade once the arena and the reactor exist.
    Build(SinkFactory),
}

impl SinkSpec {
    /// Run the builder, or hand back what was already built.
    pub fn build(self, ctx: &BuildCtx) -> moruna_kernel::Result<Box<dyn Sink>> {
        match self {
            SinkSpec::Built(sink) => Ok(sink),
            SinkSpec::Build(factory) => factory(ctx),
        }
    }
}

impl From<Box<dyn Sink>> for SinkSpec {
    fn from(sink: Box<dyn Sink>) -> SinkSpec {
        SinkSpec::Built(sink)
    }
}

/// One run, as the surface describes it (12 d.1).
pub struct RunSpec {
    /// The input.
    pub source: SourceSpec,
    /// Stages 1..=n; may be empty, in which case the pipeline is source to sink (12 h).
    pub kernels: Vec<Arc<dyn Kernel>>,
    /// The Python kernels among them, for `bind_allocator` and `gil_state` (05 d.1).
    #[cfg(feature = "python")]
    pub py_kernels: Vec<(moruna_kernel::StageId, Arc<moruna_adapters::PyKernel>)>,
    /// The output; wrapped into a `SinkHandle` by the facade.
    pub sink: SinkSpec,
    /// An explicit host ceiling in bytes; discovery clamps it.
    pub budget: Option<u64>,
    /// An explicit CPU quota in cores; discovery clamps it.
    pub cpu: Option<f64>,
    /// Where the trace file goes; `None` keeps the trace in memory.
    pub trace_path: Option<PathBuf>,
    /// Where staging segments go; `None` lets discovery resolve one.
    pub staging_dir: Option<PathBuf>,
    /// `budget.disk`.
    pub staging_limit: Option<u64>,
    /// `staging.durable` (MH 4.1, 4.7): the staging directory is on a disk that survives the
    /// machine, so the host profile is declared `durable_staging=present` and a manifest
    /// written there may be resumed on another machine (Q9). Discovery still refuses a tmpfs
    /// or overlay directory declared so (03 e.4). A `host_profile` that declares
    /// `durable_staging` otherwise is a `Config` error naming both.
    pub staging_durable: bool,
    /// What happens after a kernel error.
    pub error_policy: ErrorPolicy,
    /// Deliver morsels to the sink in sequence order.
    pub ordered: bool,
    /// Which decision function sizes morsels.
    pub sizer: SizerKind,
    /// The profile store; `None` disables it.
    pub profiles_dir: Option<PathBuf>,
    /// Credentials and endpoints for object URLs.
    pub object_store: ObjectStoreConfig,
    /// A host profile from the surface, which overrides the environment.
    pub host_profile: Option<HostProfile>,
    /// Run Python kernels serialised under a GIL interpreter rather than refusing.
    pub allow_gil: bool,
    /// Write the run manifest periodically.
    pub checkpoint: bool,
    /// The manifest cadence.
    pub checkpoint_interval_ms: u64,
    /// Keep the run directory and the final manifest after a completed run.
    pub checkpoint_keep: bool,
    /// `None`: a fresh run. `Some`: resume from this manifest.
    pub resume: Option<PathBuf>,
    /// `resume = "auto"` (MH 4.7): when `resume` is `None`, look under the staging directory
    /// for the newest `moruna-*/manifest.json` written by this same job, the same kernel
    /// fingerprints and, when the source is already built, the same plan digest, and resume
    /// it; with none, start fresh and say so in `notes`. This is what a host that restarts a
    /// destroyed machine with the same job sets.
    pub resume_auto: bool,
    /// Clamps and translations the surface reports (12 f.3).
    pub notes: Vec<String>,
    /// The job document's content address (MH 4.1), when the run was built from one. Recorded
    /// in the manifest as `spec.digest`; a resume whose document has another digest is refused.
    pub spec_digest: Option<String>,
}

impl RunSpec {
    /// A run over `source`, `kernels` and `sink` with every other field at the preamble's
    /// default (section 5). This is what `moruna.run(source, kernels, sink)` translates to
    /// before the surface applies its own arguments (PY-I3).
    pub fn new(
        source: impl Into<SourceSpec>,
        kernels: Vec<Arc<dyn Kernel>>,
        sink: impl Into<SinkSpec>,
    ) -> RunSpec {
        RunSpec {
            source: source.into(),
            kernels,
            #[cfg(feature = "python")]
            py_kernels: Vec::new(),
            sink: sink.into(),
            budget: None,
            cpu: None,
            trace_path: None,
            staging_dir: None,
            staging_limit: None,
            staging_durable: false,
            error_policy: ErrorPolicy::Terminate,
            ordered: false,
            sizer: SizerKind::Rule,
            // `None` is the preamble's default, which the facade resolves and creates
            // (12 f.1): one owner, and a caller that wants a different store says so.
            profiles_dir: None,
            object_store: ObjectStoreConfig::default(),
            host_profile: None,
            allow_gil: false,
            checkpoint: true,
            checkpoint_interval_ms: crate::config::CHECKPOINT_INTERVAL_MS,
            checkpoint_keep: false,
            resume: None,
            resume_auto: false,
            notes: Vec::new(),
            spec_digest: None,
        }
    }
}

/// One entry of the job document's `kernels[]` (MH 4.1), as far as this build knows it (MH 4.9).
///
/// The Rust variant only: the JSON field, its serde and the `kernels[].fingerprint` check at
/// load are F8.1's, which serialises this enum; `fingerprint` is the value a document pins.
#[derive(Clone, Debug)]
pub enum KernelEntry {
    /// `{ "kind": "std", "name": ..., "args": {...} }`: a standard kernel by arguments.
    Std(moruna_kernels::StdKernel),
}

impl KernelEntry {
    /// A standard kernel entry; an unknown name or a bad argument is a `Plan` error naming it.
    pub fn std(name: &str, args: serde_json::Value) -> moruna_kernel::Result<KernelEntry> {
        Ok(KernelEntry::Std(moruna_kernels::StdKernel::new(
            name, &args,
        )?))
    }

    /// The fingerprint a job document pins for this entry (MH 4.9).
    pub fn fingerprint(&self) -> moruna_kernel::Fingerprint {
        match self {
            KernelEntry::Std(kernel) => kernel.fingerprint(),
        }
    }
}

/// The kernels a list of entries runs as, in stage order, with adjacent standard kernels fused
/// where their combination is one stage (MH 4.9).
pub fn build_kernels(entries: Vec<KernelEntry>) -> Vec<Arc<dyn Kernel>> {
    let std: Vec<moruna_kernels::StdKernel> = entries
        .into_iter()
        .map(|entry| match entry {
            KernelEntry::Std(kernel) => kernel,
        })
        .collect();
    moruna_kernels::fuse_chain(std)
        .into_iter()
        .map(|kernel| Arc::new(kernel) as Arc<dyn Kernel>)
        .collect()
}

/// `profiles.dir`: the preamble's default is `~/.moruna/profiles`, and no directory at all on
/// a host with no home, which disables the profile store. `Runtime::run` resolves and creates
/// it for a `RunSpec` that leaves `profiles_dir` unset (12 f.1); this is the path it uses.
pub fn default_profiles_dir() -> Option<PathBuf> {
    #[allow(deprecated)]
    std::env::home_dir().map(|home| home.join(".moruna").join("profiles"))
}

/// Pre-built components, for tests and for a surface that must construct one itself
/// (12 d.1). Every `None` is built as in 12 f.1; a `Some` is used in its place at the same
/// step, so the startup order is observable with instrumented fakes.
#[derive(Default)]
pub struct Components {
    /// Skips the discovery step.
    pub discovered: Option<Discovered>,
    /// Skips the arena step.
    pub alloc: Option<Arc<dyn Allocator>>,
    /// Skips the reactor step.
    pub reactor: Option<Arc<dyn Reactor>>,
    /// Object metadata for an injected reactor; the facade's own reactor supplies its own.
    pub object_metadata: Option<Arc<dyn ObjectMetadata>>,
    /// Two handles to one object; the facade still starts a writer so the report has a view.
    pub trace: Option<(Arc<dyn TraceSink>, Arc<dyn TraceTail>)>,
    /// Skips building discovery's sampler.
    pub sampler: Option<Arc<dyn Sampler>>,
    /// Skips the placement step.
    pub placement: Option<Arc<dyn Placement>>,
    /// Uses this run id instead of minting one.
    pub run_id: Option<RunId>,
    /// Attached to the run's scheduler while it runs, so another thread can ask for a
    /// manifest now (MH 4.3 `checkpoint`, 4.7).
    pub checkpoint: Option<crate::checkpoint::CheckpointHandle>,
    /// A window onto the run for a host (MH 4.3); the facade attaches to it and detaches when
    /// the run ends.
    pub observer: Option<Arc<crate::observe::RunObserver>>,
}

/// The cancel token a run is driven with, re-exported so a caller needs one import.
pub type Cancel = CancelToken;

#[cfg(test)]
mod tests {
    use super::*;
    use moruna_testkit::{FakeAllocator, FakeReactor, FakeSink, FakeSource};

    fn ctx(metadata: Option<Arc<dyn ObjectMetadata>>) -> BuildCtx {
        BuildCtx {
            discovered: Discovered {
                limits: moruna_kernel::Limits {
                    memory_ceiling: 1 << 30,
                    memory_kill: None,
                    cpu_quota: 1.0,
                    page_bytes: 4096,
                    devices: Vec::new(),
                    source: moruna_kernel::LimitSource::Explicit,
                },
                profile: HostProfile::default(),
                host_tier: moruna_kernel::TierKind::Host,
                cgroup_path: None,
                disk_budget: 0,
                notes: Vec::new(),
            },
            alloc: Arc::new(FakeAllocator::new()) as Arc<dyn Allocator>,
            reactor: Arc::new(FakeReactor::new()) as Arc<dyn Reactor>,
            object_metadata: metadata,
            run_id: RunId([3; 16]),
        }
    }

    /// A built source or sink is handed back unchanged; a builder is run.
    #[test]
    fn both_shapes_produce_a_component() {
        let ctx = ctx(None);
        let built: SourceSpec = (Arc::new(FakeSource::new()) as Arc<dyn Source>).into();
        assert!(built.build(&ctx).is_ok());
        let made = SourceSpec::Build(Box::new(|_| {
            Ok(Arc::new(FakeSource::new()) as Arc<dyn Source>)
        }));
        assert!(made.build(&ctx).is_ok());

        let built: SinkSpec = (Box::new(FakeSink::new()) as Box<dyn Sink>).into();
        assert!(built.build(&ctx).is_ok());
        let made = SinkSpec::Build(Box::new(|_| Ok(Box::new(FakeSink::new()) as Box<dyn Sink>)));
        assert!(made.build(&ctx).is_ok());
    }

    /// A builder that asks for object metadata the context has not got is told so.
    #[test]
    fn missing_object_metadata_is_named() {
        let Err(error) = ctx(None).object_metadata() else {
            panic!("there is no metadata handle");
        };
        assert!(matches!(
            error,
            moruna_kernel::MorunaError::Config {
                name: "object_metadata",
                ..
            }
        ));
        let reactor = Arc::new(FakeReactor::new());
        assert!(
            ctx(Some(reactor as Arc<dyn ObjectMetadata>))
                .object_metadata()
                .is_ok()
        );
    }

    /// `RunSpec::new` is the preamble's defaults and nothing else (PY-I3).
    #[test]
    fn a_new_spec_is_all_defaults() {
        let spec = RunSpec::new(
            Arc::new(FakeSource::new()) as Arc<dyn Source>,
            Vec::new(),
            Box::new(FakeSink::new()) as Box<dyn Sink>,
        );
        assert!(spec.budget.is_none());
        assert!(!spec.ordered);
        assert!(!spec.allow_gil);
        assert!(spec.checkpoint);
        assert!(!spec.checkpoint_keep);
        assert!(spec.resume.is_none());
        assert!(!spec.resume_auto);
        assert!(!spec.staging_durable);
        assert_eq!(
            spec.profiles_dir, None,
            "the facade resolves the default, so a fresh spec names no store (12 f.1)"
        );
        assert_eq!(spec.error_policy, ErrorPolicy::Terminate);
        assert_eq!(spec.sizer, SizerKind::Rule);
        assert_eq!(
            spec.checkpoint_interval_ms,
            crate::config::CHECKPOINT_INTERVAL_MS
        );
    }

    /// Components default to building everything.
    #[test]
    fn components_default_to_nothing_injected() {
        let components = Components::default();
        assert!(components.discovered.is_none());
        assert!(components.alloc.is_none());
        assert!(components.reactor.is_none());
        assert!(components.object_metadata.is_none());
        assert!(components.trace.is_none());
        assert!(components.sampler.is_none());
        assert!(components.placement.is_none());
        assert!(components.run_id.is_none());
        assert!(components.checkpoint.is_none());
        assert!(components.observer.is_none());
        let _: Cancel = Cancel::new();
    }
}
