//! The exception hierarchy and the error to exception mapping (e.2, PY-I2).
//!
//! Every error reaches Python as one exception type with a structured payload: `MorunaError` with
//! `.kind` (the `MorunaError` variant name), `.message` and `.diagnostic`, plus `.run_id`,
//! `.manifest` and `.report` when the run produced them. The subclasses exist for `except`
//! ergonomics and are exactly e.2's table.

use moruna_kernel::{MorselFeatures, MorunaError as RsError};
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyType};

create_exception!(
    _core,
    MorunaError,
    PyException,
    "Every error the runtime raises. `.kind`, `.message`, `.diagnostic`, `.run_id`, `.manifest`, `.report`."
);
create_exception!(
    _core,
    PlanError,
    MorunaError,
    "The pipeline cannot be planned."
);
create_exception!(_core, KernelError, MorunaError, "A kernel raised.");
create_exception!(
    _core,
    BudgetError,
    MorunaError,
    "A morsel or an allocation did not fit the budget."
);
create_exception!(
    _core,
    IoError,
    MorunaError,
    "A source, a sink, the reactor or staging failed."
);
create_exception!(
    _core,
    ConfigError,
    MorunaError,
    "An argument or the host configuration is wrong."
);
create_exception!(
    _core,
    ResumeError,
    MorunaError,
    "A manifest cannot be used to resume this run."
);
create_exception!(
    _core,
    Cancelled,
    MorunaError,
    "The run was cancelled; the partial report is attached."
);

/// What the surface knows about a failed run beyond the error itself (PY-I9).
#[derive(Default)]
pub struct Attachments {
    /// The run id, when one had been minted.
    pub run_id: Option<String>,
    /// The manifest the scheduler last wrote, when the run is resumable.
    pub manifest: Option<String>,
    /// The partial report, when one could be produced.
    pub report: Option<Py<PyAny>>,
}

/// The `MorunaError` variant's name, which becomes `.kind` (PY-I2).
pub fn kind_of(err: &RsError) -> &'static str {
    match err {
        RsError::Plan(_) => "Plan",
        RsError::Source { .. } => "Source",
        RsError::Kernel { .. } => "Kernel",
        RsError::Sink(_) => "Sink",
        RsError::Alloc { .. } => "Alloc",
        RsError::Io { .. } => "Io",
        RsError::Budget { .. } => "Budget",
        RsError::Staging(_) => "Staging",
        RsError::Convert(_) => "Convert",
        RsError::Config { .. } => "Config",
        RsError::Cancelled => "Cancelled",
        RsError::Resume(_) => "Resume",
        RsError::Unsupported(_) => "Unsupported",
    }
}

/// The Python class e.2 maps this variant to.
pub fn class_of(py: Python<'_>, err: &RsError) -> Py<PyType> {
    let class = match err {
        RsError::Plan(_) | RsError::Convert(_) => py.get_type::<PlanError>(),
        RsError::Kernel { .. } => py.get_type::<KernelError>(),
        RsError::Budget { .. } | RsError::Alloc { .. } => py.get_type::<BudgetError>(),
        RsError::Source { .. } | RsError::Sink(_) | RsError::Io { .. } | RsError::Staging(_) => {
            py.get_type::<IoError>()
        }
        RsError::Config { .. } | RsError::Unsupported(_) => py.get_type::<ConfigError>(),
        RsError::Resume(_) => py.get_type::<ResumeError>(),
        RsError::Cancelled => py.get_type::<Cancelled>(),
    };
    class.unbind()
}

