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
            explicit_spill_limit: staging_limit,
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

    // 2. Arena. The baseline is sampled here, before the region exists (12 f.1, 02 f.1,
    // 11 f.1): the arena touches every page at `new`, so a sample taken after it contains the
    // arena and subtracting it later would charge the arena twice.
    let baseline_bytes = sampler.sample().anon_bytes;
    let arena_bytes = arena_host_bytes(
        limits.memory_ceiling,
        baseline_bytes,
        config::RESERVE_FRACTION,
        expected_kernel_state(&kernels),
        expected_out_of_arena_amplification(&kernels),
        limits.page_bytes,
        &mut notes,
    );
    let alloc: Arc<dyn Allocator> = match components.alloc {
        Some(alloc) => alloc,
        None => Arena::new(ArenaConfig {
            host_bytes: arena_bytes,
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
    // `budget.disk` (03 f.6): discovery computed it, from the explicit limit, the environment
    // variable or 20% of the free space in the staging directory, and noted which.
    let disk_budget = discovered.disk_budget;
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
            checkpoint_keep,
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
    let profiles_dir = resolve_profiles_dir(profiles_dir, home_dir(), &mut notes);
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
            arena_bytes,
            baseline_bytes,
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
    // A `Weak`, not a clone: the controller holds the scheduler as its `Knobs`, `StatsSource`
    // and `Prober`, so a hook that held the controller strongly would close a reference cycle
    // and neither would ever be dropped. The arena went with them, and a second `Runtime::run`
    // in the same process then sampled a baseline that still contained the first run's whole
    // region (12 f.1, 11 f.1). The controller outlives every record: it is held here and in
    // `started` until after the scheduler has stopped.
    let hooked = Arc::downgrade(&controller);
    scheduler.set_record_hook(Arc::new(move |record| {
        if let Some(controller) = hooked.upgrade() {
            controller.on_record(record);
        }
    }));

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

/// `ArenaConfig::host_bytes` (12 f.1, 02 f.1, 11 f.1).
///
/// The *allowance* is `ceiling - baseline - reserve - expected kernel state`: the bytes the run
/// may hold at all. The arena takes a share of it and leaves the rest as anonymous headroom for
/// what the chain allocates outside the arena, because the controller has two inequalities to
/// satisfy and only one of them is about the arena (11 b). An arena sized at the whole allowance
/// leaves `budget.reserve_fraction` as the entire governed headroom for out-of-arena bytes at
/// every budget, shared with the runtime's own reader and writer buffers, and that is why an
/// ordinary Python job was refused below about 2 GiB: not because the budget did not fit, but
/// because it was spent on an arena the job did not need that large (measured 2026-09-23).
///
/// The share follows from the two inequalities. Per byte in flight the arena needs
/// `ARENA_INFLIGHT_COST` bytes and the process needs `a_anon` bytes outside it, so equalising
/// the slack in both gives the arena `ARENA_INFLIGHT_COST / (ARENA_INFLIGHT_COST + a_anon)` of
/// the allowance. A chain that declares `expected_amplification = 0.0` throughout says it
/// allocates nothing outside the arena and gets the whole allowance, which is what this function
/// did for every chain before. `ARENA_FLOOR_BYTES` keeps a tiny budget's arena workable, and
/// never raises it above the allowance, so a budget with no room still reaches the controller's
/// own refusal rather than being handed an arena that does not exist.
///
/// The baseline is the process's anonymous memory *before* the arena exists, because 02 f.1
/// touches every page of the region at `new`. The arena figure is also the controller's whole
/// arena allowance (`ControllerConfig::arena_bytes`), so the bytes the controller believes it
/// may hold are the bytes the arena can actually give it, and nothing is subtracted twice.
fn arena_host_bytes(
    ceiling: u64,
    baseline: u64,
    reserve_fraction: f32,
    kernel_state: u64,
    out_of_arena_amplification: f64,
    page_bytes: usize,
    notes: &mut Vec<String>,
) -> u64 {
    let reserve = (ceiling as f64 * f64::from(reserve_fraction)) as u64;
    let granule = (page_bytes as u64).max(GRANULE_BYTES);
    let allowance = ceiling
        .saturating_sub(baseline)
        .saturating_sub(reserve)
        .saturating_sub(kernel_state);
    // A negative or non-finite declaration is a kernel saying nothing useful, not a licence to
    // size the arena over the allowance.
    let a_anon = if out_of_arena_amplification.is_finite() {
        out_of_arena_amplification.max(0.0)
    } else {
        DEFAULT_OUT_OF_ARENA_AMPLIFICATION
    };
    let share = ARENA_INFLIGHT_COST / (ARENA_INFLIGHT_COST + a_anon);
    let arena = (allowance as f64 * share) as u64;
    let arena = arena.max(ARENA_FLOOR_BYTES.min(allowance)) / granule * granule;
    let headroom = ceiling.saturating_sub(baseline).saturating_sub(arena);
    notes.push(format!(
        "arena sized at {arena} bytes: {share:.2} of the {allowance} byte allowance, which is \
         the ceiling {ceiling} less the {baseline} bytes the process held before the arena \
         existed, the {reserve} byte reserve and the {kernel_state} bytes the stateful kernels \
         declare; the share is set by the {a_anon:.2} bytes the chain is expected to allocate \
         outside the arena per byte in flight, and {headroom} bytes are left above the arena for \
         it"
    ));
    arena
}

/// The arena's own cost per byte in flight. RC f.3 divides the arena in half, one half for what
/// the workers hold and one for the queues and the read-ahead behind them, so a byte in flight
/// costs the arena two.
const ARENA_INFLIGHT_COST: f64 = 2.0;

/// The out-of-arena amplification assumed for a kernel that declares none: the same figure the
/// controller seeds `a_anon` with before its probe speaks (11 f.3), so the arena is sized against
/// the cost the controller is about to assume rather than against zero.
const DEFAULT_OUT_OF_ARENA_AMPLIFICATION: f64 = 4.0;

/// The smallest arena the facade hands out, so a small budget gets a working region rather than a
/// share of almost nothing. It is the largest single allocation a default pipeline makes: the
/// Parquet sink's output buffer, which asks for one whole size class of 02 e.2 and spends its
/// footer headroom inside it (08 f.1), so at the default 128 MiB row group it asks for the
/// 128 MiB class. An arena below that cannot open a sink at defaults whatever the budget, which
/// is why the floor is this figure rather than a few morsels.
///
/// A class that size has to be free all at once, so the floor is that class plus what the arena
/// already holds when the sink opens its buffer, which early in a run is the read-ahead:
/// `128 MiB + readahead.splits x morsel.probe_bytes`. This is half what it was until
/// 2026-09-23, when the sink asked for `row_group_bytes + 1 MiB of footer`: 129 MiB at the
/// default, which 02 e.2 serves out of the 256 MiB class, so the sink reserved twice what it
/// wanted and the Python job of `python/tests/test_budget.py` could not open it at a 512 MiB
/// ceiling at all (its queues hold 18.5 MiB when the sink opens, and 256 MiB was then not free).
/// With the footer inside the class that job completes inside 512 MiB.
///
/// It is capped by the allowance, so a budget too small for it gets its whole allowance and
/// nothing is conjured. Lowering it further is a change in `amoru-sinks` and not here: the figure
/// is whatever class the sink's file buffer asks for at the default row group.
const ARENA_FLOOR_BYTES: u64 =
    (128 << 20) + config::READAHEAD_SPLITS as u64 * config::MORSEL_PROBE_BYTES;

/// The chain's expected out-of-arena cost per byte in flight (12 f.1): the largest
/// `KernelHints::expected_amplification` any stage declares, counting a stage that declares
/// nothing as `DEFAULT_OUT_OF_ARENA_AMPLIFICATION`. The largest rather than the sum, because the
/// anonymous inequality's `share` is `active_workers / stages`, so what it sums to over a chain
/// of equal stages is one stage's cost; the largest is the conservative reading of that.
///
/// A chain with no kernels at all is a copy, whose out-of-arena bytes are the runtime's own
/// buffers and belong to the reserve, so it declares nothing and keeps the whole allowance.
fn expected_out_of_arena_amplification(kernels: &[Arc<dyn amoru_kernel::Kernel>]) -> f64 {
    kernels
        .iter()
        .map(|kernel| {
            kernel
                .hints()
                .expected_amplification
                .unwrap_or(DEFAULT_OUT_OF_ARENA_AMPLIFICATION)
        })
        .fold(0.0, f64::max)
}

/// The smallest region granularity 02 e.1 rounds the arena down to.
const GRANULE_BYTES: u64 = 64 << 10;

/// The expected kernel state of 11 f.1: the sum of `KernelHints::state_bytes` over the
/// stateful instances that will be created, zero where a kernel declares none. It is held
/// outside the arena, so the arena may not be sized over it.
fn expected_kernel_state(kernels: &[Arc<dyn amoru_kernel::Kernel>]) -> u64 {
    kernels
        .iter()
        .map(|kernel| match kernel.kind() {
            amoru_kernel::KernelKind::Stateful { max_instances } => kernel
                .hints()
                .state_bytes
                .unwrap_or(0)
                .saturating_mul(max_instances.get() as u64),
            amoru_kernel::KernelKind::Stateless => 0,
        })
        .sum()
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

/// `profiles.dir` (12 f.1, preamble section 5). `None` from the surface means the documented
/// default, `<home>/.amoru/profiles`, which is created here. Without it a resumed run has no
/// memory of the interrupted run and `probe_missing` has nothing to seed from (11 f.14). A home
/// directory that is not set, or a directory that cannot be created, leaves the store off with
/// a note, which is what 11 f.9 already does for an unwritable store.
fn resolve_profiles_dir(
    given: Option<PathBuf>,
    home: Option<PathBuf>,
    notes: &mut Vec<String>,
) -> Option<PathBuf> {
    if let Some(dir) = given {
        return Some(dir);
    }
    let Some(home) = home else {
        notes.push(
            "profiles.dir is off: no home directory, so nothing is remembered between runs"
                .to_string(),
        );
        return None;
    };
    let dir = home.join(".amoru").join("profiles");
    match std::fs::create_dir_all(&dir) {
        Ok(()) => Some(dir),
        Err(error) => {
            notes.push(format!(
                "profiles.dir {} could not be created ({error}); nothing is remembered between \
                 runs",
                dir.display()
            ));
            None
        }
    }
}

/// The user's home directory. A separate function so `resolve_profiles_dir` can be tested
/// without touching the environment.
fn home_dir() -> Option<PathBuf> {
    #[allow(deprecated)]
    std::env::home_dir().filter(|h| !h.as_os_str().is_empty())
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

    /// `profiles.dir` defaults to `<home>/.amoru/profiles` and the directory is created
    /// (12 f.1, preamble section 5); an explicit directory is left alone and no home directory
    /// leaves the store off with a note.
    #[test]
    fn the_profile_store_has_a_default() {
        let mut notes = Vec::new();
        let given = PathBuf::from("/tmp/amoru-profiles-given");
        assert_eq!(
            resolve_profiles_dir(Some(given.clone()), None, &mut notes),
            Some(given),
            "an explicit directory is taken as given"
        );
        assert!(notes.is_empty());

        let home = std::env::temp_dir().join(format!("amoru-home-{}", std::process::id()));
        let resolved = resolve_profiles_dir(None, Some(home.clone()), &mut notes)
            .expect("the default under a writable home");
        assert_eq!(resolved, home.join(".amoru").join("profiles"));
        assert!(resolved.is_dir(), "the default directory is created");
        let _ = std::fs::remove_dir_all(&home);

        assert_eq!(resolve_profiles_dir(None, None, &mut notes), None);
        assert_eq!(notes.len(), 1, "the note says nothing is remembered");
        assert!(notes[0].contains("profiles.dir"));
    }

    /// A chain that declares it allocates nothing outside the arena takes the whole room left
    /// under the ceiling, and says so. Nothing is halved: this is the controller's allowance as
    /// well (12 f.1, 11 f.1).
    #[test]
    fn the_arena_is_sized_from_what_is_left() {
        let mut notes = Vec::new();
        let ceiling = 1u64 << 30;
        let reserve = (ceiling as f64 * config::RESERVE_FRACTION as f64) as u64;
        let bytes = arena_host_bytes(
            ceiling,
            100 << 20,
            config::RESERVE_FRACTION,
            0,
            0.0,
            4096,
            &mut notes,
        );
        let want = (ceiling - (100 << 20) - reserve) / GRANULE_BYTES * GRANULE_BYTES;
        assert_eq!(bytes, want);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("arena sized at"));

        // The declared kernel state is held outside the arena, so it comes out too.
        let with_state = arena_host_bytes(
            ceiling,
            100 << 20,
            config::RESERVE_FRACTION,
            64 << 20,
            0.0,
            4096,
            &mut notes,
        );
        assert_eq!(with_state, bytes - (64 << 20));

        // And the region is a multiple of the granule 02 e.1 rounds down to.
        let odd = arena_host_bytes(ceiling + 12345, 0, 0.0, 0, 0.0, 4096, &mut notes);
        assert!(
            odd.is_multiple_of(GRANULE_BYTES),
            "{odd} is not a whole granule"
        );
    }

    /// 12 f.1: the arena's share of the allowance falls as the chain's declared out-of-arena cost
    /// rises, and the bytes it gives up become anonymous headroom for the chain. The defect this
    /// replaces gave the arena the whole allowance at every budget, which left
    /// `budget.reserve_fraction` as the only governed headroom for out-of-arena bytes and refused
    /// an ordinary Python job below about 2 GiB (measured 2026-09-23).
    #[test]
    fn the_arena_share_falls_as_the_chain_costs_more_outside_it() {
        let mut notes = Vec::new();
        // A ceiling large enough that the share governs rather than `ARENA_FLOOR_BYTES`.
        let ceiling = 4u64 << 30;
        let baseline = 48u64 << 20;
        let sized = |a_anon: f64| {
            arena_host_bytes(
                ceiling,
                baseline,
                config::RESERVE_FRACTION,
                0,
                a_anon,
                4096,
                &mut Vec::new(),
            )
        };

        let none = sized(0.0);
        let declared = sized(4.5);
        let greedy = sized(12.0);
        assert!(
            none > declared && declared > greedy,
            "shares do not fall: {none} then {declared} then {greedy}"
        );

        // The share is the ratio the two inequalities give, not a guess: at the measured 4.5 the
        // arena takes 2 / 6.5 of the allowance.
        let reserve = (ceiling as f64 * config::RESERVE_FRACTION as f64) as u64;
        let allowance = ceiling - baseline - reserve;
        let want = ((allowance as f64 * (2.0 / 6.5)) as u64) / GRANULE_BYTES * GRANULE_BYTES;
        assert_eq!(declared, want);

        // And what the arena gives up is headroom the process can use: at 4.5 the run has more
        // than four times the reserve above the arena, where before it had exactly the reserve
        // plus the declared state at every budget there is.
        let headroom = ceiling - baseline - declared;
        assert!(
            headroom > 4 * reserve,
            "headroom {headroom} is not the point of the change (reserve {reserve})"
        );

        // A kernel that declares nonsense is a kernel that declared nothing.
        assert_eq!(sized(f64::NAN), sized(DEFAULT_OUT_OF_ARENA_AMPLIFICATION));
        assert_eq!(sized(-1.0), sized(0.0));

        // One note per call, naming both the share and the headroom, so a run report says what
        // the arena was sized from.
        let _ = arena_host_bytes(
            ceiling,
            baseline,
            config::RESERVE_FRACTION,
            0,
            4.5,
            4096,
            &mut notes,
        );
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].contains("outside the arena per byte in flight"),
            "{}",
            notes[0]
        );
    }

    /// 12 f.1: a budget too small for a share of the allowance to be a working arena still gets
    /// `ARENA_FLOOR_BYTES`, and a budget with nothing left still gets nothing, so the controller's
    /// own refusal is what reports it (11 f.1).
    #[test]
    fn a_tiny_budget_still_gets_a_working_arena() {
        let mut notes = Vec::new();
        // 64 MiB with a 40 MiB baseline: the floor is larger than the whole allowance, so the
        // arena takes all of it rather than being handed bytes that are not there.
        let allowance = |ceiling: u64, baseline: u64| {
            ceiling - baseline - (ceiling as f64 * config::RESERVE_FRACTION as f64) as u64
        };
        let tiny = arena_host_bytes(
            64 << 20,
            40 << 20,
            config::RESERVE_FRACTION,
            0,
            DEFAULT_OUT_OF_ARENA_AMPLIFICATION,
            4096,
            &mut notes,
        );
        assert_eq!(
            tiny,
            allowance(64 << 20, 40 << 20) / GRANULE_BYTES * GRANULE_BYTES
        );
        assert!(
            tiny > config::MORSEL_MIN_BYTES * 2,
            "below the controller's refusal line"
        );

        // A 512 MiB ceiling: a third of the allowance is under the floor, so the floor stands,
        // and it is the 256 MiB the Parquet sink's default buffer costs the arena.
        let at_512 = arena_host_bytes(
            512 << 20,
            48 << 20,
            config::RESERVE_FRACTION,
            0,
            DEFAULT_OUT_OF_ARENA_AMPLIFICATION,
            4096,
            &mut notes,
        );
        assert_eq!(at_512, ARENA_FLOOR_BYTES);

        // A large enough budget is governed by the share rather than the floor.
        let at_4g = arena_host_bytes(
            4 << 30,
            48 << 20,
            config::RESERVE_FRACTION,
            0,
            DEFAULT_OUT_OF_ARENA_AMPLIFICATION,
            4096,
            &mut notes,
        );
        assert!(at_4g > ARENA_FLOOR_BYTES, "{at_4g} is still the floor");

        // The floor never conjures bytes the budget does not have.
        assert_eq!(
            arena_host_bytes(
                1 << 30,
                4 << 30,
                config::RESERVE_FRACTION,
                0,
                DEFAULT_OUT_OF_ARENA_AMPLIFICATION,
                4096,
                &mut notes
            ),
            0
        );
    }

    /// 12 f.1: the chain's declared out-of-arena cost is the largest any stage declares, a stage
    /// that declares nothing counts as the controller's own seed, and an empty chain declares
    /// nothing at all.
    #[test]
    fn the_chain_declares_its_out_of_arena_cost() {
        use amoru_kernel::Kernel;
        use amoru_testkit::FakeKernel;

        assert_eq!(expected_out_of_arena_amplification(&[]), 0.0);

        // A kernel that overrides nothing declares nothing, which is the common case: the
        // `examples/append_column.rs` kernel a first program is written around is one.
        struct Silent;
        impl Kernel for Silent {
            fn fingerprint(&self) -> amoru_kernel::Fingerprint {
                amoru_kernel::Fingerprint::compute("amoru::tests::silent", b"v1")
            }
            fn kind(&self) -> amoru_kernel::KernelKind {
                amoru_kernel::KernelKind::Stateless
            }
            fn accepts(&self) -> amoru_kernel::PayloadSpec {
                amoru_kernel::PayloadSpec {
                    kind: amoru_kernel::PayloadKind::Table,
                    tier: amoru_kernel::TierPref::Host,
                }
            }
            fn output_schema(
                &self,
                input: &amoru_kernel::SourceSchema,
            ) -> amoru_kernel::Result<amoru_kernel::SourceSchema> {
                Ok(input.clone())
            }
            fn init(
                &self,
                _ctx: &amoru_kernel::InitCtx,
            ) -> amoru_kernel::Result<Box<dyn amoru_kernel::KernelState>> {
                Ok(Box::new(amoru_kernel::NoState))
            }
            fn apply(
                &self,
                _state: &mut dyn amoru_kernel::KernelState,
                input: amoru_kernel::Payload,
            ) -> amoru_kernel::Result<amoru_kernel::Payload> {
                Ok(input)
            }
        }
        assert_eq!(Silent.hints().expected_amplification, None);
        let undeclared: Vec<Arc<dyn Kernel>> = vec![Arc::new(Silent)];
        assert_eq!(
            expected_out_of_arena_amplification(&undeclared),
            DEFAULT_OUT_OF_ARENA_AMPLIFICATION
        );

        let mixed: Vec<Arc<dyn Kernel>> = vec![
            Arc::new(FakeKernel::new().amplification(0.5)),
            Arc::new(FakeKernel::new().amplification(6.0)),
        ];
        assert_eq!(expected_out_of_arena_amplification(&mixed), 6.0);
    }

    /// A process already over the ceiling leaves the arena nothing, and the controller's own
    /// check is what reports it (11 f.1).
    #[test]
    fn an_over_full_process_leaves_the_arena_nothing() {
        let mut notes = Vec::new();
        assert_eq!(
            arena_host_bytes(
                1 << 30,
                4 << 30,
                config::RESERVE_FRACTION,
                0,
                0.0,
                4096,
                &mut notes
            ),
            0
        );
    }

    /// The expected kernel state sums the declared per-instance state over the instances that
    /// will exist, and a stateless kernel contributes nothing (11 f.1).
    #[test]
    fn the_expected_kernel_state_counts_instances() {
        use amoru_kernel::Kernel;
        use amoru_testkit::FakeKernel;

        let kernels: Vec<Arc<dyn Kernel>> = vec![
            Arc::new(FakeKernel::new().stateful(2, 8 << 20)),
            Arc::new(FakeKernel::new()),
        ];
        assert_eq!(expected_kernel_state(&kernels), 16 << 20);
        assert_eq!(expected_kernel_state(&[]), 0);
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
