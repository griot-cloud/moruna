//! `moruna.run`: argument translation (f.3), the GIL refusal (f.4) and the thread roles that make
//! `KeyboardInterrupt` work (f.5).
//!
//! The Rust runtime runs on a helper thread that never touches the interpreter; the Python main
//! thread stays in `run` and does nothing but poll for signals. Signals are delivered to the main
//! thread by CPython's design, which is why the roles are not the other way round.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use moruna_adapters::python_gil_enabled;
use moruna_job::{
    AzureDoc, Edge, ErrorPolicyDoc, GcsDoc, IN, Job, KindDoc, OUT, ObjectStoreDoc, Role, S3Doc,
    SizerDoc, SpecError, Task, Urls,
};
use moruna_kernel::{CancelToken, DType, MorunaError, SourceSchema};
use moruna_runtime::job::BuildOptions;
use moruna_runtime::job::build::process_env;
use moruna_runtime::job::refusal;
use moruna_runtime::kinds::parquet::compression_name;
use moruna_runtime::kinds::tensor::tensor_format_name;
use moruna_runtime::kinds::{arrow_ipc, parquet, tensor, vortex};
use moruna_runtime::{RunSpec, Runtime};
use moruna_sources::{RowFilter, ScalarValue};
use pyo3::exceptions::PyKeyboardInterrupt;
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyDict};

use crate::errors::{Attachments, to_py_err};
use crate::handles::{IteratorSchema, SinkSpec, SourceSpec};
use crate::kernel::{Stage, stage_of};
use crate::sources::type_name;
use crate::translate::{
    OnErrorArg, RawArgs, ResumeArg, SizeArg, Translated, check_sink_not_source, translate,
};
use crate::{sinks, sources};

/// PY-I4's message, word for word.
pub const GIL_REFUSAL: &str = "Python functions require a free-threaded interpreter (python3.13t or \
                               python3.14t); pass allow_gil=True to run serialised";

/// g: `run` is not re-entrant, because the arena is a process-wide reservation. This is the
/// module-level flag that enforces it.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// How often the main thread wakes to check for a signal (f.5).
const SIGNAL_POLL: Duration = Duration::from_millis(100);

/// The flag's holder; dropping it lets the next run start.
pub(crate) struct RunningGuard;

impl RunningGuard {
    /// Take the flag, or refuse because a run is active (g).
    pub(crate) fn acquire() -> Result<RunningGuard, MorunaError> {
        if RUNNING.swap(true, Ordering::AcqRel) {
            return Err(MorunaError::Config {
                name: "run",
                msg: "a run is already active in this process; the arena is a process-wide \
                      reservation, so runs cannot overlap"
                    .into(),
            });
        }
        Ok(RunningGuard)
    }
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::Release);
    }
}

/// One task of a job as `moruna.Job` lowered it (d.2): its role, its id, the field a refusal
/// names it by, the object the run holds for it, the ids of the tasks that feed its inputs in
/// port order, its input ports when the job read them from a document, and its kind and
/// settings when the package already knows them (a `python` task, a task read from a spec).
struct Given<'py> {
    role: Role,
    id: String,
    at: String,
    object: Option<Bound<'py, PyAny>>,
    inputs: Vec<String>,
    ports: Option<Vec<String>>,
    doc: Option<KindDoc>,
}

/// A lowered task as Python hands it over: `(role, id, at, object, inputs, ports, kind,
/// settings)`, `role` being `source`, `function` or `sink` and `settings` JSON.
type Lowered<'py> = (
    String,
    String,
    String,
    Option<Bound<'py, PyAny>>,
    Vec<String>,
    Option<Vec<String>>,
    Option<String>,
    Option<String>,
);

/// The tasks `moruna._job` lowered a job to, in the order they were added.
fn given<'py>(tasks: &Bound<'py, PyAny>) -> PyResult<Vec<Given<'py>>> {
    let mut out = Vec::new();
    for entry in tasks.try_iter()? {
        let (role, id, at, object, inputs, ports, kind, settings): Lowered<'py> =
            entry?.extract()?;
        let role = match role.as_str() {
            "source" => Role::Source,
            "function" => Role::Kernel,
            "sink" => Role::Sink,
            other => {
                return Err(pyo3::exceptions::PyValueError::new_err(format!(
                    "a task is a source, a function or a sink, not `{other}`"
                )));
            }
        };
        let doc = match (kind, settings) {
            (Some(kind), Some(settings)) => {
                let settings = match serde_json::from_str::<serde_json::Value>(&settings) {
                    Ok(serde_json::Value::Object(map)) => map,
                    _ => {
                        return Err(pyo3::exceptions::PyValueError::new_err(format!(
                            "{at}: the settings of a `{kind}` task are a JSON object"
                        )));
                    }
                };
                Some(KindDoc::new(kind, settings))
            }
            (None, None) => None,
            _ => {
                return Err(pyo3::exceptions::PyValueError::new_err(format!(
                    "{at}: a task's kind and settings are given together"
                )));
            }
        };
        out.push(Given {
            role,
            id,
            at,
            object: object.filter(|o| !o.is_none()),
            inputs,
            ports,
            doc,
        });
    }
    Ok(out)
}

