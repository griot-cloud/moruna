//! Python handles to the existing engine learning, artifact and checkpoint authorities.
//! Scoped storage handles are supplied by a native embedding, never constructed from Python
//! policy callbacks, URLs, credentials, or a permissive default verifier.
use crate::errors::{Attachments, to_py_err};
use moruna_runtime::{
    Components,
    cost::{CostSettings, RunLearning},
    history::{Historical, ReadPort},
    portable::{EngineCheckpoints, Publisher},
};
use moruna_storage::{Access, Metadata};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyModule};
use std::path::Path;
use std::sync::Arc;

/// One configured observation store/model shared by registration warm-up and live runs.
#[pyclass(frozen, module = "moruna._core", name = "CostSession")]
pub struct PyCostSession {
    pub(crate) session: Arc<moruna_runtime::cost::Session>,
}
impl PyCostSession {
    /// Expose the same native session created from actual granted service authority.
    pub fn from_native(session: Arc<moruna_runtime::cost::Session>) -> Self {
        Self { session }
    }
    pub(crate) fn learning(
        &self,
        job: &moruna_job::Job,
        parts: &moruna_runtime::job::Parts,
    ) -> moruna_kernel::Result<RunLearning> {
        RunLearning::for_job(self.session.online()?, job, parts)
    }
    pub(crate) fn settings(&self) -> CostSettings {
        self.session.settings()
    }
}
#[pymethods]
impl PyCostSession {
    #[new]
    #[pyo3(signature=(store,*,model=None))]
    fn new(store: String, model: Option<String>) -> PyResult<Self> {
        if store.is_empty() || model.as_ref().is_some_and(String::is_empty) {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "cost store/model must be nonempty",
            ));
        }
        let mut settings = CostSettings::directory(&store);
        if let Some(model) = model {
            settings.model = model;
        }
        Ok(Self::from_native(Arc::new(
            moruna_runtime::cost::Session::new(settings),
        )))
    }
    #[getter]
    fn store(&self) -> Option<String> {
        match self.session.settings().store {
            moruna_runtime::cost::StoreSetting::Directory(path) => Some(path.display().to_string()),
            moruna_runtime::cost::StoreSetting::Embedder(_) => None,
        }
    }
    #[getter]
    fn model(&self) -> String {
        self.session.settings().model
    }
    /// Reload the actual fitted model after registration warm-up has persisted it. Refuses
    /// during a live run rather than replacing that run's in-memory model behind its back.
    fn refresh(&self, py: Python<'_>) -> PyResult<()> {
        let _guard = crate::run::RunningGuard::acquire()
            .map_err(|e| to_py_err(py, &e, Attachments::default()))?;
        let session = self.session.clone();
        py.detach(move || session.refresh())
            .map_err(|e| to_py_err(py, &e, Attachments::default()))?;
        Ok(())
    }
}

