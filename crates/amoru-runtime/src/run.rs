//! The run lifecycle (12 f.1, f.7; preamble 4.4). The facade has no logic of its own: every
//! line here either builds a component from the configuration table or calls the next step of
//! PY-I1 in its fixed order.

use std::path::PathBuf;
use std::sync::Arc;

use amoru_arena::{Arena, ArenaConfig};
use amoru_controller::{Controller, ControllerConfig, KernelInfo, PlanSummary};
use amoru_discovery::{Discovered, DiscoveryInput, Sampler as DiscoverySampler};
use amoru_kernel::{
    Allocator, AmoruError, CancelToken, Kernel, Knobs, ObjectMetadata, Placement, Prober, Reactor,
    RunId, Sampler, SourceSchema, StatsSource, TierBudgets, TierKind, TraceSink, TraceTail,
};
use amoru_placement::{PlacementConfig, PlacementEngine, manifest::plan_digest};
use amoru_reactor::ReactorConfig;
use amoru_scheduler::{Pipeline, RunOutcome, Scheduler, SchedulerConfig};
use amoru_sinks::SinkHandle;
use amoru_trace::{RunReport, TraceConfig, TraceWriter};

use crate::cancel::{CancelOnDrop, Started};
use crate::config;
use crate::error::{Result, RunError};
use crate::report::{self, MetaInput};
use crate::spec::{BuildCtx, Components, RunSpec};

/// The facade: the only place the components are wired (12 a).
pub struct Runtime;

impl Runtime {
    /// One run, start to finish, in the order of PY-I1. Blocks until the run ends; `cancel`
    /// is polled by the scheduler. Equals `run_with(spec, cancel, Components::default())`.
    pub fn run(spec: RunSpec, cancel: CancelToken) -> Result<RunReport> {
        Runtime::run_with(spec, cancel, Components::default())
    }

    /// [`Runtime::run`] with components the caller has already built; every `None` is built
    /// at its own step of the lifecycle (12 d.1).
    pub fn run_with(
        spec: RunSpec,
        cancel: CancelToken,
        components: Components,
    ) -> Result<RunReport> {
        let mut started = Started::default();
        let mut guard = CancelOnDrop::new(cancel);
        let outcome = drive(spec, &mut guard, components, &mut started);
        guard.disarm();
        match outcome {
            Ok(report) => Ok(report),
            Err(error) => {
                let (view, notes) = started.unwind();
                let _ = view;
                Err(error.with_shutdown_notes(notes))
            }
        }
    }

    /// Discovery on its own, for `amoru.inspect_host()` (12 d.1).
    pub fn inspect(input: &DiscoveryInput) -> Result<Discovered> {
        amoru_discovery::discover(input).map_err(RunError::bare)
    }
}

/// Mint a run id from 16 random bytes (12 f.1).
fn mint_run_id() -> Result<RunId> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| {
        RunError::bare(AmoruError::Config {
            name: "run_id",
            msg: format!("the operating system returned no random bytes: {e}"),
        })
    })?;
    Ok(RunId(bytes))
}

/// The chain's schemas, stage 0 first (12 f.1: computed here so `KernelInfo::schema_hash` is
/// the stage's input schema hash). The scheduler validates the chain again at `new`; this
/// walk fails on the same errors and earlier.
fn chain_schemas(
    source_schema: SourceSchema,
    kernels: &[Arc<dyn Kernel>],
) -> amoru_kernel::Result<Vec<SourceSchema>> {
    let mut schemas = Vec::with_capacity(kernels.len() + 1);
    let mut schema = source_schema;
    schemas.push(schema.clone());
    for kernel in kernels {
        kernel.accepts().check(&schema)?;
        schema = kernel.output_schema(&schema)?;
        schemas.push(schema.clone());
    }
    Ok(schemas)
}