/// The tasks and edges of the job the given tasks are (SDD 22 d.4): a task takes its inputs on
/// the ports it was read with, else on `in` when it has one and on a port named after the task
/// that feeds it when it has several; every task gives on `out`; an edge of batches joins each
/// input to the output it names.
fn graph_of(tasks: &[Given<'_>], entries: Vec<KindDoc>) -> PyResult<(Vec<Task>, Vec<Edge>)> {
    let mut out = Vec::with_capacity(tasks.len());
    let mut edges = Vec::new();
    for (given, entry) in tasks.iter().zip(entries) {
        let ports: Vec<String> = match (&given.ports, given.inputs.as_slice()) {
            (Some(ports), _) => {
                if ports.len() != given.inputs.len() {
                    return Err(pyo3::exceptions::PyValueError::new_err(format!(
                        "{}: {} input ports for {} inputs",
                        given.at,
                        ports.len(),
                        given.inputs.len()
                    )));
                }
                ports.clone()
            }
            (None, [_]) => vec![IN.to_string()],
            (None, many) => many.to_vec(),
        };
        let mut task = match given.role {
            Role::Source => Task::source(given.id.clone(), entry),
            Role::Kernel => Task::kernel(given.id.clone(), entry),
            Role::Sink => Task::sink(given.id.clone(), entry),
        };
        // A source has no input port, whatever it was given: an edge into one is refused there.
        if given.role != Role::Source {
            task.inputs = ports.clone();
        }
        task.at = Some(given.at.clone());
        for (from, port) in given.inputs.iter().zip(&ports) {
            edges.push(Edge::new(from.clone(), OUT, given.id.clone(), port.clone()));
        }
        out.push(task);
    }
    Ok((out, edges))
}

/// A job's tasks, built: the job (tasks and edges, nothing else set) and, task by task in
/// document order, what the library's kinds hand back for it.
struct Assembled<'py> {
    job: Job,
    /// Each source's spec, when its object is one of the library's handles, and its object.
    sources: Vec<(Option<SourceSpec>, Option<Bound<'py, PyAny>>, bool)>,
    /// Each sink's spec, when its object is one of the library's handles, and its object.
    sinks: Vec<(Option<SinkSpec>, Option<Bound<'py, PyAny>>, bool)>,
    /// The clamps the sinks' constructors noted.
    sink_notes: Vec<String>,
    /// Each function's stage, when it has an object.
    stages: Vec<Option<Stage>>,
}

/// Assemble the job the lowered `tasks` describe: each task's kind and settings, from what
/// Python gave or from its object.
fn assemble<'py>(
    py: Python<'py>,
    name: Option<&str>,
    base: Option<&[u8]>,
    tasks: &[Given<'py>],
) -> PyResult<Assembled<'py>> {
    let mut entries = Vec::with_capacity(tasks.len());
    let mut sources = Vec::new();
    let mut sinks = Vec::new();
    let mut sink_notes = Vec::new();
    let mut stages = Vec::new();
    for task in tasks {
        let python = task.doc.as_ref().is_some_and(|d| d.kind == "python");
        match task.role {
            Role::Source => {
                let spec = match (&task.doc, &task.object) {
                    (None, Some(object)) => Some(sources::spec_of(object)?),
                    (None, None) => return Err(missing(&task.at)),
                    (Some(_), _) => None,
                };
                let doc = match (&task.doc, &spec) {
                    (Some(doc), _) => doc.clone(),
                    (None, Some(spec)) => map(py, source_doc(spec).map_err(refusal))?,
                    (None, None) => return Err(missing(&task.at)),
                };
                if matches!(spec, Some(SourceSpec::Python)) {
                    return Err(pyo3::exceptions::PyTypeError::new_err(format!(
                        "{}: a moruna.Source is lowered by moruna.Job, with its settings",
                        task.at
                    )));
                }
                // A user's source defines what a source must, or is refused naming it.
                if let (true, Some(object)) = (python, &task.object)
                    && !crate::extend::is_source(object)?
                {
                    return Err(pyo3::exceptions::PyTypeError::new_err(format!(
                        "{}: a python source is a moruna.Source, not {}",
                        task.at,
                        type_name(object)
                    )));
                }
                entries.push(doc);
                sources.push((spec, task.object.clone(), python));
            }
            Role::Sink => {
                let spec = match (&task.doc, &task.object) {
                    (None, Some(object)) => {
                        let (spec, notes) = sinks::spec_of(object)?;
                        sink_notes.extend(notes);
                        Some(spec)
                    }
                    (None, None) => return Err(missing(&task.at)),
                    (Some(_), _) => None,
                };
                let doc = match (&task.doc, &spec) {
                    (Some(doc), _) => doc.clone(),
                    (None, Some(spec)) => map(py, sink_doc(spec).map_err(refusal))?,
                    (None, None) => return Err(missing(&task.at)),
                };
                if matches!(spec, Some(SinkSpec::Python)) {
                    return Err(pyo3::exceptions::PyTypeError::new_err(format!(
                        "{}: a moruna.Sink is lowered by moruna.Job, with its settings",
                        task.at
                    )));
                }
                if let (true, Some(object)) = (python, &task.object)
                    && !crate::extend::is_sink(object)?
                {
                    return Err(pyo3::exceptions::PyTypeError::new_err(format!(
                        "{}: a python sink is a moruna.Sink, not {}",
                        task.at,
                        type_name(object)
                    )));
                }
                entries.push(doc);
                sinks.push((spec, task.object.clone(), python));
            }
            Role::Kernel => {
                let stage = match &task.object {
                    Some(object) => Some(stage_of(py, object)?),
                    None => None,
                };
                let doc = match (&task.doc, &task.object) {
                    (Some(doc), _) => doc.clone(),
                    (None, Some(object)) => kernel_doc(py, object, &task.at)?,
                    (None, None) => return Err(missing(&task.at)),
                };
                entries.push(doc);
                stages.push(stage);
            }
        }
    }
    let (job_tasks, edges) = graph_of(tasks, entries)?;
    // A job read from a document that sets run settings keeps them; any other job sets none.
    let mut job = match base {
        Some(bytes) => moruna_runtime::host::session::read_document(bytes)
            .map_err(|(_, refused)| map_err(py, &refusal(refused)))?,
        None => Job::new(Vec::new(), Vec::new()),
    };
    job.tasks = job_tasks;
    job.edges = edges;
    job.name = name.map(str::to_string);
    Ok(Assembled {
        job,
        sources,
        sinks,
        sink_notes,
        stages,
    })
}

fn missing(at: &str) -> PyErr {
    pyo3::exceptions::PyValueError::new_err(format!("{at}: a task needs an object or its settings"))
}

/// The declarations of the kinds the tasks' objects are served by (`@moruna.kind`).
fn served_of(
    py: Python<'_>,
    stages: &[Option<Stage>],
) -> Vec<(moruna_job::Declaration, Py<PyAny>)> {
    stages
        .iter()
        .filter_map(|stage| match stage {
            Some(Stage::Served(declaration, serve)) => {
                Some(((**declaration).clone(), serve.clone_ref(py)))
            }
            _ => None,
        })
        .collect()
}

/// The registry a job's plan and check are made against: the library's kinds and the ones its
/// tasks are served by.
fn registry_for(
    py: Python<'_>,
    stages: &[Option<Stage>],
) -> PyResult<moruna_runtime::kinds::Registry> {
    map(
        py,
        crate::kinds::library(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            served_of(py, stages),
        ),
    )
}

/// `_core.job_document(name, tasks)`: the job document version 2 (FlatBuffers) of the lowered
/// tasks: the job's name, the tasks and the edges, and nothing else set. Every task carries its kind and settings, or
/// an object of the library's whose settings are written from it.
#[pyfunction]
#[pyo3(signature = (name, tasks, base = None))]
pub fn job_document<'py>(
    py: Python<'py>,
    name: Option<&str>,
    tasks: &Bound<'py, PyAny>,
    base: Option<&[u8]>,
) -> PyResult<Bound<'py, pyo3::types::PyBytes>> {
    let tasks = given(tasks)?;
    let assembled = assemble(py, name, base, &tasks)?;
    let bytes = assembled.job.to_flatbuffer().map_err(|e| {
        map_err(
            py,
            &MorunaError::Config {
                name: "job",
                msg: e.to_string(),
            },
        )
    })?;
    Ok(pyo3::types::PyBytes::new(py, &bytes))
}

