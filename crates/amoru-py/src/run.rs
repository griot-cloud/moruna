//! `amoru.run`: argument translation (f.3), the GIL refusal (f.4) and the thread roles that make
//! `KeyboardInterrupt` work (f.5).
//!
//! The Rust runtime runs on a helper thread that never touches the interpreter; the Python main
//! thread stays in `run` and does nothing but poll for signals. Signals are delivered to the main
//! thread by CPython's design, which is why the roles are not the other way round.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use amoru_adapters::{PyKernel, python_gil_enabled};
use amoru_kernel::{AmoruError, CancelToken, DType, Kernel, SourceSchema};
use amoru_placement::PlacementEngine;
use amoru_reactor::{AzureConfig, GcsConfig, ObjectStoreConfig, S3Config};
use amoru_runtime::{RunSpec, Runtime};
use amoru_sinks::{ArrowIpcSink, ParquetSink, TensorSink};
use amoru_sources::{ParquetSource, TensorSource};
use pyo3::exceptions::PyKeyboardInterrupt;
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyDict};

use crate::errors::{Attachments, to_py_err};
use crate::handles::{IteratorSchema, SinkSpec, SourceSpec};
use crate::kernel::kernels_of;
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

struct RunningGuard;

impl RunningGuard {
    fn acquire() -> Result<RunningGuard, AmoruError> {
        if RUNNING.swap(true, Ordering::AcqRel) {
            return Err(AmoruError::Config {
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

/// `amoru.run` (d.2). Every argument beyond the first three is keyword-only and optional:
/// `amoru.run(source, kernels, sink)` is a complete call (PY-I3).
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
    let py_kernels = kernels_of(py, kernels)?;

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
    if !py_kernels.is_empty() && python_gil_enabled() && !translated.allow_gil {
        return Err(map_err(
            py,
            &AmoruError::Config {
                name: "python.allow_gil",
                msg: GIL_REFUSAL.to_string(),
            },
        ));
    }

    let object_store = match storage {
        Some(d) => object_store_config(d)?,
        None => ObjectStoreConfig::default(),
    };
    if host_profile.is_some() {
        return Err(map_err(
            py,
            &AmoruError::Config {
                name: "host_profile",
                msg: "host_profile= takes a mapping of the guarantees discovery would probe; the \
                      environment variable AMORU_HOST_PROFILE is the supported spelling in v1"
                    .into(),
            },
        ));
    }

    let iterator = iterator_object(py, source)?;
    let trace_path = translated.trace_path.clone();
    let resume_path = map(py, resolve_resume(&translated))?;
    let spec = map(
        py,
        build_spec(
            translated,
            source_spec,
            sink_spec,
            py_kernels,
            iterator,
            object_store,
            resume_path,
        ),
    )?;

    drive(py, spec, trace_path)
}

/// f.5: spawn the runtime thread, then poll for signals on the main thread until it finishes.
fn drive(py: Python<'_>, spec: RunSpec, trace_path: Option<PathBuf>) -> PyResult<PyRunReport> {
    let cancel = CancelToken::new();
    let thread_cancel = cancel.clone();
    let handle = std::thread::Builder::new()
        .name("amoru-runtime".to_string())
        .spawn(move || Runtime::run(spec, thread_cancel))
        .map_err(|e| {
            map_err(
                py,
                &AmoruError::Config {
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
                    eprintln!("amoru: cancelling; the run stops once in-flight kernels return");
                } else {
                    interrupted = true;
                    cancel.cancel();
                    eprintln!("amoru: cancelling the run");
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
                &AmoruError::Config {
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
            Err(to_py_err(py, &AmoruError::Cancelled, attached))
        }
        Err(failure) => {
            let parts = failure.into_parts();
            let mut error = parts.error;
            if interrupted && !matches!(error, AmoruError::Cancelled) {
                error = AmoruError::Cancelled;
            }
            let attached = attachments(py, parts.report, parts.manifest, trace_path)?;
            Err(to_py_err(py, &error, attached))
        }
    }
}

fn attachments(
    py: Python<'_>,
    report: Option<amoru_trace::RunReport>,
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

/// f.7: resolve `resume=` to a manifest path before anything is built. `"auto"` and a run id are
/// looked up under the staging directory; a path is taken as given.
fn resolve_resume(t: &Translated) -> Result<Option<PathBuf>, AmoruError> {
    let Some(arg) = &t.resume else {
        return Ok(None);
    };
    match arg {
        ResumeArg::Path(p) => Ok(Some(p.clone())),
        ResumeArg::Auto | ResumeArg::RunId(_) => {
            let dir = t.staging_dir.clone().ok_or_else(|| {
                AmoruError::Resume(
                    "resume= by run id or \"auto\" needs staging_dir= so the manifest can be \
                     found; pass the manifest path instead"
                        .into(),
                )
            })?;
            let id = match arg {
                ResumeArg::RunId(hex) => Some(run_id(hex)?),
                _ => None,
            };
            match PlacementEngine::find_manifest(&dir, id)? {
                Some(path) => Ok(Some(path)),
                None => Err(AmoruError::Resume(format!(
                    "no manifest to resume from under `{}`",
                    dir.display()
                ))),
            }
        }
    }
}

fn run_id(hex: &str) -> Result<amoru_kernel::RunId, AmoruError> {
    let mut bytes = [0u8; 16];
    for (i, b) in bytes.iter_mut().enumerate() {
        let pair = hex.get(i * 2..i * 2 + 2).ok_or_else(|| {
            AmoruError::Resume(format!(
                "`{hex}` is not a run id: 32 hex characters are needed"
            ))
        })?;
        *b = u8::from_str_radix(pair, 16).map_err(|_| {
            AmoruError::Resume(format!(
                "`{hex}` is not a run id: `{pair}` is not hexadecimal"
            ))
        })?;
    }
    Ok(amoru_kernel::RunId(bytes))
}

/// Build the facade's `RunSpec` from the translated arguments (d.1). The source and the sink are
/// factories: every constructor in components 7 and 8 takes the reactor and the allocator, which
/// exist only inside `Runtime::run` (preamble 4.4).
#[allow(clippy::too_many_arguments)]
fn build_spec(
    t: Translated,
    source: SourceSpec,
    sink: SinkSpec,
    py_kernels: Vec<Arc<PyKernel>>,
    iterator: Option<Py<PyAny>>,
    object_store: ObjectStoreConfig,
    resume: Option<PathBuf>,
) -> Result<RunSpec, AmoruError> {
    let kernels: Vec<Arc<dyn Kernel>> = py_kernels
        .iter()
        .map(|k| Arc::clone(k) as Arc<dyn Kernel>)
        .collect();
    let staged: Vec<(amoru_kernel::StageId, Arc<PyKernel>)> = py_kernels
        .into_iter()
        .enumerate()
        .map(|(i, k)| ((i + 1) as amoru_kernel::StageId, k))
        .collect();

    let mut spec = RunSpec::new(
        source_factory(source, iterator)?,
        kernels,
        sink_factory(sink),
    );
    spec.py_kernels = staged;
    spec.budget = t.budget;
    spec.cpu = t.cpu;
    spec.trace_path = t.trace_path;
    spec.staging_dir = t.staging_dir;
    spec.staging_limit = t.staging_limit;
    spec.error_policy = t.error_policy;
    spec.ordered = t.ordered;
    spec.sizer = t.sizer;
    if let Some(dir) = t.profiles_dir {
        spec.profiles_dir = Some(dir);
    }
    spec.object_store = object_store;
    spec.host_profile = None;
    spec.allow_gil = t.allow_gil;
    spec.checkpoint = t.checkpoint;
    spec.checkpoint_interval_ms = t.checkpoint_interval_ms;
    spec.checkpoint_keep = t.checkpoint_keep;
    spec.resume = resume;
    spec.notes = t.notes;
    Ok(spec)
}

fn source_factory(
    source: SourceSpec,
    iterator: Option<Py<PyAny>>,
) -> Result<amoru_runtime::SourceSpec, AmoruError> {
    Ok(match source {
        SourceSpec::Parquet(cfg) => amoru_runtime::SourceSpec::Build(Box::new(move |ctx| {
            let meta = ctx.object_metadata()?;
            Ok(Arc::new(ParquetSource::new(
                cfg,
                ctx.reactor.clone(),
                meta,
            )?))
        })),
        SourceSpec::Tensor(cfg) => amoru_runtime::SourceSpec::Build(Box::new(move |ctx| {
            Ok(Arc::new(TensorSource::new(cfg, ctx.reactor.clone())?))
        })),
        SourceSpec::Iterator { schema } => {
            let schema = source_schema(schema)?;
            let iterator = iterator.ok_or_else(|| {
                AmoruError::Plan("an IteratorSource was given without its iterable".into())
            })?;
            amoru_runtime::SourceSpec::Build(Box::new(move |ctx| {
                Ok(Arc::new(amoru_sources::PyIteratorSource::new(
                    iterator,
                    schema,
                    ctx.reactor.clone(),
                )?))
            }))
        }
    })
}

fn sink_factory(sink: SinkSpec) -> amoru_runtime::SinkSpec {
    match sink {
        SinkSpec::Parquet(cfg) => amoru_runtime::SinkSpec::Build(Box::new(move |ctx| {
            // The run id goes into every file's footer (08 e.2); `ParquetSinkConfig` has no
            // field for it, so the sink is told once it is built.
            let sink = ParquetSink::new(cfg, ctx.reactor.clone(), ctx.alloc.clone())?
                .with_run_id(ctx.run_id);
            Ok(Box::new(sink))
        })),
        SinkSpec::Tensor(cfg) => amoru_runtime::SinkSpec::Build(Box::new(move |ctx| {
            Ok(Box::new(TensorSink::new(
                cfg,
                ctx.reactor.clone(),
                ctx.alloc.clone(),
            )?))
        })),
        SinkSpec::ArrowIpc(cfg) => amoru_runtime::SinkSpec::Build(Box::new(move |ctx| {
            Ok(Box::new(ArrowIpcSink::new(
                cfg,
                ctx.reactor.clone(),
                ctx.alloc.clone(),
            )?))
        })),
    }
}

fn source_schema(schema: IteratorSchema) -> Result<SourceSchema, AmoruError> {
    Ok(match schema {
        IteratorSchema::Table(s) => SourceSchema::Table(s),
        IteratorSchema::Tensor { dtype, shape } => SourceSchema::Tensor {
            dtype: dtype_of(&dtype)?,
            shape,
        },
    })
}

fn dtype_of(name: &str) -> Result<DType, AmoruError> {
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
            return Err(AmoruError::Config {
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

fn object_store_config(d: &Bound<'_, PyDict>) -> PyResult<ObjectStoreConfig> {
    let mut cfg = ObjectStoreConfig::default();
    let mut s3 = S3Config::default();
    let mut gcs = GcsConfig::default();
    let mut azure = AzureConfig::default();
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
            "allow_http" => cfg.allow_http = value.extract()?,
            "local_root" => cfg.local_root = Some(PathBuf::from(value.extract::<String>()?)),
            "service_account_path" => {
                gcs.service_account_path = Some(PathBuf::from(value.extract::<String>()?));
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
        cfg.s3 = Some(s3);
    }
    if any_gcs {
        cfg.gcs = Some(gcs);
    }
    if any_azure {
        cfg.azure = Some(azure);
    }
    Ok(cfg)
}

fn map<T>(py: Python<'_>, r: Result<T, AmoruError>) -> PyResult<T> {
    r.map_err(|e| map_err(py, &e))
}

fn map_err(py: Python<'_>, e: &AmoruError) -> PyErr {
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
            Err(AmoruError::Config { name: "run", .. })
        ));
        drop(first);
        assert!(RunningGuard::acquire().is_ok());
    }

    #[test]
    fn run_ids_and_dtypes() {
        let id = run_id("0123456789abcdef0123456789abcdef").expect("a run id");
        assert_eq!(id.0[0], 0x01);
        assert!(run_id("zz").is_err());
        assert!(run_id(&"z".repeat(32)).is_err());
        assert_eq!(dtype_of("float32").expect("f32"), DType::F32);
        assert_eq!(dtype_of("i64").expect("i64"), DType::I64);
        assert!(dtype_of("decimal").is_err());
    }

    #[test]
    fn resume_needs_a_place_to_look() {
        let mut t = crate::translate::translate(
            RawArgs {
                budget: None,
                cpu: None,
                trace: None,
                staging_dir: None,
                staging_limit: None,
                on_error: OnErrorArg::Name("terminate".into()),
                ordered: false,
                sizer: "rule".into(),
                profiles_dir: None,
                allow_gil: false,
                checkpoint: true,
                checkpoint_interval: 5.0,
                keep_checkpoint: false,
                resume: Some("auto".into()),
            },
            Vec::new(),
        )
        .expect("translate");
        assert!(matches!(resolve_resume(&t), Err(AmoruError::Resume(_))));
        t.resume = Some(ResumeArg::Path(PathBuf::from("/tmp/m.json")));
        assert_eq!(
            resolve_resume(&t).expect("a path"),
            Some(PathBuf::from("/tmp/m.json"))
        );
        t.resume = None;
        assert_eq!(resolve_resume(&t).expect("none"), None);
    }
}