/// The whole lifecycle. Every `?` leaves `started` holding what is running, so `run_with`
/// unwinds it in reverse (PY-I1).
fn drive(
    spec: RunSpec,
    guard: &mut CancelOnDrop,
    components: Components,
    started: &mut Started,
) -> Result<RunReport> {
    let start_ns = report::now_ns();
    let RunSpec {
        source: source_spec,
        kernels,
        #[cfg(feature = "python")]
        py_kernels,
        sink: sink_spec,
        budget,
        cpu,
        trace_path,
        staging_dir: explicit_staging_dir,
        staging_limit,
        error_policy,
        ordered,
        sizer,
        profiles_dir,
        object_store,
        host_profile,
        allow_gil,
        checkpoint,
        checkpoint_interval_ms,
        checkpoint_keep,
        resume,
        notes: spec_notes,
    } = spec;

    // f.4: the GIL refusal comes before any Rust component starts (PY-I4).
    #[cfg(feature = "python")]
    if !py_kernels.is_empty() && amoru_adapters::python_gil_enabled() && !allow_gil {
        return Err(RunError::bare(AmoruError::Config {
            name: "python.allow_gil",
            msg: "Python kernels require a free-threaded interpreter (python3.13t or \
                  python3.14t); pass allow_gil=True to run serialised"
                .into(),
        }));
    }
    #[cfg(not(feature = "python"))]
    let _ = allow_gil;

    let mut notes: Vec<String> = Vec::new();

    // f.7: the manifest header first, so the new process continues the old run's identity.
    let header = match &resume {
        Some(path) => Some(PlacementEngine::read_manifest_header(path)?),
        None => None,
    };
    let run_id = match (&header, components.run_id) {
        (Some(header), _) => header.run_id,
        (None, Some(id)) => id,
        (None, None) => mint_run_id()?,
    };
    let node = match &header {
        Some(header) => header.node,
        None => amoru_kernel::LOCAL_NODE,
    };

    // 1. Discover.
    let discovered = match components.discovered {
        Some(discovered) => discovered,
        None => amoru_discovery::discover(&DiscoveryInput {
            explicit_budget: budget,
            explicit_cpu: cpu,
            explicit_staging_dir: explicit_staging_dir.clone(),
            profile_override: host_profile,
        })?,
    };
    notes.extend(discovered.notes.iter().cloned());
    notes.extend(spec_notes);
    let limits = discovered.limits.clone();
    let staging_dir = discovered.profile.staging_dir.clone();
    let workers_max = workers_from(limits.cpu_quota);

    // The sampler exists before the arena, because the arena's size depends on what the
    // process already holds (see `arena_host_bytes`).
    let sampler: Arc<dyn Sampler> = match components.sampler {
        Some(sampler) => sampler,
        None => Arc::new(DiscoverySampler::new(&discovered)?),
    };

    // 2. Arena.
    let alloc: Arc<dyn Allocator> = match components.alloc {
        Some(alloc) => alloc,
        None => Arena::new(ArenaConfig {
            host_bytes: arena_host_bytes(limits.memory_ceiling, &sampler, &mut notes),
            host_tier: discovered.host_tier,
            device_bytes: limits
                .devices
                .iter()
                .map(|d| {
                    (
                        d.id,
                        (d.free_bytes as f64 * config::DEVICE_BUDGET_FRACTION) as u64,
                    )
                })
                .collect(),
            page_bytes: limits.page_bytes,
            huge_pages: discovered.profile.huge_pages,
            memlock: discovered.profile.memlock,
            register_rdma: false,
        })?,
    };

    // 3. Reactor.
    let (reactor, object_metadata): (Arc<dyn Reactor>, Option<Arc<dyn ObjectMetadata>>) =
        match components.reactor {
            Some(reactor) => (reactor, components.object_metadata),
            None => {
                let built = amoru_reactor::Reactor::new(
                    ReactorConfig {
                        threads: config::REACTOR_THREADS,
                        object_concurrency: config::REACTOR_OBJECT_CONCURRENCY,
                        file_depth: config::REACTOR_FILE_DEPTH,
                        page_bytes: limits.page_bytes,
                        profile: discovered.profile.clone(),
                        devices: limits.devices.iter().map(|d| d.id).collect(),
                        object_store,
                    },
                    alloc.clone(),
                )?;
                (built.clone(), Some(built))
            }
        };
    started.reactor = Some(reactor.clone());
    let io_paths = reactor.paths();

    // 4. Trace. A writer is always started, even when a fake sink and tail are injected, so
    // the report has a view to be computed from (12 f.1: "an empty view when components.trace
    // injected a fake"); `TraceView` has no other constructor.
    let trace_writer = TraceWriter::start(TraceConfig {
        path: trace_path,
        staging_dir: staging_dir.clone().unwrap_or_else(std::env::temp_dir),
        channel_capacity: config::TRACE_CHANNEL_CAPACITY,
        memory_limit: config::TRACE_MEMORY_LIMIT,
        run_id,
    })?;
    started.trace = Some(trace_writer.clone());
    let (trace_sink, trace_tail): (Arc<dyn TraceSink>, Arc<dyn TraceTail>) = match components.trace
    {
        Some((sink, tail)) => (sink, tail),
        None => (trace_writer.clone(), trace_writer.clone()),
    };

    // 5. Sources and sinks built.
    let ctx = BuildCtx {
        discovered: discovered.clone(),
        alloc: alloc.clone(),
        reactor: reactor.clone(),
        object_metadata,
        run_id,
    };
    let source = source_spec.build(&ctx)?;
    let sink = sink_spec.build(&ctx)?;
    let plan = source.plan()?;
    let plan_summary = summarise(&plan);
    let repeatable = source.repeatable();
    if resume.is_some() && !repeatable {
        return Err(RunError::bare(AmoruError::Resume(
            "the source is not repeatable, so this run cannot be resumed".into(),
        )));
    }
    let sink_handle = SinkHandle::wrap(sink, ordered, config::ORDERING_BUFFER_BYTES);

    // 6. Kernels built: bind the arena, walk the chain, build the controller's view of them.
    #[cfg(feature = "python")]
    for (_, py_kernel) in &py_kernels {
        py_kernel.bind_allocator(alloc.clone());
    }
    let schemas = chain_schemas(source.schema(), &kernels)?;
    let kernel_infos: Vec<KernelInfo> = kernels
        .iter()
        .enumerate()
        .map(|(at, kernel)| KernelInfo {
            stage: (at + 1) as amoru_kernel::StageId,
            fingerprint: kernel.fingerprint(),
            schema_hash: schemas[at].hash(),
            hints: kernel.hints(),
            kind: kernel.kind(),
        })
        .collect();

    // 7. Placement.
    let disk_budget = disk_budget(staging_limit, staging_dir.as_ref(), &mut notes);
    let checkpoint_enabled = checkpoint && staging_dir.is_some() && repeatable && disk_budget > 0;
    if checkpoint && !checkpoint_enabled {
        notes.push(
            "checkpointing is off: it needs a staging directory, a disk budget and a repeatable \
             source"
                .to_string(),
        );
    }
    let stages = (kernels.len() + 1) as u16;
    let host_budget =
        (limits.memory_ceiling as f64 * (1.0 - config::RESERVE_FRACTION as f64)) as u64;
    let mut budgets = TierBudgets {
        disk: disk_budget,
        ..TierBudgets::default()
    };
    match discovered.host_tier {
        TierKind::PinnedHost => budgets.pinned_host = host_budget,
        TierKind::Host => budgets.host = host_budget,
        TierKind::Device | TierKind::Disk | TierKind::Remote => {
            return Err(RunError::bare(AmoruError::Config {
                name: "host_tier",
                msg: "discovery named a host tier that is not a host tier".into(),
            }));
        }
    }
    let mut engine: Option<Arc<PlacementEngine>> = None;
    let placement: Arc<dyn Placement> = match components.placement {
        Some(placement) => placement,
        None => {
            let built = PlacementEngine::new(
                PlacementConfig {
                    run_id,
                    node,
                    stages,
                    budgets: budgets.clone(),
                    staging_dir: staging_dir.clone(),
                    durable_staging: discovered.profile.durable_staging.is_guaranteed(),
                    disk_budget,
                    segment_bytes: config::STAGING_SEGMENT_BYTES,
                    codec: amoru_kernel::StagingCodec::Raw,
                    page_bytes: limits.page_bytes,
                    gds: discovered.profile.gds.is_available()
                        && cfg!(feature = "gds")
                        && io_paths.gds,
                    devices: limits.devices.iter().map(|d| d.id).collect(),
                    plan_digest: plan_digest(&plan),
                    fingerprints: kernels.iter().map(|k| k.fingerprint()).collect(),
                    resume_policy: kernels.iter().map(|k| k.hints().resume).collect(),
                    config: config::resolved(
                        limits.memory_ceiling,
                        disk_budget,
                        workers_max,
                        config::READAHEAD_SPLITS,
                        &error_policy,
                        ordered,
                        sizer,
                        checkpoint_enabled,
                        checkpoint_interval_ms,
                    ),
                    checkpoint_enabled,
                },
                alloc.clone(),
                reactor.clone(),
            )?;
            engine = Some(built.clone());
            built
        }
    };
    started.placement = Some(placement.clone());
    if !repeatable {
        // 07 SO-I8, PL-I6: a one-shot source's Q0 is staged rather than evicted.
        placement.set_staging(0, true);
        notes.push("iterator source: no resume, Q0 staged".to_string());
    }

    // f.7: restore before the scheduler is built, so its refusals land before the sink.
    let resume_point = match &resume {
        Some(path) => Some(placement.restore(
            path,
            &plan,
            &kernels.iter().map(|k| k.fingerprint()).collect::<Vec<_>>(),
        )?),
        None => None,
    };

    // 8. Scheduler `new`: validates the chain, opens the sink on a fresh run, spawns the
    // workers parked.
    let scheduler = Arc::new(Scheduler::new(
        SchedulerConfig {
            workers_max,
            workers_active: workers_max,
            read_ahead: config::READAHEAD_SPLITS,
            sink_concurrency: config::SINK_CONCURRENCY,
            error_policy,
            initial_morsel_target: config::MORSEL_PROBE_BYTES,
            morsel_min: config::MORSEL_MIN_BYTES,
            morsel_max: config::MORSEL_MAX_BYTES,
            checkpoint_enabled,
            checkpoint_interval_ms,
            heartbeat_interval_ms: config::HEARTBEAT_INTERVAL_MS,
            resuming: resume.is_some(),
            node,
        },
        Pipeline {
            source: source.clone(),
            kernels: kernels.clone(),
            sink: sink_handle,
        },
        placement.clone(),
        alloc.clone(),
        trace_sink,
        sampler.clone(),
    )?);
    started.scheduler = Some(scheduler.clone());

    // 9. Instances: eagerly, before the baseline is sampled.
    match resume_point {
        Some(point) => scheduler.apply_resume_point(point)?,
        None => scheduler.init_instances()?,
    }

    // 10. Controller, then the record hook, so the hook exists before any probe record.
    let controller = Arc::new(Controller::new(
        ControllerConfig {
            limits: limits.clone(),
            plan: plan_summary,
            workers_max,
            pinned: alloc.is_pinned(),
            reserve_fraction: config::RESERVE_FRACTION,
            target_fraction: config::TARGET_FRACTION,
            safety_initial: config::SAFETY_INITIAL,
            safety_floor: config::SAFETY_FLOOR,
            increase_step: config::INCREASE_STEP,
            tick_ms: config::TICK_MS,
            oscillation_flips: config::OSCILLATION_FLIPS,
            freeze_morsels: config::FREEZE_MORSELS,
            morsel_min: config::MORSEL_MIN_BYTES,
            morsel_max: config::MORSEL_MAX_BYTES,
            probe_bytes: config::MORSEL_PROBE_BYTES,
            sizer,
            fallback_error_ratio: config::SIZER_FALLBACK_ERROR_RATIO,
            profiles_dir,
            disk_budget,
            checkpoint_enabled,
            checkpoint_interval_ms,
        },
        scheduler.clone() as Arc<dyn Knobs>,
        scheduler.clone() as Arc<dyn StatsSource>,
        scheduler.clone() as Arc<dyn Prober>,
        sampler,
        trace_tail,
        placement.clone(),
        kernel_infos,
    )?);
    started.controller = Some(controller.clone());
    let hooked = controller.clone();
    scheduler.set_record_hook(Arc::new(move |record| hooked.on_record(record)));

    // 11. prepare, probe, start, run.
    controller.prepare()?;
    if resume.is_some() {
        controller.probe_missing()?;
    } else {
        controller.probe_all()?;
    }
    controller.start()?;
    let outcome = if resume.is_some() {
        scheduler.run_resumed(guard.token())?
    } else {
        scheduler.run(guard.token())?
    };

    // 12. stop, finish, report.
    let (exit, outcome_manifest) = report::exit_of(&outcome);
    let summary = started
        .controller
        .take()
        .map(|controller| controller.stop());
    let (view, shutdown_notes) = started.unwind();
    notes.extend(shutdown_notes);
    let manifest = outcome_manifest.or_else(|| match (checkpoint_keep, &engine) {
        (true, Some(engine)) => engine.manifest_path(),
        _ => None,
    });
    // f.7: on `Completed` the run directory goes, unless `checkpoint_keep` kept it.
    if matches!(exit, amoru_trace::ExitReason::Completed)
        && !checkpoint_keep
        && let Some(dir) = engine
            .as_ref()
            .and_then(|e| e.manifest_path())
            .and_then(|m| m.parent().map(std::path::Path::to_path_buf))
        && let Err(error) = std::fs::remove_dir_all(&dir)
    {
        notes.push(format!(
            "the run directory {} was not removed: {error}",
            dir.display()
        ));
    }
    let gil = gil_states(
        #[cfg(feature = "python")]
        &py_kernels,
    );
    let meta = report::meta(MetaInput {
        run_id,
        exit,
        start_ns,
        end_ns: report::now_ns(),
        resumed: resume.is_some(),
        manifest: manifest.clone(),
        notes,
        gil,
        io_paths,
        controller: summary,
    });
    let view = view.ok_or_else(|| {
        RunError::bare(AmoruError::Io {
            op: "trace.finish",
            target: "the run trace".to_string(),
            msg: "the trace writer produced no view, so no report can be computed".to_string(),
        })
    })?;
    let computed = report::compute(&view, &limits, &meta);
    match outcome {
        RunOutcome::Completed { .. } => Ok(computed),
        RunOutcome::Terminated { diagnostic, .. } => Err(RunError::bare(diagnostic)
            .with_report(Some(computed))
            .with_manifest(manifest)),
        RunOutcome::Cancelled { .. } => Err(RunError::bare(AmoruError::Cancelled)
            .with_report(Some(computed))
            .with_manifest(manifest)),
    }
}

