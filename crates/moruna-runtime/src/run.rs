//! The run lifecycle (12 f.1, f.7; preamble 4.4). The facade has no logic of its own: every
//! line here either builds a component from the configuration table or calls the next step of
//! PY-I1 in its fixed order.

use std::path::PathBuf;
use std::sync::Arc;

use moruna_arena::{Arena, ArenaConfig};
use moruna_controller::{Controller, ControllerConfig, KernelInfo, PlanSummary};
use moruna_discovery::{
    Discovered, DiscoveryInput, ElasticBounds, LimitsWatch, Sampler as DiscoverySampler,
};
use moruna_kernel::{
    Allocator, CancelToken, Kernel, Knobs, MorunaError, ObjectMetadata, Placement, Prober, Reactor,
    RunId, Sampler, Sink, StatsSource, TierBudgets, TierKind, TraceSink, TraceTail,
};
use moruna_placement::{PlacementConfig, PlacementEngine, manifest::plan_digest};
use moruna_reactor::ReactorConfig;
use moruna_scheduler::{Pipeline, RunOutcome, Scheduler, SchedulerConfig};
use moruna_sinks::SinkHandle;
use moruna_trace::{RunReport, TraceConfig, TraceWriter};

use crate::cancel::{CancelOnDrop, Started};
use crate::config;
use crate::elastic::{WatchCtx, WatchState, Watcher};
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
        crate::allocator::give_freed_memory_back();
        let mut started = Started::default();
        let mut guard = CancelOnDrop::new(cancel);
        let outcome = drive(spec, &mut guard, components, &mut started);
        guard.disarm();
        // The run's kept buffers go back with it (12 l).
        crate::allocator::reuse::end();
        match outcome {
            Ok(report) => Ok(report),
            Err(error) => {
                let (view, notes) = started.unwind();
                let _ = view;
                Err(error.with_shutdown_notes(notes))
            }
        }
    }

    /// Discovery on its own, for `moruna.inspect_host()` (12 d.1).
    pub fn inspect(input: &DiscoveryInput) -> Result<Discovered> {
        moruna_discovery::discover(input).map_err(RunError::bare)
    }
}

/// Mint a run id from 16 random bytes (12 f.1).
pub(crate) fn mint_run_id() -> Result<RunId> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| {
        RunError::bare(MorunaError::Config {
            name: "run_id",
            msg: format!("the operating system returned no random bytes: {e}"),
        })
    })?;
    Ok(RunId(bytes))
}

