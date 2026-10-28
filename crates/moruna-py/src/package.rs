//! The module's half of the function package (SDD 16): the manifest as Rust validates and writes
//! it (16 f.4), a component verified and given its manifest (16 f.5), a package checked through
//! its runtime (16 f.9) and compiled ahead of time (16 f.10).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use moruna_kernel::{Allocator, InitCtx, KernelState, Payload, Tier};
use moruna_package::{Format, Inputs, Manifest, Package};
use moruna_runtime::check::package::check_package as check_through_runtime;
use moruna_runtime::packaged::{self, PackageHost, Packaged};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyModule};
use pyo3_arrow::PyRecordBatch;

/// The arena `moruna.load` lands a kernel's outputs in (16 i).
pub const LOAD_ARENA_BYTES: u64 = 256 << 20;

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

/// `_core.package_manifest(path)`: a package's manifest (16 e.1), read as a loader reads it.
#[pyfunction]
pub fn package_manifest(path: &str) -> PyResult<String> {
    let package =
        Package::open(Path::new(path)).map_err(|e| PyValueError::new_err(e.to_string()))?;
    Ok(package.manifest.to_json())
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

/// The inputs of 16 b from their spellings: the arguments as JSON text, the assets as a path.
fn inputs(args: Option<&str>, assets: Option<String>) -> PyResult<Inputs> {
    let args = match args {
        Some(text) => Some(
            serde_json::from_str::<serde_json::Value>(text)
                .map_err(|e| PyValueError::new_err(format!("args: not JSON: {e}")))?,
        ),
        None => None,
    };
    Ok(Inputs {
        args,
        assets: assets.map(PathBuf::from),
    })
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
#[pyo3(signature = (path, *, seed = 0, profiles_dir = None, sandbox_python = None, sandbox_cgroup = None, args = None, assets = None))]
#[allow(clippy::too_many_arguments)]
pub fn check_package(
    py: Python<'_>,
    path: &str,
    seed: u64,
    profiles_dir: Option<String>,
    sandbox_python: Option<String>,
    sandbox_cgroup: Option<String>,
    args: Option<String>,
    assets: Option<String>,
) -> PyResult<String> {
    let host = host(sandbox_python, sandbox_cgroup);
    let inputs = inputs(args.as_deref(), assets)?;
    let path = PathBuf::from(path);
    let profiles = profiles_dir.map(PathBuf::from);
    let report = py
        .detach(move || check_through_runtime(&path, seed, profiles, &host, inputs))
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

/// What `moruna.load` returns (16 d.3): a packaged kernel under its own runtime on this machine,
/// callable on a batch, and accepted by `moruna.run`, which runs the same file under the run's
/// own instance (12 f.8).
#[pyclass(frozen, module = "moruna._core", name = "PackageKernel")]
pub struct PyPackageKernel {
    /// The package's file.
    pub(crate) path: String,
    /// `sha256:<hex>`.
    pub(crate) fingerprint: String,
    /// What a `.pex` needs, for the run this kernel is given to.
    pub(crate) host: PackageHost,
    /// Its arguments and assets, for the run this kernel is given to (16 b).
    pub(crate) inputs: Inputs,
    format: &'static str,
    manifest: String,
    packaged: Packaged,
    /// Dropped before `alloc`, the arena its outputs land in (fields drop in order).
    state: Mutex<Box<dyn KernelState>>,
    #[allow(dead_code)]
    alloc: Arc<dyn Allocator>,
}

#[pymethods]
impl PyPackageKernel {
    /// The package's file.
    #[getter]
    fn path(&self) -> String {
        self.path.clone()
    }

    /// `sha256:<hex>` of the file (16 b).
    #[getter]
    fn fingerprint(&self) -> String {
        self.fingerprint.clone()
    }

    /// `wasm` or `pex`.
    #[getter]
    fn format(&self) -> &'static str {
        self.format
    }

    /// The manifest (16 e.1), as a dict.
    #[getter]
    fn manifest<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        py.import("json")?.call_method1("loads", (&self.manifest,))
    }

    /// One batch through the kernel, under its runtime (16 f.11).
    fn __call__<'py>(
        &self,
        py: Python<'py>,
        batch: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let input: PyRecordBatch = batch.extract().map_err(|e| {
            pyo3::exceptions::PyTypeError::new_err(format!(
                "a package kernel takes a pyarrow.RecordBatch: {e}"
            ))
        })?;
        let input = input.into_inner();
        let kernel = self.packaged.kernel();
        let out = py
            .detach(|| {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                kernel.apply(state.as_mut(), Payload::Table(input, Tier::Host))
            })
            .map_err(|e| to_py_err(py, &e, Attachments::default()))?;
        let _ = moruna_kernel::take_kernel_alloc();
        match out {
            Payload::Table(batch, _) => PyRecordBatch::new(batch)
                .into_pyarrow(py)
                .map(|b| b.into_any()),
            Payload::Tensor(..) => Err(pyo3::exceptions::PyTypeError::new_err(
                "a package kernel returned a tensor",
            )),
        }
    }

    fn __repr__(&self) -> String {
        format!("moruna.load({:?}) {}", self.path, self.fingerprint)
    }
}