/// The Python stages' interpreter state, for `RunMeta.gil` (12 f.2, f.4).
fn gil_states(
    #[cfg(feature = "python")] py_kernels: &[(
        amoru_kernel::StageId,
        Arc<amoru_adapters::PyKernel>,
    )],
) -> Vec<(amoru_kernel::StageId, amoru_kernel::GilState)> {
    #[cfg(feature = "python")]
    {
        py_kernels
            .iter()
            .map(|(stage, kernel)| (*stage, kernel.gil_state()))
            .collect()
    }
    #[cfg(not(feature = "python"))]
    {
        Vec::new()
    }
}

/// `ArenaConfig::host_bytes`.
///
/// The preamble's `budget.host` row is the host ceiling, and 02 d.1 says the arena's host
/// region is that budget. The arena touches every page of its region at `new` (02 f.1), so the
/// whole region is resident by the time the controller samples the baseline, and the
/// controller's host budget is `ceiling - baseline - reserve` (11 f.1): an arena sized at the
/// ceiling leaves the controller nothing, whatever the ceiling is. The two models cannot both
/// hold; until one of them is changed, the facade splits the room under the ceiling that the
/// process is not already using, so the bytes the controller believes it may hold equal the
/// bytes the arena can actually give it.
fn arena_host_bytes(ceiling: u64, sampler: &Arc<dyn Sampler>, notes: &mut Vec<String>) -> u64 {
    let before = sampler.sample().anon_bytes;
    let reserve = (ceiling as f64 * config::RESERVE_FRACTION as f64) as u64;
    let available = ceiling.saturating_sub(before).saturating_sub(reserve);
    let arena = available / 2;
    notes.push(format!(
        "arena sized at {arena} bytes: the ceiling {ceiling} less the {before} bytes the          process already held and the {reserve} byte reserve, halved, because the arena is          resident before the baseline is sampled"
    ));
    arena
}

