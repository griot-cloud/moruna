//! The run report (PY-I5) as `moruna.run` returns it: one JSON document, which
//! `moruna.RunReport` (`python/moruna/_report.py`) reads and serialises canonically.
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
use pyo3::prelude::*;
use serde_json::{Map, Value, json};

use crate::run::ReportContext;

/// The report document's schema: `MAJOR.MINOR`. 2: `tasks` holds every figure of a task, and
/// `stages` is gone.
pub const SCHEMA_VERSION: &str = "2.0";

/// The report of a run as the JSON document `moruna.RunReport` reads: `report` and what
/// `context` knows of the job, `error` the error the run ended with, when it failed.
pub(crate) fn report_json(
    report: &RunReport,
    error: Option<&MorunaError>,
    context: &ReportContext,
) -> String {
    report_value(report, error, context).to_string()
}

fn report_value(report: &RunReport, error: Option<&MorunaError>, context: &ReportContext) -> Value {
    let mut value = serde_json::to_value(report).unwrap_or_else(|e| {
        json!({"run_id": report.run_id, "notes": [format!("the report did not serialise: {e}")]})
    });
    let (code, status, diagnostic) = match (&report.exit, error) {
        (ExitReason::Completed, None) => (moruna_runtime::host::exit::COMPLETED, "completed", None),
        (ExitReason::Cancelled, _) | (_, Some(MorunaError::Cancelled)) => {
            (moruna_runtime::host::exit::CANCELLED, "cancelled", None)
        }
        (ExitReason::Terminated { diagnostic }, error) => (
            error.map_or(
                moruna_runtime::host::exit::FAILED,
                moruna_runtime::host::exit_code,
            ),
            "terminated",
            Some(diagnostic.clone()),
        ),
        (ExitReason::Completed, Some(error)) => (
            moruna_runtime::host::exit_code(error),
            "terminated",
            Some(error.to_string()),
        ),
    };
    let object = value
        .as_object_mut()
        .expect("a report serialises as an object");
    // `tasks` holds what `stages` did, task by task; the totals the trace keeps per run stay.
    object.remove("stages");
    object.remove("cpu_ns");
    for optional in ["write", "read", "buffers", "policy", "memory_basis"] {
        object.entry(optional).or_insert(Value::Null);
    }
    object.insert(
        "exit".into(),
        json!({"code": code, "status": status, "diagnostic": diagnostic}),
    );
    let completed = status == "completed";
    let tasks = tasks(report, context, completed);
    object.insert("tasks".into(), Value::Array(tasks.clone()));
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
    object.insert("sinks".into(), Value::Array(sinks(report, context)));
    normalise_units(&mut value);
    let object = value.as_object_mut().expect("an object");
    object.insert("schema_version".into(), json!(SCHEMA_VERSION));
    object.insert("moruna".into(), json!(crate::VERSION));
    object.insert("host".into(), host(report));
    object.insert("args".into(), Value::Object(context.args.clone()));
    object.insert("job_digest".into(), json!(context.job_digest));
    object.insert("plan".into(), context.plan.clone().unwrap_or(Value::Null));
    object.insert(
        "plan_steps".into(),
        context
            .plan
            .as_ref()
            .map_or(Value::Null, |plan| plan_steps(plan, &tasks)),
    );
    object.insert(
        "trace_path".into(),
        json!(
            context
                .trace_path
                .as_ref()
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

/// What the plan predicted for a task beside what was observed, per morsel; its observation in
/// the cost model's form; and the recommendation for its next run, each with its basis.
fn learned(entry: &mut Map<String, Value>, task: &moruna_job::Task, plan: Option<&Value>) {
    let read = |k: &str| entry.get(k).and_then(Value::as_f64);
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
    let prediction = plan.and_then(|plan| planned(plan, &task.id));
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
    let mut outcome = json!({
        "origin": "run",
        "duration": per_morsel_ms / 1e3,
        "worker_time": per_morsel_ms / 1e3,
        "peak_memory": peak,
    });
    for (field, value) in [("rows_out", rows_out), ("bytes_out", bytes_out)] {
        if let Some(n) = value {
            outcome[field] = json!(n / morsels);
        }
    }
    if let Some(growth) = growth {
        outcome["state_growth"] = json!(growth);
    }
    let settings = moruna_runtime::cost::registration_settings(
        &task.kind,
        &Value::Object(task.settings.clone()),
    );
    put(
        entry,
        "observation",
        json!({
            "key": {"role": task.role.as_str(), "kind": task.kind, "settings": settings.to_hex()},
            "outcome": outcome,
            "units": morsels,
            "units_note": "the outcome is the mean of one unit (one morsel), in seconds and bytes as moruna-cost stores it",
        }),
    );
    let error = prediction.as_ref().map(|(_, cost)| {
        let predicted = cost.peak_memory.p95.max(1.0);
        (peak - predicted) / predicted
    });
    let mut memory = json!({
        "value_bytes": peak.ceil(),
        "basis": {
            "statistic": "peak",
            "percentile": 100,
            "observed_peak_bytes": peak,
            "observations": morsels,
            "runs": 1,
        },
    });
    if let Some(error) = error {
        memory["basis"]["last_prediction_error_ratio"] = json!(error);
    }
    let seconds = wall_ms / 1e3;
    let mut recommendation = json!({"memory": memory});
    if seconds > 0.0 {
        recommendation["cpu"] = json!({
            "value_cores": cpu / seconds,
            "basis": {
                "statistic": "mean",
                "observed_cpu_core_s": cpu,
                "observed_wall_ms": wall_ms,
                "observations": morsels,
                "runs": 1,
            },
        });
        let parallel = (busy_ms / wall_ms).ceil().max(1.0);
        recommendation["instances"] = json!({
            "value": parallel.min(instances.max(parallel)),
            "basis": {
                "statistic": "mean",
                "observed_instances": instances,
                "observed_busy_ms": busy_ms,
                "observed_wall_ms": wall_ms,
                "observations": morsels,
                "runs": 1,
            },
        });
    }
    put(entry, "recommendation", recommendation);
}

/// One entry per task, document order, keyed by its name in the spec: its role and kind (and
/// image), its status, and everything measured of it. A figure the run did not measure for the
/// task is absent.
fn tasks(report: &RunReport, context: &ReportContext, completed: bool) -> Vec<Value> {
    let job = &context.job;
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
        learned(&mut entry, task, context.plan.as_ref());
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

/// The report document as the Python object `moruna.RunReport` is.
pub(crate) fn to_python(py: Python<'_>, text: &str) -> PyResult<Py<PyAny>> {
    Ok(py
        .import("moruna._report")?
        .getattr("RunReport")?
        .call_method1("_from_json", (text,))?
        .unbind())
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
        }
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
            parts: moruna_runtime::job::Parts {
                sources: vec![0],
                stages: vec![vec![1, 2]],
                sinks: vec![3, 4],
            },
            sink_targets: vec![String::new(), "file:///copy".into()],
            job_digest: Some("sha256:ab".into()),
            plan: None,
            trace_path: None,
            args: serde_json::Map::from_iter([("start".to_string(), json!("2026-11-01"))]),
            task_ports: Vec::new(),
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
        let value = report_value(&r, None, &context());
        assert_eq!(value["schema_version"], SCHEMA_VERSION);
        assert_eq!(value["moruna"], crate::VERSION);
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
        assert_eq!(tasks[1]["observation"]["key"]["kind"], "image");
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
        let value = report_value(&failed, Some(&error), &context());
        assert_eq!(value["exit"]["status"], "terminated");
        assert_eq!(value["exit"]["diagnostic"], "function stage 1");
        assert_eq!(
            value["exit"]["code"],
            json!(moruna_runtime::host::exit_code(&error))
        );
        failed.exit = ExitReason::Cancelled;
        let value = report_value(&failed, None, &context());
        assert_eq!(value["exit"]["code"], 130);
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

    /// A run that ended before a task ran says it was skipped; one whose morsels failed, failed.
    #[test]
    fn a_task_that_did_not_run_is_skipped() {
        let mut entry = Map::new();
        learned(
            &mut entry,
            &moruna_job::Task::kernel("t", moruna_job::KindDoc::new("std", Map::new())),
            None,
        );
        assert!(entry.is_empty(), "nothing measured, nothing derived");
    }
}
