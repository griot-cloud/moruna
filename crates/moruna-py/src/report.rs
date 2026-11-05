//! The run report (PY-I5) as `moruna.run` returns it: one JSON document, which
//! `moruna.RunReport` (`python/moruna/_report.py`) reads and serialises canonically.
//!
//! The document is the trace's report (`moruna_trace::RunReport`, 04 d.1) with what the job adds:
//! the format (`moruna_report`), the spec's digest (`job_digest`), how the run ended as an exit
//! code and a diagnostic (`exit`), one entry per task (`tasks`), what the run consumed
//! (`resources`), one entry per sink with the version it committed (`sinks`), the plan when it
//! was asked for (`plan`) and the trace file (`trace_path`).

use moruna_job::Role;
use moruna_kernel::MorunaError;
use moruna_trace::{ExitReason, RunReport, StageReport};
use pyo3::prelude::*;
use serde_json::{Value, json};

use crate::run::ReportContext;

/// The report document's format version.
pub const REPORT_FORMAT: u32 = 1;

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
    // Every field is in the document, `null` when the run has nothing for it, so a reader
    // finds the same keys in every report.
    for optional in ["write", "read", "buffers", "policy", "memory_basis"] {
        object.entry(optional).or_insert(Value::Null);
    }
    object.insert("moruna_report".into(), json!(REPORT_FORMAT));
    object.insert("moruna".into(), json!(crate::VERSION));
    object.insert("job_digest".into(), json!(context.job_digest));
    object.insert(
        "exit".into(),
        json!({"code": code, "status": status, "diagnostic": diagnostic}),
    );
    object.insert("tasks".into(), Value::Array(tasks(report, context)));
    object.insert(
        "resources".into(),
        json!({
            "cpu_ns": report.cpu_ns,
            "peak_memory_bytes": report.peak_anon_bytes,
            "memory_basis": report.memory_basis,
            "mem_byte_seconds": report.mem_byte_seconds,
            "usage_measured": report.usage_measured,
        }),
    );
    object.insert("sinks".into(), Value::Array(sinks(report, context)));
    object.insert("plan".into(), context.plan.clone().unwrap_or(Value::Null));
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

/// One entry per task, document order: its key, role and kind; rows and bytes in and out; its
/// time; the rows it set aside. A figure the run does not measure for the task is `null`.
fn tasks(report: &RunReport, context: &ReportContext) -> Vec<Value> {
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
        let mut entry = json!({
            "key": task.id,
            "role": task.role.as_str(),
            "kind": task.kind,
            "stage": null,
            "fused_with": [],
            "rows_in": null,
            "rows_out": null,
            "bytes_in": null,
            "bytes_out": null,
            "time_s": null,
            "busy_s": null,
            "excluded_rows": 0,
            "skipped_morsels": 0,
            "errors": 0,
        });
        match task.role {
            Role::Source => {
                let consumers = job
                    .edges
                    .iter()
                    .filter(|e| e.from.task == task.id)
                    .filter_map(|e| job.tasks.iter().position(|t| t.id == e.to.task));
                if let Some((rows, bytes)) = consumers.filter_map(taken).next() {
                    entry["rows_out"] = json!(rows);
                    entry["bytes_out"] = json!(bytes);
                }
            }
            Role::Kernel => {
                if let Some((stage, members, found)) = stage_of(report, context, i) {
                    entry["stage"] = json!(stage);
                    entry["fused_with"] = json!(
                        members
                            .iter()
                            .filter(|&&m| m != i)
                            .map(|&m| job.tasks[m].id.clone())
                            .collect::<Vec<_>>()
                    );
                    if let Some(s) = found {
                        entry["rows_in"] = json!(s.rows_in);
                        entry["rows_out"] = json!(s.rows_out);
                        entry["bytes_in"] = json!(s.bytes_in);
                        entry["bytes_out"] = json!(s.bytes_out);
                        entry["time_s"] = json!(s.wall_s);
                        entry["busy_s"] = json!(s.kernel_busy_s);
                        entry["skipped_morsels"] = json!(s.skipped);
                        entry["errors"] = json!(s.errors);
                    }
                }
            }
            Role::Sink => {
                if let Some(s) = sink_outcome(i) {
                    entry["rows_in"] = json!(s.rows);
                    entry["bytes_in"] = json!(s.bytes);
                    entry["excluded_rows"] = json!(s.excluded_rows);
                }
            }
        }
        out.push(entry);
    }
    out
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
                Task::kernel("shout", entry("python")),
                Task::kernel("keep", entry("std")),
                Task::sink("out", entry("python")),
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
        r.cpu_ns = 5;
        let value = report_value(&r, None, &context());
        assert_eq!(value["moruna_report"], json!(REPORT_FORMAT));
        assert_eq!(value["job_digest"], "sha256:ab");
        assert_eq!(
            value["exit"],
            json!({"code": 0, "status": "completed", "diagnostic": null})
        );
        assert_eq!(value["resources"]["cpu_ns"], 5);
        assert_eq!(value["resources"]["peak_memory_bytes"], 4u64 << 30);
        let tasks = value["tasks"].as_array().expect("tasks");
        assert_eq!(tasks.len(), 5);
        assert_eq!(tasks[0]["key"], "rows");
        assert_eq!(tasks[0]["rows_out"], 1_000);
        assert_eq!(tasks[1]["fused_with"], json!(["keep"]));
        assert_eq!(tasks[2]["fused_with"], json!(["shout"]));
        assert_eq!(tasks[1]["stage"], 1);
        assert_eq!(tasks[1]["rows_in"], 1_000);
        assert_eq!(tasks[3]["excluded_rows"], 3);
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
