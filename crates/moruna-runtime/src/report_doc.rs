//! The run report (PY-I5, MH 4.2): the one document `moruna.run` returns, the writer port writes,
//! `moruna run` writes to its report file and the host protocol's `report` carries, in its
//! canonical bytes (keys sorted, no whitespace; `moruna.RunReport` reads them back).
//!
//! The document is the trace's report (`moruna_trace::RunReport`, 04 d.1) with what the job adds:
//! its schema's version (`schema_version`, `MAJOR.MINOR`; a reader refuses a major it does not
//! know), the Moruna that ran it, the host it ran on, the run's arguments, the spec's digest
//! (`job_digest`), how the run ended (`exit`), one entry per task (`tasks`, each keyed by the
//! task's name in the spec, with everything measured of it, what its needs cost, what the plan
//! predicted, its observation in the cost model's form and a recommendation for the next run),
//! what the run consumed (`resources`), one entry per sink with the version it committed
//! (`sinks`), and the plan and each of its steps' actual start and end when it was asked for.
//!
//! Every number names its unit: bytes (`_bytes`), milliseconds (`_ms`, the trace's seconds and
//! nanoseconds converted), CPU in core-seconds (`cpu_core_s`), rates per second (`_per_s`),
//! dimensionless ratios (`_ratio`) and counts (calls, rows, morsels, instances). A measurement
//! that was not made is absent; zero is a measured zero.

use std::collections::BTreeMap;

use moruna_job::Role;
use moruna_kernel::MorunaError;
use moruna_trace::{ExitReason, RunReport, StageReport};
use serde_json::{Map, Value, json};

/// The report document's schema: `MAJOR.MINOR`. 2: `tasks` holds every figure of a task, and
/// `stages` is gone.
pub const SCHEMA_VERSION: &str = "2.0";

/// What the document adds to the trace's report, which only the job knows.
pub struct ReportContext {
    /// The job that ran.
    pub job: moruna_job::Job,
    /// Which task each part of the run was.
    pub parts: crate::job::Parts,
    /// Where each sink wrote, as its kind named the target.
    pub sink_targets: Vec<String>,
    /// The plan the run started from, when it is reported.
    pub plan: Option<Value>,
    /// The trace file, when one was written.
    pub trace_path: Option<std::path::PathBuf>,
    /// The run's argument values, by name.
    pub args: Map<String, Value>,
    /// The ports each task calls, by its place in the job.
    pub task_ports: Vec<(usize, std::sync::Arc<moruna_kernel::StagePorts>)>,
    /// What identifies each task's code, by its place in the job ([`crate::job::Built::identities`]).
    pub identities: Vec<moruna_cost::Digest>,
    /// The observation store the run's learner wrote to, when one is configured: each task's
    /// recommendation draws on every run of its key the store holds.
    pub store: Option<std::sync::Arc<dyn moruna_cost::ObservationStore>>,
}

impl ReportContext {
    /// The context of `job` as `built` builds it, with no plan, trace, arguments or store.
    pub fn of(job: &moruna_job::Job, built: &crate::job::Built) -> ReportContext {
        ReportContext {
            job: job.clone(),
            parts: built.parts.clone(),
            sink_targets: built.sink_targets.clone(),
            plan: None,
            trace_path: None,
            args: Map::new(),
            task_ports: built.task_ports.clone(),
            identities: built.identities.clone(),
            store: None,
        }
    }
}

/// How a run ended, as the report's `exit` says: its exit code (MH 4.2) and why.
#[derive(Clone, Debug)]
pub struct Exit {
    /// The exit code ([`crate::host::exit`]).
    pub code: i32,
    /// Why, when the code is not 0.
    pub diagnostic: Option<String>,
}

impl Exit {
    /// How the run `report` is of ended, `error` the error it ended with, when it failed.
    pub fn of(report: &RunReport, error: Option<&MorunaError>) -> Exit {
        use crate::host::{exit, exit_code};
        let (code, diagnostic) = match (&report.exit, error) {
            (ExitReason::Completed, None) => (exit::COMPLETED, None),
            (ExitReason::Cancelled, _) | (_, Some(MorunaError::Cancelled)) => {
                (exit::CANCELLED, None)
            }
            (ExitReason::Terminated { diagnostic }, error) => (
                error.map_or(exit::FAILED, exit_code),
                Some(diagnostic.clone()),
            ),
            (ExitReason::Completed, Some(error)) => (exit_code(error), Some(error.to_string())),
        };
        Exit { code, diagnostic }
    }

    /// `completed`, `cancelled`, `terminated`, or, for a run that never started (`ran` false),
    /// `refused` when the document, its budget or its resume was refused.
    pub fn status(&self, ran: bool) -> &'static str {
        use crate::host::exit;
        match self.code {
            exit::COMPLETED => "completed",
            exit::CANCELLED => "cancelled",
            exit::SPEC_REFUSED | exit::BUDGET_REFUSED | exit::RESUME_REFUSED if !ran => "refused",
            _ => "terminated",
        }
    }
}

/// The report's canonical bytes: what the report file, a writer port and `moruna.run` carry.
pub fn report_bytes(
    identity: (&str, Option<&str>),
    report: Option<&RunReport>,
    exit: &Exit,
    context: Option<&ReportContext>,
) -> Vec<u8> {
    moruna_job::canonical_json(&document(identity, report, exit, context)).into_bytes()
}