fn resolve_run_id(
    header: Option<&moruna_placement::manifest::ManifestHeader>,
    assigned: Option<RunId>,
) -> moruna_kernel::Result<RunId> {
    match (header, assigned) {
        (Some(header), Some(assigned)) if header.run_id != assigned => Err(MorunaError::Resume(
            "materialized checkpoint run differs from explicitly assigned run identity".into(),
        )),
        (Some(header), _) => Ok(header.run_id),
        (None, Some(assigned)) => Ok(assigned),
        (None, None) => mint_run_id().map_err(|error| error.into_parts().error),
    }
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
        sources: source_specs,
        mut kernels,
        topology,
        #[cfg(feature = "python")]
        py_kernels,
        packaged,
        ports: stage_ports,
        sinks: sink_specs,
        budget,
        cpu,
        elastic: elastic_budget,
        trace_path,
        staging_dir: explicit_staging_dir,
        staging_limit,
        staging_durable,
        staging_url,
        error_policy,
        ordered,
        sizer,
        policy,
        profiles_dir,
        object_store,
        host_profile,
        allow_gil,
        checkpoint,
        checkpoint_interval_ms,
        checkpoint_keep,
        resume,
        resume_auto,
        measured: carried,
        measured_by_node,
        notes: spec_notes,
        spec_digest,
    } = spec;
    let checkpoint_handle = components.checkpoint.clone();
    let _detach = crate::checkpoint::DetachOnDrop(checkpoint_handle.clone());
    let observer = components.observer.clone();

    // f.4: the GIL refusal comes before any Rust component starts (PY-I4).
    #[cfg(feature = "python")]
    if !py_kernels.is_empty() && moruna_adapters::python_gil_enabled() && !allow_gil {
        return Err(RunError::bare(MorunaError::Config {
            name: "python.allow_gil",
            msg: "Python kernels require a free-threaded interpreter (python3.13t or \
                  python3.14t); pass allow_gil=True to run serialised"
                .into(),
        }));
    }
    #[cfg(not(feature = "python"))]
    let _ = allow_gil;

    let mut notes: Vec<String> = Vec::new();

    // MH 4.7: `staging.durable` is `durable_staging=present` spelled where a host thinks of it.
    // It goes in through the profile override, so discovery's own refusal of an ephemeral
    // staging directory declared durable still applies (03 e.4).
    let host_profile = durable_profile(host_profile, staging_durable)?;

    // 1. Discover.
    let discovery_input = DiscoveryInput {
        explicit_budget: budget,
        explicit_cpu: cpu,
        explicit_staging_dir: explicit_staging_dir.clone(),
        explicit_spill_limit: staging_limit,
        profile_override: host_profile,
    };
    // The watcher reads the host's files when the facade discovered the host itself; a
    // caller that injected the findings describes a machine those files are not, so the watcher
    // follows only a source the caller also injected.
    let host_discovered = components.discovered.is_none();
    let discovered = match components.discovered {
        Some(discovered) => discovered,
        None => moruna_discovery::discover(&discovery_input)?,
    };

    // f.7: the manifest header before anything takes the run id, so the new process continues
    // the old run's identity. `resume_auto` looks for one here (MH 4.7), after discovery,
    // because the staging directory it looks in is discovery's answer.
    let resume = match (resume, resume_auto) {
        (Some(path), _) => Some(path),
        (None, false) => None,
        (None, true) => {
            let found = auto_manifest(
                discovered.profile.staging_dir.as_deref(),
                &kernels,
                &source_specs,
                spec_digest.as_deref(),
            )?;
            match &found {
                Some(path) => notes.push(format!("resume auto: resuming {}", path.display())),
                None => notes.push(
                    "resume auto: no manifest of this job in the staging directory; a fresh run"
                        .to_string(),
                ),
            }
            found
        }
    };
    let header = match &resume {
        Some(path) => Some(PlacementEngine::read_manifest_header(path)?),
        None => None,
    };
    // MH 4.3: a resume from a document is refused when the manifest was written by another
    // document, which the plan digest alone cannot see (a changed kernel hint, a changed sink).
    if let (Some(path), Some(digest)) = (&resume, &spec_digest) {
        check_spec_digest(path, digest)?;
    }
    let run_id = resolve_run_id(header.as_ref(), components.run_id)?;
    let node = match &header {
        Some(header) => header.node,
        None => moruna_kernel::LOCAL_NODE,
    };
    notes.extend(discovered.notes.iter().cloned());
    notes.extend(spec_notes);
    let limits = discovered.limits.clone();
    // 12 l (2026-11-14): what a heap frees at its top stays for the next morsel, a share of the
    // ceiling, so the buffers a run takes and frees morsel after morsel are not faulted in and
    // zeroed by the kernel each time. F8.9's threshold and heap count stand.
    // 12 l (2026-11-14): Moruna's own large buffers are kept for reuse while the run lasts, up
    // to a sixteenth of its ceiling, in a program that installs the counting allocator.
    crate::allocator::reuse::keep_up_to(limits.memory_ceiling / 16);
    if let Some((trim, pad)) = crate::allocator::keep_heap_tops(limits.memory_ceiling) {
        notes.push(format!(
            "the C heap keeps up to {trim} bytes free at a heap's top for the next morsel and grows \
             a heap {pad} bytes at a time (12 l)"
        ));
    }
    if let Some(observer) = &observer {
        observer.discovered(&limits, kernels.len());
    }
    let staging_dir = discovered.profile.staging_dir.clone();
    // RB4 (2026-11-13): a run follows the machine both ways by default. The pool is created up
    // to `budget.elastic.cpu_max` or, without one, up to the CPUs the machine has online, and
    // parked; the starting N is what is active. An explicit CPU count is the caller's own figure
    // and is not grown past. A machine that adds CPUs beyond those online at start (a hot-plug)
    // is followed only as far as the pool.
    let workers_start = workers_from(limits.cpu_quota);
    let machine_cores = match (&components.limits_source, host_discovered) {
        (Some(source), _) => source.read().cpus_online,
        (None, true) => moruna_discovery::machine_cores(),
        (None, false) => limits.cpu_quota,
    };
    let workers_max = match (elastic_budget.cpu_max, cpu) {
        (Some(cpu_max), _) => cpu_max.clamp(1, 1024),
        (None, Some(_)) => workers_start,
        (None, None) => workers_from(machine_cores).max(workers_start),
    };
    let workers_start = workers_start.min(workers_max);
    if workers_max > workers_start {
        notes.push(format!(
            "{workers_max} worker threads created and {workers_start} active: the pool follows \
             the machine up to {workers_max} CPUs"
        ));
    }

    // The limits watcher starts from what discovery found, and the sampler reads the limits
    // it publishes, so every sample carries the ceiling of its moment. Without
    // `budget.elastic.memory_max_bytes` the ceiling follows the machine up to all it has, which
    // e.3's derivation already holds it to (0.9 x memory.max, or 0.9 x total RAM); an explicit
    // budget is the caller's own figure and stays where it is set (RB4).
    let memory_max = match (elastic_budget.memory_max_bytes, budget) {
        (Some(max), _) => max,
        (None, Some(_)) => limits.memory_ceiling,
        (None, None) => u64::MAX,
    };
    if memory_max < limits.memory_ceiling {
        notes.push(format!(
            "budget.elastic.memory_max_bytes ({memory_max}) is below the starting ceiling \
             ({}); the run does not grow past its starting ceiling",
            limits.memory_ceiling
        ));
    }
    let watch = match (components.limits_source, host_discovered) {
        (Some(source), _) => Some(LimitsWatch::new(&discovered, &discovery_input, source)?),
        (None, true) => Some(LimitsWatch::host(&discovered, &discovery_input)?),
        (None, false) => {
            notes.push(
                "the limits watcher is off: the discovery findings were supplied by the caller \
                 and no limits source was"
                    .to_string(),
            );
            None
        }
    }
    .map(|watch| {
        let watch = watch.with_bounds(ElasticBounds {
            memory_max_bytes: memory_max.max(limits.memory_ceiling),
            cpu_max: f64::from(workers_max),
        });
        for subscriber in components.limits_subscribers {
            watch.subscribe(subscriber);
        }
        // MH 4.4: a packaged stage's ceiling moves with the run's (17 f.7).
        for stage in &packaged {
            let stage = stage.clone();
            watch.subscribe(Box::new(move |_| stage.limits_changed()));
        }
        Arc::new(watch)
    });

    // The sampler exists before the arena, because the arena's size depends on what the
    // process already holds (see `arena_host_bytes`).
    let sampler: Arc<dyn Sampler> = match components.sampler {
        Some(sampler) => sampler,
        None => {
            #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
            let native: Option<Arc<dyn Sampler>> = None;
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            let native = if moruna_micro::on_kernel() {
                let watched = watch.as_ref().map(|watch| watch.limits());
                let fixed = discovered.limits.clone();
                Some(
                    crate::native_account::sampler(
                        || {
                            // ACCOUNT_INFO(0) is handled for this actual calling
                            // Process before personality forwarding; no startup slot
                            // is taken and the Linux server's Account is never used.
                            let usage = moruna_sys::Usage::own().map_err(|e| MorunaError::Io {
                                op: "account_info",
                                target: "current native Account".into(),
                                msg: e.name().into(),
                            })?;
                            // Lifetime usage.peak is deliberately not an interval/RSS mark.
                            Ok(crate::native_account::Reading {
                                used: usage.used,
                                reserved: usage.reserved,
                                limit: usage.limit,
                            })
                        },
                        move || match &watched {
                            Some(limits) => {
                                let limits = limits.load();
                                (limits.memory_ceiling, limits.cpu_quota)
                            }
                            None => (fixed.memory_ceiling, fixed.cpu_quota),
                        },
                        moruna_sys::now,
                    )
                    .map_err(|error| *error)?,
                )
            } else {
                None
            };
            match native {
                Some(sampler) => sampler,
                None => match &watch {
                    Some(watch) => {
                        Arc::new(DiscoverySampler::following(&discovered, watch.limits())?)
                    }
                    None => Arc::new(DiscoverySampler::new(&discovered)?),
                },
            }
        }
    };
    // SB-I9: the memory a packaged stage's child holds is the run's, and every sample says so.
    let outside: Vec<_> = packaged.iter().filter_map(|p| p.outside()).collect();
    let sampler: Arc<dyn Sampler> = if outside.is_empty() {
        sampler
    } else {
        Arc::new(crate::packaged::WithOutside::new(sampler, outside))
    };

    let memory_basis = sampler.memory_basis();
    if memory_basis == moruna_kernel::MemoryBasis::NativeAccountCharge {
        notes.push("native Account charged-byte pressure, not process RSS; file residency and CPU billing are not measured; peaks are observed samples, not kernel interval marks; registered child reservations are added once by outside accounting".into());
    }

    // 2. Arena. The baseline is sampled here, before the region exists (12 f.1, 02 f.1,
    // 11 f.1): the arena touches every page at `new`, so a sample taken after it contains the
    // arena and subtracting it later would charge the arena twice.
    let baseline_bytes = sampler.sample()?.anon_bytes;
    // System design 3A: a function's memory is measured, never assumed. What `moruna check`
    // measured of each stage's function is in the store the run learns from, under the key the
    // check kept it against, which is known before any source is built, so the room above the
    // arena is sized from it here. A store that cannot be read sizes nothing: the run says so
    // and goes on as for functions nothing is known of.
    let found = match &components.learning {
        Some(learning) => learning.measured(&kernels).unwrap_or_else(|error| {
            notes.push(format!(
                "the functions' measured memory could not be read ({error}), so the run is \
                 sized as for functions nothing is known of"
            ));
            vec![None; kernels.len()]
        }),
        None => vec![None; kernels.len()],
    };
    // What the job came with stands first (on a node, the registry's, in the `spec` message:
    // system design 3E); the run's own store answers for a stage that came with none.
    // The node's own readings of each stage's function, which size its instances' accounts and
    // planes too; a registration's sizes the arena's share and the instances only (PM,
    // 2026-11-18: a client's process holds none of the function's interpreter).
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    let by_node: Vec<Option<moruna_cost::Memory>> = carried
        .iter()
        .enumerate()
        .map(|(at, memory)| {
            memory
                .clone()
                .filter(|_| measured_by_node.get(at).copied().unwrap_or(false))
        })
        .collect();
    let measured = with_carried(carried, &measured_by_node, found, &mut notes);
    let outside = out_of_arena(&kernels, &measured, workers_start, &mut notes);
    let kernel_state = expected_kernel_state(&kernels).saturating_add(outside.fixed_bytes);
    let sizing = Sizing {
        baseline: baseline_bytes,
        kernel_state,
        out_of_arena: outside.per_byte,
        page_bytes: limits.page_bytes,
        queues: u64::from(topology.queue_count()),
    };
    let arena_bytes = sizing.divide(limits.memory_ceiling, &mut notes);
    // N-7: a budget under the run's floor cannot be honoured, and is refused here, before
    // anything exists, with the floor named. A refusal is honest; an overshoot is not. An
    // injected allocator is the caller's arena and not the facade's to size.
    let floor = sizing.floor();
    if components.alloc.is_none() && limits.memory_ceiling < floor {
        return Err(MorunaError::Config {
            name: "budget.host",
            msg: format!(
                "the budget of {} bytes is below this run's floor of {floor} bytes, the least it \
                 can be honoured in: {}. The process held {} bytes before the arena existed; that \
                 figure is what it has resident, which counts pages an allocator is holding for \
                 its own reuse as well as pages in use, so a caller that has just built and freed \
                 a dataset may be holding far more than it is using and can hand it back. \
                 Otherwise give the run a budget of at least {floor} bytes, or start it in a \
                 process that holds less.",
                limits.memory_ceiling,
                sizing.floor_terms(),
                baseline_bytes,
            ),
        }
        .into());
    }
    // Stream C (data OS): on the Moruna kernel the arena is granted memory objects and the
    // data disks are the engine's own. Elsewhere nothing is granted and nothing changes.
    let granted = data_os::take()?;
    let (alloc, arena): (Arc<dyn Allocator>, Option<Arc<Arena>>) = match components.alloc {
        Some(alloc) => (alloc, None),
        None => {
            let config = ArenaConfig {
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
                pretouch: false,
            };
            let arena = data_os::arena(config, granted.as_ref())?;
            (arena.clone() as Arc<dyn Allocator>, Some(arena))
        }
    };

    // 3. Reactor. What the object stores may have in flight is the budget's (3A, 3.5b): the
    // arena's room for bytes in flight, shared by requests of at least S3's least part.
    let object_io = object_io_for(arena_bytes, &mut notes);
    let mut object_io_built = None;
    let (reactor, object_metadata): (Arc<dyn Reactor>, Option<Arc<dyn ObjectMetadata>>) =
        match components.reactor {
            Some(reactor) => (reactor, components.object_metadata),
            None => {
                object_io_built = Some(moruna_trace::ObjectIoReport {
                    room_bytes: object_room(arena_bytes),
                    requests: object_io.requests as u64,
                    part_bytes: object_io.part_bytes,
                });
                let built = moruna_reactor::Reactor::new(
                    ReactorConfig {
                        threads: config::REACTOR_THREADS,
                        object_io,
                        file_depth: config::REACTOR_FILE_DEPTH,
                        page_bytes: limits.page_bytes,
                        profile: discovered.profile.clone(),
                        devices: limits.devices.iter().map(|d| d.id).collect(),
                        object_store: object_store.clone(),
                    },
                    alloc.clone(),
                )?;
                data_os::reactor(granted.as_ref(), built.clone(), Some(built), alloc.clone())?
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
    // The limits timeline goes to the writer the report is computed from, whatever sink the
    // records go to.
    let limits_trace: Arc<dyn TraceSink> = trace_writer.clone();

    // 5. Sources and sinks built.
    let ctx = BuildCtx {
        discovered: discovered.clone(),
        alloc: alloc.clone(),
        reactor: reactor.clone(),
        object_metadata,
        run_id,
        arena_bytes,
        headroom_bytes: sizing
            .allowance(limits.memory_ceiling)
            .saturating_sub(arena_bytes),
        sinks: topology.sinks(),
        workers: workers_max,
        sink_buffer: Arc::new(std::sync::atomic::AtomicU64::new(0)),
    };
    // MH 4.10: the stages' ports answer to the run's cancel, and their waits are told to the
    // scheduler's counters (which the controller reads) and to the stage hook (which the host
    // hears). They are bound before any source or sink is built: an image's source or sink starts
    // its process as it is built, and a source plans there, asking its ports (0.6.3 bound them
    // after, so a source image's plan was refused, "the kernel called a port outside a run").
    let waits = moruna_kernel::StageWaits::new(components.stage_hook.clone());
    let run_ports = moruna_kernel::RunPorts {
        cancel: guard.token(),
        waits: waits.clone(),
    };
    for ports in &stage_ports {
        ports.bind_run(run_ports.clone());
    }
    // stream P: a packaged stage's own waits answer to the run's cancel too (a native stage's
    // waits on its tasks were never told it, so a cancel did not end a unit waiting on one).
    for stage in &packaged {
        stage.bind_run_ports(&run_ports);
    }
    let mut sources: Vec<Arc<dyn moruna_kernel::Source>> = Vec::with_capacity(source_specs.len());
    for spec in source_specs {
        sources.push(spec.build(&ctx)?);
    }
    // What an engine source's engine decided when it planned the read: true of the run whatever
    // becomes of it, since the plan was made (and its budgets charged) when the source was built.
    let mut built_sinks: Vec<Box<dyn Sink>> = Vec::with_capacity(sink_specs.len());
    for spec in sink_specs {
        built_sinks.push(spec.build(&ctx)?);
    }
    let sink_buffer_bytes = ctx.sink_buffer_bytes();
    // A sink that encodes files in the arena holds two file buffers of half its claim; a morsel
    // is at most half a file, so every morsel fits an ordinary file and none needs a buffer of
    // its own that the arena may no longer be able to give.
    let claim = match sink_buffer_bytes {
        0 => 0,
        _ => ctx.file_buffer_share(),
    };
    // A sink that encodes on workers (08 f.12) divides the same bytes into one buffer per
    // worker that may encode at once, so a morsel is at most half of one of those.
    let buffers = if built_sinks.iter().any(|sink| sink.encodes_on_workers()) {
        u64::from(workers_max.max(2))
    } else {
        2
    };
    let (morsel_max, lane) = morsel_cap(claim, buffers);
    if morsel_max < config::MORSEL_MAX_BYTES {
        notes.push(format!(
            "morsels are at most {morsel_max} bytes, half of one of the sink's {buffers} {lane} \
             byte file buffers"
        ));
    }
    let mut plans = Vec::with_capacity(sources.len());
    for source in &sources {
        plans.push(source.plan()?);
    }
    let plan_summary = summarise(&plans.concat());
    let repeatable = sources.iter().all(|source| source.repeatable());
    if resume.is_some() && !repeatable {
        return Err(RunError::bare(MorunaError::Resume(
            "the source is not repeatable, so this run cannot be resumed".into(),
        )));
    }
    // 12 f.7, SC f.1: a sink whose checkpoint is `None` before it is opened cannot be resumed,
    // and the scheduler turns checkpointing off for it; the report says so, as it does for a
    // one-shot source (a user's `moruna.Sink` without `checkpoint`, 08 f.10, is the usual case).
    let mut sink_handles = Vec::with_capacity(built_sinks.len());
    for sink in built_sinks {
        if checkpoint && sink.checkpoint()?.is_none() {
            notes.push("sink: no resume, the sink does not checkpoint".to_string());
        }
        sink_handles.push(SinkHandle::wrap(
            sink,
            ordered,
            config::ORDERING_BUFFER_BYTES,
        ));
    }

    // 6. Kernels built: bind the arena, walk the chain, build the controller's view of them.
    // 05 f.9, E13: one gate for the run, bound to every Python kernel and every packaged one; it
    // refuses only for those that leave `memory_guard` on, and counting needs none.
    #[cfg(feature = "python")]
    let python_present = !py_kernels.is_empty();
    #[cfg(not(feature = "python"))]
    let python_present = false;
    let gate = (python_present || !packaged.is_empty())
        .then(|| moruna_adapters::guard::MemoryGate::new(sampler.clone(), limits.memory_ceiling))
        .transpose()?;
    #[cfg(feature = "python")]
    for (_, py_kernel) in &py_kernels {
        py_kernel.bind_allocator(alloc.clone());
        if let Some(gate) = &gate {
            py_kernel.bind_gate(gate.clone());
        }
    }
    // Every kernel lets go of this run's arena and gate when the run ends, however it ends: a
    // kernel outlives its run inside its task object, and one that held the arena kept the
    // whole region mapped for the life of the process (0.6.3).
    #[cfg(feature = "python")]
    let _release = ReleaseRun(py_kernels.iter().map(|(_, k)| k.clone()).collect());
    if let Some(gate) = &gate {
        for stage in packaged.iter().filter(|stage| !stage.native_singleton()) {
            stage.bind(gate, &sampler, limits.memory_ceiling);
        }
    }
    // The run's DAG, checked here so `KernelInfo::schema_hash` is each stage's input schema hash
    // (its port 0's, 12 f.1); the scheduler checks it again at `new`.
    if let Some(history) = components.history {
        started.history = Some(history.start(
            &mut kernels,
            alloc.clone(),
            guard.token(),
            &run_id.to_hex(),
            resume.as_deref(),
        )?);
    } else if let Some(path) = &resume
        && crate::history::checkpoint_prefixes(path, 64 * 1024 * 1024)?.is_some()
    {
        return Err(RunError::bare(MorunaError::Resume(
            "checkpoint requires its original historical capture authority".into(),
        )));
    }
    let pipeline = Pipeline {
        sources: sources.clone(),
        kernels: kernels.clone(),
        sinks: sink_handles,
        topology: topology.clone(),
    };
    let schemas = moruna_scheduler::validate_graph(&pipeline)?;
    #[cfg_attr(
        not(all(target_os = "linux", target_arch = "x86_64")),
        allow(unused_mut)
    )]
    let mut kernel_infos: Vec<KernelInfo> = kernels
        .iter()
        .enumerate()
        .map(|(at, kernel)| {
            let stage = (at + 1) as moruna_kernel::StageId;
            let input = topology.inputs(stage).first().copied().unwrap_or(0);
            KernelInfo {
                stage,
                fingerprint: kernel.fingerprint(),
                schema_hash: schemas[usize::from(input)].hash(),
                hints: kernel.hints(),
                kind: kernel.kind(),
            }
        })
        .collect();

    // The room a stage's warm-up finds its other tasks do not need (system design 3A): the
    // limits watcher hands it to the arena, through the arena's arithmetic.
    let freed_room = Arc::new(std::sync::atomic::AtomicU64::new(0));
    // Native singleton commitments must also leave the controller's initial
    // minimum-morsel work funded, before any child is started.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    if let Some(gate) = &gate {
        // Each live instance of a stage is a child of its own (one task process each, one per
        // CPU of the job unless its function declares fewer, system design 3E): the share is a
        // child's, and each instance carves its account within it (stream E, for the gate's
        // native tasks on every core).
        let stages: Vec<&crate::packaged::Packaged> = packaged
            .iter()
            .filter(|stage| stage.native_singleton())
            .collect();
        let wanted =
            crate::native_account::native_instances(stages.iter().map(|s| s.kernel().kind()))
                .map_err(|error| *error)?;
        if !wanted.is_empty() {
            let planning = moruna_controller::fixed_commitment_budget(
                &moruna_controller::FixedCommitmentInput {
                    ceiling_bytes: limits.memory_ceiling,
                    baseline_bytes,
                    arena_bytes,
                    morsel_min: config::MORSEL_MIN_BYTES,
                    safety_initial: config::SAFETY_INITIAL,
                },
                &kernel_infos,
            )?;
            let physical = sizing
                .allowance(limits.memory_ceiling)
                .saturating_sub(arena_bytes);
            // The stages the node measured are sized from their readings first (system design
            // 3A), leaving the others room for one instance each at their least; the others then
            // share what is left by today's rule.
            let room = physical.min(planning);
            let stage_of = |stage: &crate::packaged::Packaged| {
                kernels
                    .iter()
                    .position(|k| std::ptr::addr_eq(Arc::as_ptr(k), Arc::as_ptr(&stage.kernel())))
            };
            let facts: Vec<Option<moruna_cost::Memory>> = stages
                .iter()
                .map(|stage| stage_of(stage).and_then(|at| by_node.get(at).cloned().flatten()))
                .collect();
            let others = facts.iter().filter(|f| f.is_none()).count() as u64;
            let (by_stage, used) = fund_by_node(
                room.saturating_sub(others * moruna_micro::layout::NATIVE_CHILD_MIN),
                &wanted,
                &facts,
            )?;
            for (at, sized) in by_stage.iter().enumerate() {
                let Some(sized) = sized else {
                    continue;
                };
                let stage = stages[at];
                let name = stage.fund(sized.instances);
                stage.size_measured(sized.piece, sized.account);
                notes.extend(moruna_adapters::instances::short_note(
                    &name,
                    sized.instances,
                    wanted[at],
                ));
                let plane = moruna_micro::layout::plane_for(sized.piece).1;
                notes.push(format!(
                    "{name}: {} instances sized from the node's readings of its function, each a \
                     plane of {plane} bytes for pieces of {} bytes and an account of {} bytes",
                    sized.instances, sized.piece, sized.account
                ));
                stage.bind(gate, &sampler, plane + sized.account);
            }
            let room = room.saturating_sub(used);
            let unmeasured: Vec<usize> = (0..stages.len())
                .filter(|&at| facts[at].is_none())
                .collect();
            let wanted_u: Vec<usize> = unmeasured.iter().map(|&at| wanted[at]).collect();
            // Memory bounds the instances and never fails a job that ran before (PM,
            // 2026-11-18): as many as the room funds at their least, at least one a stage.
            let funded = moruna_adapters::instances::fund(
                room,
                &wanted_u,
                moruna_micro::layout::PAGE,
                // stream P: a native task's account at its least beside its plane at its least.
                moruna_micro::layout::NATIVE_CHILD_MIN,
            )
            .map_err(|msg| MorunaError::Config {
                name: "budget.native_children",
                msg,
            })?
            .unwrap_or(moruna_adapters::instances::Funded {
                share: 0,
                counts: Vec::new(),
            });
            for ((stage, &asked), &instances) in unmeasured
                .iter()
                .map(|&at| stages[at])
                .zip(&wanted_u)
                .zip(&funded.counts)
            {
                let name = stage.fund(instances);
                notes.extend(moruna_adapters::instances::short_note(
                    &name, instances, asked,
                ));
                // stream P: a native task stands for what it holds (its account and its plane
                // at their largest), not for its whole share: the rest stays the run's.
                let share = funded.share.min(moruna_micro::layout::NATIVE_CHILD_MAX);
                stage.bind(gate, &sampler, share);
                // Its pieces no larger than its account holds by its function's measured line,
                // whoever measured it (system design 3A; PM, 2026-11-18).
                if let Some(memory) =
                    stage_of(stage).and_then(|at| measured.get(at).cloned().flatten())
                {
                    let piece = instance_piece(&memory, share);
                    stage.cap_pieces(piece);
                    if piece < u64::MAX {
                        notes.push(format!(
                            "{name}: pieces of at most {piece} bytes, what an instance's account of \
                             {} bytes holds by its function's measured memory",
                            moruna_micro::layout::native_split(share).1
                        ));
                    }
                }
                // A function the node has no readings of, and none resting on enough units to
                // stand for a warm-up, warms up on the job's first pieces on its first task, and
                // its others start once that sizes them (system design 3A; PM, 2026-11-18).
                let line = stage_of(stage).and_then(|at| measured.get(at).cloned().flatten());
                if warms_up(line.as_ref(), instances) {
                    let freed = Arc::clone(&freed_room);
                    let warms = stage.warm_up(
                        line.as_ref(),
                        Box::new(move |bytes| {
                            freed.fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
                        }),
                    );
                    if warms {
                        notes.push(format!(
                            "{name}: warms up on its first task before its other {} start",
                            instances - 1
                        ));
                    }
                }
            }
            // The controller sees the instances the run starts.
            for (info, kernel) in kernel_infos.iter_mut().zip(&kernels) {
                info.kind = kernel.kind();
            }
        }
    }

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
            return Err(RunError::bare(MorunaError::Config {
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
                    topology: topology.clone(),
                    budgets: budgets.clone(),
                    staging_dir: staging_dir.clone(),
                    durable_staging: discovered.profile.durable_staging.is_guaranteed(),
                    disk_budget,
                    // RB6: a segment holds the run's largest morsel. A fixed 128 MiB segment
                    // met a 256 MiB morsel on its first spill and the run ended ("a record of
                    // ... bytes does not fit a segment", the 0.6.5 benchmark's staging runs).
                    // Twice the most a morsel may be, so one is never the last thing to fit, and
                    // a whole number of 128 MiB units.
                    segment_bytes: staging_segment_bytes(morsel_max),
                    codec: moruna_kernel::StagingCodec::Raw,
                    page_bytes: limits.page_bytes,
                    gds: discovered.profile.gds.is_available()
                        && cfg!(feature = "gds")
                        && io_paths.gds,
                    devices: limits.devices.iter().map(|d| d.id).collect(),
                    plan_digests: plans.iter().map(|plan| plan_digest(plan)).collect(),
                    fingerprints: kernels.iter().map(|k| k.fingerprint()).collect(),
                    resume_policy: kernels.iter().map(|k| k.hints().resume).collect(),
                    config: with_segment_bytes(
                        with_spec_digest(
                            config::resolved(
                                limits.memory_ceiling,
                                disk_budget,
                                workers_max,
                                config::READAHEAD_SPLITS,
                                &error_policy,
                                ordered,
                                sizer,
                                checkpoint_enabled,
                                checkpoint_interval_ms,
                                &policy,
                                object_io,
                            ),
                            spec_digest.as_deref(),
                        ),
                        staging_segment_bytes(morsel_max),
                    ),
                    checkpoint_enabled,
                    eviction: Arc::new(moruna_placement::catalog::KnownFuture),
                },
                alloc.clone(),
                reactor.clone(),
            )?;
            engine = Some(built.clone());
            built
        }
    };
    started.placement = Some(placement.clone());
    for (index, source) in sources.iter().enumerate() {
        if source.repeatable() {
            continue;
        }
        // 07 SO-I8, PL-I6: a one-shot source's queues are staged rather than evicted. An
        // iterator source is one; a `moruna.Source` that sets `repeatable = False` is the other
        // (07 e.6).
        for &queue in topology.source_outputs(index as u16) {
            placement.set_staging(queue, true);
        }
        notes.push("source is not repeatable (an iterator source, or repeatable = False): no resume, Q0 staged".to_string());
    }

    // f.7: restore before the scheduler is built, so its refusals land before the sink.
    let resume_point = match &resume {
        Some(path) => Some(placement.restore(
            path,
            &plans,
            &kernels.iter().map(|k| k.fingerprint()).collect::<Vec<_>>(),
        )?),
        None => None,
    };

    // 8. Scheduler `new`: validates the chain, opens the sink on a fresh run, spawns the
    // workers parked.
    let scheduler = Arc::new(Scheduler::new(
        SchedulerConfig {
            workers_max,
            workers_active: workers_start,
            read_ahead: config::READAHEAD_SPLITS,
            sink_concurrency: config::SINK_CONCURRENCY,
            error_policy,
            initial_morsel_target: config::MORSEL_PROBE_BYTES,
            morsel_min: config::MORSEL_MIN_BYTES,
            morsel_max,
            checkpoint_enabled,
            checkpoint_interval_ms,
            checkpoint_keep,
            heartbeat_interval_ms: config::HEARTBEAT_INTERVAL_MS,
            resuming: resume.is_some(),
            node,
            policy: policy.clone(),
            policies: moruna_scheduler::policies::standard(),
            clock: Arc::new(moruna_scheduler::SystemClock::default()),
            // Discovery measures no disk bandwidth: the scheduler's declared figure, what a
            // lookahead plays a spill and a read-back at.
            disk_bytes_per_second: moruna_scheduler::DISK_BYTES_PER_SECOND,
            // Stream C (data OS, C5): each worker placed on a core as it starts.
            on_worker_start: data_os::worker_start(granted.as_ref()),
        },
        pipeline,
        placement.clone(),
        alloc.clone(),
        trace_sink,
        sampler.clone(),
    )?);
    started.scheduler = Some(scheduler.clone());
    if let Some(learning) = components.learning {
        let keys = learning.keys(&kernels, &topology, &schemas)?;
        scheduler.set_cost(learning.online, keys, Some(Arc::from(run_id.to_hex())))?;
    }
    if let Some(hook) = components.stage_hook.clone() {
        scheduler.set_stage_hook(hook);
    }
    if let Some(sink) = components.output.clone() {
        scheduler.set_output(sink);
    }
    scheduler.set_waits(waits);
    if staging_url.is_some() {
        return Err(RunError::bare(MorunaError::Config {
            name: "staging.url",
            msg: "raw manifest copying is not portable; use an explicit scoped native checkpoint port".into(),
        }));
    }
    if let Some(checkpoints) = &components.checkpoints {
        if checkpoints.access.run != run_id.to_hex() || !checkpoint_enabled {
            return Err(RunError::bare(MorunaError::Resume(
                "portable checkpoint port identity or checkpoint readiness mismatch".into(),
            )));
        }
        let engine = engine.as_ref().ok_or_else(|| {
            RunError::bare(MorunaError::Resume(
                "portable checkpoints require the native placement snapshot authority".into(),
            ))
        })?;
        // Prevent eviction-to-requery from creating missing immutable dependencies. Every
        // queue may stage its actual bytes from the beginning, including repeatable sources.
        for queue in 0..topology.queue_count() {
            placement.set_staging(queue, true);
        }
        scheduler.set_manifest_preparation(
            checkpoints.preparation(
                alloc.clone(),
                engine.clone(),
                started
                    .history
                    .as_ref()
                    .map(crate::history::HistoryRun::checkpoint),
            ),
        );
    }
    if components.checkpoints.is_none()
        && checkpoint_enabled
        && let Some(history) = &started.history
    {
        scheduler.set_manifest_preparation(history.checkpoint().preparation(alloc.clone()));
    }
    if let Some(handle) = &checkpoint_handle {
        handle.attach(&scheduler);
    }

    // 9. Instances: eagerly, before the baseline is sampled.
    match resume_point {
        Some(point) => scheduler.apply_resume_point(point)?,
        None => scheduler.init_instances()?,
    }

    // Native singleton init has already carved and retained its real Account. These are
    // the same held limits WithOutside adds to every sample, not process growth to fit
    // independently to each stage. Keep them funded once even if a child later retires.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    let fixed_native_bytes = packaged
        .iter()
        .filter(|stage| stage.native_singleton())
        .try_fold(0u64, |total, stage| -> moruna_kernel::Result<u64> {
            let outside = stage.outside().ok_or_else(|| MorunaError::Config {
                name: "budget.native_children",
                msg: "initialized native singleton has no Account observer".into(),
            })?;
            let held = outside.in_use()?;
            if held == 0 {
                return Err(MorunaError::Config {
                    name: "budget.native_children",
                    msg: "initialized native singleton has no retained Account limit".into(),
                });
            }
            total.checked_add(held).ok_or_else(|| MorunaError::Config {
                name: "budget.native_children",
                msg: "actual native Account commitment total overflow".into(),
            })
        })?;
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    let fixed_native_bytes = 0;

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
            morsel_max,
            probe_bytes: config::MORSEL_PROBE_BYTES,
            sizer,
            fallback_error_ratio: config::SIZER_FALLBACK_ERROR_RATIO,
            profiles_dir,
            disk_budget,
            arena_bytes,
            baseline_bytes,
            fixed_native_bytes,
            sink_buffer_bytes,
            checkpoint_enabled,
            checkpoint_interval_ms,
        },
        scheduler.clone() as Arc<dyn Knobs>,
        scheduler.clone() as Arc<dyn StatsSource>,
        scheduler.clone() as Arc<dyn Prober>,
        sampler.clone(),
        trace_tail,
        placement.clone(),
        topology.clone(),
        kernel_infos,
    )?);
    let external_state_stages: Vec<_> = kernels
        .iter()
        .enumerate()
        .filter(|(_, kernel)| !kernel.state_uses_arena())
        .map(|(at, _)| (at + 1) as moruna_kernel::StageId)
        .collect();
    controller.set_external_state(&external_state_stages)?;
    started.controller = Some(controller.clone());
    // A `Weak`, not a clone: the controller holds the scheduler as its `Knobs`, `StatsSource`
    // and `Prober`, so a hook that held the controller strongly would close a reference cycle
    // and neither would ever be dropped. The arena went with them, and a second `Runtime::run`
    // in the same process then sampled a baseline that still contained the first run's whole
    // region (12 f.1, 11 f.1). The controller outlives every record: it is held here and in
    // `started` until after the scheduler has stopped.
    let hooked = Arc::downgrade(&controller);
    let watching = observer.clone();
    scheduler.set_record_hook(Arc::new(move |record| {
        if let Some(controller) = hooked.upgrade() {
            controller.on_record(record);
        }
        if let Some(observer) = &watching {
            observer.on_record(record);
        }
    }));
    if let Some(observer) = &observer {
        observer.attach(&scheduler, &controller, &placement, &sampler);
    }
    // Detached however the run ends, so a watcher never holds the last reference to anything.
    let _detach = DetachOnDrop(observer.clone());

    // 11. prepare, probe, start, run.
    controller.prepare()?;
    if resume.is_some() {
        controller.probe_missing()?;
    } else {
        controller.probe_all()?;
    }
    controller.start()?;
    // The limits watcher, from the moment the controller is running until the scheduler
    // returns. The arena arithmetic is the same function that sized the first region, with
    // everything but the ceiling fixed at what the run started with.
    let watcher = watch.map(|watch| {
        Watcher::start(
            WatchCtx {
                watch,
                trace: limits_trace,
                arena,
                controller: controller.clone(),
                scheduler: scheduler.clone(),
                sizing: {
                    let freed = Arc::clone(&freed_room);
                    Box::new(move |ceiling| {
                        sizing
                            .divide(ceiling, &mut Vec::new())
                            .saturating_add(freed.load(std::sync::atomic::Ordering::Relaxed))
                    })
                },
                start_ns,
                workers_max,
                tick: std::time::Duration::from_millis(config::TICK_MS),
            },
            WatchState::new(arena_bytes),
        )
    });
    // stream P: a cancelled run says where its workers are as the cancel reaches it, into the
    // run's output, which the store tells as events at once: a run that then hangs in its
    // workers and is shut down still leaves its diagnosis (the NYC gate, 2026-11-17).
    let diagnosed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let diagnosis = {
        let token = guard.token();
        let done = Arc::clone(&diagnosed);
        let scheduler = scheduler.clone();
        let controller = started.controller.clone();
        let stages = packaged.clone();
        let output = components.output.clone();
        std::thread::Builder::new()
            .name("moruna-diagnosis".to_string())
            .spawn(move || {
                use std::sync::atomic::Ordering::Relaxed;
                while !done.load(Relaxed) && !token.is_cancelled() {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                // A run that ended without a cancel has nothing to say here; a cancelled one says
                // it however soon it ended.
                if !token.is_cancelled() {
                    return;
                }
                let mut lines = Vec::new();
                if let Some(controller) = &controller {
                    lines.extend(controller.summary().notes);
                }
                lines.extend(scheduler.worker_notes());
                lines.extend(stages.iter().filter_map(|stage| stage.task_note()));
                for line in lines {
                    tracing::warn!(target: "run.cancel", "{line}");
                    if let Some(output) = &output {
                        output.record(
                            None,
                            Some(moruna_kernel::output::Level::Warning),
                            &format!("at the cancel: {line}"),
                        );
                    }
                }
            })
            .ok()
    };
    let outcome = if resume.is_some() {
        scheduler.run_resumed(guard.token())
    } else {
        scheduler.run(guard.token())
    };
    diagnosed.store(true, std::sync::atomic::Ordering::Relaxed);
    if let Some(diagnosis) = diagnosis {
        let _ = diagnosis.join();
    }
    let (drains, watch_notes) = match watcher {
        Some(watcher) => {
            let watched = watcher.stop();
            (watched.drains, watched.notes)
        }
        None => (Vec::new(), Vec::new()),
    };
    let outcome = outcome?;
    if let Some(history) = started.history.take() {
        // A successful error-policy skip is not complete historical task coverage.
        let lost = scheduler
            .scheduler_stats()
            .per_stage
            .iter()
            .any(|stage| stage.errors != 0 || stage.skipped != 0);
        history.finish(matches!(outcome, RunOutcome::Completed { .. }) && !lost)?;
    }
    notes.extend(watch_notes);

    // 12. stop, finish, report. One last sample, so a rise of the process's high-water mark
    // since the last tick is in the peak the report states.
    let accounting_error = sampler.sample().err();
    let accounting_failed = accounting_error.is_some();
    let mut process_peak = sampler.process_peak();
    let mut process_usage = sampler.process_usage();
    let (mut exit, outcome_manifest) = report::exit_of(&outcome);
    let final_failure = final_accounting_failure(&outcome, accounting_error, &mut notes);
    if let Some(cause) = &final_failure {
        exit = moruna_trace::ExitReason::Terminated {
            diagnostic: cause.to_string(),
        };
    }
    if accounting_failed {
        // Earlier observations remain evidence, but cannot certify the final window.
        process_peak.exact = false;
        process_usage.measured = false;
    }
    // 10 f.3: what the policy decided, read before the scheduler is unwound.
    let policy = started.scheduler.as_ref().map(|scheduler| {
        moruna_trace::PolicyReport::of(
            &moruna_kernel::StatsSource::scheduler_stats(scheduler.as_ref()).policy,
        )
    });
    // The gate's split: each source's reads apart from its decodes, each sink's encodes apart
    // from its writes, read before the scheduler is unwound.
    let io = started
        .scheduler
        .as_ref()
        .map(|scheduler| report::io_report(&scheduler.io_times()))
        .unwrap_or_default();
    let summary = started
        .controller
        .take()
        .map(|controller| controller.stop());
    // stream P: where each worker's time went, as the run ends.
    let worker_notes = started
        .scheduler
        .as_ref()
        .map(|scheduler| scheduler.worker_notes())
        .unwrap_or_default();
    let (view, shutdown_notes) = started.unwind();

    notes.extend(shutdown_notes);
    notes.extend(worker_notes);
    // stream P: each native stage's tasks, once they have ended (E's gate shows them).
    notes.extend(packaged.iter().filter_map(|stage| stage.task_note()));
    // Stream C (data OS, C3): a completed run's outputs are one layer on `disk.out`.
    let layer_failure = data_os::finish(
        granted.as_ref(),
        matches!(exit, moruna_trace::ExitReason::Completed),
        alloc.clone(),
        &mut notes,
    );
    let manifest = outcome_manifest.or_else(|| match (checkpoint_keep, &engine) {
        (true, Some(engine)) => engine.manifest_path(),
        _ => None,
    });
    // f.7: on `Completed` the run directory goes, unless `checkpoint_keep` kept it.
    if matches!(exit, moruna_trace::ExitReason::Completed)
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
        drains,
        memory_basis,
        process_peak,
        process_usage,
        sinks: match &outcome {
            RunOutcome::Completed { sinks } => sinks.clone(),
            RunOutcome::Terminated { .. } | RunOutcome::Cancelled { .. } => Vec::new(),
        },
        policy,
        buffers: engine.as_ref().map(|engine| {
            moruna_trace::BufferReport::of(engine.eviction_order(), &engine.catalog_stats())
        }),
        io,
        object_io: object_io_built,
    });
    let view = view.ok_or_else(|| {
        RunError::bare(MorunaError::Io {
            op: "trace.finish",
            target: "the run trace".to_string(),
            msg: "the trace writer produced no view, so no report can be computed".to_string(),
        })
    })?;
    let mut computed = report::compute(&view, &limits, &meta);
    let digest_of = |q: Option<moruna_kernel::StageId>| {
        q.and_then(|q| schemas.get(usize::from(q)))
            .map(moruna_kernel::SourceSchema::hash)
    };
    computed.part_digests = moruna_trace::PartDigests {
        source_schemas: (0..topology.sources())
            .map(|s| digest_of(topology.source_outputs(s).first().copied()))
            .collect(),
        stage_schemas: (1..=topology.stages())
            .map(|stage| digest_of(topology.inputs(stage).first().copied()))
            .collect(),
        stage_fingerprints: kernels.iter().map(|k| k.fingerprint().0).collect(),
        sink_schemas: (0..topology.sinks())
            .map(|s| digest_of(topology.sink_input(s)))
            .collect(),
    };
    computed.instance_memory = instance_memory(&kernels, &packaged);
    if let Some(cause) = final_failure.or(layer_failure) {
        return Err(RunError::bare(cause)
            .with_report(Some(computed))
            .with_manifest(manifest));
    }
    match outcome {
        RunOutcome::Completed { .. } => Ok(computed),
        RunOutcome::Terminated { diagnostic, .. } => Err(RunError::bare(diagnostic)
            .with_report(Some(computed))
            .with_manifest(manifest)),
        RunOutcome::Cancelled { .. } => Err(RunError::bare(MorunaError::Cancelled)
            .with_report(Some(computed))
            .with_manifest(manifest)),
    }
}