/// `_core.check_job(tasks)`: every refusal of the job the tasks are, as `<field>: <message>`:
/// its names, ports and edges (SDD 22 JB-I13), then each task's kind and settings against the
/// kinds `moruna.run` has. Empty when the job is sound.
#[pyfunction]
#[pyo3(signature = (name, tasks, base = None))]
pub fn check_job(
    py: Python<'_>,
    name: Option<&str>,
    tasks: &Bound<'_, PyAny>,
    base: Option<&[u8]>,
) -> PyResult<Vec<String>> {
    let tasks = given(tasks)?;
    let assembled = assemble(py, name, base, &tasks)?;
    if let Err(refusals) = moruna_job::check_graph(&assembled.job) {
        return Ok(refusals.iter().map(ToString::to_string).collect());
    }
    let kinds = registry_for(py, &assembled.stages)?;
    let catalogue = moruna_job::Catalogue::from_listing(kinds.listing()).map_err(|e| {
        map_err(
            py,
            &MorunaError::Config {
                name: "kinds",
                msg: e.to_string(),
            },
        )
    })?;
    Ok(match moruna_job::check(&assembled.job, &catalogue) {
        Ok(()) => Vec::new(),
        Err(refusals) => refusals.iter().map(ToString::to_string).collect(),
    })
}

/// `_core.explain_job(tasks, *, planner=None)`: the plan of the job the tasks are, as JSON, by
/// the planner `planner` names (the default when none), without running it: the same function
/// `moruna explain JOB` plans a document with. A refused job is a `ConfigError` naming its
/// fields; an unknown planner a `PlanError`.
#[pyfunction]
#[pyo3(signature = (name, tasks, base = None, *, planner = None))]
pub fn explain_job(
    py: Python<'_>,
    name: Option<&str>,
    tasks: &Bound<'_, PyAny>,
    base: Option<&[u8]>,
    planner: Option<String>,
) -> PyResult<String> {
    let tasks = given(tasks)?;
    let assembled = assemble(py, name, base, &tasks)?;
    let kinds = registry_for(py, &assembled.stages)?;
    let job = assembled.job;
    let plan = py
        .detach(|| moruna_runtime::job::plan::explain(&job, planner.as_deref(), &kinds))
        .map_err(|e| map_err(py, &e.into_error()))?;
    Ok(plan.to_value().to_string())
}

/// `_core.read_job(document)`: the name and tasks of a job document (version 2's FlatBuffers or
/// version 1's JSON), as JSON: `{"name", "tasks": [{"id", "role", "kind", "settings", "ports",
/// "inputs"}], "settings": [...]}`, `inputs` the ids of the tasks that feed `ports`, in port
/// order, and `settings` the run settings the document sets besides its tasks and edges (a
/// budget, staging, an object store, ...), by field. A task with an output port other than
/// `out`, a barrier or members is refused, naming it.
#[pyfunction]
pub fn read_job(py: Python<'_>, document: &[u8]) -> PyResult<String> {
    let job = moruna_runtime::host::session::read_document(document)
        .map_err(|(_, refused)| map_err(py, &refusal(refused)))?;
    let refuse = |msg: String| map_err(py, &MorunaError::Config { name: "job", msg });
    let mut bare = job.clone();
    bare.tasks.clear();
    bare.edges.clear();
    bare.name = None;
    let set = serde_json::to_value(&bare).map_err(|e| refuse(e.to_string()))?;
    let unset = serde_json::to_value(Job::new(Vec::new(), Vec::new()))
        .map_err(|e| refuse(e.to_string()))?;
    let mut settings: Vec<String> = Vec::new();
    if let (Some(set), Some(unset)) = (set.as_object(), unset.as_object()) {
        settings = set
            .iter()
            .filter(|(key, value)| unset.get(*key) != Some(*value))
            .map(|(key, _)| key.clone())
            .chain(unset.keys().filter(|key| !set.contains_key(*key)).cloned())
            .collect();
    }
    let mut tasks = Vec::with_capacity(job.tasks.len());
    for (i, task) in job.tasks.iter().enumerate() {
        let field = job.field(i);
        let outputs: &[&str] = if task.role == Role::Sink { &[] } else { &[OUT] };
        if task
            .outputs
            .iter()
            .map(String::as_str)
            .ne(outputs.iter().copied())
        {
            return Err(refuse(format!(
                "{field}.outputs: a task of a job gives on `out` alone (a sink on none), and \
                 `{}` gives on {:?}",
                task.id, task.outputs
            )));
        }
        if task.barrier || !task.members.is_empty() {
            return Err(refuse(format!(
                "{field}: a barrier or members are not part of a job spec"
            )));
        }
        let mut inputs = Vec::with_capacity(task.inputs.len());
        for port in &task.inputs {
            let feeding: Vec<&Edge> = job
                .edges
                .iter()
                .filter(|e| e.to.task == task.id && e.to.port == *port)
                .collect();
            match feeding.as_slice() {
                [edge] if edge.from.port == OUT => inputs.push(edge.from.task.clone()),
                [edge] => {
                    return Err(refuse(format!(
                        "{field}.inputs.{port}: fed by `{}`'s port `{}`; a job's tasks give \
                         on `out`",
                        edge.from.task, edge.from.port
                    )));
                }
                _ => {
                    return Err(refuse(format!(
                        "{field}.inputs.{port}: fed by {} edges, and a port takes one",
                        feeding.len()
                    )));
                }
            }
        }
        tasks.push(serde_json::json!({
            "id": task.id,
            "role": task.role.as_str(),
            "kind": task.kind,
            "settings": task.settings,
            "ports": task.inputs,
            "inputs": inputs,
        }));
    }
    Ok(serde_json::json!({"name": job.name, "tasks": tasks, "settings": settings}).to_string())
}