/// `_core.load(path, *, python, cgroup)` (16 d.3, f.11).
#[pyfunction]
#[pyo3(signature = (path, *, python = None, cgroup = None, args = None, assets = None))]
pub fn load(
    py: Python<'_>,
    path: &str,
    python: Option<String>,
    cgroup: Option<String>,
    args: Option<&Bound<'_, PyAny>>,
    assets: Option<String>,
) -> PyResult<PyPackageKernel> {
    let args_text: Option<String> = match args {
        Some(dict) => Some(
            py.import("json")?
                .call_method1("dumps", (dict,))?
                .extract()?,
        ),
        None => None,
    };
    let inputs = inputs(args_text.as_deref(), assets)?;
    let format =
        Format::of_path(Path::new(path)).map_err(|e| PyValueError::new_err(e.to_string()))?;
    let mut host = PackageHost {
        sandbox_python: python.map(PathBuf::from),
        sandbox_cgroup: cgroup.map(PathBuf::from),
    };
    if format == Format::Pex {
        if host.sandbox_python.is_none() {
            let sys = py.import("sys")?;
            let gil: bool = sys.call_method0("_is_gil_enabled")?.extract()?;
            if !gil {
                return Err(PyValueError::new_err(
                    "moruna.load: a .pex runs on the standard build of CPython; this interpreter \
                     is free threaded, so name one with python=",
                ));
            }
            host.sandbox_python = Some(PathBuf::from(
                sys.getattr("executable")?.extract::<String>()?,
            ));
        }
        if host.sandbox_cgroup.is_none() {
            return Err(PyValueError::new_err(
                "moruna.load: a .pex runs under a cgroup; name one with cgroup= (a cgroup v2 \
                 directory with memory and cpu delegated, 17 e.6)",
            ));
        }
    }
    let path_buf = PathBuf::from(path);
    let opened_host = host.clone();
    let opened_inputs = inputs.clone();
    let (packaged, alloc, state) = py
        .detach(move || -> moruna_kernel::Result<_> {
            let packaged = packaged::open(&path_buf, None, &opened_host, opened_inputs)?;
            packaged.bind_here()?;
            let arena = moruna_arena_for_load()?;
            let state = packaged.kernel().init(&InitCtx {
                instance: 0,
                device: None,
                alloc: Arc::clone(&arena),
            })?;
            Ok((packaged, arena, state))
        })
        .map_err(|e| to_py_err(py, &e, Attachments::default()))?;
    let package = packaged.package().cloned().ok_or_else(|| {
        to_py_err(
            py,
            &moruna_kernel::MorunaError::Plan(
                "moruna.load opens a package, and an image is not one".into(),
            ),
            Attachments::default(),
        )
    })?;
    Ok(PyPackageKernel {
        path: path.to_string(),
        fingerprint: package.fingerprint_string(),
        format: package.format.as_str(),
        manifest: package.manifest.to_json(),
        host,
        inputs,
        packaged,
        state: Mutex::new(state),
        alloc,
    })
}

fn moruna_arena_for_load() -> moruna_kernel::Result<Arc<dyn Allocator>> {
    let arena = moruna_arena::Arena::new(moruna_arena::ArenaConfig {
        host_bytes: LOAD_ARENA_BYTES,
        ..moruna_arena::ArenaConfig::default()
    })?;
    Ok(arena as Arc<dyn Allocator>)
}

/// Registers the package functions on the module.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("KERNEL_ABI", moruna_kernel::declare::ABI_VERSION)?;
    m.add_function(wrap_pyfunction!(manifest_json, m)?)?;
    m.add_function(wrap_pyfunction!(package_manifest, m)?)?;
    m.add_function(wrap_pyfunction!(package_wasm, m)?)?;
    m.add_function(wrap_pyfunction!(check_package, m)?)?;
    m.add_function(wrap_pyfunction!(aot, m)?)?;
    m.add_function(wrap_pyfunction!(load, m)?)?;
    m.add_class::<PyPackageKernel>()?;
    #[cfg(target_os = "linux")]
    moruna_sandbox::python::register(m)?;
    Ok(())
}
