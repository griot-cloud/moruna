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
    SizerDoc, SpecError, Task, Urls, V1_SINK, V1_SOURCE,
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
use crate::kernel::{PyKernelHandle, Stage, stage_of};
use crate::report::PyRunReport;
use crate::sources::type_name;
use crate::translate::{
    OnErrorArg, RawArgs, ResumeArg, SizeArg, Translated, check_sink_not_source, translate,
};
use crate::{sinks, sources};

/// PY-I4's message, word for word.
pub const GIL_REFUSAL: &str = "Python kernels require a free-threaded interpreter (python3.13t or \
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

/// One task of a run as `moruna.run` was given it (d.2): its role, its id, the field a refusal
/// names it by, the object, and the ids of the tasks that feed its inputs, port order.
struct Given<'py> {
    role: Role,
    id: String,
    at: String,
    object: Bound<'py, PyAny>,
    inputs: Vec<String>,
}

/// `moruna.run(source, kernels, sink)` as the DAG with one path it is (SDD 22 JB-I10): the tasks
/// `source`, `kernel-1` .. `kernel-n` and `sink`, each fed by the one before, named in a refusal
/// by the field a version 1 document writes them under.
fn chain<'py>(
    source: &Bound<'py, PyAny>,
    kernels: &Bound<'py, PyAny>,
    sink: &Bound<'py, PyAny>,
) -> PyResult<Vec<Given<'py>>> {
    let items: Vec<Bound<'py, PyAny>> = if kernels.is_instance_of::<pyo3::types::PyList>()
        || kernels.is_instance_of::<pyo3::types::PyTuple>()
    {
        kernels.try_iter()?.collect::<PyResult<Vec<_>>>()?
    } else {
        vec![kernels.clone()]
    };
    let mut tasks = Vec::with_capacity(items.len() + 2);
    tasks.push(Given {
        role: Role::Source,
        id: V1_SOURCE.into(),
        at: "source".into(),
        object: source.clone(),
        inputs: Vec::new(),
    });
    for (i, item) in items.into_iter().enumerate() {
        let before = tasks[i].id.clone();
        tasks.push(Given {
            role: Role::Kernel,
            id: format!("kernel-{}", i + 1),
            at: format!("kernels[{i}]"),
            object: item,
            inputs: vec![before],
        });
    }
    let before = tasks[tasks.len() - 1].id.clone();
    tasks.push(Given {
        role: Role::Sink,
        id: V1_SINK.into(),
        at: "sink".into(),
        object: sink.clone(),
        inputs: vec![before],
    });
    Ok(tasks)
}

/// `moruna.run(graph=...)`: the tasks of a `moruna.Graph` (`Graph.tasks`, each a tuple of its
/// role, its name, its object and the names of the tasks that feed it), in the order they were
/// added, named in a refusal as `graph["<name>"]`.
fn graph<'py>(graph: &Bound<'py, PyAny>) -> PyResult<Vec<Given<'py>>> {
    let listed = graph.getattr("tasks").map_err(|_| {
        pyo3::exceptions::PyTypeError::new_err(format!(
            "graph= takes a moruna.Graph, not {}",
            type_name(graph)
        ))
    })?;
    let mut tasks = Vec::new();
    for entry in listed.try_iter()? {
        let (role, id, object, inputs): (String, String, Bound<'py, PyAny>, Vec<String>) =
            entry?.extract()?;
        let role = match role.as_str() {
            "source" => Role::Source,
            "kernel" => Role::Kernel,
            "sink" => Role::Sink,
            other => {
                return Err(pyo3::exceptions::PyValueError::new_err(format!(
                    "a graph task is a source, a kernel or a sink, not `{other}`"
                )));
            }
        };
        tasks.push(Given {
            role,
            at: format!("graph[{id:?}]"),
            id,
            object,
            inputs,
        });
    }
    Ok(tasks)
}

/// The tasks and edges of the job the given tasks are (SDD 22 d.4): a task with one input takes
/// it on `in`, one with several takes each on a port named after the task that feeds it, and
/// every task gives on `out`; an edge of batches joins each input to the output it names.
fn graph_of(tasks: &[Given<'_>], entries: Vec<KindDoc>) -> (Vec<Task>, Vec<Edge>) {
    let mut out = Vec::with_capacity(tasks.len());
    let mut edges = Vec::new();
    for (given, entry) in tasks.iter().zip(entries) {
        let ports: Vec<String> = match given.inputs.as_slice() {
            [_] => vec![IN.to_string()],
            many => many.to_vec(),
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
    (out, edges)
}

/// `moruna.run` (d.2). A run is a source, kernels and a sink, the chain `moruna.run(source,
/// kernels, sink)` is a complete call of (PY-I3), or a `moruna.Graph` (`graph=`), a DAG of
/// tasks; both are built into the one job model (SDD 22 d.4). Every other argument is
/// keyword-only and optional.
#[pyfunction]
#[pyo3(signature = (source = None, kernels = None, sink = None, *, graph = None, budget = None,
                    cpu = None, trace = None, staging_dir = None, staging_limit = None,
                    on_error = None, ordered = false, sizer = "rule", profiles_dir = None,
                    storage = None, host_profile = None, allow_gil = false, checkpoint = true,
                    checkpoint_interval = 5.0, keep_checkpoint = false, resume = None,
                    cost = None, engine_ports = None))]