/// `moruna.run` (d.2): the job `moruna._job` lowered (`tasks`) with every other argument
/// keyword-only and optional, built into the one job model (SDD 22 d.4) and run. Returns the
/// report as JSON, which `moruna.RunReport` reads.
#[pyfunction]
#[pyo3(signature = (name, tasks, *, job_digest = None, explain = false, budget = None,
                    cpu = None, trace = None, staging_dir = None, staging_limit = None,
                    on_error = None, ordered = false, sizer = "rule", profiles_dir = None,
                    storage = None, host_profile = None, allow_gil = false, checkpoint = true,
                    checkpoint_interval = 5.0, keep_checkpoint = false, resume = None,
                    cost = None, engine_ports = None))]
#[allow(clippy::too_many_arguments)]
pub fn run(
    py: Python<'_>,
    name: &str,
    tasks: &Bound<'_, PyAny>,
    job_digest: Option<String>,
    explain: bool,
    budget: Option<&Bound<'_, PyAny>>,
    cpu: Option<f64>,
    trace: Option<String>,
    staging_dir: Option<String>,
    staging_limit: Option<&Bound<'_, PyAny>>,
    on_error: Option<&Bound<'_, PyAny>>,
    ordered: bool,
    sizer: &str,
    profiles_dir: Option<String>,
    storage: Option<&Bound<'_, PyDict>>,
    host_profile: Option<&Bound<'_, PyAny>>,
    allow_gil: bool,
    checkpoint: bool,
    checkpoint_interval: f64,
    keep_checkpoint: bool,
    resume: Option<String>,
    cost: Option<PyRef<'_, crate::engine::PyCostSession>>,
    engine_ports: Option<PyRef<'_, crate::engine::PyEnginePorts>>,
) -> PyResult<String> {
    let _guard = map(py, RunningGuard::acquire())?;
    let tasks = given(tasks)?;
    let assembled = assemble(py, Some(name), None, &tasks)?;

    let any_python = assembled
        .stages
        .iter()
        .any(|s| matches!(s, Some(Stage::Py(_))));
    // 12 f.8: the sandbox host the loaded packages were given; a run has one.
    let mut packages = moruna_runtime::packaged::PackageHost::default();
    for stage in assembled.stages.iter().flatten() {
        if let Stage::Package(host) = stage
            && *host != moruna_runtime::packaged::PackageHost::default()
        {
            if packages != moruna_runtime::packaged::PackageHost::default() && packages != *host {
                return Err(map_err(
                    py,
                    &MorunaError::Config {
                        name: "functions",
                        msg: "the loaded packages name two sandbox hosts (python=, cgroup=); a \
                              run has one"
                            .into(),
                    },
                ));
            }
            packages = host.clone();
        }
    }

    let raw = RawArgs {
        budget: match budget {
            Some(v) => Some(size_arg(v, "budget.host")?),
            None => None,
        },
        cpu,
        trace,
        staging_dir,
        staging_limit: match staging_limit {
            Some(v) => Some(size_arg(v, "budget.disk")?),
            None => None,
        },
        on_error: match on_error {
            Some(v) => on_error_arg(v)?,
            None => OnErrorArg::Name("terminate".into()),
        },
        ordered,
        sizer: sizer.to_string(),
        profiles_dir,
        allow_gil,
        checkpoint,
        checkpoint_interval,
        keep_checkpoint,
        resume,
    };
    let translated = map(py, translate(raw, assembled.sink_notes.clone()))?;
    for (sink_spec, _, _) in &assembled.sinks {
        for (source_spec, _, _) in &assembled.sources {
            if let (Some(sink_spec), Some(source_spec)) = (sink_spec, source_spec) {
                map(py, check_sink_not_source(sink_spec, source_spec))?;
            }
        }
    }

    // f.4, PY-I4: before any Rust component starts.
    if any_python && python_gil_enabled() && !translated.allow_gil {
        return Err(map_err(
            py,
            &MorunaError::Config {
                name: "python.allow_gil",
                msg: GIL_REFUSAL.to_string(),
            },
        ));
    }

    let (object_store, credentials) = match storage {
        Some(d) => storage_of(d)?,
        None => (ObjectStoreDoc::default(), Credentials::default()),
    };
    if host_profile.is_some() {
        return Err(map_err(
            py,
            &MorunaError::Config {
                name: "host_profile",
                msg: "host_profile= takes a mapping of the guarantees discovery would probe; the \
                      environment variable MORUNA_HOST_PROFILE is the supported spelling in v1"
                    .into(),
            },
        ));
    }

    // 12 f.3, MH 4.1: the arguments become the job a document would hold, and the job becomes
    // the `RunSpec` through the one function that builds every run.
    let trace_path = translated.trace_path.clone();
    let (job, notes) = job_of(translated, assembled.job, object_store);
    // SDD 22 JB-I13: a job's names, ports and edges are checked before anything is built, and
    // every refusal is named, not only the first.
    if let Err(refusals) = moruna_job::check_graph(&job) {
        return Err(map_err(
            py,
            &MorunaError::Config {
                name: "job",
                msg: refusals
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; "),
            },
        ));
    }

    // The objects the library's kinds hand back, each at its task's place among its role's.
    let mut iterables = Vec::with_capacity(assembled.sources.len());
    let mut user_sources = Vec::with_capacity(assembled.sources.len());
    for (spec, object, python) in &assembled.sources {
        iterables.push(match (spec, object) {
            (Some(SourceSpec::Iterator { schema }), Some(object)) => {
                match iterator_object(py, object)? {
                    Some(iterable) => {
                        Some((iterable, map(py, source_schema(clone_schema(schema)))?))
                    }
                    None => None,
                }
            }
            _ => None,
        });
        user_sources.push(match (python, object) {
            (true, Some(object)) => Some(object.clone().unbind()),
            _ => None,
        });
    }
    let user_sinks: Vec<Option<Py<PyAny>>> = assembled
        .sinks
        .iter()
        .map(|(_, object, python)| match (python, object) {
            (true, Some(object)) => Some(object.clone().unbind()),
            _ => None,
        })
        .collect();
    let served = served_of(py, &assembled.stages);
    let given_kernels: Vec<Option<Arc<moruna_adapters::PyKernel>>> = assembled
        .stages
        .iter()
        .map(|stage| match stage {
            Some(Stage::Py(k)) => Some(Arc::clone(k)),
            _ => None,
        })
        .collect();
    let kinds = map(
        py,
        crate::kinds::library(given_kernels, iterables, user_sources, user_sinks, served),
    )?;
    // A run starts from its plan (SDD 26 f.5): the default planner's, as `moruna run` does.
    let plan = map(py, moruna_runtime::job::plan::default_plan(&job))?;
    let mut built = map(
        py,
        moruna_runtime::job::build(
            &job,
            &plan,
            &kinds,
            BuildOptions {
                strict: false,
                env: &process_env,
                notes,
                packages,
                host_ports: None,
            },
        ),
    )?;
    credentials.apply(&mut built.spec.object_store);
    let context = ReportContext {
        job: job.clone(),
        parts: built.parts.clone(),
        sink_targets: built.sink_targets.clone(),
        job_digest,
        plan: explain.then(|| plan.to_value()),
        trace_path,
    };

    let mut components = match engine_ports {
        Some(ports) => ports
            .components(&job, &built.parts)
            .map_err(|e| map_err(py, &e))?,
        None => match py
            .detach(moruna_runtime::bootstrap::Guest::for_current_process)
            .map_err(|e| map_err(py, &e))?
        {
            Some(guest) => guest
                .components(&job, &built.parts)
                .map_err(|e| map_err(py, &e))?,
            None => moruna_runtime::Components::default(),
        },
    };
    if let Some(cost) = cost {
        let cost = &*cost;
        let learning = py
            .detach(|| cost.learning(&job, &built.parts))
            .map_err(|e| map_err(py, &e))?;
        if components
            .learning
            .as_ref()
            .is_some_and(|bound| !Arc::ptr_eq(&bound.online, &learning.online))
        {
            return Err(map_err(
                py,
                &MorunaError::Config {
                    name: "cost",
                    msg: "guest bootstrap and cost parameter name two model authorities".into(),
                },
            ));
        }
        components.learning = Some(learning);
    }
    drive(py, built.spec, components, &context)
}