/// The report document of run `identity` (its id and its spec's digest): `report` the trace's
/// report, when the run made one, with what `context` knows of the job, when it was built. A
/// run that made no report (refused before it started, or failed building) has its identity,
/// its exit, its notes and each task skipped.
pub fn document(
    (run_id, job_digest): (&str, Option<&str>),
    report: Option<&RunReport>,
    exit: &Exit,
    context: Option<&ReportContext>,
) -> Value {
    let status = exit.status(report.is_some());
    let mut value = match report {
        Some(report) => serde_json::to_value(report).unwrap_or_else(
            |e| json!({"run_id": run_id, "notes": [format!("the report did not serialise: {e}")]}),
        ),
        None => json!({"run_id": run_id, "notes": []}),
    };
    let object = value
        .as_object_mut()
        .expect("a report serialises as an object");
    let mut tasks = Vec::new();
    if let Some(report) = report {
        // `tasks` holds what `stages` did, task by task; the totals the trace keeps per run stay.
        object.remove("stages");
        object.remove("cpu_ns");
        for optional in ["write", "read", "buffers", "policy", "memory_basis"] {
            object.entry(optional).or_insert(Value::Null);
        }
        object.insert(
            "resources".into(),
            json!({
                "cpu_core_s": report.cpu_ns as f64 / 1e9,
                "peak_memory_bytes": report.peak_anon_bytes,
                "memory_basis": report.memory_basis,
                "mem_byte_seconds": report.mem_byte_seconds,
                "usage_measured": report.usage_measured,
            }),
        );
    }
    let mut notes = Vec::new();
    if let Some(context) = context {
        tasks = match report {
            Some(report) => self::tasks(report, context, status == "completed", &mut notes),
            None => context
                .job
                .tasks
                .iter()
                .map(|t| {
                    json!({"key": t.id, "role": t.role.as_str(), "kind": t.kind, "status": "skipped"})
                })
                .collect(),
        };
        object.insert(
            "sinks".into(),
            Value::Array(report.map_or_else(Vec::new, |r| sinks(r, context))),
        );
    } else {
        object.insert("sinks".into(), json!([]));
    }
    if !notes.is_empty()
        && let Value::Array(kept) = object.entry("notes").or_insert_with(|| json!([]))
    {
        kept.extend(notes.into_iter().map(Value::String));
    }
    object.insert("tasks".into(), Value::Array(tasks.clone()));
    object.insert(
        "exit".into(),
        json!({"code": exit.code, "status": status, "diagnostic": exit.diagnostic}),
    );
    normalise_units(&mut value);
    let object = value.as_object_mut().expect("an object");
    object.insert("schema_version".into(), json!(SCHEMA_VERSION));
    object.insert("moruna".into(), json!(env!("CARGO_PKG_VERSION")));
    object.insert("job_digest".into(), json!(job_digest));
    if let Some(report) = report {
        object.insert("host".into(), host(report));
    }
    let plan = context.and_then(|c| c.plan.clone());
    object.insert(
        "plan_steps".into(),
        plan.as_ref()
            .map_or(Value::Null, |plan| plan_steps(plan, &tasks)),
    );
    object.insert("plan".into(), plan.unwrap_or(Value::Null));
    object.insert(
        "args".into(),
        Value::Object(context.map(|c| c.args.clone()).unwrap_or_default()),
    );
    object.insert(
        "trace_path".into(),
        json!(
            context
                .and_then(|c| c.trace_path.as_ref())
                .map(|p| p.to_string_lossy().into_owned())
        ),
    );
    value
}

