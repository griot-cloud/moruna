//! A packaged kernel opened for a run, a check or a load (16 f.8): the package read once, its pin
//! checked, and its runtime chosen by its extension (17).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use moruna_kernel::{Kernel, MorunaError, OutsideMemory, Result, Sampler};
use moruna_package::{Format, Inputs, Package};
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
    /// A `.pex` in a confined child.
    #[cfg(target_os = "linux")]
    Pex(Arc<moruna_sandbox::SandboxKernel>),
}

impl Packaged {
    /// The kernel, as a run uses it.
    pub fn kernel(&self) -> Arc<dyn Kernel> {
        match self {
            Packaged::Wasm(k) => Arc::clone(k) as Arc<dyn Kernel>,
            #[cfg(target_os = "linux")]
            Packaged::Pex(k) => Arc::clone(k) as Arc<dyn Kernel>,
        }
    }

    /// The package.
    pub fn package(&self) -> &Arc<Package> {
        match self {
            Packaged::Wasm(k) => k.package(),
            #[cfg(target_os = "linux")]
            Packaged::Pex(k) => k.package(),
        }
    }

    /// What a run should say about how the kernel was made (a compile, a tree).
    pub fn notes(&self) -> Vec<String> {
        match self {
            Packaged::Wasm(k) => k.notes().to_vec(),
            #[cfg(target_os = "linux")]
            Packaged::Pex(k) => k.notes(),
        }
    }

    /// `wasm` or `pex`, for a check report.
    pub fn kind(&self) -> &'static str {
        self.package().format.as_str()
    }

    /// Bind the run (17 f.4, f.7): a `.wasm` answers to the run's gate; a `.pex`'s stage
    /// ceiling is computed from the run's sampler and starting ceiling.
    pub fn bind(
        &self,
        gate: &Arc<moruna_adapters::guard::MemoryGate>,
        sampler: &Arc<dyn Sampler>,
        ceiling: u64,
    ) {
        match self {
            Packaged::Wasm(k) => k.bind_gate(Arc::clone(gate)),
            #[cfg(target_os = "linux")]
            Packaged::Pex(k) => k.bind_run(Arc::clone(sampler), ceiling),
        }
        let _ = (sampler, ceiling);
    }

    /// Bind to this process alone, for a check or a load (16 f.9, f.11): the machine's ceiling
    /// as discovery finds it, its sampler, and a gate over that sampler.
    pub fn bind_here(&self) -> Result<()> {
        let discovered = moruna_discovery::discover(&moruna_discovery::DiscoveryInput::default())?;
        let sampler: Arc<dyn Sampler> = Arc::new(moruna_discovery::Sampler::new(&discovered)?);
        let ceiling = discovered.limits.memory_ceiling;
        let gate = moruna_adapters::guard::MemoryGate::new(Arc::clone(&sampler), ceiling);
        self.bind(&gate, &sampler, ceiling);
        Ok(())
    }

    /// Memory the stage holds outside this process (SB-I9): a `.pex`'s child.
    pub fn outside(&self) -> Option<Arc<dyn OutsideMemory>> {
        match self {
            Packaged::Wasm(_) => None,
            #[cfg(target_os = "linux")]
            Packaged::Pex(k) => Some(k.outside()),
        }
    }

    /// The limits moved (MH 4.4): a `.pex` sends its stage ceiling now.
    pub fn limits_changed(&self) {
        match self {
            Packaged::Wasm(_) => {}
            #[cfg(target_os = "linux")]
            Packaged::Pex(k) => k.limits_changed(),
        }
    }
}

/// Open the package at `path`, check `pin` against it when one is given (PK-I2), and make its
/// kernel (16 f.8).
pub fn open(
    path: &Path,
    pin: Option<&str>,
    host: &PackageHost,
    inputs: Inputs,
) -> Result<Packaged> {
    let package = Arc::new(Package::open(path).map_err(MorunaError::from)?);
    if let Some(pin) = pin {
        package.check_pin(pin).map_err(MorunaError::from)?;
    }
    match package.format {
        Format::Wasm => Ok(Packaged::Wasm(Arc::new(WasmKernel::new(package, inputs)?))),
        Format::Pex => pex(package, host, inputs),
    }
}

#[cfg(target_os = "linux")]
fn pex(package: Arc<Package>, host: &PackageHost, inputs: Inputs) -> Result<Packaged> {
    let missing = |option: &str| MorunaError::Config {
        name: "sandbox",
        msg: format!(
            "{}: a .pex runs in the sandbox, which needs {option} (17 e.6)",
            package.path.display()
        ),
    };
    let python = host.sandbox_python.clone().ok_or_else(|| {
        missing("--sandbox-python, a standard-build CPython with moruna and pyarrow")
    })?;
    let cgroup = host.sandbox_cgroup.clone().ok_or_else(|| {
        missing("--sandbox-cgroup, a cgroup v2 directory with memory and cpu delegated")
    })?;
    let host = moruna_sandbox::SandboxHost { python, cgroup };
    Ok(Packaged::Pex(Arc::new(moruna_sandbox::SandboxKernel::new(
        Arc::clone(&package),
        host,
        inputs,
    )?)))
}

#[cfg(not(target_os = "linux"))]
fn pex(package: Arc<Package>, _host: &PackageHost, _inputs: Inputs) -> Result<Packaged> {
    Err(MorunaError::Config {
        name: "sandbox",
        msg: format!("{}: {}", package.path.display(), moruna_sandbox::NOT_LINUX),
    })
}

/// A sampler that adds the memory a run's packaged stages hold outside the process (SB-I9).
pub struct WithOutside {
    inner: Arc<dyn Sampler>,
    outside: Vec<Arc<dyn OutsideMemory>>,
    peak: std::sync::atomic::AtomicU64,
}

impl WithOutside {
    /// `inner`, with `outside` added to every sample.
    pub fn new(inner: Arc<dyn Sampler>, outside: Vec<Arc<dyn OutsideMemory>>) -> WithOutside {
        WithOutside {
            inner,
            outside,
            peak: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn held(&self) -> u64 {
        self.outside.iter().map(|o| o.in_use()).sum()
    }
}

impl Sampler for WithOutside {
    fn sample(&self) -> moruna_kernel::Sample {
        let mut sample = self.inner.sample();
        let held = self.held();
        sample.anon_bytes = sample.anon_bytes.saturating_add(held);
        sample.peak_anon_bytes = sample.peak_anon_bytes.saturating_add(held);
        self.peak
            .fetch_max(sample.anon_bytes, std::sync::atomic::Ordering::AcqRel);
        sample
    }

    fn reset_peak(&self) {
        self.inner.reset_peak();
    }

    fn process_peak(&self) -> moruna_kernel::ProcessPeak {
        let mut peak = self.inner.process_peak();
        let seen = self.peak.load(std::sync::atomic::Ordering::Acquire);
        if seen > peak.bytes {
            peak.bytes = seen;
            peak.exact = false;
        }
        peak
    }

    fn process_usage(&self) -> moruna_kernel::ProcessUsage {
        self.inner.process_usage()
    }
}