/// The job `moruna.run`'s arguments describe (MH 4.1): `job`'s tasks and edges with the run's
/// settings. Every value was translated and clamped already, so building it clamps nothing
/// twice; the notes the translation made come back to go first in the report.
fn job_of(t: Translated, mut job: Job, object_store: ObjectStoreDoc) -> (Job, Vec<String>) {
    job.budget.memory_bytes = t.budget;
    job.budget.cpu = t.cpu;
    job.trace = t.trace_path.map(|p| p.to_string_lossy().into_owned());
    job.staging.dir = t.staging_dir.map(|p| p.to_string_lossy().into_owned());
    job.staging.limit_bytes = t.staging_limit;
    job.error_policy = match t.error_policy {
        moruna_kernel::ErrorPolicy::Terminate => ErrorPolicyDoc::Terminate,
        moruna_kernel::ErrorPolicy::Skip => ErrorPolicyDoc::Skip,
        moruna_kernel::ErrorPolicy::Budget(n) => ErrorPolicyDoc::Budget(n),
    };
    job.ordered = t.ordered;
    job.sizer = match t.sizer {
        moruna_kernel::SizerKind::Rule => SizerDoc::Rule,
        moruna_kernel::SizerKind::Learned => SizerDoc::Learned,
    };
    job.profiles_dir = t.profiles_dir.map(|p| p.to_string_lossy().into_owned());
    job.object_store = object_store;
    job.allow_gil = t.allow_gil;
    job.checkpoint.enabled = t.checkpoint;
    job.checkpoint.interval_ms = t.checkpoint_interval_ms;
    job.checkpoint.keep = t.checkpoint_keep;
    job.resume = t.resume.map(|r| match r {
        ResumeArg::Auto => "auto".to_string(),
        ResumeArg::RunId(hex) => hex,
        ResumeArg::Path(p) => p.to_string_lossy().into_owned(),
    });
    (job, t.notes)
}