/// Every number of the trace's in the report's units: a field in seconds (`_s`) becomes
/// milliseconds (`_ms`), and so does one in nanoseconds (`_ns`) or microseconds (`_us`);
/// rates (`_per_s`) and CPU (`cpu_core_s`) stay.
fn normalise_units(value: &mut Value) {
    match value {
        Value::Object(map) => {
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in keys {
                let Some(mut inner) = map.remove(&key) else {
                    continue;
                };
                normalise_units(&mut inner);
                let scaled =
                    |suffix: &str, factor: f64, inner: &Value| -> Option<(String, Value)> {
                        let stem = key.strip_suffix(suffix)?;
                        let n = inner.as_f64()?;
                        Some((format!("{stem}_ms"), json!(n * factor)))
                    };
                let renamed = if key.ends_with("_per_s") || key.ends_with("cpu_core_s") {
                    None
                } else {
                    scaled("_s", 1e3, &inner)
                        .or_else(|| scaled("_ns", 1e-6, &inner))
                        .or_else(|| scaled("_us", 1e-3, &inner))
                };
                match renamed {
                    Some((key, scaled)) => {
                        map.insert(key, scaled);
                    }
                    None => {
                        map.insert(key, inner);
                    }
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(normalise_units),
        _ => {}
    }
}

/// The host the run ran on: its platform, its processors, the memory limit and CPU quota the
/// run started with, and its devices.
fn host(report: &RunReport) -> Value {
    let limits = &report.limits_initial;
    json!({
        "platform": format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
        "cpus": std::thread::available_parallelism().map(|n| n.get()).ok(),
        "memory_limit_bytes": limits.memory_ceiling,
        "cpu_quota_cores": limits.cpu_quota,
        "limits_source": limits.source,
        "devices": serde_json::to_value(&limits.devices).unwrap_or(Value::Null),
    })
}

/// The stage that runs task `i`, 1-based, and the stage's report, when the trace saw it.
fn stage_of<'r>(
    report: &'r RunReport,
    context: &ReportContext,
    i: usize,
) -> Option<(u16, Vec<usize>, Option<&'r StageReport>)> {
    let at = context
        .parts
        .stages
        .iter()
        .position(|tasks| tasks.contains(&i))?;
    let stage = u16::try_from(at + 1).ok()?;
    Some((
        stage,
        context.parts.stages[at].clone(),
        report.stages.iter().find(|s| s.stage == stage),
    ))
}

fn put(entry: &mut Map<String, Value>, key: &str, value: impl Into<Value>) {
    entry.insert(key.to_string(), value.into());
}

/// The figures the trace measured of a stage, in the report's units.
fn stage_figures(entry: &mut Map<String, Value>, s: &StageReport) {
    let ms = |seconds: f64| seconds * 1e3;
    if let (Some(start), Some(end)) = (s.started_ns, s.ended_ns) {
        put(entry, "started_at_ms", start as f64 / 1e6);
        put(entry, "ended_at_ms", end as f64 / 1e6);
    }
    put(entry, "wall_ms", ms(s.wall_s));
    put(entry, "busy_ms", ms(s.kernel_busy_s));
    put(entry, "cpu_core_s", s.cpu_core_s);
    put(entry, "peak_memory_bytes", s.peak_memory_bytes);
    put(entry, "instances", s.instances);
    put(entry, "morsels_in", s.morsels);
    put(entry, "morsels_out", s.morsels_out);
    put(entry, "rows_in", s.rows_in);
    put(entry, "rows_out", s.rows_out);
    put(entry, "bytes_in", s.bytes_in);
    put(entry, "bytes_out", s.bytes_out);
    if s.wall_s > 0.0 {
        put(entry, "rows_per_s", s.rows_per_s);
        put(entry, "bytes_per_s", s.bytes_per_s);
    }
    if s.bytes_in > 0 {
        put(entry, "amplification_p50_ratio", s.amplification_p50);
        put(entry, "amplification_p95_ratio", s.amplification_p95);
    }
    put(entry, "state_bytes_max", s.state_bytes_max);
    put(entry, "state_growth_bytes", s.state_growth);
    if let Some(alloc) = &s.alloc
        && let Ok(mut alloc) = serde_json::to_value(alloc)
    {
        normalise_units(&mut alloc);
        put(entry, "alloc", alloc);
    }
    put(entry, "failed_morsels", s.errors);
    put(entry, "skipped_morsels", s.skipped);
    if let Some(wait) = s.input_wait_s {
        put(entry, "input_wait_ms", ms(wait));
    }
    if let Some(wait) = s.output_wait_s {
        put(entry, "output_wait_ms", ms(wait));
    }
}

/// The plan's step that runs task `key`, and that step's predicted cost per morsel.
fn planned(plan: &Value, key: &str) -> Option<(String, moruna_cost::UnitCost)> {
    let step = plan.get("steps")?.as_array()?.iter().find(|step| {
        step.get("tasks")
            .and_then(Value::as_array)
            .is_some_and(|t| t.iter().any(|k| k.as_str() == Some(key)))
    })?;
    let cost: moruna_cost::UnitCost = serde_json::from_value(step.get("cost")?.clone()).ok()?;
    Some((step.get("id")?.as_str()?.to_string(), cost))
}

fn estimate(e: &moruna_cost::Estimate, factor: f64) -> Value {
    json!({
        "p50": e.p50 * factor,
        "p95": e.p95 * factor,
        "basis": serde_json::to_value(e.basis).unwrap_or(Value::Null),
        "observations": e.observations,
    })
}

/// Task `i`'s cost key ([`moruna_cost::CostKey`]): its role and kind, the code it runs, its
/// settings' digest and its input schema's. A fused stage's is the stage's (kind `fused`), on
/// the task that leads it, since the figures are the stage's; a task fused into another's stage
/// has none. `None` where the run resolved no schema for the task.
fn cost_key(
    report: &RunReport,
    context: &ReportContext,
    stages: &[crate::cost::StageCost],
    i: usize,
) -> Option<moruna_cost::CostKey> {
    use moruna_cost::Digest;
    let task = &context.job.tasks[i];
    let parts = &context.parts;
    let digests = &report.part_digests;
    let own = |schema: Option<&Option<[u8; 32]>>| {
        Some(moruna_cost::CostKey {
            role: task.role,
            kind: task.kind.clone(),
            fingerprint: *context.identities.get(i)?,
            settings: crate::cost::registration_settings(
                &task.kind,
                &Value::Object(task.settings.clone()),
            ),
            schema: Digest((*schema?)?),
        })
    };
    match task.role {
        Role::Source => own(parts
            .sources
            .iter()
            .position(|&t| t == i)
            .and_then(|at| digests.source_schemas.get(at))),
        Role::Sink => own(parts
            .sinks
            .iter()
            .position(|&t| t == i)
            .and_then(|at| digests.sink_schemas.get(at))),
        Role::Kernel => {
            let at = parts.stages.iter().position(|m| m.first() == Some(&i))?;
            let identity = stages.get(at)?;
            let fingerprint = identity
                .fingerprint
                .or_else(|| digests.stage_fingerprints.get(at).copied().map(Digest))?;
            Some(moruna_cost::CostKey {
                role: Role::Kernel,
                kind: identity.kind.clone(),
                fingerprint,
                settings: identity.settings,
                schema: Digest((*digests.stage_schemas.get(at)?)?),
            })
        }
    }
}

/// What the store holds of `key` that a recommendation draws on: the runs its records name,
/// the records, and the sums of their wall and worker time and their durations.
struct Stored {
    runs: u64,
    observations: u64,
    summarised: u64,
    worker_s: f64,
    duration_s: f64,
    durations: Vec<f64>,
}

fn stored(
    store: &dyn moruna_cost::ObservationStore,
    key: &moruna_cost::CostKey,
) -> moruna_cost::Result<Stored> {
    use moruna_cost::Target;
    let history = store.scan(key)?;
    let mut out = Stored {
        runs: history.runs(),
        observations: history.observations.len() as u64,
        summarised: history.summary.as_ref().map_or(0, |s| s.count),
        worker_s: 0.0,
        duration_s: 0.0,
        durations: Vec::new(),
    };
    for o in &history.observations {
        if let (Some(worker), Some(duration)) = (
            o.outcome.get(Target::WorkerTime),
            o.outcome.get(Target::Duration),
        ) {
            out.worker_s += worker;
            out.duration_s += duration;
            out.durations.push(duration);
        }
    }
    out.durations.sort_by(f64::total_cmp);
    Ok(out)
}

/// The value at quantile `q` of sorted `values`, nearest rank.
fn quantile(values: &[f64], q: f64) -> Option<f64> {
    let last = values.len().checked_sub(1)?;
    values.get(((last as f64) * q).round() as usize).copied()
}

/// What the plan predicted for a task beside what was observed, per morsel; its observation in
/// the cost model's form, under its full key; and the recommendation for its next run, each with
/// its basis: the store's records of the key across runs when a store is configured and holds
/// them, else this run only.
fn learned(
    entry: &mut Map<String, Value>,
    task: &moruna_job::Task,
    key: Option<&moruna_cost::CostKey>,
    report: &RunReport,
    context: &ReportContext,
    notes: &mut Vec<String>,
) {
    let read = |k: &str| entry.get(k).and_then(Value::as_f64);
    if task.role != Role::Kernel {
        // A source's or a sink's observation is the whole run's: what it gave or wrote.
        let (rows, bytes) = match task.role {
            Role::Source => ("rows_out", "bytes_out"),
            _ => ("rows_in", "bytes_in"),
        };
        let (rows, bytes) = (entry.get(rows).cloned(), entry.get(bytes).cloned());
        if let (Some(key), Some(rows), Some(bytes)) = (key, rows, bytes) {
            put(
                entry,
                "observation",
                json!({
                    "key": key,
                    "run": report.run_id,
                    "unit": "run",
                    "outcome": {"origin": "run", "rows_out": rows, "bytes_out": bytes},
                }),
            );
        }
        return;
    }
    let (Some(morsels), Some(busy_ms), Some(wall_ms)) =
        (read("morsels_in"), read("busy_ms"), read("wall_ms"))
    else {
        return;
    };
    if morsels < 1.0 {
        return;
    }
    let peak = read("peak_memory_bytes").unwrap_or(0.0);
    let cpu = read("cpu_core_s").unwrap_or(0.0);
    let rows_out = read("rows_out");
    let bytes_out = read("bytes_out");
    let growth = read("state_growth_bytes");
    let instances = read("instances").unwrap_or(1.0);
    let per_morsel_ms = busy_ms / morsels;
    let prediction = context
        .plan
        .as_ref()
        .and_then(|plan| planned(plan, &task.id));
    if let Some((step, cost)) = &prediction {
        put(
            entry,
            "planned",
            json!({
                "step": step,
                "per_morsel": {
                    "duration_ms": estimate(&cost.duration, 1e3),
                    "memory_bytes": estimate(&cost.peak_memory, 1.0),
                },
                "observed_per_morsel": {"duration_ms": per_morsel_ms, "memory_bytes": peak},
            }),
        );
    }
    // The unit the live learner keeps of a stage: one morsel's wall and worker time and what it
    // gave; a unit's memory is not measured in a shared process, so it has none.
    let mut outcome = json!({
        "origin": "run",
        "duration": per_morsel_ms / 1e3,
        "worker_time": cpu / morsels,
    });
    for (field, value) in [("rows_out", rows_out), ("bytes_out", bytes_out)] {
        if let Some(n) = value {
            outcome[field] = json!(n / morsels);
        }
    }
    if let Some(growth) = growth {
        outcome["state_growth"] = json!(growth / morsels);
    }
    if let Some(key) = key {
        put(
            entry,
            "observation",
            json!({
                "key": key,
                "run": report.run_id,
                "unit": "morsel",
                "units": morsels,
                "outcome": outcome,
            }),
        );
    }
    let this_run = |extra: Value| {
        let mut basis = json!({"source": "this_run", "runs": 1, "observations": morsels});
        if let (Some(basis), Value::Object(extra)) = (basis.as_object_mut(), extra) {
            basis.extend(extra);
        }
        basis
    };
    let error = prediction.as_ref().map(|(_, cost)| {
        let predicted = cost.peak_memory.p95.max(1.0);
        (peak - predicted) / predicted
    });
    // A unit's memory is not in the store, so the task's peak is this run's.
    let mut memory = json!({
        "value_bytes": peak.ceil(),
        "basis": this_run(json!({
            "statistic": "peak",
            "percentile": 100,
            "observed_peak_bytes": peak,
        })),
    });
    if let Some(error) = error {
        memory["basis"]["last_prediction_error_ratio"] = json!(error);
    }
    let mut recommendation = json!({"memory": memory});
    let from_store = match (key, context.store.as_deref()) {
        (Some(key), Some(store)) => match stored(store, key) {
            Ok(found) if found.duration_s > 0.0 => Some(found),
            Ok(_) => None,
            Err(e) => {
                notes.push(format!(
                    "task `{}`: the observation store could not be read, so its recommendation \
                     is this run's only: {e}",
                    task.id
                ));
                None
            }
        },
        _ => None,
    };
    let seconds = wall_ms / 1e3;
    if seconds > 0.0 {
        let parallel = (busy_ms / wall_ms).ceil().max(1.0);
        let parallel = parallel.min(instances.max(parallel));
        recommendation["instances"] = json!({
            "value": parallel,
            "basis": this_run(json!({
                "statistic": "mean",
                "observed_instances": instances,
                "observed_busy_ms": busy_ms,
                "observed_wall_ms": wall_ms,
            })),
        });
        recommendation["cpu"] = match &from_store {
            // The cores one instance uses, from every unit the store keeps of the key, times the
            // instances this run needed.
            Some(found) => json!({
                "value_cores": found.worker_s / found.duration_s * parallel,
                "basis": {
                    "source": "store",
                    "statistic": "mean",
                    "runs": found.runs,
                    "observations": found.observations,
                    "summarised_observations": found.summarised,
                    "observed_worker_s": found.worker_s,
                    "observed_duration_s": found.duration_s,
                    "instances": parallel,
                },
            }),
            None => json!({
                "value_cores": cpu / seconds,
                "basis": this_run(json!({
                    "statistic": "mean",
                    "observed_cpu_core_s": cpu,
                    "observed_wall_ms": wall_ms,
                })),
            }),
        };
    }
    recommendation["morsel_duration_ms"] = match &from_store {
        Some(found) => json!({
            "p50": quantile(&found.durations, 0.5).map(|s| s * 1e3),
            "p95": quantile(&found.durations, 0.95).map(|s| s * 1e3),
            "basis": {
                "source": "store",
                "runs": found.runs,
                "observations": found.observations,
                "summarised_observations": found.summarised,
            },
        }),
        None => json!({
            "p50": per_morsel_ms,
            "p95": per_morsel_ms,
            "basis": this_run(json!({"statistic": "mean"})),
        }),
    };
    put(entry, "recommendation", recommendation);
}

/// One entry per task, document order, keyed by its name in the spec: its role and kind (and
/// image), its status, and everything measured of it. A figure the run did not measure for the
/// task is absent.
fn tasks(
    report: &RunReport,
    context: &ReportContext,
    completed: bool,
    notes: &mut Vec<String>,
) -> Vec<Value> {
    let job = &context.job;
    let stages = crate::cost::stage_identities(job, &context.parts).unwrap_or_else(|e| {
        notes.push(format!("the run's stages have no cost keys: {e}"));
        Vec::new()
    });
    let sink_outcome = |i: usize| {
        let ordinal = context.parts.sinks.iter().position(|&t| t == i)?;
        report.sinks.iter().find(|s| s.sink == ordinal)
    };
    // What task `i` takes from a task feeding it, when that is measured: a function of one input
    // takes all it is given; a sink takes what it writes.
    let taken = |i: usize| -> Option<(u64, u64)> {
        let task = &job.tasks[i];
        match task.role {
            Role::Kernel if task.inputs.len() == 1 => stage_of(report, context, i)
                .and_then(|(_, tasks, s)| (tasks.first() == Some(&i)).then_some(s).flatten())
                .map(|s| (s.rows_in, s.bytes_in)),
            Role::Sink => sink_outcome(i).map(|s| (s.rows, s.bytes)),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(job.tasks.len());
    for (i, task) in job.tasks.iter().enumerate() {
        let mut entry = Map::new();
        put(&mut entry, "key", task.id.clone());
        put(&mut entry, "role", task.role.as_str());
        put(&mut entry, "kind", task.kind.clone());
        if let Some(digest) = task
            .settings
            .get("image")
            .and_then(|i| i.get("digest"))
            .and_then(Value::as_str)
        {
            put(&mut entry, "image_digest", digest);
        }
        let mut errors = 0;
        let mut ran = false;
        match task.role {
            Role::Source => {
                let consumers = job
                    .edges
                    .iter()
                    .filter(|e| e.from.task == task.id)
                    .filter_map(|e| job.tasks.iter().position(|t| t.id == e.to.task));
                if let Some((rows, bytes)) = consumers.filter_map(taken).next() {
                    put(&mut entry, "rows_out", rows);
                    put(&mut entry, "bytes_out", bytes);
                    ran = true;
                }
            }
            Role::Kernel => {
                if let Some((stage, members, found)) = stage_of(report, context, i) {
                    let fused: Vec<String> = members
                        .iter()
                        .filter(|&&m| m != i)
                        .map(|&m| job.tasks[m].id.clone())
                        .collect();
                    if !fused.is_empty() {
                        put(&mut entry, "fused_with", json!(fused));
                    }
                    if let Some(s) = found.filter(|_| members.first() == Some(&i)) {
                        stage_figures(&mut entry, s);
                        errors = s.errors;
                        ran = s.morsels > 0;
                    }
                    let _ = stage;
                }
            }
            Role::Sink => {
                if let Some(s) = sink_outcome(i) {
                    put(&mut entry, "rows_in", s.rows);
                    put(&mut entry, "bytes_in", s.bytes);
                    put(&mut entry, "excluded_rows", s.excluded_rows);
                    ran = true;
                }
            }
        }
        let status = if errors > 0 {
            "failed"
        } else if completed || ran {
            "ok"
        } else {
            "skipped"
        };
        put(&mut entry, "status", status);
        let needs: BTreeMap<String, Value> = context
            .task_ports
            .iter()
            .filter(|(t, _)| *t == i)
            .flat_map(|(_, ports)| ports.stats())
            .map(|(name, s)| {
                (
                    name,
                    json!({
                        "calls": s.calls,
                        "bytes_in": s.bytes_in,
                        "bytes_out": s.bytes_out,
                        "time_ms": s.wait_ns as f64 / 1e6,
                    }),
                )
            })
            .collect();
        if !needs.is_empty() {
            put(&mut entry, "needs", json!(needs));
        }
        let key = cost_key(report, context, &stages, i);
        learned(&mut entry, task, key.as_ref(), report, context, notes);
        out.push(Value::Object(entry));
    }
    out
}

/// Each plan step's actual start and end: the first of its tasks to start, the last to end,
/// so the parallelism the run achieved shows beside the plan's.
fn plan_steps(plan: &Value, tasks: &[Value]) -> Value {
    let Some(steps) = plan.get("steps").and_then(Value::as_array) else {
        return Value::Null;
    };
    let at = |key: &str, field: &str| -> Option<f64> {
        tasks
            .iter()
            .find(|t| t.get("key").and_then(Value::as_str) == Some(key))?
            .get(field)?
            .as_f64()
    };
    Value::Array(
        steps
            .iter()
            .map(|step| {
                let keys: Vec<&str> = step
                    .get("tasks")
                    .and_then(Value::as_array)
                    .map(|t| t.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                let mut out = Map::new();
                put(
                    &mut out,
                    "step",
                    step.get("id").cloned().unwrap_or(Value::Null),
                );
                put(&mut out, "tasks", json!(keys));
                let starts: Vec<f64> = keys.iter().filter_map(|k| at(k, "started_at_ms")).collect();
                let ends: Vec<f64> = keys.iter().filter_map(|k| at(k, "ended_at_ms")).collect();
                if !starts.is_empty() {
                    put(
                        &mut out,
                        "started_at_ms",
                        starts.iter().copied().fold(f64::MAX, f64::min),
                    );
                }
                if !ends.is_empty() {
                    put(
                        &mut out,
                        "ended_at_ms",
                        ends.iter().copied().fold(0.0, f64::max),
                    );
                }
                Value::Object(out)
            })
            .collect(),
    )
}

/// One entry per sink, document order: its task, its target as its settings name it, what it
/// wrote, and the version it committed (`snapshot_id`, which every committing sink names, and
/// `version`).
fn sinks(report: &RunReport, context: &ReportContext) -> Vec<Value> {
    report
        .sinks
        .iter()
        .map(|s| {
            let task = context
                .parts
                .sinks
                .get(s.sink)
                .and_then(|&i| context.job.tasks.get(i));
            let target = context
                .sink_targets
                .get(s.sink)
                .filter(|t| !t.is_empty())
                .cloned();
            json!({
                "task": task.map(|t| t.id.clone()),
                "target": target,
                "rows": s.rows,
                "bytes": s.bytes,
                "files": s.files,
                "excluded_rows": s.excluded_rows,
                "snapshot_id": s.snapshot_id,
                "version": s.version,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use moruna_job::{Edge, Job, KindDoc, Task};
    use moruna_kernel::{GilState, IoPaths};
    use moruna_trace::{LimitsSummary, SinkOutcome, StageReport};

    fn stage(stage: u16) -> StageReport {
        StageReport {
            stage,
            morsels: 12,
            rows_in: 1_000,
            rows_out: 1_000,
            bytes_in: 1 << 20,
            bytes_out: 1 << 20,
            wall_s: 1.0,
            kernel_busy_s: 0.9,
            rows_per_s: 1_000.0,
            bytes_per_s: 1.0,
            amplification_p50: 1.0,
            amplification_p95: 1.1,
            placement_miss_wait_s: 0.0,
            errors: 0,
            skipped: 0,
            state_bytes_max: 0,
            state_growth: 0,
            alloc: None,
            started_ns: Some(1_000_000),
            ended_ns: Some(1_001_000_000),
            cpu_core_s: 0.5,
            peak_memory_bytes: 3 << 20,
            instances: 2,
            morsels_out: 12,
            input_wait_s: Some(0.25),
            output_wait_s: Some(0.0),
        }
    }

    fn report(stages: usize, notes: usize) -> RunReport {
        RunReport {
            run_id: "0".repeat(32),
            exit: ExitReason::Completed,
            resumed: false,
            manifest: Some("/tmp/run/manifest.json".into()),
            wall_s: 12.5,
            limits_initial: LimitsSummary {
                memory_ceiling: 8 << 30,
                memory_kill: None,
                cpu_quota: 8.0,
                source: "cgroup".into(),
                devices: Vec::new(),
            },
            limits_timeline: Vec::new(),
            drains: Vec::new(),
            io_paths: IoPaths {
                direct_io: true,
                io_uring: false,
                gds: false,
                pinned: false,
                rdma: false,
            },
            memory_basis: None,
            peak_anon_bytes: 4 << 30,
            peak_fraction_of_ceiling: 0.5,
            peak_ceiling_bytes: 8 << 30,
            cpu_ns: 0,
            mem_byte_seconds: 0,
            usage_measured: false,
            worker_busy_fraction: 0.9,
            cpu_throttled_fraction: 0.0,
            source_bytes_per_s: 1.0,
            source_bandwidth: 1.0,
            staging_bandwidth: 0.0,
            staging_bytes_written: 0,
            staging_engaged: false,
            gil: vec![(1, GilState::FreeThreaded)],
            gil_serialised: false,
            sizer_used: "rule".into(),
            sizer_fallback_at: None,
            bottleneck_timeline: Vec::new(),
            stages: (0..stages).map(|i| stage(i as u16 + 1)).collect(),
            notes: (0..notes).map(|i| format!("note {i}")).collect(),
            overflow_failed: false,
            late_records: 0,
            write: None,
            read: None,
            sinks: Vec::new(),
            buffers: None,
            policy: None,
            part_digests: moruna_trace::PartDigests {
                source_schemas: vec![Some([1; 32])],
                stage_schemas: (0..stages).map(|_| Some([2; 32])).collect(),
                stage_fingerprints: (0..stages).map(|_| [3; 32]).collect(),
                sink_schemas: vec![Some([4; 32]), Some([5; 32])],
            },
        }
    }

    /// The document of `r`, of the run `context()` builds.
    fn doc(r: &RunReport, error: Option<&MorunaError>, context: &ReportContext) -> Value {
        document(
            (&r.run_id, Some("sha256:ab")),
            Some(r),
            &Exit::of(r, error),
            Some(context),
        )
    }

    fn context() -> ReportContext {
        let entry = |kind: &str| KindDoc::new(kind, serde_json::Map::new());
        let job = Job::new(
            vec![
                Task::source("rows", entry("parquet")),
                Task::kernel("shout", entry("image")),
                Task::kernel("keep", entry("std")),
                Task::sink("out", entry("image")),
                Task::sink("copy", entry("parquet")),
            ],
            vec![
                Edge::new("rows", "out", "shout", "in"),
                Edge::new("shout", "out", "keep", "in"),
                Edge::new("keep", "out", "out", "in"),
                Edge::new("rows", "out", "copy", "in"),
            ],
        );
        ReportContext {
            job,
            parts: crate::job::Parts {
                sources: vec![0],
                stages: vec![vec![1, 2]],
                sinks: vec![3, 4],
            },
            sink_targets: vec![String::new(), "file:///copy".into()],
            plan: None,
            trace_path: None,
            args: serde_json::Map::from_iter([("start".to_string(), json!("2026-11-01"))]),
            task_ports: Vec::new(),
            identities: (0..5).map(|i| moruna_cost::Digest([10 + i; 32])).collect(),
            store: None,
        }
    }

    /// The report a run returns names its job's digest, its exit as a code, every task by its
    /// key with what it took and gave, what it consumed, and every sink with the version it
    /// committed; a fused stage's figures are its tasks', which name each other.
    #[test]
    fn the_report_names_the_job() {
        let mut r = report(1, 0);
        r.sinks = vec![
            SinkOutcome {
                sink: 0,
                rows: 1_000,
                bytes: 10,
                files: Vec::new(),
                excluded_rows: 3,
                snapshot_id: Some("42".into()),
                version: Some("v7".into()),
            },
            SinkOutcome {
                sink: 1,
                rows: 1_000,
                bytes: 20,
                files: vec!["part-0.parquet".into()],
                excluded_rows: 0,
                snapshot_id: None,
                version: None,
            },
        ];
        r.cpu_ns = 5_000_000_000;
        let value = doc(&r, None, &context());
        assert_eq!(value["schema_version"], SCHEMA_VERSION);
        assert_eq!(value["moruna"], env!("CARGO_PKG_VERSION"));
        assert_eq!(value["args"], json!({"start": "2026-11-01"}));
        assert!(
            value["host"]["platform"]
                .as_str()
                .is_some_and(|p| p.contains('/'))
        );
        assert_eq!(value["host"]["memory_limit_bytes"], 8u64 << 30);
        assert!(value.get("stages").is_none(), "tasks hold what stages did");
        assert!(value.get("moruna_report").is_none() && value.get("cpu_ns").is_none());
        assert_eq!(value["job_digest"], "sha256:ab");
        assert_eq!(
            value["exit"],
            json!({"code": 0, "status": "completed", "diagnostic": null})
        );
        assert_eq!(value["resources"]["cpu_core_s"], 5.0);
        assert_eq!(
            value["wall_ms"], 12_500.0,
            "the trace's seconds are milliseconds"
        );
        assert_eq!(value["resources"]["peak_memory_bytes"], 4u64 << 30);
        let tasks = value["tasks"].as_array().expect("tasks");
        assert_eq!(tasks.len(), 5);
        assert_eq!(tasks[0]["key"], "rows");
        assert_eq!(tasks[0]["rows_out"], 1_000);
        assert_eq!(tasks[1]["fused_with"], json!(["keep"]));
        assert_eq!(tasks[2]["fused_with"], json!(["shout"]));
        assert_eq!(tasks[1]["rows_in"], 1_000);
        assert_eq!(tasks[1]["status"], "ok");
        assert_eq!(tasks[1]["wall_ms"], 1_000.0);
        assert_eq!(tasks[1]["started_at_ms"], 1.0);
        assert_eq!(tasks[1]["cpu_core_s"], 0.5);
        assert_eq!(tasks[1]["input_wait_ms"], 250.0);
        assert_eq!(tasks[1]["output_wait_ms"], 0.0, "a measured zero is zero");
        assert_eq!(tasks[1]["amplification_p95_ratio"], 1.1);
        assert_eq!(tasks[1]["state_bytes_max"], 0);
        let memory = &tasks[1]["recommendation"]["memory"];
        assert_eq!(memory["value_bytes"], (3u64 << 20) as f64);
        assert_eq!(memory["basis"]["percentile"], 100);
        assert_eq!(memory["basis"]["observations"], 12.0);
        // A fused stage's observation is the stage's, on the task that leads it, under its full
        // key: the kernel's fingerprint, the stage's settings, its input schema's hash.
        let key = &tasks[1]["observation"]["key"];
        assert_eq!(key["role"], "function");
        assert_eq!(key["kind"], "fused");
        assert_eq!(key["fingerprint"], "03".repeat(32));
        assert_eq!(key["schema"], "02".repeat(32));
        assert_eq!(tasks[1]["observation"]["unit"], "morsel");
        assert_eq!(tasks[1]["observation"]["run"], "0".repeat(32));
        assert!(
            tasks[1]["observation"]["outcome"]
                .get("peak_memory")
                .is_none(),
            "a unit's memory is not measured"
        );
        assert_eq!(memory["basis"]["source"], "this_run");
        assert_eq!(memory["basis"]["runs"], 1);
        assert_eq!(
            tasks[1]["recommendation"]["cpu"]["basis"]["source"],
            "this_run"
        );
        // A source's and a sink's observation is the run's, under the catalogue's identity of
        // its kind and the schema it gives or is given.
        assert_eq!(
            tasks[0]["observation"]["key"]["fingerprint"],
            "0a".repeat(32)
        );
        assert_eq!(tasks[0]["observation"]["key"]["schema"], "01".repeat(32));
        assert_eq!(tasks[0]["observation"]["unit"], "run");
        assert_eq!(
            tasks[4]["observation"]["key"]["fingerprint"],
            "0e".repeat(32)
        );
        assert_eq!(tasks[4]["observation"]["key"]["schema"], "05".repeat(32));
        assert_eq!(tasks[4]["observation"]["outcome"]["bytes_out"], 20);
        // The second task of a fused stage, and a source, have no figures of their own: absent.
        for absent in ["wall_ms", "input_wait_ms", "recommendation", "planned"] {
            assert!(tasks[2].get(absent).is_none(), "{absent}");
        }
        assert!(tasks[0].get("wall_ms").is_none() && tasks[0].get("rows_in").is_none());
        assert_eq!(tasks[3]["excluded_rows"], 3);
        assert_eq!(tasks[3]["status"], "ok");
        let sinks = value["sinks"].as_array().expect("sinks");
        assert_eq!(sinks[0]["task"], "out");
        assert_eq!(sinks[0]["target"], Value::Null);
        assert_eq!(sinks[0]["snapshot_id"], "42");
        assert_eq!(sinks[0]["version"], "v7");
        assert_eq!(sinks[1]["task"], "copy");
        assert_eq!(sinks[1]["target"], "file:///copy");
        assert_eq!(sinks[1]["files"], json!(["part-0.parquet"]));

        let mut failed = report(1, 0);
        failed.exit = ExitReason::Terminated {
            diagnostic: "function stage 1".into(),
        };
        let error = MorunaError::Plan("x".into());
        let value = doc(&failed, Some(&error), &context());
        assert_eq!(value["exit"]["status"], "terminated");
        assert_eq!(value["exit"]["diagnostic"], "function stage 1");
        assert_eq!(value["exit"]["code"], json!(crate::host::exit_code(&error)));
        failed.exit = ExitReason::Cancelled;
        let value = doc(&failed, None, &context());
        assert_eq!(value["exit"]["code"], 130);
        assert_eq!(value["exit"]["status"], "cancelled");
    }

    /// An image task's key names its build key, so two runs of the same code are one key
    /// whatever image digest each built; with a store configured its recommendation draws on
    /// every run the store holds of the key, and says how many.
    #[test]
    fn an_image_task_is_keyed_by_its_build_key_and_learns_across_runs() {
        use moruna_cost::{Inputs, Observation, ObservationSource, ObservationStore, Origin};
        use moruna_cost::{Outcome, Target};
        let build_key = format!("sha256:{}", "ab".repeat(32));
        let mut settings = Map::new();
        settings.insert(
            "image".into(),
            json!({"build_key": build_key, "digest": format!("sha256:{}", "cd".repeat(32))}),
        );
        let mut context = context();
        context.job.tasks[1] = Task::kernel("shout", KindDoc::new("image", settings));
        context.parts.stages = vec![vec![1], vec![2]];
        let r = report(2, 0);
        let value = doc(&r, None, &context);
        let key = value["tasks"][1]["observation"]["key"].clone();
        assert_eq!(key["kind"], "image");
        assert_eq!(key["fingerprint"], "ab".repeat(32));
        assert_eq!(
            value["tasks"][2]["observation"]["key"]["fingerprint"],
            "03".repeat(32)
        );

        let dir = std::env::temp_dir().join(format!(
            "moruna-report-store-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let store = std::sync::Arc::new(moruna_cost::LocalStore::open(&dir).expect("opens"));
        let key: moruna_cost::CostKey = serde_json::from_value(key).expect("a cost key");
        for (run, duration) in [("r1", 0.1), ("r1", 0.3), ("r2", 0.2)] {
            let mut outcome = Outcome::new(Origin::Run);
            outcome
                .set(Target::Duration, duration)
                .set(Target::WorkerTime, duration / 2.0);
            let mut observation = Observation::new(key.clone(), Inputs::batch(10, 100), outcome, 1);
            observation.run = Some(run.to_string());
            store.append(&observation).expect("appends");
        }
        assert_eq!(store.scan(&key).expect("scans").runs(), 2);
        context.store = Some(store);
        let value = doc(&r, None, &context);
        let recommendation = &value["tasks"][1]["recommendation"];
        let cpu = &recommendation["cpu"]["basis"];
        assert_eq!(cpu["source"], "store");
        assert_eq!(cpu["runs"], 2);
        assert_eq!(cpu["observations"], 3);
        assert_eq!(
            recommendation["cpu"]["value_cores"], 0.5,
            "half a core a unit, one instance"
        );
        assert_eq!(recommendation["morsel_duration_ms"]["p50"], 200.0);
        assert_eq!(recommendation["morsel_duration_ms"]["basis"]["runs"], 2);
        assert_eq!(recommendation["memory"]["basis"]["source"], "this_run");
        // A task the store holds nothing of is this run's only.
        assert_eq!(
            value["tasks"][2]["recommendation"]["cpu"]["basis"]["source"],
            "this_run"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A run refused before it started still has a document: its identity, its exit, its notes
    /// and every task skipped.
    #[test]
    fn a_refused_run_has_a_document() {
        let exit = Exit {
            code: crate::host::exit::SPEC_REFUSED,
            diagnostic: Some("tasks[1].settings: no".into()),
        };
        let value = document(("ab", None), None, &exit, Some(&context()));
        assert_eq!(value["schema_version"], SCHEMA_VERSION);
        assert_eq!(value["exit"]["status"], "refused");
        assert_eq!(value["job_digest"], Value::Null);
        assert_eq!(value["tasks"][3]["status"], "skipped");
        assert_eq!(value["sinks"], json!([]));
        assert!(value.get("host").is_none() && value.get("resources").is_none());
        let bare = document(("ab", None), None, &exit, None);
        assert_eq!(bare["tasks"], json!([]));
    }

    /// A run that ended before a task ran says it was skipped; one whose morsels failed, failed.
    #[test]
    fn a_task_that_did_not_run_is_skipped() {
        let mut entry = Map::new();
        let context = context();
        let report = report(1, 0);
        learned(
            &mut entry,
            &moruna_job::Task::kernel("t", moruna_job::KindDoc::new("std", Map::new())),
            None,
            &report,
            &context,
            &mut Vec::new(),
        );
        assert!(entry.is_empty(), "nothing measured, nothing derived");
    }
}

#[cfg(test)]
mod units {
    use super::*;

    #[test]
    fn every_number_names_its_unit() {
        let mut value = json!({
            "wall_s": 1.5, "cpu_core_s": 2.0, "observed_cpu_core_s": 1.0, "rows_per_s": 10.0,
            "lease_wait_us": 1000,
            "decision_ns": 2_000_000, "nested": [{"idle_ready_s": 0.5}], "name_s": "text",
        });
        normalise_units(&mut value);
        assert_eq!(
            value,
            json!({
                "wall_ms": 1500.0, "cpu_core_s": 2.0, "observed_cpu_core_s": 1.0,
                "rows_per_s": 10.0, "lease_wait_ms": 1.0,
                "decision_ms": 2.0, "nested": [{"idle_ready_ms": 500.0}], "name_s": "text",
            })
        );
    }
}
