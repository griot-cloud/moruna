//! The module's half of the function package (SDD 16): the manifest as Rust validates and writes
//! it (16 f.4), a component verified and given its manifest (16 f.5), a package checked through
//! its runtime (16 f.9) and compiled ahead of time (16 f.10).

use std::path::{Path, PathBuf};

use moruna_package::{Format, Manifest, Package};
use moruna_runtime::check::package::check_package as check_through_runtime;
use moruna_runtime::packaged::PackageHost;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyModule;

use crate::errors::{Attachments, to_py_err};

/// `_core.manifest_json(text)`: the manifest of 16 e.1 from a JSON object, validated (PK-I3) and
/// written in the canonical form; a refusal is a `ValueError` naming the field.
#[pyfunction]
pub fn manifest_json(text: &str) -> PyResult<String> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| PyValueError::new_err(format!("moruna.json: not JSON: {e}")))?;
    let manifest =
        Manifest::from_value(&value).map_err(|e| PyValueError::new_err(e.to_string()))?;
    Ok(manifest.to_json())
}

/// `_core.package_wasm(module, manifest, out)` (16 f.5): the component at `module` verified
/// against the world and against `manifest`, written to `out` with the manifest embedded.
#[pyfunction]
pub fn package_wasm(py: Python<'_>, module: &str, manifest: &str, out: &str) -> PyResult<()> {
    let manifest =
        Manifest::parse(manifest.as_bytes()).map_err(|e| PyValueError::new_err(e.to_string()))?;
    let bytes =
        std::fs::read(module).map_err(|e| PyValueError::new_err(format!("{module}: {e}")))?;
    let packaged = py
        .detach(|| moruna_wasm::package(&bytes, &manifest))
        .map_err(|e| PyValueError::new_err(e.to_string()))?;
    std::fs::write(out, packaged).map_err(|e| PyValueError::new_err(format!("{out}: {e}")))?;
    Ok(())
}

fn host(python: Option<String>, cgroup: Option<String>) -> PackageHost {
    PackageHost {
        sandbox_python: python.map(PathBuf::from),
        sandbox_cgroup: cgroup.map(PathBuf::from),
    }
}

/// `_core.check_package(path, *, seed, profiles_dir, sandbox_python, sandbox_cgroup)`: the
/// harness of MH 4.9 through the package's runtime (16 f.9); the JSON report plus `summary`.
#[pyfunction]
#[pyo3(signature = (path, *, seed = 0, profiles_dir = None, sandbox_python = None, sandbox_cgroup = None))]
pub fn check_package(
    py: Python<'_>,
    path: &str,
    seed: u64,
    profiles_dir: Option<String>,
    sandbox_python: Option<String>,
    sandbox_cgroup: Option<String>,
) -> PyResult<String> {
    let host = host(sandbox_python, sandbox_cgroup);
    let path = PathBuf::from(path);
    let profiles = profiles_dir.map(PathBuf::from);
    let report = py
        .detach(move || check_through_runtime(&path, seed, profiles, &host))
        .map_err(|e| to_py_err(py, &e, Attachments::default()))?;
    let mut json = report.to_json();
    if let serde_json::Value::Object(map) = &mut json {
        map.insert(
            "summary".into(),
            serde_json::Value::String(report.summary()),
        );
    }
    Ok(json.to_string())
}

/// `_core.aot(path)` (16 f.10): a `.wasm`'s `.cwasm`, or a `.pex`'s tree, written beside the
/// package; returns what was written.
#[pyfunction]
pub fn aot(py: Python<'_>, path: &str) -> PyResult<String> {
    let path = PathBuf::from(path);
    let written = py
        .detach(move || -> moruna_kernel::Result<PathBuf> {
            let package = Package::open(Path::new(&path))?;
            match package.format {
                Format::Wasm => moruna_wasm::aot(&package),
                Format::Pex => Ok(moruna_package::write_beside(&package)?),
            }
        })
        .map_err(|e| to_py_err(py, &e, Attachments::default()))?;
    Ok(written.display().to_string())
}

/// Registers the package functions on the module.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("KERNEL_ABI", moruna_kernel::declare::ABI_VERSION)?;
    m.add_function(wrap_pyfunction!(manifest_json, m)?)?;
    m.add_function(wrap_pyfunction!(package_wasm, m)?)?;
    m.add_function(wrap_pyfunction!(check_package, m)?)?;
    m.add_function(wrap_pyfunction!(aot, m)?)?;
    Ok(())
}