fn source_doc(source: &SourceSpec) -> Result<KindDoc, SpecError> {
    match source {
        SourceSpec::Parquet(cfg) => KindDoc::of(
            "parquet",
            &parquet::SourceSettings {
                url: Urls(cfg.urls.clone()),
                options: parquet::SourceOptions {
                    columns: cfg.columns.clone(),
                    filters: cfg.filters.iter().map(filter_doc).collect(),
                },
            },
        ),
        SourceSpec::Vortex(cfg) => KindDoc::of(
            "vortex",
            &vortex::SourceSettings {
                url: Urls(cfg.urls.clone()),
                options: vortex::SourceOptions {
                    columns: cfg.columns.clone(),
                    split_bytes: cfg.split_bytes,
                },
            },
        ),
        SourceSpec::ArrowIpc(cfg) => KindDoc::of(
            "arrow_ipc",
            &arrow_ipc::SourceSettings {
                url: Urls(cfg.urls.clone()),
                options: arrow_ipc::SourceOptions {
                    columns: cfg.columns.clone(),
                    split_rows: cfg.split_rows,
                },
            },
        ),
        SourceSpec::Tensor(cfg) => KindDoc::of(
            "tensor",
            &tensor::SourceSettings {
                url: Urls(
                    cfg.paths
                        .iter()
                        .map(|p| p.to_string_lossy().into_owned())
                        .collect(),
                ),
                options: tensor::SourceOptions {
                    tensors: cfg.tensors.clone(),
                },
            },
        ),
        SourceSpec::Iterator { .. } => Ok(KindDoc::new("iterator", serde_json::Map::new())),
        SourceSpec::Python => Err(SpecError::new(
            "source",
            "a moruna.Source's settings are written by moruna.Job",
        )),
    }
}

fn filter_doc(filter: &RowFilter) -> parquet::Filter {
    let (column, op, value) = match filter {
        RowFilter::Gt(c, v) => (c, ">", v),
        RowFilter::Lt(c, v) => (c, "<", v),
        RowFilter::Eq(c, v) => (c, "==", v),
    };
    let value = match value {
        ScalarValue::I64(i) => serde_json::json!(i),
        ScalarValue::U64(u) => serde_json::json!(u),
        ScalarValue::F64(f) => serde_json::json!(f),
        ScalarValue::Str(s) => serde_json::json!(s),
        ScalarValue::Bool(b) => serde_json::json!(b),
    };
    parquet::Filter(column.clone(), op.to_string(), value)
}

fn sink_doc(sink: &SinkSpec) -> Result<KindDoc, SpecError> {
    match sink {
        SinkSpec::Parquet(cfg) => KindDoc::of(
            "parquet",
            &parquet::SinkSettings {
                url: cfg.url.clone(),
                options: parquet::SinkOptions {
                    row_group_bytes: Some(cfg.row_group_bytes),
                    file_bytes: Some(cfg.file_bytes),
                    compression: Some(compression_name(cfg.compression).to_string()),
                },
            },
        ),
        SinkSpec::Vortex(cfg) => KindDoc::of(
            "vortex",
            &vortex::SinkSettings {
                url: cfg.url.clone(),
                options: vortex::SinkOptions {
                    file_bytes: Some(cfg.file_bytes),
                },
            },
        ),
        SinkSpec::Tensor(cfg) => KindDoc::of(
            "tensor",
            &tensor::SinkSettings {
                url: cfg.path.to_string_lossy().into_owned(),
                options: tensor::SinkOptions {
                    format: Some(tensor_format_name(cfg.format).to_string()),
                    one_file_per_morsel: cfg.one_file_per_morsel,
                    name: Some(cfg.name.clone()),
                },
            },
        ),
        SinkSpec::ArrowIpc(cfg) => KindDoc::of(
            "arrow_ipc",
            &arrow_ipc::SinkSettings {
                url: cfg.path.to_string_lossy().into_owned(),
                options: arrow_ipc::SinkOptions {
                    file_bytes: Some(cfg.file_bytes),
                    compression: Some(cfg.compression.name().to_string()),
                    level: match cfg.compression {
                        moruna_kernel::ipc::Compression::Zstd(level) => Some(level),
                        _ => None,
                    },
                },
            },
        ),
        SinkSpec::Python => Err(SpecError::new(
            "sink",
            "a moruna.Sink's settings are written by moruna.Job",
        )),
    }
}

/// A function of the library's own, described as a document names it: a standard function by
/// name and arguments, a package by path and pin, a task of a declared kind by its kind and
/// settings. A Python function's settings are `moruna.Job`'s to write.
fn kernel_doc(py: Python<'_>, item: &Bound<'_, PyAny>, at: &str) -> PyResult<KindDoc> {
    let doc = if let Ok(std) = item.cast::<crate::check::PyStdKernel>() {
        let kernel = &std.get().kernel;
        KindDoc::of(
            "std",
            &moruna_runtime::kinds::standard::Settings {
                name: kernel.name().to_string(),
                args: Some(kernel.args().clone()),
                fingerprint: None,
            },
        )
    } else if let Ok(task) = item.cast::<crate::kinds::PyDeclaredTask>() {
        let task = task.get();
        Ok(KindDoc::new(
            task.declaration.name.clone(),
            task.settings.clone(),
        ))
    } else if let Ok(package) = item.cast::<crate::package::PyPackageKernel>() {
        let package = package.get();
        KindDoc::of(
            "package",
            &moruna_runtime::kinds::package::Settings {
                path: package.path.clone(),
                fingerprint: package.fingerprint.clone(),
                args: package.inputs.args.clone(),
                assets: package
                    .inputs
                    .assets
                    .as_ref()
                    .map(|a| a.display().to_string()),
                ports: None,
            },
        )
    } else {
        return Err(pyo3::exceptions::PyTypeError::new_err(format!(
            "{at}: a Python function's settings are written by moruna.Job, and {} came without \
             them",
            type_name(item)
        )));
    };
    doc.map_err(|e| map_err(py, &refusal(e)))
}