/// Preserve an established terminal cause while making failed cleanup accounting visible.
fn final_accounting_failure(
    outcome: &RunOutcome,
    failure: Option<MorunaError>,
    notes: &mut Vec<String>,
) -> Option<MorunaError> {
    let cause = failure?;
    notes.push(format!("final accounting sample failed: {cause}"));
    matches!(outcome, RunOutcome::Completed { .. }).then_some(cause)
}

/// `staging.durable` as the profile override discovery is handed (MH 4.7, 03 d.1). A caller's
/// profile that already declares `durable_staging` must agree with it.
fn durable_profile(
    profile: Option<moruna_kernel::HostProfile>,
    durable: bool,
) -> Result<Option<moruna_kernel::HostProfile>> {
    use moruna_kernel::Guarantee;
    if !durable {
        return Ok(profile);
    }
    let mut profile = profile.unwrap_or_default();
    match profile.durable_staging {
        Guarantee::Unknown | Guarantee::Present => {}
        Guarantee::Absent | Guarantee::Probed(_) => {
            return Err(RunError::bare(MorunaError::Config {
                name: "staging.durable",
                msg: format!(
                    "staging.durable is set and host_profile.durable_staging is {:?}; the two \
                     must agree",
                    profile.durable_staging
                ),
            }));
        }
    }
    profile.durable_staging = Guarantee::Present;
    Ok(Some(profile))
}

