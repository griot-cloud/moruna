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
    AzureDoc, ErrorPolicyDoc, GcsDoc, JobSpec, KindDoc, ObjectStoreDoc, S3Doc, SizerDoc, SpecError,
    Urls,
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
use crate::kernel::{PyKernelHandle, Stage, kernels_of};
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

/// `moruna.run` (d.2). Every argument beyond the first three is keyword-only and optional:
/// `moruna.run(source, kernels, sink)` is a complete call (PY-I3).
#[pyfunction]
#[pyo3(signature = (source, kernels, sink, *, budget = None, cpu = None, trace = None,
                    staging_dir = None, staging_limit = None, on_error = None, ordered = false,
                    sizer = "rule", profiles_dir = None, storage = None, host_profile = None,
                    allow_gil = false, checkpoint = true, checkpoint_interval = 5.0,
                    keep_checkpoint = false, resume = None))]
#[allow(clippy::too_many_arguments)]
pub fn run(
    py: Python<'_>,
    source: &Bound<'_, PyAny>,
    kernels: &Bound<'_, PyAny>,
    sink: &Bound<'_, PyAny>,
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
) -> PyResult<PyRunReport> {
    let _guard = map(py, RunningGuard::acquire())?;

    let source_spec = sources::spec_of(source)?;
    let (sink_spec, sink_notes) = sinks::spec_of(sink)?;
    let stages = kernels_of(py, kernels)?;
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
    map(py, check_sink_not_source(&sink_spec, &source_spec))?;

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

    // 12 f.3, MH 4.1: the arguments become the job document a file would hold, and the
    // document becomes the `RunSpec` through the one function that builds every run.
    let iterator = iterator_object(py, source)?;
    let trace_path = translated.trace_path.clone();
    let described = kernel_docs(py, kernels)?;
    let (job, notes) = map(
        py,
        job_of(
            translated,
            &source_spec,
            &sink_spec,
            described,
            object_store,
        )
        .map_err(refusal),
    )?;
    let iterable = match (&source_spec, iterator) {
        (SourceSpec::Iterator { schema }, Some(object)) => {
            Some((object, map(py, source_schema(clone_schema(schema)))?))
        }
        _ => None,
    };
    // A user's `moruna.Source` and `moruna.Sink` (07 e.6, 08 f.10): the objects themselves.
    let user_source = match source_spec {
        SourceSpec::Python => Some(source.clone().unbind()),
        _ => None,
    };
    let user_sink = match sink_spec {
        SinkSpec::Python => Some(sink.clone().unbind()),
        _ => None,
    };
    let kinds = map(
        py,
        crate::kinds::library(
            stages
                .into_iter()
                .map(|s| match s {
                    Stage::Py(k) => Some(k),
                    Stage::Std(_) | Stage::Package(_) => None,
                })
                .collect(),
            iterable,
            user_source,
            user_sink,
        ),
    )?;
    let built = map(
        py,
        moruna_runtime::job::build(
            &job,
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

    drive(py, built.spec, trace_path)
}

/// The document `moruna.run`'s arguments describe (MH 4.1). Every value was translated and
/// clamped already, so building it clamps nothing twice; the notes the translation made come
/// back to go first in the report.
fn job_of(
    t: Translated,
    source: &SourceSpec,
    sink: &SinkSpec,
    kernels: Vec<KindDoc>,
    object_store: ObjectStoreDoc,
) -> Result<(JobSpec, Vec<String>), SpecError> {
    let mut job = JobSpec::new(source_doc(source)?, kernels, sink_doc(sink)?);
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
    Ok((job, t.notes))
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
                },
            },
        ),
        SinkSpec::Python => Ok(KindDoc::new("python", serde_json::Map::new())),
    }
}

/// Each kernel `moruna.run` was given, described as a document names it: a standard kernel by
/// name and arguments, a package by path and pin, a Python kernel by its module and qualified
/// name. The Python objects themselves are what [`crate::kinds::Given`] hands back.
fn kernel_docs(py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<Vec<KindDoc>> {
    let items: Vec<Bound<'_, PyAny>> = if value.is_instance_of::<pyo3::types::PyList>()
        || value.is_instance_of::<pyo3::types::PyTuple>()
    {
        value.try_iter()?.collect::<PyResult<Vec<_>>>()?
    } else {
        vec![value.clone()]
    };
    let text = |obj: &Bound<'_, PyAny>, attr: &str| {
        obj.getattr(attr)
            .and_then(|v| v.extract::<String>())
            .unwrap_or_else(|_| format!("<{}>", type_name(obj)))
    };
    let mut docs = Vec::with_capacity(items.len());
    for item in &items {
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
        docs.push(doc.map_err(|e| map_err(py, &refusal(e)))?);
    }
    Ok(docs)
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
fn drive(py: Python<'_>, spec: RunSpec, trace_path: Option<PathBuf>) -> PyResult<PyRunReport> {
    let cancel = CancelToken::new();
    let thread_cancel = cancel.clone();
    let handle = std::thread::Builder::new()
        .name("moruna-runtime".to_string())
        .spawn(move || Runtime::run(spec, thread_cancel))
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