/// The diagnostic dict e.2 requires for this variant.
pub fn diagnostic<'py>(py: Python<'py>, err: &RsError) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    match err {
        RsError::Kernel {
            stage,
            seq,
            msg,
            features,
        } => {
            d.set_item("stage", stage)?;
            d.set_item("seq", seq)?;
            d.set_item("traceback", msg)?;
            d.set_item("features", features_dict(py, features.as_ref())?)?;
        }
        RsError::Budget {
            seq,
            stage,
            footprint,
            budget,
            features,
        } => {
            d.set_item("seq", seq)?;
            d.set_item("stage", stage)?;
            d.set_item("footprint", footprint)?;
            d.set_item("budget", budget)?;
            d.set_item("features", features_dict(py, Some(features))?)?;
        }
        RsError::Alloc {
            bytes,
            tier,
            budget,
            in_use,
        } => {
            d.set_item("bytes", bytes)?;
            d.set_item("tier", format!("{tier:?}"))?;
            d.set_item("budget", budget)?;
            d.set_item("in_use", in_use)?;
        }
        RsError::Source { split, msg } => {
            d.set_item("op", "read")?;
            d.set_item("target", "source")?;
            d.set_item("split", split)?;
            d.set_item("message", msg)?;
        }
        RsError::Sink(msg) => {
            d.set_item("op", "write")?;
            d.set_item("target", "sink")?;
            d.set_item("message", msg)?;
        }
        RsError::Io { op, target, msg } => {
            d.set_item("op", *op)?;
            d.set_item("target", target)?;
            d.set_item("message", msg)?;
        }
        RsError::Staging(msg) => {
            d.set_item("op", "staging")?;
            d.set_item("target", "staging")?;
            d.set_item("message", msg)?;
        }
        RsError::Config { name, msg } => {
            d.set_item("name", *name)?;
            d.set_item("message", msg)?;
        }
        RsError::Unsupported(feature) => {
            d.set_item("name", "feature")?;
            d.set_item("message", format!("built without {feature}"))?;
        }
        RsError::Plan(msg) | RsError::Resume(msg) => {
            d.set_item("message", msg)?;
        }
        RsError::Convert(e) => {
            d.set_item("message", e.to_string())?;
        }
        RsError::Cancelled => {}
    }
    Ok(d)
}

fn features_dict<'py>(
    py: Python<'py>,
    features: Option<&MorselFeatures>,
) -> PyResult<Option<Bound<'py, PyDict>>> {
    let Some(f) = features else { return Ok(None) };
    let d = PyDict::new(py);
    d.set_item("rows", f.rows)?;
    d.set_item("bytes", f.bytes)?;
    d.set_item("column_bytes", f.column_bytes.clone())?;
    d.set_item("mean_string_len", f.mean_string_len)?;
    d.set_item("null_ratio", f.null_ratio)?;
    d.set_item("shape", f.shape.clone())?;
    d.set_item("dtype", f.dtype.map(|dt| format!("{dt:?}")))?;
    Ok(Some(d))
}

/// Map an `MorunaError` to its Python exception, with the structured payload PY-I2 requires.
pub fn to_py_err(py: Python<'_>, err: &RsError, attached: Attachments) -> PyErr {
    let message = err.to_string();
    match build(py, err, &message, attached) {
        Ok(e) => e,
        // The mapping itself failed (an interpreter error while building the dict). The message is
        // still the runtime's, so nothing is lost but the structure.
        Err(e) => e,
    }
}

fn build(py: Python<'_>, err: &RsError, message: &str, attached: Attachments) -> PyResult<PyErr> {
    let class = class_of(py, err).into_bound(py);
    let instance = class.call1((message,))?;
    instance.setattr("kind", kind_of(err))?;
    instance.setattr("message", message)?;
    instance.setattr("diagnostic", diagnostic(py, err)?)?;
    instance.setattr("run_id", attached.run_id)?;
    instance.setattr("manifest", attached.manifest)?;
    instance.setattr("report", attached.report)?;
    Ok(PyErr::from_value(instance))
}