/// `resume = "auto"` (MH 4.7): the newest manifest in the staging directory that this run could
/// resume, judged by the kernels, the job document's digest when the run has one, and, when
/// the source is already built, the plan. A source
/// still to be built cannot be asked for its plan before the run id exists (PY-I1 builds it
/// after the trace), so for one the plan is left to `restore`, which refuses a mismatch by
/// name. No staging directory is no manifest.
fn auto_manifest(
    staging_dir: Option<&std::path::Path>,
    kernels: &[Arc<dyn Kernel>],
    sources: &[crate::spec::SourceSpec],
    spec_digest: Option<&str>,
) -> Result<Option<PathBuf>> {
    let Some(dir) = staging_dir else {
        return Ok(None);
    };
    let fingerprints: Vec<moruna_kernel::Fingerprint> =
        kernels.iter().map(|k| k.fingerprint()).collect();
    let mut plan_digests = Some(Vec::with_capacity(sources.len()));
    for source in sources {
        match (source, plan_digests.as_mut()) {
            (crate::spec::SourceSpec::Built(source), Some(digests)) => {
                digests.push(plan_digest(&source.plan()?));
            }
            _ => plan_digests = None,
        }
    }
    let want = moruna_placement::manifest::ManifestMatch {
        fingerprints: &fingerprints,
        plan_digests,
        spec_digest,
    };
    Ok(PlacementEngine::find_resumable_manifest(dir, &want)?)
}

/// Detaches an observer when the run's lifecycle function returns, by any path.
struct DetachOnDrop(Option<Arc<crate::observe::RunObserver>>);

impl Drop for DetachOnDrop {
    fn drop(&mut self) {
        if let Some(observer) = &self.0 {
            observer.detach();
        }
    }
}

/// The resolved configuration table with the job document's digest added under `spec.digest`
/// (MH 4.3), so the manifest records which document wrote it.
fn with_spec_digest(mut config: serde_json::Value, digest: Option<&str>) -> serde_json::Value {
    if let (Some(digest), serde_json::Value::Object(map)) = (digest, &mut config) {
        map.insert(
            "spec.digest".to_string(),
            serde_json::Value::String(digest.to_string()),
        );
    }
    config
}

/// Refuse a resume whose manifest was written under another job document (MH 4.3, MH 4.1). A
/// manifest that records no digest was written by a library run, and is left to the checks
/// `Placement::restore` makes.
fn check_spec_digest(manifest: &std::path::Path, digest: &str) -> Result<()> {
    let text = std::fs::read_to_string(manifest).map_err(|e| {
        RunError::bare(MorunaError::Resume(format!(
            "{}: the manifest could not be read: {e}",
            manifest.display()
        )))
    })?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        RunError::bare(MorunaError::Resume(format!(
            "{}: the manifest is not JSON: {e}",
            manifest.display()
        )))
    })?;
    match value.get("config").and_then(|c| c.get("spec.digest")) {
        Some(serde_json::Value::String(theirs)) if theirs != digest => {
            Err(RunError::bare(MorunaError::Resume(format!(
                "{}: the manifest was written by the job document {theirs}, and this one is {digest}",
                manifest.display()
            ))))
        }
        _ => Ok(()),
    }
}

/// The Python stages' interpreter state, for `RunMeta.gil` (12 f.2, f.4).
fn gil_states(
    #[cfg(feature = "python")] py_kernels: &[(
        moruna_kernel::StageId,
        Arc<moruna_adapters::PyKernel>,
    )],
) -> Vec<(moruna_kernel::StageId, moruna_kernel::GilState)> {
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

/// Everything the arena arithmetic needs but the ceiling, so the limits watcher can re-run it
/// for each new ceiling (12 f.1, MH 4.4).
#[derive(Clone, Copy, Debug)]
struct Sizing {
    baseline: u64,
    /// Held outside the arena whatever the morsel: the stateful kernels' declared state, and
    /// what the functions were measured to hold whatever their batch (system design 3A).
    kernel_state: u64,
    out_of_arena: f64,
    page_bytes: usize,
    /// The run's queues: the buffer catalog's floor holds every one's promotion window.
    queues: u64,
}

impl Sizing {
    /// The smallest ceiling at which this run can be honoured (N-7): the arena at least
    /// `ARENA_FLOOR_BYTES`, over what the process already held, the reserve and the declared
    /// kernel state. Below it no arrangement of the knobs
    /// fits, so a budget below it is refused before the run starts.
    fn floor(&self) -> u64 {
        let fits = |ceiling: u64| {
            self.allowance(ceiling) >= ARENA_FLOOR_BYTES && self.catalog_floor(ceiling).holds()
        };
        // Every term grows with the ceiling, so the floor is found by bisection, to a page.
        let (mut low, mut high) = (0u64, 1u64 << 50);
        while high - low > 4096 {
            let mid = low + (high - low) / 2;
            if fits(mid) {
                high = mid;
            } else {
                low = mid;
            }
        }
        high
    }

    /// The buffer catalog's floor at `ceiling` (placement f.18 rule 13, KD7b): every queue's
    /// promotion window and one unit's input and output, at the smallest morsel the controller
    /// can make, within the placement's budget (the ceiling less the reserve).
    fn catalog_floor(&self, ceiling: u64) -> moruna_kernel::CatalogFloor {
        let budget = (ceiling as f64 * (1.0 - config::RESERVE_FRACTION as f64)) as u64;
        moruna_kernel::CatalogFloor::of(
            self.queues,
            self.queues * u64::from(moruna_kernel::DEFAULT_PROMOTION_WINDOW),
            config::MORSEL_MIN_BYTES,
            budget,
        )
    }

    /// What the floor is made of, for the refusal.
    fn floor_terms(&self) -> String {
        let mut terms = vec![format!("an arena of {ARENA_FLOOR_BYTES} bytes")];
        terms.push(format!(
            "a buffer catalog of {}",
            self.catalog_floor(0).terms()
        ));
        if self.kernel_state > 0 {
            terms.push(format!(
                "{} bytes of declared kernel state and measured function memory",
                self.kernel_state
            ));
        }
        terms.push(format!(
            "and the reserve, {} of the budget",
            config::RESERVE_FRACTION
        ));
        terms.join(", ")
    }

    /// `ceiling - baseline - reserve - expected kernel state`: the bytes the run may hold at all.
    fn allowance(&self, ceiling: u64) -> u64 {
        let reserve = (ceiling as f64 * f64::from(config::RESERVE_FRACTION)) as u64;
        ceiling
            .saturating_sub(self.baseline)
            .saturating_sub(reserve)
            .saturating_sub(self.kernel_state)
    }

    /// The arena's host bytes for `ceiling`: sized over the allowance, `ceiling - baseline -
    /// reserve - expected kernel state`, the bytes the run may hold at all.
    fn divide(&self, ceiling: u64, notes: &mut Vec<String>) -> u64 {
        arena_host_bytes(
            ceiling,
            self.baseline,
            config::RESERVE_FRACTION,
            self.kernel_state,
            self.out_of_arena,
            self.page_bytes,
            notes,
        )
    }
}

// Stream C (2026-11-17): a lane's buffer, and so a morsel, is capped whatever the budget.
/// The morsel cap for a sink that claimed `claim` bytes of the arena into `buffers` file
/// buffers, and the size of one of those buffers: a morsel is at most half a buffer, so every
/// morsel fits an ordinary file, and a buffer is at most [`moruna_sinks::LANE_MAX_BYTES`], so
/// a larger budget buys room and concurrency, not larger first morsels. No claim, no cap
/// below `MORSEL_MAX_BYTES`.
fn morsel_cap(claim: u64, buffers: u64) -> (u64, u64) {
    if claim == 0 {
        return (config::MORSEL_MAX_BYTES, 0);
    }
    let lane = (claim / buffers.max(1)).min(moruna_sinks::LANE_MAX_BYTES);
    let morsel = (lane / 2).clamp(config::MORSEL_MIN_BYTES, config::MORSEL_MAX_BYTES);
    (morsel, lane)
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
         existed, the {reserve} byte reserve and the {kernel_state} bytes held outside it for \
         the stateful kernels' declared state, what the functions were measured to hold whatever their batch, and a plan's operators; the share is set by the {a_anon:.2} bytes the chain is expected to allocate \
         outside the arena per byte in flight, and {headroom} bytes are left above the arena for \
         it"
    ));
    arena
}