/// Exact existing native scoped ports a trusted embedding gives to the Python module.
/// There is deliberately no Python constructor or credential/policy factory.
pub struct NativePorts {
    /// Actual consumed guest start/service/account grants, when bootstrapped in the VM.
    pub guest: Option<Arc<moruna_runtime::bootstrap::Guest>>,
    /// Required capture configured against the checked job/run, including real budgets.
    pub history: Option<Arc<Historical>>,
    /// Existing live-authorized retained artifact reader.
    pub reader: Option<Arc<ReadPort>>,
    /// Existing checkpoint writer, preserving actual conditional bucket receipts.
    pub checkpoints: Option<Arc<Publisher>>,
    /// Existing engine byte materializer and live request authority for restore.
    pub restore: Option<(Arc<EngineCheckpoints>, Arc<Access>)>,
}
/// Native-authorized engine capabilities; absent ports refuse explicitly.
#[pyclass(frozen, module = "moruna._core", name = "EnginePorts")]
pub struct PyEnginePorts {
    ports: NativePorts,
}
impl PyEnginePorts {
    /// Hand real scoped authorities into Python from a native embedding. No authority is
    /// minted, copied, inferred or relaxed by this conversion.
    pub fn from_native(ports: NativePorts) -> Self {
        Self { ports }
    }
    pub(crate) fn components(
        &self,
        job: &moruna_job::Job,
        parts: &moruna_runtime::job::Parts,
    ) -> moruna_kernel::Result<Components> {
        if let Some(guest) = &self.ports.guest {
            return guest.components(job, parts);
        }
        let invalid = |why: &str| {
            moruna_kernel::MorunaError::Plan(format!("native Python engine ports: {why}"))
        };
        let mut run: Option<String> = None;
        if let Some(history) = &self.ports.history {
            let actual = Historical::bindings_for_job(job, parts, &history.outputs)?;
            if actual.len() != history.bindings.len()
                || actual.iter().zip(&history.bindings).any(|(a, b)| {
                    a.len() != b.len()
                        || a.iter()
                            .zip(b)
                            .any(|(a, b)| a.task != b.task || a.ports != b.ports)
                })
            {
                return Err(invalid(
                    "retention bindings differ from the actual checked job",
                ));
            }
            for output in &history.outputs {
                let identity = &output.request.identity.run;
                if output.access.run != *identity || run.as_ref().is_some_and(|run| run != identity)
                {
                    return Err(invalid("retention request identities disagree"));
                }
                run = Some(identity.clone());
            }
        }
        if let Some(checkpoints) = &self.ports.checkpoints {
            if run
                .as_ref()
                .is_some_and(|run| run != &checkpoints.access.run)
            {
                return Err(invalid("checkpoint/retention run identities disagree"));
            }
            run = Some(checkpoints.access.run.clone());
        }
        let run_id = run
            .map(|run| {
                moruna_kernel::RunId::from_hex(&run)
                    .ok_or_else(|| invalid("run identity is not canonical"))
            })
            .transpose()?;
        Ok(Components {
            run_id,
            history: self.ports.history.clone(),
            checkpoints: self.ports.checkpoints.clone(),
            ..Components::default()
        })
    }
}
fn unavailable(port: &str) -> PyErr {
    Python::attach(|py| {
        to_py_err(
            py,
            &moruna_kernel::MorunaError::Config {
                name: "engine_ports",
                msg: format!("native engine port unavailable: {port}"),
            },
            Attachments::default(),
        )
    })
}
fn encoded<T: serde::Serialize>(value: &T) -> PyResult<String> {
    serde_json::to_string(value)
        .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
}
fn pinned(value: &str) -> PyResult<Metadata> {
    // Transport metadata is bounded; the native reader still performs live authorization and
    // exact immutable metadata/hash checks, so parsing never confers authority.
    if value.len() > 65536 {
        return Err(pyo3::exceptions::PyValueError::new_err(
            "pinned metadata exceeds 65536 bytes",
        ));
    }
    serde_json::from_str(value)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("pinned metadata: {e}")))
}
fn runtime() -> PyResult<tokio::runtime::Runtime> {
    moruna_reactor::configure_runtime(&mut tokio::runtime::Builder::new_current_thread())
        .build()
        .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
}
fn storage_error(error: moruna_storage::Error) -> PyErr {
    Python::attach(|py| {
        let build = || -> PyResult<PyErr> {
            let result = crate::errors::IoError::new_err(error.to_string());
            let value = result.value(py);
            let diagnostic = PyDict::new(py);
            diagnostic.set_item(
                "storage_refusal",
                format!("{:?}", moruna_storage::port::refusal(&error)),
            )?;
            if let moruna_storage::Error::StagingPending { need } = &error {
                diagnostic.set_item("need", need)?;
            }
            if let Some((need, budget)) = error.capacity() {
                diagnostic.set_item("need", need)?;
                diagnostic.set_item("budget", budget)?;
            }
            if let Some((metadata, cause)) = error.local_checkpoint() {
                let actual = py
                    .import("json")?
                    .call_method1("loads", (encoded(metadata)?,))?;
                diagnostic.set_item("local_checkpoint", actual)?;
                diagnostic.set_item(
                    "cause_refusal",
                    format!("{:?}", moruna_storage::port::refusal(cause)),
                )?;
                diagnostic.set_item("cause", cause.to_string())?;
                if let moruna_storage::Error::StagingPending { need } = cause {
                    diagnostic.set_item("cause_need", need)?;
                }
                if let Some((need, budget)) = cause.capacity() {
                    diagnostic.set_item("cause_need", need)?;
                    diagnostic.set_item("cause_budget", budget)?;
                }
            }
            value.setattr("kind", "Io")?;
            value.setattr("message", error.to_string())?;
            value.setattr("diagnostic", diagnostic)?;
            value.setattr("run_id", py.None())?;
            value.setattr("manifest", py.None())?;
            value.setattr("report", py.None())?;
            Ok(result)
        };
        build().unwrap_or_else(|error| error)
    })
}
#[pymethods]
impl PyEnginePorts {
    /// Consume real named guest startup grants, without tokens, URL permissions or defaults.
    #[staticmethod]
    fn from_guest(py: Python<'_>) -> PyResult<Self> {
        let guest = py
            .detach(moruna_runtime::bootstrap::Guest::from_start)
            .map_err(|e| to_py_err(py, &e, Attachments::default()))?;
        Ok(Self::from_native(NativePorts {
            reader: guest.reader(),
            checkpoints: guest.checkpoints(),
            restore: guest.restore(),
            history: None,
            guest: Some(guest),
        }))
    }
    /// The same actual governed cost session used by this guest's live engine run.
    fn cost_session(&self) -> PyResult<PyCostSession> {
        let session = self
            .ports
            .guest
            .as_ref()
            .and_then(|guest| guest.cost_session())
            .ok_or_else(|| unavailable("guest cost session"))?;
        Ok(PyCostSession::from_native(session))
    }