#[allow(clippy::too_many_arguments)]
pub fn run(
    py: Python<'_>,
    source: Option<&Bound<'_, PyAny>>,
    kernels: Option<&Bound<'_, PyAny>>,
    sink: Option<&Bound<'_, PyAny>>,
    graph: Option<&Bound<'_, PyAny>>,
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
) -> PyResult<PyRunReport> {
    let _guard = map(py, RunningGuard::acquire())?;

    let tasks = match (source, kernels, sink, graph) {
        (Some(source), Some(kernels), Some(sink), None) => chain(source, kernels, sink)?,
        (None, None, None, Some(g)) => self::graph(g)?,
        (_, _, _, Some(_)) => {
            return Err(pyo3::exceptions::PyTypeError::new_err(
                "moruna.run takes a graph= or a source, kernels and a sink, not both",
            ));
        }
        _ => {
            return Err(pyo3::exceptions::PyTypeError::new_err(
                "moruna.run takes a source, kernels and a sink, or a moruna.Graph as graph=",
            ));
        }
    };

    // Each task as its kind sees it, in document order: the sources' and sinks' specs, the
    // kernels' stages.
    let mut source_specs: Vec<(SourceSpec, &Bound<'_, PyAny>)> = Vec::new();
    let mut sink_specs: Vec<SinkSpec> = Vec::new();
    let mut sink_notes = Vec::new();
    let mut stages: Vec<Stage> = Vec::new();
    for task in &tasks {
        match task.role {
            Role::Source => source_specs.push((sources::spec_of(&task.object)?, &task.object)),
            Role::Sink => {
                let (spec, notes) = sinks::spec_of(&task.object)?;
                sink_specs.push(spec);
                sink_notes.extend(notes);
            }
            Role::Kernel => stages.push(stage_of(py, &task.object)?),
        }
    }
    let any_python = stages.iter().any(|s| matches!(s, Stage::Py(_)));
    // 12 f.8: the sandbox host the loaded packages were given; a run has one.
    let mut packages = moruna_runtime::packaged::PackageHost::default();
    for stage in &stages {
        if let Stage::Package(host) = stage
            && *host != moruna_runtime::packaged::PackageHost::default()
        {
            if packages != moruna_runtime::packaged::PackageHost::default() && packages != *host {
                return Err(map_err(
                    py,
                    &MorunaError::Config {
                        name: "kernels",
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
    let translated = map(py, translate(raw, sink_notes))?;
    for sink_spec in &sink_specs {
        for (source_spec, _) in &source_specs {
            map(py, check_sink_not_source(sink_spec, source_spec))?;
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

    let object_store = match storage {
        Some(d) => object_store_doc(d)?,
        None => ObjectStoreDoc::default(),
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
    let mut entries = Vec::with_capacity(tasks.len());
    let (mut sources_at, mut sinks_at) = (source_specs.iter(), sink_specs.iter());
    for task in &tasks {
        let entry = match task.role {
            Role::Source => source_doc(&sources_at.next().expect("one spec per source").0),
            Role::Sink => sink_doc(sinks_at.next().expect("one spec per sink")),
            Role::Kernel => Ok(kernel_doc(py, &task.object)?),
        };
        entries.push(map(py, entry.map_err(refusal))?);
    }
    let (job_tasks, edges) = graph_of(&tasks, entries);
    let (job, notes) = job_of(translated, Job::new(job_tasks, edges), object_store);
    // SDD 22 JB-I13: a graph's names, ports and edges are checked before anything is built,
    // and every refusal is named, not only the first.
    if graph.is_some()
        && let Err(refusals) = moruna_job::check_graph(&job)
    {
        return Err(map_err(
            py,
            &MorunaError::Config {
                name: "graph",
                msg: refusals
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; "),
            },
        ));
    }

    // The objects the library's kinds hand back, each at its task's place among its role's.
    let mut iterables = Vec::with_capacity(source_specs.len());
    let mut user_sources = Vec::with_capacity(source_specs.len());
    for (spec, object) in &source_specs {
        iterables.push(match spec {
            SourceSpec::Iterator { schema } => match iterator_object(py, object)? {
                Some(iterable) => Some((iterable, map(py, source_schema(clone_schema(schema)))?)),
                None => None,
            },
            _ => None,
        });
        user_sources.push(match spec {
            SourceSpec::Python => Some(Bound::clone(object).unbind()),
            _ => None,
        });
    }
    let user_sinks: Vec<Option<Py<PyAny>>> = tasks
        .iter()
        .filter(|task| task.role == Role::Sink)
        .zip(&sink_specs)
        .map(|(task, spec)| match spec {
            SinkSpec::Python => Some(task.object.clone().unbind()),
            _ => None,
        })
        .collect();
    let mut served = Vec::new();
    let mut given = Vec::with_capacity(stages.len());
    for stage in stages {
        match stage {
            Stage::Py(k) => given.push(Some(k)),
            Stage::Served(declaration, serve) => {
                served.push((*declaration, serve));
                given.push(None);
            }
            Stage::Std(_) | Stage::Package(_) => given.push(None),
        }
    }
    let kinds = map(
        py,
        crate::kinds::library(given, iterables, user_sources, user_sinks, served),
    )?;
    // A run starts from its plan (SDD 26 f.5): the default planner's, as `moruna run` does.
    let plan = map(py, moruna_runtime::job::plan::default_plan(&job))?;
    let built = map(
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
    drive(py, built.spec, trace_path, components)
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
        SourceSpec::Python => Ok(KindDoc::new("python", serde_json::Map::new())),
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
        SinkSpec::Python => Ok(KindDoc::new("python", serde_json::Map::new())),
    }
}

/// A kernel `moruna.run` was given, described as a document names it: a standard kernel by name
/// and arguments, a package by path and pin, a Python kernel by its module and qualified name.
/// The Python objects themselves are what [`crate::kinds::Given`] hands back.
fn kernel_doc(py: Python<'_>, item: &Bound<'_, PyAny>) -> PyResult<KindDoc> {
    let text = |obj: &Bound<'_, PyAny>, attr: &str| {
        obj.getattr(attr)
            .and_then(|v| v.extract::<String>())
            .unwrap_or_else(|_| format!("<{}>", type_name(obj)))
    };
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
        let target = match item.cast::<PyKernelHandle>() {
            Ok(handle) => handle.get().callable.bind(py).clone(),
            Err(_) => item.clone(),
        };
        KindDoc::of(
            "python",
            &crate::kinds::Settings::named(
                text(&target, "__module__"),
                text(&target, "__qualname__"),
            ),
        )
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

/// f.5: spawn the runtime thread, then poll for signals on the main thread until it finishes.
fn drive(
    py: Python<'_>,
    spec: RunSpec,
    trace_path: Option<PathBuf>,
    components: moruna_runtime::Components,
) -> PyResult<PyRunReport> {
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
                    eprintln!("moruna: cancelling; the run stops once in-flight kernels return");
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
        // kernel call is never blocked by it.
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
        Ok(report) if !interrupted => Ok(PyRunReport::new(report, trace_path)),
        Ok(report) => {
            // The run finished before the cancellation reached it. PY-I6 still raises, with the
            // whole report attached rather than a partial one.
            let attached = attachments(py, Some(report), None, trace_path)?;
            Err(to_py_err(py, &MorunaError::Cancelled, attached))
        }
        Err(failure) => {
            let parts = failure.into_parts();
            let mut error = parts.error;
            if interrupted && !matches!(error, MorunaError::Cancelled) {
                error = MorunaError::Cancelled;
            }
            let attached = attachments(py, parts.report, parts.manifest, trace_path)?;
            Err(to_py_err(py, &error, attached))
        }
    }
}

fn attachments(
    py: Python<'_>,
    report: Option<moruna_trace::RunReport>,
    manifest: Option<PathBuf>,
    trace_path: Option<PathBuf>,
) -> PyResult<Attachments> {
    let run_id = report.as_ref().map(|r| r.run_id.clone());
    let report = match report {
        Some(r) => Some(Py::new(py, PyRunReport::new(r, trace_path))?.into_any()),
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

fn object_store_doc(d: &Bound<'_, PyDict>) -> PyResult<ObjectStoreDoc> {
    let mut doc = ObjectStoreDoc::default();
    let mut s3 = S3Doc::default();
    let mut gcs = GcsDoc::default();
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
            "access_key" | "access_key_id" => {
                s3.access_key_id = Some(value.extract()?);
                any_s3 = true;
            }
            "secret_key" | "secret_access_key" => {
                s3.secret_access_key = Some(value.extract()?);
                any_s3 = true;
            }
            "session_token" => {
                s3.session_token = Some(value.extract()?);
                any_s3 = true;
            }
            "bucket" => {
                s3.bucket = Some(value.extract()?);
                any_s3 = true;
            }
            "allow_http" => doc.allow_http = value.extract()?,
            "local_root" => doc.local_root = Some(value.extract()?),
            "service_account_path" => {
                gcs.service_account_path = Some(value.extract()?);
                any_gcs = true;
            }
            "service_account_json" => {
                gcs.service_account_json = Some(value.extract()?);
                any_gcs = true;
            }
            "account" => {
                azure.account = Some(value.extract()?);
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
    Ok(doc)
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