/// The arena's own cost per byte in flight. RC f.3 divides the arena in half, one half for what
/// the workers hold and one for the queues and the read-ahead behind them, so a byte in flight
/// costs the arena two.
const ARENA_INFLIGHT_COST: f64 = 2.0;

/// The bytes an arena of `arena_bytes` may have in flight: half of it, a byte in flight costing
/// the arena two (`ARENA_INFLIGHT_COST`). The object stores' requests share it (3A, 3.5b).
pub(crate) fn object_room(arena_bytes: u64) -> u64 {
    (arena_bytes as f64 / ARENA_INFLIGHT_COST) as u64
}

/// What the budget lets the object stores have in flight (3A, item 3.5b), derived and never set:
/// N requests at once is the arena's room over S3's least part, at least one and at most the
/// job's socket budget (3D: `moruna_storage::net::SOCKETS`, the engine's copy of the node's
/// `egress::SOCKETS`), and each request's part is the room over N, at least S3's least part. A
/// range a request reads lands in the arena buffer charged for it and a part it writes is a view
/// of the sink's, so a request holds outside the arena only the piece on its way. Named in
/// `notes`, and in the report's `object_io`.
fn object_io_for(arena_bytes: u64, notes: &mut Vec<String>) -> moruna_reactor::ObjectIo {
    let room = object_room(arena_bytes);
    let io = moruna_reactor::ObjectIo::for_room(room, moruna_storage::net::SOCKETS);
    notes.push(format!(
        "object stores: {} requests at once in parts of {} bytes: the {room} bytes the \
         {arena_bytes} byte arena has in flight over S3's least part of {} bytes, at most the \
         job's {} sockets",
        io.requests,
        io.part_bytes,
        moruna_reactor::S3_PART_MIN_BYTES,
        moruna_storage::net::SOCKETS,
    ));
    io
}

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
/// That class has to be free all at once and one buffer of it is live for as long as a file is
/// open, so the floor is that class plus the slots the source drive needs behind it. The
/// read-ahead holds `readahead.splits` of them from the first split, and the drive has to be able
/// to read the *next* split while they are full, so the floor is
/// `128 MiB + (readahead.splits + 1) x morsel.probe_bytes`.
///
/// The `+ 1` is not a margin. A floor of exactly `128 MiB + readahead.splits x
/// morsel.probe_bytes` is 160 MiB, and the sink's 128 MiB class plus two 16 MiB read-ahead slots
/// is 160 MiB to the byte: the job of `python/tests/test_budget.py` filled its arena to
/// 167247872 of 167772160 bytes and then failed to allocate 780000 bytes for the next split
/// (2026-09-23). The figure this replaces, `256 MiB + readahead`, was the same knife edge one
/// class up, and was reached by asking only what the sink needs in order to *open*.
///
/// It is capped by the allowance, so a budget too small for it gets its whole allowance and
/// nothing is conjured. Changing the first term is a change in `moruna-sinks` and not here: it is
/// whatever class the sink's file buffer asks for at the default row group (08 f.1). That it is
/// the *default* row group and not the configured one is a limitation of `SinkSpec`, which is a
/// built sink or a factory and cannot be asked what it will allocate before it exists: a run with
/// a 16 MiB row group still pays the default's floor.
/// Revised 2026-09-23. The first term was 128 MiB because the sink demanded one whole size
/// class of that size before it would open, which was the configured row group's class. That
/// requirement is gone (08 f.1: a row group is a target, not a requirement), so the floor no
/// longer has to carry it, and carrying it did real harm: at a 512 MiB ceiling on a host whose
/// resting footprint is larger than this laptop's, the floor exceeded the whole allowance, the
/// arena took all of it, and the only governed headroom left for a Python kernel's own objects
/// was the reserve, so the controller refused morsel 2 with a 115 MB footprint against a 53 MB
/// budget. The floor is now one smallest class for the sink plus the read-ahead slots, which is
/// what the arena needs in order to be useful rather than what the sink once needed in order to
/// open.
const ARENA_FLOOR_BYTES: u64 =
    (16 << 20) + (config::READAHEAD_SPLITS as u64 + 1) * config::MORSEL_PROBE_BYTES;

/// The chain's expected out-of-arena cost per byte in flight (12 f.1): the largest
/// `KernelHints::expected_amplification` any stage declares, counting a stage that declares
/// nothing as `DEFAULT_OUT_OF_ARENA_AMPLIFICATION`. The largest rather than the sum, because the
/// anonymous inequality's `share` is `active_workers / stages`, so what it sums to over a chain
/// of equal stages is one stage's cost; the largest is the conservative reading of that.
///
/// A chain with no kernels at all is a copy, and a copy whose source is not an engine is not
/// free outside the arena: the source decodes every range on the heap before copying it in, and
/// the allocator holds what it freed until it is asked for again. It is charged
/// `COPY_OUT_OF_ARENA_AMPLIFICATION`. Until 2026-09-27 it was charged nothing and given the whole
/// allowance as arena, which left the reserve as all the room there was for those bytes: a
/// Parquet file copied at 256 MiB peaked at 0.99 of its ceiling with a 177 MB arena it used a
/// fraction of, measured by the operating system (F8.9).
fn expected_out_of_arena_amplification(kernels: &[Arc<dyn moruna_kernel::Kernel>]) -> f64 {
    if kernels.is_empty() {
        return COPY_OUT_OF_ARENA_AMPLIFICATION;
    }
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

/// The largest piece one native task of a stage takes in its `share` by its function's measured
/// line (system design 3A; PM, 2026-11-18): piece <= (account - fixed) / per-byte, its account
/// what the share leaves beside its plane (`moruna_micro::layout::native_split`); at least one
/// byte, and `u64::MAX` for a line that does not grow.
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code)
)]
fn instance_piece(memory: &moruna_cost::Memory, share: u64) -> u64 {
    let account = moruna_micro::layout::native_split(share).1;
    let piece = memory.piece_within(account as f64);
    if piece.is_finite() {
        (piece as u64).max(1)
    } else {
        u64::MAX
    }
}

/// The units a function's measurement rests on past which a stage does not warm up on the
/// job's first pieces (system design 3A): the controller's rule for a profile it trusts.
const WARM_UP_SKIPPED_AT: u64 = 32;

/// Whether a stage of `instances` tasks whose function the node has no readings of warms up on
/// its first task (system design 3A; PM, 2026-11-18): when what it carries (`line`, a
/// registration's or the store's) rests on fewer than [`WARM_UP_SKIPPED_AT`] units, or there is
/// none, and it has other tasks to size.
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code)
)]
fn warms_up(line: Option<&moruna_cost::Memory>, instances: usize) -> bool {
    instances > 1 && line.is_none_or(|m| m.units < WARM_UP_SKIPPED_AT)
}

/// What the run sizes a measured stage's instances at (system design 3A).
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code)
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SizedByNode {
    /// The instances it starts.
    instances: usize,
    /// The pieces each is given, the largest.
    piece: u64,
    /// Each one's account, in whole pages.
    account: u64,
}

/// The native stages whose functions the node measured (`facts[at]`, its own readings of the
/// function's account), sized from those readings (system design 3A; PM, 2026-11-18): an
/// instance costs its plane for its piece and its account at that piece
/// (`moruna_cost::Memory::account`), the largest piece at which every instance asked for fits
/// in `room` is taken, else the least piece with as many as fit there
/// (`moruna_adapters::instances::fund_measured`). Each stage's sizing, `None` for one the node
/// did not measure, and the bytes the measured stages take of the room. Refused, naming the
/// budget, when one instance a measured stage does not fit at the least piece.
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code)
)]
fn fund_by_node(
    room: u64,
    wanted: &[usize],
    facts: &[Option<moruna_cost::Memory>],
) -> Result<(Vec<Option<SizedByNode>>, u64)> {
    use moruna_micro::layout::{PAGE, PIECES, plane_for};
    let account = |memory: &moruna_cost::Memory, piece: u64| {
        (memory.account(piece as f64).ceil() as u64).next_multiple_of(PAGE)
    };
    let measured: Vec<(usize, &moruna_cost::Memory)> = facts
        .iter()
        .enumerate()
        .filter_map(|(at, memory)| Some((at, memory.as_ref()?)))
        .collect();
    let mut out = vec![None; facts.len()];
    if measured.is_empty() {
        return Ok((out, 0));
    }
    let table: Vec<(usize, Vec<u64>)> = measured
        .iter()
        .map(|&(at, memory)| {
            let costs = PIECES
                .iter()
                .map(|&piece| plane_for(piece).1 + account(memory, piece))
                .collect();
            (wanted.get(at).copied().unwrap_or(1), costs)
        })
        .collect();
    let (k, counts) = moruna_adapters::instances::fund_measured(room, &table).map_err(|msg| {
        MorunaError::Config {
            name: "budget.native_children",
            msg,
        }
    })?;
    let piece = PIECES[k];
    let mut used = 0u64;
    for ((&(at, memory), &instances), (_, costs)) in measured.iter().zip(&counts).zip(&table) {
        out[at] = Some(SizedByNode {
            instances,
            piece,
            account: account(memory, piece),
        });
        used = used.saturating_add(costs[k].saturating_mul(instances as u64));
    }
    Ok((out, used))
}

/// What each instance of each kernel stage held, stage 1 first (system design 3A): for a stage
/// whose function runs as the data plane's tasks, its instances' account peaks and limits and
/// the largest pieces they were given; for any other stage, nothing. A stage is the packaged
/// stage whose kernel it runs.
fn instance_memory(
    kernels: &[Arc<dyn moruna_kernel::Kernel>],
    packaged: &[crate::packaged::Packaged],
) -> Vec<Vec<moruna_trace::InstanceMemory>> {
    let stages: Vec<Vec<moruna_trace::InstanceMemory>> = kernels
        .iter()
        .map(|kernel| {
            packaged
                .iter()
                .find(|stage| std::ptr::addr_eq(Arc::as_ptr(&stage.kernel()), Arc::as_ptr(kernel)))
                .and_then(crate::packaged::Packaged::task_figures)
                .unwrap_or_default()
                .into_iter()
                .flatten()
                .map(|f| moruna_trace::InstanceMemory {
                    account_peak_bytes: f.peak,
                    account_limit_bytes: f.limit,
                    largest_piece_bytes: f.largest,
                })
                .collect()
        })
        .collect();
    if stages.iter().all(Vec::is_empty) {
        Vec::new()
    } else {
        stages
    }
}

/// The out-of-arena cost per byte in flight of a chain with no kernels (see
/// `expected_out_of_arena_amplification`): one byte beside each byte in flight, which leaves
/// the arena two thirds of what the shares leave.
const COPY_OUT_OF_ARENA_AMPLIFICATION: f64 = 1.0;

/// Each stage's measured memory: what the job came with (`carried`, one a stage or none at
/// all), and for a stage that came with none, what the run's own store holds (`found`). The
/// stages sized from what the job came with are said in `notes`, each that the node itself
/// measured (`by_node`, one a stage or none at all) as the node's readings.
fn with_carried(
    carried: Vec<Option<moruna_cost::Memory>>,
    by_node: &[bool],
    found: Vec<Option<moruna_cost::Memory>>,
    notes: &mut Vec<String>,
) -> Vec<Option<moruna_cost::Memory>> {
    let mut carried = carried.into_iter();
    let mut stages = Vec::new();
    let measured: Vec<Option<moruna_cost::Memory>> = found
        .into_iter()
        .enumerate()
        .map(|(at, found)| match carried.next().flatten() {
            Some(memory) => {
                let whose = if by_node.get(at).copied().unwrap_or(false) {
                    " (the node's own readings)"
                } else {
                    ""
                };
                stages.push(format!("{}{whose}", at + 1));
                Some(memory)
            }
            None => found,
        })
        .collect();
    if !stages.is_empty() {
        notes.push(format!(
            "the measured memory of the function of stage {} came with the job",
            stages.join(", ")
        ));
    }
    measured
}