fn clone_schema(schema: &IteratorSchema) -> IteratorSchema {
    match schema {
        IteratorSchema::Table(s) => IteratorSchema::Table(Arc::clone(s)),
        IteratorSchema::Tensor { dtype, shape } => IteratorSchema::Tensor {
            dtype: dtype.clone(),
            shape: shape.clone(),
        },
    }
}

/// What a run's report says about its job beyond the trace's report: the job it ran, which task
/// each source, stage and sink is, where each sink writes, the spec's digest, the plan when it
/// was asked for, and the trace file.
pub(crate) struct ReportContext {
    pub(crate) job: Job,
    pub(crate) parts: moruna_runtime::job::Parts,
    pub(crate) sink_targets: Vec<String>,
    pub(crate) job_digest: Option<String>,
    pub(crate) plan: Option<serde_json::Value>,
    pub(crate) trace_path: Option<PathBuf>,
}

/// f.5: spawn the runtime thread, then poll for signals on the main thread until it finishes.
fn drive(
    py: Python<'_>,
    spec: RunSpec,
    components: moruna_runtime::Components,
    context: &ReportContext,
) -> PyResult<String> {
    let cancel = CancelToken::new();
    let thread_cancel = cancel.clone();
    let handle = std::thread::Builder::new()
        .name("moruna-runtime".to_string())
        .spawn(move || Runtime::run_with(spec, thread_cancel, components))
        .map_err(|e| {
            map_err(
                py,
                &MorunaError::Config {
                    name: "run",
                    msg: format!("the runtime thread could not be spawned: {e}"),
                },
            )
        })?;

    let mut interrupted = false;
    while !handle.is_finished() {
        match Python::check_signals(py) {
            Ok(()) => {}
            Err(e) if e.is_instance_of::<PyKeyboardInterrupt>(py) => {
                if interrupted {
                    // A second interrupt during cancellation is swallowed with a message (f.5):
                    // the cancellation sequence is already running and cannot be hurried.
                    eprintln!("moruna: cancelling; the run stops once in-flight functions return");
                } else {
                    interrupted = true;
                    cancel.cancel();
                    eprintln!("moruna: cancelling the run");
                }
            }
            // Any other exception a signal handler raised belongs to the caller: stop the run and
            // let it propagate once the runtime thread is home.
            Err(e) => {
                cancel.cancel();
                let _ = py.detach(|| handle.join());
                return Err(e);
            }
        }
        // The main thread holds no attachment while it sleeps (g), so a worker attaching for a
        // function call is never blocked by it.
        py.detach(|| std::thread::sleep(SIGNAL_POLL));
    }

    let joined = py.detach(|| handle.join());
    let outcome = match joined {
        Ok(outcome) => outcome,
        Err(_) => {
            return Err(map_err(
                py,
                &MorunaError::Config {
                    name: "run",
                    msg: "the runtime thread panicked; the run produced no report".into(),
                },
            ));
        }
    };

    match outcome {
        Ok(report) if !interrupted => Ok(crate::report::report_json(&report, None, context)),
        Ok(report) => {
            // The run finished before the cancellation reached it. PY-I6 still raises, with the
            // whole report attached rather than a partial one.
            let attached = attachments(
                py,
                Some(report),
                None,
                Some(&MorunaError::Cancelled),
                context,
            )?;
            Err(to_py_err(py, &MorunaError::Cancelled, attached))
        }
        Err(failure) => {
            let parts = failure.into_parts();
            let mut error = parts.error;
            if interrupted && !matches!(error, MorunaError::Cancelled) {
                error = MorunaError::Cancelled;
            }
            let attached = attachments(py, parts.report, parts.manifest, Some(&error), context)?;
            Err(to_py_err(py, &error, attached))
        }
    }
}

fn attachments(
    py: Python<'_>,
    report: Option<moruna_trace::RunReport>,
    manifest: Option<PathBuf>,
    error: Option<&MorunaError>,
    context: &ReportContext,
) -> PyResult<Attachments> {
    let run_id = report.as_ref().map(|r| r.run_id.clone());
    let report = match report {
        Some(r) => Some(crate::report::to_python(
            py,
            &crate::report::report_json(&r, error, context),
        )?),
        None => None,
    };
    Ok(Attachments {
        run_id,
        manifest: manifest.map(|p| p.to_string_lossy().into_owned()),
        report,
    })
}

fn source_schema(schema: IteratorSchema) -> Result<SourceSchema, MorunaError> {
    Ok(match schema {
        IteratorSchema::Table(s) => SourceSchema::Table(s),
        IteratorSchema::Tensor { dtype, shape } => SourceSchema::Tensor {
            dtype: dtype_of(&dtype)?,
            shape,
        },
    })
}

fn dtype_of(name: &str) -> Result<DType, MorunaError> {
    Ok(match name.to_ascii_lowercase().as_str() {
        "i8" | "int8" => DType::I8,
        "i16" | "int16" => DType::I16,
        "i32" | "int32" => DType::I32,
        "i64" | "int64" => DType::I64,
        "u8" | "uint8" => DType::U8,
        "u16" | "uint16" => DType::U16,
        "u32" | "uint32" => DType::U32,
        "u64" | "uint64" => DType::U64,
        "f16" | "float16" => DType::F16,
        "bf16" | "bfloat16" => DType::BF16,
        "f32" | "float32" => DType::F32,
        "f64" | "float64" => DType::F64,
        "bool" => DType::Bool,
        other => {
            return Err(MorunaError::Config {
                name: "schema",
                msg: format!("unknown tensor dtype `{other}`"),
            });
        }
    })
}

fn iterator_object(py: Python<'_>, source: &Bound<'_, PyAny>) -> PyResult<Option<Py<PyAny>>> {
    match source.cast::<crate::sources::PyIteratorSourceHandle>() {
        Ok(handle) => Ok(Some(handle.get().iterator.clone_ref(py))),
        Err(_) => Ok(None),
    }
}

