//! A packaged kernel opened for a run, a check or a load (16 f.8): the package read once, its pin
//! checked, and its runtime chosen by its extension (17).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use moruna_kernel::{Kernel, MorunaError, Result};
use moruna_package::{Format, Package};
use moruna_wasm::WasmKernel;

/// What a host gives a `.pex` (17 e.6): the standard-build interpreter and the sandbox cgroup.
/// Both come from the command line (`--sandbox-python`, `--sandbox-cgroup`) or the library's
/// `moruna.load(python=, cgroup=)`; nothing is read from the environment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackageHost {
    /// The standard-build CPython 3.14 with `moruna` and `pyarrow` installed.
    pub sandbox_python: Option<PathBuf>,
    /// A cgroup v2 directory with `memory` and `cpu` delegated to its children.
    pub sandbox_cgroup: Option<PathBuf>,
}

/// A package and the kernel its runtime made of it.
#[derive(Clone)]
pub enum Packaged {
    /// A `.wasm` in wasmtime.
    Wasm(Arc<WasmKernel>),
}

impl Packaged {
    /// The kernel, as a run uses it.
    pub fn kernel(&self) -> Arc<dyn Kernel> {
        match self {
            Packaged::Wasm(k) => Arc::clone(k) as Arc<dyn Kernel>,
        }
    }

    /// The package.
    pub fn package(&self) -> &Arc<Package> {
        match self {
            Packaged::Wasm(k) => k.package(),
        }
    }

    /// What a run should say about how the kernel was made (a compile, a tree).
    pub fn notes(&self) -> Vec<String> {
        match self {
            Packaged::Wasm(k) => k.notes().to_vec(),
        }
    }

    /// `wasm` or `pex`, for a check report.
    pub fn kind(&self) -> &'static str {
        self.package().format.as_str()
    }
}

/// Open the package at `path`, check `pin` against it when one is given (PK-I2), and make its
/// kernel (16 f.8).
pub fn open(path: &Path, pin: Option<&str>, host: &PackageHost) -> Result<Packaged> {
    let package = Arc::new(Package::open(path).map_err(MorunaError::from)?);
    if let Some(pin) = pin {
        package.check_pin(pin).map_err(MorunaError::from)?;
    }
    match package.format {
        Format::Wasm => Ok(Packaged::Wasm(Arc::new(WasmKernel::new(package)?))),
        Format::Pex => {
            let _ = host;
            Err(MorunaError::Config {
                name: "package",
                msg: format!(
                    "{}: a .pex runs in moruna-sandbox (SDD 17), which this build does not have \
                     yet",
                    path.display()
                ),
            })
        }
    }
}
