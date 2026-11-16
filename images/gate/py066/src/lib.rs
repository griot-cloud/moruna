//! `moruna_gate._native`: the gate's seven Rust functions, each a pyarrow `RecordBatch` in and a
//! pyarrow `RecordBatch` out over the Arrow C data interface, the GIL released while the function
//! runs. It declares itself free-threading safe: it keeps no Python state.

use arrow::array::RecordBatch;
use moruna_gate_functions::Error;
use moruna_gate_functions::batch;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3_arrow::PyRecordBatch;

type Function = fn(&RecordBatch) -> Result<RecordBatch, Error>;

fn call<'py>(py: Python<'py>, f: Function, b: PyRecordBatch) -> PyResult<Bound<'py, PyAny>> {
    let input = b.into_inner();
    let out = py
        .detach(|| f(&input))
        .map_err(|e| PyValueError::new_err(e.to_string()))?;
    PyRecordBatch::new(out).into_pyarrow(py)
}

macro_rules! functions {
    ($($name:ident),*) => {
        $(
            #[pyfunction]
            fn $name<'py>(py: Python<'py>, batch: PyRecordBatch) -> PyResult<Bound<'py, PyAny>> {
                call(py, batch::$name, batch)
            }
        )*

        #[pymodule(gil_used = false)]
        fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
            $( m.add_function(wrap_pyfunction!($name, m)?)?; )*
            Ok(())
        }
    };
}

functions!(
    score, occupancy, enrich, audit, ov_shape, ov_attrs, fw_minhash
);