    /// Exact native manifest state, latest persisted pointer and complete pointer for each
    /// requested task port. Partial/refused states are preserved, never promoted to complete.
    fn retained_outputs_json(&self) -> PyResult<String> {
        let history = self
            .ports
            .history
            .clone()
            .or_else(|| self.ports.guest.as_ref().and_then(|guest| guest.history()))
            .ok_or_else(|| unavailable("retained outputs"))?;
        let facts = history
            .publications()
            .into_iter()
            .map(|fact| {
                serde_json::json!({
                    "manifest":fact.manifest,"latest":fact.latest,"complete":fact.complete
                })
            })
            .collect::<Vec<_>>();
        encoded(&facts)
    }
    fn artifact_manifest_json(&self, py: Python<'_>, pinned_json: &str) -> PyResult<String> {
        let metadata = pinned(pinned_json)?;
        let reader = self
            .ports
            .reader
            .clone()
            .ok_or_else(|| unavailable("artifact reader"))?;
        py.detach(move || {
            let result = runtime()?
                .block_on(reader.manifest(&metadata))
                .map_err(storage_error)?;
            encoded(&result)
        })
    }
    fn artifact_chunk<'py>(
        &self,
        py: Python<'py>,
        pinned_json: &str,
        index: usize,
        byte_budget: u64,
    ) -> PyResult<Bound<'py, PyBytes>> {
        let metadata = pinned(pinned_json)?;
        let reader = self
            .ports
            .reader
            .clone()
            .ok_or_else(|| unavailable("artifact reader"))?;
        let data = py.detach(move || {
            runtime()?.block_on(async {
                let manifest = reader.manifest(&metadata).await.map_err(storage_error)?;
                reader
                    .chunk(&manifest, index, byte_budget)
                    .await
                    .map_err(storage_error)
            })
        })?;
        // This is an explicit Python bytes materialization, not an asserted zero-copy view.
        Ok(PyBytes::new(py, data.as_ref()))
    }
    fn checkpoint_receipt_json(&self) -> PyResult<Option<String>> {
        let publisher = self
            .ports
            .checkpoints
            .as_ref()
            .ok_or_else(|| unavailable("checkpoint writer"))?;
        publisher.receipt().as_ref().map(encoded).transpose()
    }
    fn checkpoint_pending_json(&self) -> PyResult<Option<String>> {
        let publisher = self
            .ports
            .checkpoints
            .as_ref()
            .ok_or_else(|| unavailable("checkpoint writer"))?;
        publisher.local_pending().as_ref().map(encoded).transpose()
    }
    fn restore_checkpoint(
        &self,
        py: Python<'_>,
        pinned_json: &str,
        staging_root: String,
    ) -> PyResult<String> {
        let metadata = pinned(pinned_json)?;
        let (bridge, access) = self
            .ports
            .restore
            .clone()
            .ok_or_else(|| unavailable("checkpoint restore"))?;
        let publisher = self.ports.checkpoints.clone();
        py.detach(move || {
            runtime()?.block_on(async {
                let path = bridge
                    .restore(&access, &metadata, Path::new(&staging_root))
                    .await
                    .map_err(storage_error)?;
                if let Some(publisher) = publisher {
                    publisher
                        .resume_at(&metadata)
                        .await
                        .map_err(storage_error)?;
                }
                Ok(path.display().to_string())
            })
        })
    }
}
/// Register handles without installing any default storage authority.
pub fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyCostSession>()?;
    module.add_class::<PyEnginePorts>()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sdk_errors_keep_real_native_capacity_and_do_not_fabricate_pending_numbers() {
        Python::initialize();
        Python::attach(|py| {
            let refusal = storage_error(moruna_storage::Error::CaptureCapacity {
                need: 8192,
                budget: 4096,
            });
            assert!(refusal.is_instance_of::<crate::errors::IoError>(py));
            let diagnostic = refusal.value(py).getattr("diagnostic").unwrap();
            assert_eq!(
                diagnostic
                    .get_item("storage_refusal")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "Capacity"
            );
            assert_eq!(
                diagnostic
                    .get_item("need")
                    .unwrap()
                    .extract::<u64>()
                    .unwrap(),
                8192
            );
            assert_eq!(
                diagnostic
                    .get_item("budget")
                    .unwrap()
                    .extract::<u64>()
                    .unwrap(),
                4096
            );
            let pending = storage_error(moruna_storage::Error::StagingPending { need: 8192 });
            let diagnostic = pending.value(py).getattr("diagnostic").unwrap();
            assert_eq!(
                diagnostic
                    .get_item("need")
                    .unwrap()
                    .extract::<u64>()
                    .unwrap(),
                8192
            );
            assert!(!diagnostic.contains("budget").unwrap());
            assert!(!diagnostic.contains("local_checkpoint").unwrap());
        });
    }
    #[test]
    fn absent_native_capability_is_a_loud_configuration_refusal() {
        Python::initialize();
        Python::attach(|py| {
            let ports = PyEnginePorts::from_native(NativePorts {
                guest: None,
                history: None,
                reader: None,
                checkpoints: None,
                restore: None,
            });
            let error = ports.retained_outputs_json().unwrap_err();
            assert!(error.is_instance_of::<crate::errors::ConfigError>(py));
            assert!(
                error
                    .value(py)
                    .getattr("message")
                    .unwrap()
                    .extract::<String>()
                    .unwrap()
                    .contains("retained outputs")
            );
        });
    }
}