/// What a chain holds outside the arena, as the run knows it before it sizes anything.
#[derive(Clone, Copy, Debug, PartialEq)]
struct OutOfArena {
    /// Bytes per byte in flight: the largest of the stages'.
    per_byte: f64,
    /// Bytes its measured functions hold whatever their batch, for every call that can be at
    /// work at once. Held outside the arena like declared kernel state, so the arena is not
    /// sized over them.
    fixed_bytes: u64,
}

/// The chain's out-of-arena cost with each stage's measurement in place of what it declares,
/// and of the default (system design 3A: a function's memory is measured, never assumed; item
/// 6.3g). `measured` is one entry a stage, what `moruna check`'s warm-up fitted of the stage's
/// function ([`moruna_cost::Memory`]), `None` for a stage with no warm-up; `workers` is the
/// run's.
///
/// A measured stage costs what it was measured to hold outside its batch:
///
/// - per byte in flight, its line's slope widened by its margin (`Memory::margin`: the units'
///   spread, nothing fixed), and never less than `moruna_controller::MIN_AMPLIFICATION`, the
///   least the controller plans a morsel at whatever a kernel measures, so the arena is not
///   sized over room the controller holds back;
/// - its line's fixed term, widened likewise, once for every call that can be at work at once:
///   its instances, or the run's workers for a stateless function.
///
/// A stage with no measurement is counted as `expected_out_of_arena_amplification` counts it,
/// by what it declares or `DEFAULT_OUT_OF_ARENA_AMPLIFICATION`, which is where a function
/// nothing is known of starts and not a measurement of anything. The chain's cost per byte is
/// the largest of its stages', for that function's reason. Each measured stage is said in
/// `notes`, with the units and the range of input its line was fitted from.
fn out_of_arena(
    kernels: &[Arc<dyn moruna_kernel::Kernel>],
    measured: &[Option<moruna_cost::Memory>],
    workers: u16,
    notes: &mut Vec<String>,
) -> OutOfArena {
    if measured.iter().all(Option::is_none) {
        return OutOfArena {
            per_byte: expected_out_of_arena_amplification(kernels),
            fixed_bytes: 0,
        };
    }
    let mut chain = OutOfArena {
        per_byte: 0.0,
        fixed_bytes: 0,
    };
    for (at, kernel) in kernels.iter().enumerate() {
        let Some(memory) = measured.get(at).and_then(Option::as_ref) else {
            chain.per_byte =
                chain
                    .per_byte
                    .max(expected_out_of_arena_amplification(std::slice::from_ref(
                        kernel,
                    )));
            continue;
        };
        let margin = memory.margin();
        let per_byte =
            (memory.per_input_byte * (1.0 + margin)).max(moruna_controller::MIN_AMPLIFICATION);
        let at_once = match kernel.kind() {
            moruna_kernel::KernelKind::Stateful { max_instances } => max_instances.get() as u64,
            moruna_kernel::KernelKind::Stateless => u64::from(workers.max(1)),
        };
        let fixed = ((memory.fixed_bytes * (1.0 + margin)).ceil() as u64).saturating_mul(at_once);
        chain.per_byte = chain.per_byte.max(per_byte);
        chain.fixed_bytes = chain.fixed_bytes.saturating_add(fixed);
        notes.push(format!(
            "stage {}: its function's memory was measured, not assumed: {:.0} bytes whatever its \
             batch plus {:.2} bytes a byte of input, outside the batch, from {} units of {} to \
             {} bytes of input, with a margin of {:.2} (the 95th percentile of the units' \
             spread about that line, {:.3} x {:.3}); the run sets aside {per_byte:.2} bytes a \
             byte in flight and {fixed} bytes for the {at_once} calls that can be at work at once",
            at + 1,
            memory.fixed_bytes,
            memory.per_input_byte,
            memory.units,
            memory.input_bytes_min,
            memory.input_bytes_max,
            margin,
            moruna_cost::Z95,
            memory.spread,
        ));
    }
    chain
}

/// The smallest region granularity 02 e.1 rounds the arena down to.
const GRANULE_BYTES: u64 = 64 << 10;