/// Register the hierarchy on the module (e.3: `_errors.py` re-exports them).
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    m.add("MorunaError", py.get_type::<MorunaError>())?;
    m.add("PlanError", py.get_type::<PlanError>())?;
    m.add("KernelError", py.get_type::<KernelError>())?;
    m.add("BudgetError", py.get_type::<BudgetError>())?;
    m.add("IoError", py.get_type::<IoError>())?;
    m.add("ConfigError", py.get_type::<ConfigError>())?;
    m.add("ResumeError", py.get_type::<ResumeError>())?;
    m.add("Cancelled", py.get_type::<Cancelled>())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use moruna_kernel::{ConvertError, Tier};

    fn features() -> MorselFeatures {
        MorselFeatures {
            rows: 10,
            bytes: 2048,
            column_bytes: vec![1024, 1024],
            mean_string_len: Some(8.0),
            null_ratio: Some(0.0),
            shape: None,
            dtype: None,
        }
    }

    /// PY-T2 exception_mapping (the mapping half; the `run_with` half needs the facade). Every
    /// `MorunaError` variant arrives as the class e.2 names, with `.kind` and the diagnostic dict
    /// populated.
    #[test]
    fn py_t2_exception_mapping() {
        Python::initialize();
        Python::attach(|py| {
            let cases: Vec<(RsError, &str, &str)> = vec![
                (RsError::Plan("chain".into()), "PlanError", "Plan"),
                (
                    RsError::Convert(ConvertError::MixedDTypes),
                    "PlanError",
                    "Convert",
                ),
                (
                    RsError::Config {
                        name: "sizer",
                        msg: "unknown".into(),
                    },
                    "ConfigError",
                    "Config",
                ),
                (RsError::Unsupported("rdma"), "ConfigError", "Unsupported"),
                (
                    RsError::Kernel {
                        stage: 1,
                        seq: 7,
                        msg: "ValueError: boom".into(),
                        features: Some(features()),
                    },
                    "KernelError",
                    "Kernel",
                ),
                (
                    RsError::Budget {
                        seq: 3,
                        stage: 2,
                        footprint: 99,
                        budget: 50,
                        features: features(),
                    },
                    "BudgetError",
                    "Budget",
                ),
                (
                    RsError::Alloc {
                        bytes: 16,
                        tier: Tier::Host,
                        budget: 8,
                        in_use: 8,
                    },
                    "BudgetError",
                    "Alloc",
                ),
                (
                    RsError::Source {
                        split: 4,
                        msg: "eof".into(),
                    },
                    "IoError",
                    "Source",
                ),
                (RsError::Sink("closed".into()), "IoError", "Sink"),
                (
                    RsError::Io {
                        op: "read",
                        target: "/tmp/x".into(),
                        msg: "enoent".into(),
                    },
                    "IoError",
                    "Io",
                ),
                (RsError::Staging("full".into()), "IoError", "Staging"),
                (RsError::Resume("digest".into()), "ResumeError", "Resume"),
                (RsError::Cancelled, "Cancelled", "Cancelled"),
            ];
            for (err, class_name, kind) in cases {
                let py_err = to_py_err(py, &err, Attachments::default());
                let value = py_err.value(py);
                assert_eq!(
                    value.get_type().name().expect("name").to_string(),
                    class_name,
                    "{kind}"
                );
                assert!(py_err.is_instance_of::<MorunaError>(py), "{kind}");
                let got: String = value.getattr("kind").expect("kind").extract().expect("str");
                assert_eq!(got, kind);
                let message: String = value
                    .getattr("message")
                    .expect("message")
                    .extract()
                    .expect("str");
                assert_eq!(message, err.to_string());
                assert!(
                    value
                        .getattr("diagnostic")
                        .expect("diagnostic")
                        .is_instance_of::<PyDict>()
                );
                assert!(value.getattr("report").expect("report").is_none());
            }
        });
    }

    #[test]
    fn diagnostic_fields_and_attachments() {
        Python::initialize();
        Python::attach(|py| {
            let err = RsError::Budget {
                seq: 3,
                stage: 2,
                footprint: 99,
                budget: 50,
                features: features(),
            };
            let py_err = to_py_err(
                py,
                &err,
                Attachments {
                    run_id: Some("f".repeat(32)),
                    manifest: Some("/tmp/run/manifest.json".into()),
                    report: None,
                },
            );
            let value = py_err.value(py);
            let d = value.getattr("diagnostic").expect("diagnostic");
            let footprint: u64 = d
                .get_item("footprint")
                .expect("item")
                .extract()
                .expect("u64");
            assert_eq!(footprint, 99);
            let feats = d.get_item("features").expect("features");
            let rows: u64 = feats
                .get_item("rows")
                .expect("rows")
                .extract()
                .expect("u64");
            assert_eq!(rows, 10);
            let manifest: String = value
                .getattr("manifest")
                .expect("manifest")
                .extract()
                .expect("str");
            assert_eq!(manifest, "/tmp/run/manifest.json");
            let run_id: String = value
                .getattr("run_id")
                .expect("run_id")
                .extract()
                .expect("str");
            assert_eq!(run_id.len(), 32);

            // A kernel error carries the original traceback string, a source error its split.
            let kernel = to_py_err(
                py,
                &RsError::Kernel {
                    stage: 1,
                    seq: 7,
                    msg: "ValueError: boom\n  File ...".into(),
                    features: None,
                },
                Attachments::default(),
            );
            let d = kernel.value(py).getattr("diagnostic").expect("diagnostic");
            let tb: String = d.get_item("traceback").expect("tb").extract().expect("str");
            assert!(tb.starts_with("ValueError: boom"));
            assert!(d.get_item("features").expect("features").is_none());
        });
    }
}
