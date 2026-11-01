//! `moruna.read_events(...)` (MH 4.3 version 7): the events a run wrote as records, as dicts.
//!
//! A run's events are records (SDD 21) whose body is FlatBuffers; the run's output store keeps
//! them in files of records back to back. This decodes such bytes with `moruna-event` and hands
//! the package the JSON each event renders as, which it reads into dicts with the field names the
//! events of version 6 had.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

/// `_core.read_events(records)`: every event in `records`, as a JSON array of the events'
/// renderings. Bytes that are not whole events are a `ValueError` naming where they stop being
/// one.
#[pyfunction]
pub fn read_events(py: Python<'_>, records: &[u8]) -> PyResult<String> {
    let events = py
        .detach(|| moruna_event::read_records(records))
        .map_err(PyValueError::new_err)?;
    let values: Vec<serde_json::Value> = events.iter().map(|e| e.to_value()).collect();
    serde_json::to_string(&values).map_err(|e| PyValueError::new_err(e.to_string()))
}
