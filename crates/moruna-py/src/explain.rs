//! `moruna.explain(...)` (SDD 26 d.5): a job document's plan, as JSON the package reads into a
//! dict, before anything runs.

use pyo3::prelude::*;

use crate::errors::{Attachments, to_py_err};

/// `_core.explain(document, *, planner=None)`: the plan of the job document in `document` (its
/// bytes: version 2's FlatBuffers or version 1's JSON) by the planner `planner` names, the
/// default when it names none, as JSON. A document that is not one, or one its kinds refuse, is a
/// `ConfigError` naming the field; an unknown planner or a planner's failure is a `PlanError`.
#[pyfunction]
#[pyo3(signature = (document, *, planner = None))]
pub fn explain(py: Python<'_>, document: &[u8], planner: Option<String>) -> PyResult<String> {
    let job = moruna_runtime::host::session::read_document(document).map_err(|(_, refused)| {
        to_py_err(
            py,
            &moruna_runtime::job::refusal(refused),
            Attachments::default(),
        )
    })?;
    let kinds = crate::kinds::command().map_err(|e| to_py_err(py, &e, Attachments::default()))?;
    let plan = py
        .detach(|| moruna_runtime::job::plan::explain(&job, planner.as_deref(), &kinds))
        .map_err(|e| to_py_err(py, &e.into_error(), Attachments::default()))?;
    Ok(plan.to_value().to_string())
}