fn size_arg(value: &Bound<'_, PyAny>, name: &'static str) -> PyResult<SizeArg> {
    if let Ok(n) = value.extract::<u64>() {
        return Ok(SizeArg::Bytes(n));
    }
    if let Ok(s) = value.extract::<String>() {
        return Ok(SizeArg::Text(s));
    }
    Err(pyo3::exceptions::PyTypeError::new_err(format!(
        "{name} must be an int or a size string such as \"6GiB\", not {}",
        type_name(value)
    )))
}

fn on_error_arg(value: &Bound<'_, PyAny>) -> PyResult<OnErrorArg> {
    if let Ok(name) = value.extract::<String>() {
        return Ok(OnErrorArg::Name(name));
    }
    if let Ok(t) = value.cast::<pyo3::types::PyTuple>()
        && t.len() == 2
        && t.get_item(0)?.extract::<String>()? == "budget"
    {
        return Ok(OnErrorArg::Budget(t.get_item(1)?.extract()?));
    }
    Err(pyo3::exceptions::PyTypeError::new_err(
        "on_error must be \"terminate\", \"skip\" or (\"budget\", n)",
    ))
}

/// The credentials `storage=` supplies at run time: never part of the job, applied to the run's
/// object store after the job is built.
#[derive(Default)]
struct Credentials {
    access_key_id: Option<String>,
    secret_access_key: Option<String>,
    session_token: Option<String>,
    service_account_path: Option<String>,
    service_account_json: Option<String>,
    access_key: Option<String>,
}

impl Credentials {
    /// Set each credential given on the store it is for, which the job's object store names or
    /// which it makes.
    fn apply(self, store: &mut moruna_reactor::ObjectStoreConfig) {
        if self.access_key_id.is_some()
            || self.secret_access_key.is_some()
            || self.session_token.is_some()
        {
            let s3 = store.s3.get_or_insert_with(Default::default);
            s3.access_key_id = self.access_key_id;
            s3.secret_access_key = self.secret_access_key;
            s3.session_token = self.session_token;
        }
        if self.service_account_path.is_some() || self.service_account_json.is_some() {
            let gcs = store.gcs.get_or_insert_with(Default::default);
            gcs.service_account_path = self.service_account_path.map(PathBuf::from);
            gcs.service_account_json = self.service_account_json;
        }
        if self.access_key.is_some() {
            store.azure.get_or_insert_with(Default::default).access_key = self.access_key;
        }
    }
}

/// `storage=`: the object store's endpoints and names, which go into the run's job, and its
/// credentials, which the runner applies to the run after the job is built so no document ever
/// holds one.
fn storage_of(d: &Bound<'_, PyDict>) -> PyResult<(ObjectStoreDoc, Credentials)> {
    let mut doc = ObjectStoreDoc::default();
    let mut credentials = Credentials::default();
    let mut s3 = S3Doc::default();
    let gcs = GcsDoc::default();
    let mut azure = AzureDoc::default();
    let mut any_s3 = false;
    let mut any_gcs = false;
    let mut any_azure = false;
    for (key, value) in d.iter() {
        let key: String = key.extract()?;
        match key.as_str() {
            "endpoint" => {
                s3.endpoint = Some(value.extract()?);
                any_s3 = true;
            }
            "region" => {
                s3.region = Some(value.extract()?);
                any_s3 = true;
            }
            "access_key_id" => {
                credentials.access_key_id = Some(value.extract()?);
                any_s3 = true;
            }
            "secret_access_key" => {
                credentials.secret_access_key = Some(value.extract()?);
                any_s3 = true;
            }
            "session_token" => {
                credentials.session_token = Some(value.extract()?);
                any_s3 = true;
            }
            "bucket" => {
                s3.bucket = Some(value.extract()?);
                any_s3 = true;
            }
            "allow_http" => doc.allow_http = value.extract()?,
            "local_root" => doc.local_root = Some(value.extract()?),
            "service_account_path" => {
                credentials.service_account_path = Some(value.extract()?);
                any_gcs = true;
            }
            "service_account_json" => {
                credentials.service_account_json = Some(value.extract()?);
                any_gcs = true;
            }
            "account" => {
                azure.account = Some(value.extract()?);
                any_azure = true;
            }
            "access_key" => {
                credentials.access_key = Some(value.extract()?);
                any_azure = true;
            }
            "container" => {
                azure.container = Some(value.extract()?);
                any_azure = true;
            }
            other => {
                return Err(pyo3::exceptions::PyValueError::new_err(format!(
                    "unknown storage option `{other}`"
                )));
            }
        }
    }
    if any_s3 {
        doc.s3 = Some(s3);
    }
    if any_gcs {
        doc.gcs = Some(gcs);
    }
    if any_azure {
        doc.azure = Some(azure);
    }
    Ok((doc, credentials))
}

fn map<T>(py: Python<'_>, r: Result<T, MorunaError>) -> PyResult<T> {
    r.map_err(|e| map_err(py, &e))
}

fn map_err(py: Python<'_>, e: &MorunaError) -> PyErr {
    to_py_err(py, e, Attachments::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PY-T9 reentrancy: a second concurrent `run` raises `ConfigError` (g). The guard is what
    /// `run` takes first, so the Python level assertion in `tests/test_reentrancy.py` and this
    /// one are the same fact.
    #[test]
    fn py_t9_reentrancy() {
        let first = RunningGuard::acquire().expect("the first run takes the flag");
        let second = RunningGuard::acquire();
        assert!(matches!(
            second,
            Err(MorunaError::Config { name: "run", .. })
        ));
        drop(first);
        assert!(RunningGuard::acquire().is_ok());
    }

    #[test]
    fn dtypes() {
        assert_eq!(dtype_of("float32").expect("f32"), DType::F32);
        assert_eq!(dtype_of("i64").expect("i64"), DType::I64);
        assert!(dtype_of("decimal").is_err());
    }
}