/// `workers.max`: the discovered CPU quota, rounded up, at least one (preamble section 5).
fn workers_from(cpu_quota: f64) -> u16 {
    let ceil = cpu_quota.ceil();
    if !ceil.is_finite() || ceil < 1.0 {
        1
    } else {
        ceil.min(1024.0) as u16
    }
}

/// `budget.disk`. The preamble's default is 20% of the free space in the staging directory,
/// which needs a `statvfs` the facade cannot make (12 l permits no `unsafe`, and no crate in
/// 12 d.2 reports free space), so an unset limit leaves the disk tier off with a note.
fn disk_budget(
    staging_limit: Option<u64>,
    staging_dir: Option<&PathBuf>,
    notes: &mut Vec<String>,
) -> u64 {
    match (staging_limit, staging_dir) {
        (Some(limit), _) => limit,
        (None, Some(dir)) => {
            notes.push(format!(
                "budget.disk was not given and free space at {} could not be measured; the disk \
                 tier is off",
                dir.display()
            ));
            0
        }
        (None, None) => 0,
    }
}

/// What `Source::plan` adds up to, for the controller (12 f.1).
fn summarise(plan: &[amoru_kernel::Split]) -> PlanSummary {
    PlanSummary {
        total_bytes: plan.iter().map(|s| s.uncompressed_bytes).sum(),
        total_rows: plan.iter().map(|s| s.rows).sum(),
        splits: plan.len() as u32,
        max_split_bytes: plan.iter().map(|s| s.uncompressed_bytes).max().unwrap_or(0),
        sub_splittable_all: !plan.is_empty() && plan.iter().all(|s| s.sub_splittable),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amoru_kernel::Sample;

    /// A sampler that reports one fixed anonymous figure.
    struct Fixed(u64);

    impl Sampler for Fixed {
        fn sample(&self) -> Sample {
            Sample {
                anon_bytes: self.0,
                ..Sample::default()
            }
        }

        fn reset_peak(&self) {}
    }

    /// `workers.max` follows the discovered quota, rounded up, and is never zero.
    #[test]
    fn workers_follow_the_cpu_quota() {
        assert_eq!(workers_from(2.0), 2);
        assert_eq!(workers_from(2.1), 3);
        assert_eq!(workers_from(0.25), 1);
        assert_eq!(workers_from(0.0), 1);
        assert_eq!(workers_from(f64::NAN), 1);
        assert_eq!(workers_from(1.0e9), 1024);
    }

    /// `budget.disk` is the given limit, or nothing with a note naming the directory.
    #[test]
    fn the_disk_budget_is_given_or_absent() {
        let mut notes = Vec::new();
        assert_eq!(disk_budget(Some(99), None, &mut notes), 99);
        assert!(notes.is_empty());
        let dir = std::path::PathBuf::from("/tmp/amoru-staging");
        assert_eq!(disk_budget(None, Some(&dir), &mut notes), 0);
        assert_eq!(notes.len(), 1, "the note names the directory: {notes:?}");
        assert!(notes[0].contains("amoru-staging"));
        assert_eq!(disk_budget(None, None, &mut notes), 0);
        assert_eq!(notes.len(), 1, "no staging directory needs no note");
    }

    /// The arena is sized from the room left under the ceiling, and says so.
    #[test]
    fn the_arena_is_sized_from_what_is_left() {
        let mut notes = Vec::new();
        let sampler: Arc<dyn Sampler> = Arc::new(Fixed(100 << 20));
        let bytes = arena_host_bytes(1 << 30, &sampler, &mut notes);
        // 1 GiB less 100 MiB held and the reserve, halved.
        let reserve = ((1u64 << 30) as f64 * config::RESERVE_FRACTION as f64) as u64;
        assert_eq!(bytes, ((1u64 << 30) - (100 << 20) - reserve) / 2);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("arena sized at"));
    }

    /// A process already over the ceiling leaves the arena nothing, and the controller's own
    /// check is what reports it (11 f.1).
    #[test]
    fn an_over_full_process_leaves_the_arena_nothing() {
        let mut notes = Vec::new();
        let sampler: Arc<dyn Sampler> = Arc::new(Fixed(4 << 30));
        assert_eq!(arena_host_bytes(1 << 30, &sampler, &mut notes), 0);
    }

    /// The plan summary is the sum of the splits, and an empty plan is not sub-splittable.
    #[test]
    fn the_plan_summary_adds_the_splits_up() {
        let split = |id: u32, rows: u64, bytes: u64, sub: bool| amoru_kernel::Split {
            id,
            rows,
            uncompressed_bytes: bytes,
            estimated: false,
            column_bytes: vec![bytes],
            null_counts: vec![Some(0)],
            sub_splittable: sub,
        };
        let summary = summarise(&[split(0, 10, 100, true), split(1, 20, 300, true)]);
        assert_eq!(summary.total_rows, 30);
        assert_eq!(summary.total_bytes, 400);
        assert_eq!(summary.splits, 2);
        assert_eq!(summary.max_split_bytes, 300);
        assert!(summary.sub_splittable_all);

        let mixed = summarise(&[split(0, 10, 100, true), split(1, 20, 300, false)]);
        assert!(!mixed.sub_splittable_all);
        assert!(!summarise(&[]).sub_splittable_all);
        assert_eq!(summarise(&[]).max_split_bytes, 0);
    }

    /// The chain walk refuses a kernel whose `accepts` does not match the schema it is given.
    #[test]
    fn the_chain_walk_checks_every_stage() {
        let schema = SourceSchema::Tensor {
            dtype: amoru_kernel::DType::F32,
            shape: vec![-1, 8],
        };
        let schemas = chain_schemas(schema.clone(), &[]).expect("an empty chain is the source");
        assert_eq!(schemas.len(), 1);
    }

    /// A run id is 16 bytes and two of them differ.
    #[test]
    fn run_ids_are_minted() {
        let one = mint_run_id().expect("the host has randomness");
        let two = mint_run_id().expect("the host has randomness");
        assert_ne!(one.to_hex(), two.to_hex());
        assert_eq!(one.to_hex().len(), 32);
    }
}