/// The expected kernel state of 11 f.1: the sum of `KernelHints::state_bytes` over the
/// stateful instances that will be created, zero where a kernel declares none. It is held
/// outside the arena, so the arena may not be sized over it.
fn expected_kernel_state(kernels: &[Arc<dyn moruna_kernel::Kernel>]) -> u64 {
    kernels
        .iter()
        .map(|kernel| match kernel.kind() {
            moruna_kernel::KernelKind::Stateful { max_instances } => kernel
                .hints()
                .state_bytes
                .unwrap_or(0)
                .saturating_mul(max_instances.get() as u64),
            moruna_kernel::KernelKind::Stateless => 0,
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
/// default, `<home>/.moruna/profiles`, which is created here. Without it a resumed run has no
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
    let dir = home.join(".moruna").join("profiles");
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
fn summarise(plan: &[moruna_kernel::Split]) -> PlanSummary {
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

    /// MH 4.7: `staging.durable` declares `durable_staging=present`, over a profile or none,
    /// and refuses a profile that says otherwise.
    /// Stream C (2026-11-17): at the arenas E's one-month Score had at budgets of 2, 8 and
    /// 20.5 GiB, a sink on eight lanes gets buffers of 16, 64 and 64 MiB, and morsels of at
    /// most 8, 32 and 32 MiB: the budget no longer makes the first morsels larger.
    #[test]
    fn morsels_and_lane_buffers_are_capped_whatever_the_budget() {
        let claim = |arena: u64| {
            let share = (arena as f64 * config::SINK_BUFFER_SHARE) as u64;
            1u64 << (63 - share.leading_zeros())
        };
        let mib = 1u64 << 20;
        for (arena, lane, morsel) in [
            (641_925_120u64, 16 * mib, 8 * mib),
            (2_574_647_296, 64 * mib, 32 * mib),
            (6_601_179_136, 64 * mib, 32 * mib),
        ] {
            assert_eq!(morsel_cap(claim(arena), 8), (morsel, lane), "arena {arena}");
        }
        const { assert!(moruna_sinks::LANE_MAX_BYTES / 2 >= config::MORSEL_MIN_BYTES) };
        assert_eq!(
            morsel_cap(0, 8),
            (config::MORSEL_MAX_BYTES, 0),
            "no claim, no cap"
        );
        assert_eq!(
            morsel_cap(4 * mib, 2).0,
            config::MORSEL_MIN_BYTES,
            "never below the floor"
        );
    }

    #[test]
    fn staging_durable_is_a_declared_profile() {
        use moruna_kernel::{Guarantee, HostProfile};
        assert!(durable_profile(None, false).expect("off").is_none());
        let kept = durable_profile(
            Some(HostProfile {
                memlock: Guarantee::Absent,
                ..HostProfile::default()
            }),
            false,
        )
        .expect("off")
        .expect("the caller's profile");
        assert_eq!(kept.durable_staging, Guarantee::Unknown);
        let made = durable_profile(None, true).expect("on").expect("a profile");
        assert_eq!(made.durable_staging, Guarantee::Present);
        let merged = durable_profile(
            Some(HostProfile {
                memlock: Guarantee::Absent,
                durable_staging: Guarantee::Present,
                ..HostProfile::default()
            }),
            true,
        )
        .expect("agrees")
        .expect("a profile");
        assert_eq!(merged.memlock, Guarantee::Absent, "the rest of it is kept");
        assert_eq!(merged.durable_staging, Guarantee::Present);
        let refused = durable_profile(
            Some(HostProfile {
                durable_staging: Guarantee::Absent,
                ..HostProfile::default()
            }),
            true,
        );
        match refused {
            Err(error) => assert!(error.to_string().contains("staging.durable"), "{error}"),
            Ok(_) => panic!("a profile declaring the staging not durable is refused"),
        }
    }

    /// MH 4.7: with no staging directory, or none that holds a manifest of this job, `resume
    /// auto` finds nothing; a built source's plan is read to compare digests.
    #[test]
    fn resume_auto_finds_nothing_where_there_is_nothing() {
        use moruna_testkit::FakeSource;
        let built = crate::spec::SourceSpec::Built(
            Arc::new(FakeSource::new()) as Arc<dyn moruna_kernel::Source>
        );
        assert_eq!(
            auto_manifest(None, &[], std::slice::from_ref(&built), None).expect("no dir"),
            None
        );
        let dir = std::env::temp_dir().join(format!("moruna-auto-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        assert_eq!(
            auto_manifest(Some(&dir), &[], std::slice::from_ref(&built), None).expect("empty"),
            None
        );
        let later = crate::spec::SourceSpec::Build(Box::new(|_| {
            Ok(Arc::new(FakeSource::new()) as Arc<dyn moruna_kernel::Source>)
        }));
        assert_eq!(
            auto_manifest(Some(&dir), &[], std::slice::from_ref(&later), None).expect("empty"),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
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

    /// `profiles.dir` defaults to `<home>/.moruna/profiles` and the directory is created
    /// (12 f.1, preamble section 5); an explicit directory is left alone and no home directory
    /// leaves the store off with a note.
    #[test]
    fn the_profile_store_has_a_default() {
        let mut notes = Vec::new();
        let given = PathBuf::from("/tmp/moruna-profiles-given");
        assert_eq!(
            resolve_profiles_dir(Some(given.clone()), None, &mut notes),
            Some(given),
            "an explicit directory is taken as given"
        );
        assert!(notes.is_empty());

        let home = std::env::temp_dir().join(format!("moruna-home-{}", std::process::id()));
        let resolved = resolve_profiles_dir(None, Some(home.clone()), &mut notes)
            .expect("the default under a writable home");
        assert_eq!(resolved, home.join(".moruna").join("profiles"));
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

        // A 512 MiB ceiling is governed by the share, not the floor, since the sink stopped
        // demanding its row group's whole size class (08 f.1): the arena takes a third of the
        // allowance and the other two thirds stay available for what a kernel allocates
        // outside it. That is the whole point of the share, and while the floor was 128 MiB
        // plus read-ahead the floor won here instead, which left a Python kernel nothing but
        // the reserve on any host with a resting footprint larger than this laptop's.
        let at_512 = arena_host_bytes(
            512 << 20,
            48 << 20,
            config::RESERVE_FRACTION,
            0,
            DEFAULT_OUT_OF_ARENA_AMPLIFICATION,
            4096,
            &mut notes,
        );
        assert!(
            at_512 > ARENA_FLOOR_BYTES,
            "the share governs at 512 MiB, not the floor: {at_512} against {ARENA_FLOOR_BYTES}"
        );
        let reserve_512 = ((512u64 << 20) as f64 * config::RESERVE_FRACTION as f64) as u64;
        let allowance_512 = (512u64 << 20) - (48 << 20) - reserve_512;
        assert!(
            at_512 < allowance_512 / 2,
            "and it leaves most of the allowance for out-of-arena bytes: {at_512} of {allowance_512}"
        );

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
    /// that declares nothing counts as the controller's own seed, and an empty chain is a copy,
    /// which costs a byte beside each byte in flight unless its source is an engine that decodes
    /// inside its own share (F8.9).
    #[test]
    fn the_chain_declares_its_out_of_arena_cost() {
        use moruna_kernel::Kernel;
        use moruna_testkit::FakeKernel;

        assert_eq!(
            expected_out_of_arena_amplification(&[]),
            COPY_OUT_OF_ARENA_AMPLIFICATION
        );

        // A kernel that overrides nothing declares nothing, which is the common case: the
        // `examples/append_column.rs` kernel a first program is written around is one.
        struct Silent;
        impl Kernel for Silent {
            fn fingerprint(&self) -> moruna_kernel::Fingerprint {
                moruna_kernel::Fingerprint::compute("moruna::tests::silent", b"v1")
            }
            fn kind(&self) -> moruna_kernel::KernelKind {
                moruna_kernel::KernelKind::Stateless
            }
            fn accepts(&self) -> moruna_kernel::PayloadSpec {
                moruna_kernel::PayloadSpec {
                    kind: moruna_kernel::PayloadKind::Table,
                    tier: moruna_kernel::TierPref::Host,
                }
            }
            fn output_schema(
                &self,
                input: &moruna_kernel::SourceSchema,
            ) -> moruna_kernel::Result<moruna_kernel::SourceSchema> {
                Ok(input.clone())
            }
            fn init(
                &self,
                _ctx: &moruna_kernel::InitCtx,
            ) -> moruna_kernel::Result<Box<dyn moruna_kernel::KernelState>> {
                Ok(Box::new(moruna_kernel::NoState))
            }
            fn apply(
                &self,
                _state: &mut dyn moruna_kernel::KernelState,
                input: moruna_kernel::Payload,
            ) -> moruna_kernel::Result<moruna_kernel::Payload> {
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

        // With nothing measured, that is the whole of it, and nothing is said.
        let mut notes = Vec::new();
        assert_eq!(
            out_of_arena(&mixed, &[None, None], 4, &mut notes),
            OutOfArena {
                per_byte: 6.0,
                fixed_bytes: 0
            }
        );
        assert_eq!(out_of_arena(&[], &[], 4, &mut notes).per_byte, 1.0);
        assert!(notes.is_empty());

        // What a job came with stands before what the run's store holds, stage by stage.
        let line =
            |rate: f64| moruna_cost::Memory::fit(&[(1e6, rate * 1e6), (2e6, rate * 2e6)], None);
        let rates = |measured: Vec<Option<moruna_cost::Memory>>| -> Vec<Option<f64>> {
            measured
                .iter()
                .map(|m| m.as_ref().map(|m| m.per_input_byte))
                .collect()
        };
        let mut said = Vec::new();
        assert_eq!(
            rates(with_carried(
                vec![line(3.0), None],
                &[],
                vec![line(5.0), line(7.0), None],
                &mut said
            )),
            vec![Some(3.0), Some(7.0), None]
        );
        assert_eq!(
            said,
            vec!["the measured memory of the function of stage 1 came with the job"]
        );
        // The node's own readings are said as such.
        let mut said = Vec::new();
        with_carried(
            vec![line(3.0), line(4.0)],
            &[false, true],
            vec![None, None],
            &mut said,
        );
        assert_eq!(
            said,
            vec![
                "the measured memory of the function of stage 1, 2 (the node's own readings) \
                 came with the job"
            ]
        );
        let mut said = Vec::new();
        assert_eq!(
            rates(with_carried(
                Vec::new(),
                &[],
                vec![line(5.0), None],
                &mut said
            )),
            vec![Some(5.0), None]
        );
        assert!(said.is_empty());

        // Beside a measured stage, a stage nothing is known of still starts at the default.
        let one = moruna_cost::Memory::fit(&[(1e6, 2e6), (2e6, 3e6)], None).expect("fitted");
        let beside: Vec<Arc<dyn Kernel>> = vec![Arc::new(FakeKernel::new()), Arc::new(Silent)];
        assert_eq!(
            out_of_arena(&beside, &[Some(one), None], 4, &mut notes).per_byte,
            DEFAULT_OUT_OF_ARENA_AMPLIFICATION
        );
    }

    /// System design 3A (6.3g phase 3): a stage the node measured is sized from its readings,
    /// the piece shrinking before its instances do; another is left to today's rule.
    #[test]
    fn a_stage_warms_up_unless_its_facts_rest_on_enough_units_or_it_has_one_task() {
        let line = |units: u64| moruna_cost::Memory {
            fixed_bytes: 1.0,
            per_input_byte: 1.0,
            output_per_input_byte: None,
            spread: 0.0,
            units,
            input_bytes_min: 1,
            input_bytes_max: 2,
        };
        assert!(warms_up(None, 4), "nothing measured it");
        assert!(warms_up(Some(&line(6)), 4), "a check's six units");
        assert!(
            !warms_up(Some(&line(WARM_UP_SKIPPED_AT)), 4),
            "enough units"
        );
        assert!(!warms_up(None, 1), "no other task to size");
    }

    #[test]
    fn an_instance_s_pieces_are_what_its_account_holds_by_the_measured_line() {
        const MIB: u64 = 1 << 20;
        let share = 256 * MIB;
        let account = moruna_micro::layout::native_split(share).1;
        // A Python function's 30 MiB and six bytes a byte.
        let line = moruna_cost::Memory {
            fixed_bytes: (30 * MIB) as f64,
            per_input_byte: 6.0,
            output_per_input_byte: None,
            spread: 0.0,
            units: 6,
            input_bytes_min: MIB,
            input_bytes_max: 16 * MIB,
        };
        assert_eq!(instance_piece(&line, share), (account - 30 * MIB) / 6);
        // The fixed part alone past the account: one byte, the least there is.
        let big = moruna_cost::Memory {
            fixed_bytes: (account + MIB) as f64,
            ..line.clone()
        };
        assert_eq!(instance_piece(&big, share), 1);
        // A line that does not grow bounds nothing.
        let flat = moruna_cost::Memory {
            fixed_bytes: 0.0,
            per_input_byte: 0.0,
            ..line
        };
        assert_eq!(instance_piece(&flat, share), u64::MAX);
    }

    #[test]
    fn a_stage_the_node_measured_is_sized_from_its_readings() {
        use moruna_micro::layout::{PIECES, plane_for};
        const MIB: u64 = 1 << 20;
        // Score's readings: 4.5 MiB at 16 MiB pieces, one size.
        let score = moruna_cost::Memory {
            fixed_bytes: 4.5 * MIB as f64,
            per_input_byte: 0.0,
            output_per_input_byte: None,
            spread: 0.0,
            units: 4,
            input_bytes_min: 16 * MIB,
            input_bytes_max: 16 * MIB,
        };
        let account = (4.5 * MIB as f64) as u64;
        let at = |piece: u64| plane_for(piece).1 + account;
        // Nothing measured: nothing sized, nothing taken.
        assert_eq!(
            fund_by_node(1 << 40, &[4], &[None]).expect("nothing to fund"),
            (vec![None], 0)
        );
        // Room for four at 16 MiB: four, at 16 MiB, each its plane and its 4.5 MiB account.
        let (sized, used) =
            fund_by_node(4 * at(PIECES[0]), &[4, 2], &[Some(score.clone()), None]).expect("funded");
        assert_eq!(
            sized,
            vec![
                Some(SizedByNode {
                    instances: 4,
                    piece: 16 * MIB,
                    account
                }),
                None
            ]
        );
        assert_eq!(used, 4 * at(PIECES[0]));
        // Room for four only at 4 MiB: the piece shrinks, the instances stay.
        let (sized, _) = fund_by_node(4 * at(PIECES[2]), &[4], &[Some(score.clone())]).expect("f");
        assert_eq!(sized[0].map(|s| (s.instances, s.piece)), Some((4, 4 * MIB)));
        // Not even at 1 MiB: fewer instances at the least piece.
        let (sized, _) = fund_by_node(2 * at(MIB), &[4], &[Some(score.clone())]).expect("f");
        assert_eq!(sized[0].map(|s| (s.instances, s.piece)), Some((2, MIB)));
        // Not one: refused by the budget's name.
        let refused = fund_by_node(MIB, &[4], &[Some(score)]).unwrap_err();
        assert!(
            refused.to_string().contains("budget.native_children"),
            "{refused}"
        );
    }

    /// System design 3A, item 6.3g: a function's memory is measured, never assumed. A stage
    /// whose function was measured is sized by its line and its margin, whatever it declares;
    /// one with no measurement keeps what it declares, or the start default.
    #[test]
    fn a_measured_function_is_sized_by_its_measurement() {
        use moruna_cost::Memory;
        use moruna_kernel::Kernel;
        use moruna_testkit::FakeKernel;

        const MIB: f64 = 1_048_576.0;
        // Twelve bytes a byte of input and 3 MiB whatever the batch, exactly: no margin.
        let hungry = Memory::fit(
            &[
                (MIB, 15.0 * MIB),
                (4.0 * MIB, 51.0 * MIB),
                (16.0 * MIB, 195.0 * MIB),
            ],
            Some(1.0),
        )
        .expect("fitted");
        assert!(hungry.margin() < 1e-9);
        // Nothing of its own.
        let lean = Memory::fit(&[(MIB, 0.0), (16.0 * MIB, 0.0)], Some(1.0)).expect("fitted");
        // Ten bytes a byte, a tenth either side of it: a margin of 1.645 tenths.
        let noisy = Memory::fit(
            &[
                (MIB, 11.0 * MIB),
                (MIB, 9.0 * MIB),
                (16.0 * MIB, 176.0 * MIB),
                (16.0 * MIB, 144.0 * MIB),
            ],
            None,
        )
        .expect("fitted");

        // A stateless function that declares 0.5: its measurement stands, not its
        // declaration, and its fixed term is held by each of the run's four workers.
        let declares: Vec<Arc<dyn Kernel>> = vec![Arc::new(FakeKernel::new().amplification(0.5))];
        let mut notes = Vec::new();
        let sized = out_of_arena(&declares, &[Some(hungry.clone())], 4, &mut notes);
        assert!((sized.per_byte - 12.0).abs() < 1e-6, "{sized:?}");
        assert!(
            sized.fixed_bytes.abs_diff(4 * 3 * (1 << 20)) <= 4,
            "{sized:?}"
        );
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].starts_with("stage 1: its function's memory was measured, not assumed")
                && notes[0].contains("12.00 bytes a byte of input")
                && notes[0].contains("from 3 units of 1048576 to 16777216 bytes of input")
                && notes[0].contains("the 4 calls that can be at work at once"),
            "{}",
            notes[0]
        );

        // A function measured to hold nothing is sized at the least the controller plans a
        // morsel at, which leaves the arena four fifths of the allowance, not a third.
        let mut notes = Vec::new();
        let sized = out_of_arena(&declares, &[Some(lean)], 4, &mut notes);
        assert_eq!(
            sized,
            OutOfArena {
                per_byte: moruna_controller::MIN_AMPLIFICATION,
                fixed_bytes: 0
            }
        );
        let allowance = 1u64 << 30;
        let arena = arena_host_bytes(allowance, 0, 0.0, 0, sized.per_byte, 4096, &mut notes);
        assert!(
            (arena as f64 / allowance as f64 - 0.8).abs() < 0.001,
            "{arena}"
        );

        // The margin is the measured spread: the slope widened by 1.645 times it.
        let mut notes = Vec::new();
        let sized = out_of_arena(&declares, &[Some(noisy.clone())], 1, &mut notes);
        let widened = noisy.per_input_byte * (1.0 + moruna_cost::Z95 * noisy.spread);
        assert!((sized.per_byte - widened).abs() < 1e-9, "{sized:?}");
        assert!(sized.per_byte > 11.0 && sized.per_byte < 12.5, "{sized:?}");

        // A chain: the largest of its stages', the unmeasured one by what it declares (6.0),
        // and a stateful function's fixed term by its instances, not the run's workers.
        let chain: Vec<Arc<dyn Kernel>> = vec![
            Arc::new(FakeKernel::new().amplification(6.0)),
            Arc::new(FakeKernel::new().stateful(3, 0)),
        ];
        let mut notes = Vec::new();
        let small = Memory::fit(&[(MIB, 2.0 * MIB), (2.0 * MIB, 3.0 * MIB)], None).expect("fit");
        let sized = out_of_arena(&chain, &[None, Some(small)], 8, &mut notes);
        assert_eq!(sized.per_byte, 6.0, "the declared stage's is the larger");
        assert!(
            sized.fixed_bytes.abs_diff(3 * (1 << 20)) <= 3,
            "1 MiB for each of 3 instances: {sized:?}"
        );
        assert!(notes[0].starts_with("stage 2:"), "{}", notes[0]);
    }

    /// What a job's budget lets the object stores have in flight (3A, 3.5b), derived from the
    /// budget through the node's split (`host::node::sizes`) and the arena sizing of a copy, at
    /// two budgets: 3.5's lock turn's, a 2 GiB node's `job_memory_bytes` of 590,237,696 bytes
    /// (ceiling 342,016,000, arena 201,457,664, as its report said); and a 16 GiB node's job
    /// bound of 14,360 MiB (`kernel/node/core`'s `the_cloud_profile_fits_on_2_8_and_16_gib_nodes`).
    /// At the small one N is the room over S3's least part and each part a little over it; at
    /// the large one N reaches the job's socket budget and the parts grow with the room.
    #[test]
    fn a_jobs_budget_sets_the_object_stores_requests_and_parts() {
        const MIB: u64 = 1 << 20;
        let copy = |baseline: u64| Sizing {
            baseline,
            kernel_state: 0,
            out_of_arena: COPY_OUT_OF_ARENA_AMPLIFICATION,
            page_bytes: 4096,
            queues: 2,
        };
        let arena_of = |job: u64| {
            let ceiling = crate::host::node::sizes(job, false).arena;
            (ceiling, copy(5_530_736).divide(ceiling, &mut Vec::new()))
        };

        let (ceiling, arena) = arena_of(590_237_696);
        assert_eq!(
            (ceiling, arena),
            (342_016_000, 201_457_664),
            "3.5's run, as reported"
        );
        let mut notes = Vec::new();
        let small = object_io_for(arena, &mut notes);
        assert_eq!(object_room(arena), 100_728_832);
        assert_eq!(small.requests, 19, "the room over S3's least part");
        assert_eq!(small.part_bytes, 100_728_832 / 19);
        assert!(small.part_bytes >= moruna_reactor::S3_PART_MIN_BYTES);
        assert!(
            notes[0].starts_with("object stores: 19 requests at once in parts of 5301517 bytes"),
            "{notes:?}"
        );
        // The month's files of about 3 MB are each one put; a 64 MiB file is parts of the share.
        assert!(small.single_put(3_000_000));
        assert_eq!(small.part_for(64 * MIB), 5_301_517);
        assert_eq!(small.parts_in_flight(64 * MIB), 13);

        let (_, arena) = arena_of(14_360 * MIB);
        let large = object_io_for(arena, &mut Vec::new());
        assert_eq!(
            large.requests,
            moruna_storage::net::SOCKETS,
            "a 16 GiB node's job reaches the socket budget"
        );
        assert_eq!(large.part_bytes, object_room(arena) / 64);
        assert_eq!(
            large.part_bytes, 52_435_968,
            "the parts grow with the room: about ten times the small budget's"
        );
    }

    fn sizing() -> Sizing {
        Sizing {
            baseline: 16 << 20,
            kernel_state: 0,
            out_of_arena: 0.0,
            page_bytes: 4096,
            queues: 2,
        }
    }

    /// The arena is sized over the whole allowance, and says so.
    #[test]
    fn the_arena_is_sized_over_the_allowance() {
        let ceiling = 1u64 << 30;
        let allowance = sizing().allowance(ceiling);
        let mut notes = Vec::new();
        let arena = sizing().divide(ceiling, &mut notes);
        assert!(arena <= allowance);
        assert!(arena > 0);
        assert!(notes[0].contains("arena sized at"));
    }

    /// N-7: the floor is the least ceiling at which the arena and the buffer catalog each get
    /// their least, and just below it they do not.
    #[test]
    fn a_run_has_a_floor() {
        let plain = sizing();
        let floor = plain.floor();
        assert!(plain.allowance(floor) >= ARENA_FLOOR_BYTES);
        assert!(
            plain.allowance(floor - (8 << 20)) < ARENA_FLOOR_BYTES
                || !plain.catalog_floor(floor - (8 << 20)).holds()
        );
        let terms = plain.floor_terms();
        assert!(terms.contains("an arena of"), "{terms}");
        let stateful = Sizing {
            kernel_state: 64 << 20,
            ..plain
        };
        assert!(stateful.floor() > floor);
        assert!(stateful.floor_terms().contains("kernel state"));
        // KD7b: a run of many queues needs room for every queue's promotion window, at the
        // smallest morsel, and the floor grows with them; it says so.
        let wide = Sizing {
            queues: 64,
            ..plain
        };
        let catalog = wide.catalog_floor(wide.floor());
        assert!(
            wide.floor() > floor,
            "the catalog's floor is the larger term"
        );
        assert!(catalog.holds(), "{catalog:?}");
        assert!(!wide.catalog_floor(wide.floor() - (8 << 20)).holds());
        assert_eq!(
            catalog.bytes,
            (64 * u64::from(moruna_kernel::DEFAULT_PROMOTION_WINDOW) + 2)
                * config::MORSEL_MIN_BYTES
        );
        assert!(
            wide.floor_terms().contains("buffer catalog"),
            "{}",
            wide.floor_terms()
        );
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
        use moruna_kernel::Kernel;
        use moruna_testkit::FakeKernel;

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
        let split = |id: u32, rows: u64, bytes: u64, sub: bool| moruna_kernel::Split {
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

    /// The run's DAG is walked before anything is built: a copy's one queue carries its
    /// source's schema, and a topology that does not match the run's parts is refused.
    #[test]
    fn the_graph_walk_checks_every_stage() {
        use moruna_testkit::{FakeSink, FakeSource};
        let source = Arc::new(FakeSource::new()) as Arc<dyn moruna_kernel::Source>;
        let schema = source.schema();
        let sink = || SinkHandle::wrap(Box::new(FakeSink::new()), false, 1 << 20);
        let copy = Pipeline::chain(source.clone(), Vec::new(), sink());
        let schemas = moruna_scheduler::validate_graph(&copy).expect("a copy is the source");
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].hash(), schema.hash());
        let wrong = Pipeline {
            topology: moruna_kernel::Topology::chain(2),
            ..Pipeline::chain(source, Vec::new(), sink())
        };
        assert!(moruna_scheduler::validate_graph(&wrong).is_err());
    }

    #[test]
    fn assigned_guest_run_identity_cannot_be_replaced_by_resume_metadata() {
        let assigned = RunId([7; 16]);
        let header = moruna_placement::manifest::ManifestHeader {
            version: moruna_placement::manifest::MANIFEST_VERSION,
            run_id: assigned,
            node: moruna_kernel::NodeId(4),
            hostname: "original-producer-node".into(),
            written_ns: 1,
        };
        assert_eq!(resolve_run_id(None, Some(assigned)).unwrap(), assigned);
        assert_eq!(
            resolve_run_id(Some(&header), Some(assigned)).unwrap(),
            assigned
        );
        assert_eq!(resolve_run_id(Some(&header), None).unwrap(), assigned);
        assert!(matches!(
            resolve_run_id(Some(&header), Some(RunId([8; 16]))),
            Err(MorunaError::Resume(_))
        ));
    }

    #[test]
    fn final_accounting_failure_preserves_existing_terminal_facts() {
        let failure = || MorunaError::Io {
            op: "read",
            target: "memory.current".into(),
            msg: "account disappeared".into(),
        };
        let mut notes = Vec::new();
        let terminated = RunOutcome::Terminated {
            diagnostic: MorunaError::Config {
                name: "original",
                msg: "kernel refused".into(),
            },
            manifest: None,
        };
        assert!(final_accounting_failure(&terminated, Some(failure()), &mut notes).is_none());
        assert!(matches!(
            terminated,
            RunOutcome::Terminated {
                diagnostic: MorunaError::Config {
                    name: "original",
                    ..
                },
                ..
            }
        ));
        assert!(
            final_accounting_failure(
                &RunOutcome::Cancelled { manifest: None },
                Some(failure()),
                &mut notes
            )
            .is_none()
        );
        assert!(
            matches!(final_accounting_failure(&RunOutcome::Completed { sinks: Vec::new() }, Some(failure()), &mut notes), Some(MorunaError::Io { target, .. }) if target == "memory.current")
        );
        assert_eq!(notes.len(), 3);
        assert!(
            notes
                .iter()
                .all(|note| note.contains("account disappeared"))
        );
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

/// The Python kernels a run bound to its arena and gate, released when the run's scope ends.
#[cfg(feature = "python")]
struct ReleaseRun(Vec<Arc<moruna_adapters::PyKernel>>);

#[cfg(feature = "python")]
impl Drop for ReleaseRun {
    fn drop(&mut self) {
        for kernel in &self.0 {
            kernel.release_run();
        }
    }
}

#[cfg(test)]
mod spec_digest_tests {
    use super::*;

    /// A resume is refused when its manifest was written under another document, naming both
    /// digests in one plain sentence; an unreadable or non-JSON manifest is refused; a manifest
    /// that records no digest, or this one, is left to the placement's own checks.
    #[test]
    fn a_resume_manifest_of_another_document_is_refused() {
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce).unwrap();
        let dir = std::env::temp_dir().join(format!(
            "moruna-spec-digest-{}",
            moruna_kernel::RunId(nonce).to_hex()
        ));
        std::fs::create_dir(&dir).unwrap();
        struct Remove(std::path::PathBuf);
        impl Drop for Remove {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _remove = Remove(dir.clone());
        let manifest = dir.join("manifest.json");
        let write = |text: &str| std::fs::write(&manifest, text).unwrap();
        let said = |digest: &str| {
            check_spec_digest(&manifest, digest).map_err(|e| e.into_parts().error.to_string())
        };

        write(r#"{"config": {"spec.digest": "sha256:theirs"}}"#);
        let refused = said("sha256:ours").unwrap_err();
        assert!(
            refused.ends_with(
                "the manifest was written by the job document sha256:theirs, and this one is \
                 sha256:ours"
            ),
            "{refused}"
        );
        said("sha256:theirs").unwrap();
        write(r#"{"config": {}}"#);
        said("sha256:ours").unwrap();
        write("not json");
        assert!(said("sha256:ours").unwrap_err().contains("is not JSON"));
        std::fs::remove_file(&manifest).unwrap();
        assert!(
            said("sha256:ours")
                .unwrap_err()
                .contains("could not be read")
        );
    }
}

/// The resolved configuration with the segment size the run actually stages with (RB6), so a
/// resume compares what was used and not the default.
fn with_segment_bytes(mut config: serde_json::Value, bytes: u64) -> serde_json::Value {
    if let Some(map) = config.as_object_mut() {
        map.insert(
            "staging.segment_bytes".to_string(),
            serde_json::json!(bytes),
        );
    }
    config
}

/// The staging segment for a run whose morsels are at most `morsel_max` bytes (RB6): the
/// configured 128 MiB unit, or the whole number of units that holds two of the largest morsel.
fn staging_segment_bytes(morsel_max: u64) -> u64 {
    let unit = config::STAGING_SEGMENT_BYTES;
    morsel_max
        .saturating_mul(2)
        .div_ceil(unit)
        .max(1)
        .saturating_mul(unit)
}

#[cfg(test)]
mod staging_segment_tests {
    use super::staging_segment_bytes;

    #[test]
    fn a_segment_holds_two_of_the_largest_morsel() {
        const MIB: u64 = 1 << 20;
        assert_eq!(staging_segment_bytes(4 * MIB), 128 * MIB);
        assert_eq!(staging_segment_bytes(64 * MIB), 128 * MIB);
        assert_eq!(staging_segment_bytes(256 * MIB), 512 * MIB);
        assert_eq!(staging_segment_bytes(300 * MIB), 640 * MIB);
    }
}

/// Stream C (data OS, `architecture/data-os.md` §3): on the Moruna kernel the arena is granted
/// memory objects, each scheduler worker is placed on a core, the data disks are the engine's
/// own and a completed run's outputs are one layer on `disk.out`. Elsewhere each of these is
/// today's path, unchanged.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod data_os {
    use std::sync::Arc;

    use std::sync::atomic::{AtomicU64, Ordering};

    use moruna_arena::{Arena, ArenaConfig};
    use moruna_kernel::{Allocator, MorunaError, ObjectMetadata, Reactor};
    pub(crate) use moruna_micro::blk::Granted;
    use moruna_scheduler::WorkerStart;

    use crate::error::Result;

    /// Where a sink or task that writes local files puts its outputs inside the VM
    /// (`MORUNA_OUTPUT_ROOT` overrides it): they join the layer at the run's end. A sink's
    /// URL under `disk://out/` writes straight onto the output disk instead. The root is in the
    /// engine's scratch, because the root filesystem is read only and the only writable places
    /// are a task's scratch and the engine's output disk (data-os §3a).
    pub(crate) const OUTPUT_ROOT: &str = "/tmp/out";

    pub(crate) fn take() -> Result<Option<Granted>> {
        Granted::take().map_err(|msg| {
            MorunaError::Config {
                name: "data_os",
                msg,
            }
            .into()
        })
    }

    pub(crate) fn arena(config: ArenaConfig, granted: Option<&Granted>) -> Result<Arc<Arena>> {
        Ok(match granted {
            Some(g) => Arena::with_backing(config, g.backing())?,
            None => Arena::new(config)?,
        })
    }

    /// Workers placed on a core, and placements the kernel refused.
    static PLACED: AtomicU64 = AtomicU64::new(0);
    static REFUSED: AtomicU64 = AtomicU64::new(0);

    /// Places worker `n` on CPU `n % cpus` as it starts, once: a place, never a function.
    pub(crate) fn worker_start(granted: Option<&Granted>) -> Option<WorkerStart> {
        granted?;
        let cpus = moruna_micro::start::kernel()?.cpus.max(1) as u32;
        Some(WorkerStart(Arc::new(move |worker| {
            match moruna_sys::datapath::pin_current_thread(Some(u32::from(worker) % cpus)) {
                Ok(()) => PLACED.fetch_add(1, Ordering::Relaxed),
                Err(_) => REFUSED.fetch_add(1, Ordering::Relaxed),
            };
        })))
    }

    #[allow(clippy::type_complexity)]
    pub(crate) fn reactor(
        granted: Option<&Granted>,
        reactor: Arc<dyn Reactor>,
        metadata: Option<Arc<dyn ObjectMetadata>>,
        alloc: Arc<dyn Allocator>,
    ) -> Result<(Arc<dyn Reactor>, Option<Arc<dyn ObjectMetadata>>)> {
        match granted {
            Some(g) => g.reactor(reactor, metadata, alloc).map_err(|e| {
                MorunaError::Io {
                    op: "data_os",
                    target: "the data disks".into(),
                    msg: e.to_string(),
                }
                .into()
            }),
            None => Ok((reactor, metadata)),
        }
    }

    /// Finishes the output layer when the run completed and has an output disk, and each layer
    /// its sinks wrote onto a volume (item 3.3), and says what the arena, the disks and the
    /// volumes did; the first failure, if a layer could not be finished.
    pub(crate) fn finish(
        granted: Option<&Granted>,
        completed: bool,
        alloc: Arc<dyn Allocator>,
        notes: &mut Vec<String>,
    ) -> Option<MorunaError> {
        let granted = granted?;
        let mut failure = None;
        let root = std::env::var("MORUNA_OUTPUT_ROOT").unwrap_or_else(|_| OUTPUT_ROOT.into());
        match granted.finish(completed, std::path::Path::new(&root), alloc) {
            Some(Ok(w)) => notes.push(format!(
                "data OS: the output layer is {} bytes on {}: {} files of {} bytes in {} \
                 directories, written as the sinks wrote them, {} picked up from {root}",
                w.bytes,
                moruna_micro::devices::OUTPUT,
                w.files,
                w.data,
                w.directories,
                w.picked_up
            )),
            Some(Err(msg)) => {
                failure = Some(MorunaError::Io {
                    op: "output layer",
                    target: moruna_micro::devices::OUTPUT.to_string(),
                    msg,
                })
            }
            None => {}
        }
        // Item 3.3: the layers the sinks wrote onto their volumes, each finished in place.
        for (serial, finished) in granted.finish_volumes(completed) {
            match finished {
                Ok(w) => notes.push(format!(
                    "data OS: volume {serial}'s layer is {} bytes: {} files of {} bytes in {} \
                     directories, written as the sinks wrote them",
                    w.bytes, w.files, w.data, w.directories
                )),
                Err(msg) => {
                    failure.get_or_insert(MorunaError::Io {
                        op: "a volume's layer",
                        target: serial,
                        msg,
                    });
                }
            }
        }
        notes.extend(granted.notes());
        notes.push(format!(
            "data OS: {} workers placed on cores, {} placements refused",
            PLACED.load(Ordering::Relaxed),
            REFUSED.load(Ordering::Relaxed)
        ));
        failure
    }
}

/// Off the Moruna kernel nothing is granted: today's arena, allocator and reactor.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
mod data_os {
    use std::sync::Arc;

    use moruna_arena::{Arena, ArenaConfig};
    use moruna_kernel::{Allocator, MorunaError, ObjectMetadata, Reactor};

    use crate::error::Result;

    /// Nothing granted.
    pub(crate) enum Granted {}

    pub(crate) fn take() -> Result<Option<Granted>> {
        Ok(None)
    }

    pub(crate) fn arena(config: ArenaConfig, _: Option<&Granted>) -> Result<Arc<Arena>> {
        Ok(Arena::new(config)?)
    }

    pub(crate) fn worker_start(_: Option<&Granted>) -> Option<moruna_scheduler::WorkerStart> {
        None
    }

    #[allow(clippy::type_complexity)]
    pub(crate) fn reactor(
        _: Option<&Granted>,
        reactor: Arc<dyn Reactor>,
        metadata: Option<Arc<dyn ObjectMetadata>>,
        _: Arc<dyn Allocator>,
    ) -> Result<(Arc<dyn Reactor>, Option<Arc<dyn ObjectMetadata>>)> {
        Ok((reactor, metadata))
    }

    pub(crate) fn finish(
        _: Option<&Granted>,
        _: bool,
        _: Arc<dyn Allocator>,
        _: &mut Vec<String>,
    ) -> Option<MorunaError> {
        None
    }
}
