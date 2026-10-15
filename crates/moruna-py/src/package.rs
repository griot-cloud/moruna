//! The module's half of the function package (SDD 16): the manifest as Rust validates and writes
//! it, for `moruna package` (16 f.4).

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyModule;

/// `_core.manifest_json(text)`: the manifest of 16 e.1 from a JSON object, validated (PK-I3) and
/// written in the canonical form; a refusal is a `ValueError` naming the field.
#[pyfunction]
pub fn manifest_json(text: &str) -> PyResult<String> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| PyValueError::new_err(format!("moruna.json: not JSON: {e}")))?;
    let manifest = moruna_package::Manifest::from_value(&value)
        .map_err(|e| PyValueError::new_err(e.to_string()))?;
    Ok(manifest.to_json())
}

/// Registers the package functions on the module.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("KERNEL_ABI", moruna_kernel::declare::ABI_VERSION)?;
    m.add_function(wrap_pyfunction!(manifest_json, m)?)?;
    Ok(())
}
