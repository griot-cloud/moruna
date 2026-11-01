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
    /// A PEX in an actual charged native process, not a Linux sandbox emulation.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    NativePex(Arc<crate::native_pex::NativePex>),
    /// An image's command in a confined process (SDD 19).
    #[cfg(target_os = "linux")]
    Image(Arc<moruna_sandbox::ImageKernel>),
    /// An image's command as a task of the data plane, on the Moruna kernel (SDD 30 f.5).
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    Task(Arc<moruna_micro::task::ImageTask>),
}

impl Packaged {
    /// Native tasks retain their entire carved Account until certified retirement.
    /// These variants each have exactly one live state instance.
    pub(crate) fn native_singleton(&self) -> bool {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        if matches!(self, Self::Task(_) | Self::NativePex(_)) {
            return true;
        }
        false
    }

    /// The kernel, as a run uses it.
    pub fn kernel(&self) -> Arc<dyn Kernel> {
        match self {
            Packaged::Wasm(k) => Arc::clone(k) as Arc<dyn Kernel>,
            #[cfg(target_os = "linux")]
            Packaged::Pex(k) => Arc::clone(k) as Arc<dyn Kernel>,
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            Packaged::NativePex(k) => Arc::clone(k) as Arc<dyn Kernel>,
            #[cfg(target_os = "linux")]
            Packaged::Image(k) => Arc::clone(k) as Arc<dyn Kernel>,
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            Packaged::Task(k) => Arc::clone(k) as Arc<dyn Kernel>,
        }
    }

    /// The package; an image has none.
    pub fn package(&self) -> Option<&Arc<Package>> {
        match self {
            Packaged::Wasm(k) => Some(k.package()),
            #[cfg(target_os = "linux")]
            Packaged::Pex(k) => Some(k.package()),
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            Packaged::NativePex(k) => Some(k.package()),
            #[cfg(target_os = "linux")]
            Packaged::Image(_) => None,
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            Packaged::Task(_) => None,
        }
    }

    /// What a run should say about how the kernel was made (a compile, a tree).
    pub fn notes(&self) -> Vec<String> {
        match self {
            Packaged::Wasm(k) => k.notes().to_vec(),
            #[cfg(target_os = "linux")]
            Packaged::Pex(k) => k.notes(),
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            Packaged::NativePex(k) => k.notes(),
            #[cfg(target_os = "linux")]
            Packaged::Image(_) => Vec::new(),
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            Packaged::Task(_) => Vec::new(),
        }
    }

    /// `wasm`, `pex` or `image`, for a check report.
    pub fn kind(&self) -> &'static str {
        match self.package() {
            Some(package) => package.format.as_str(),
            None => "image",
        }
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
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            Packaged::NativePex(k) => k.bind_run(ceiling),
            #[cfg(target_os = "linux")]
            Packaged::Image(k) => k.bind_run(Arc::clone(sampler), ceiling),
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            Packaged::Task(k) => k.bind_run(ceiling),
        }
        let _ = (sampler, ceiling);
    }

    /// Bind to this process alone, for a check or a load (16 f.9, f.11): the machine's ceiling
    /// as discovery finds it, its sampler, and a gate over that sampler.
    pub fn bind_here(&self) -> Result<()> {
        let discovered = moruna_discovery::discover(&moruna_discovery::DiscoveryInput::default())?;
        let sampler: Arc<dyn Sampler> = Arc::new(moruna_discovery::Sampler::new(&discovered)?);
        let ceiling = discovered.limits.memory_ceiling;
        let gate = moruna_adapters::guard::MemoryGate::new(Arc::clone(&sampler), ceiling)?;
        self.bind(&gate, &sampler, ceiling);
        Ok(())
    }

    /// The stage's ports (MH 4.10): a `.wasm`'s `call` reaches them, and an image's task's
    /// `port_call` on the Moruna kernel (SDD 30 f.7); a `.pex` calls none, nor an image on a
    /// Linux node, and a document that lists ports for one is refused before this.
    pub fn bind_ports(&self, ports: Arc<moruna_kernel::StagePorts>) {
        match self {
            Packaged::Wasm(k) => k.bind_ports(ports),
            #[cfg(target_os = "linux")]
            Packaged::Pex(_) | Packaged::Image(_) => {}
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            Packaged::Task(k) => k.bind_ports(ports),
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            Packaged::NativePex(_) => {}
        }
    }

    /// The run starts (MH 4.10): its cancel and its waits, for the stage's ports.
    pub fn bind_run_ports(&self, run: &moruna_kernel::RunPorts) {
        match self {
            Packaged::Wasm(k) => {
                if let Some(ports) = k.ports() {
                    ports.bind_run(run.clone());
                }
            }
            #[cfg(target_os = "linux")]
            Packaged::Pex(_) | Packaged::Image(_) => {}
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            Packaged::Task(k) => k.bind_run_ports(run),
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            Packaged::NativePex(k) => k.bind_run_ports(run),
        }
    }

    /// Memory the stage holds outside this process (SB-I9): a `.pex`'s child.
    pub fn outside(&self) -> Option<Arc<dyn OutsideMemory>> {
        match self {
            Packaged::Wasm(_) => None,
            #[cfg(target_os = "linux")]
            Packaged::Pex(k) => Some(k.outside()),
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            Packaged::NativePex(k) => Some(k.outside()),
            #[cfg(target_os = "linux")]
            Packaged::Image(k) => Some(k.outside()),
            // The task's account, which the kernel holds it to (SDD 30 f.11).
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            Packaged::Task(k) => Some(k.outside()),
        }
    }

    /// The limits moved (MH 4.4): a `.pex` sends its stage ceiling now.
    pub fn limits_changed(&self) {
        match self {
            Packaged::Wasm(_) => {}
            #[cfg(target_os = "linux")]
            Packaged::Pex(k) => k.limits_changed(),
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            Packaged::NativePex(_) => {}
            #[cfg(target_os = "linux")]
            Packaged::Image(k) => k.limits_changed(),
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            Packaged::Task(_) => {}
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
    #[cfg(target_arch = "x86_64")]
    if moruna_micro::on_kernel() {
        return crate::native_pex::NativePex::new(package, host, inputs)
            .map(|kernel| Packaged::NativePex(Arc::new(kernel)));
    }
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

    fn held(&self) -> moruna_kernel::Result<u64> {
        self.outside.iter().try_fold(0u64, |sum, outside| {
            sum.checked_add(outside.in_use()?)
                .ok_or_else(|| moruna_kernel::MorunaError::Config {
                    name: "outside_memory",
                    msg: "accounted byte total overflow".into(),
                })
        })
    }
}

impl Sampler for WithOutside {
    fn memory_basis(&self) -> moruna_kernel::MemoryBasis {
        self.inner.memory_basis()
    }
    fn sample(&self) -> moruna_kernel::Result<moruna_kernel::Sample> {
        let mut sample = self.inner.sample()?;
        let held = self.held()?;
        sample.anon_bytes = sample.anon_bytes.checked_add(held).ok_or_else(|| {
            moruna_kernel::MorunaError::Config {
                name: "outside_memory",
                msg: "sample byte total overflow".into(),
            }
        })?;
        sample.peak_anon_bytes = sample.peak_anon_bytes.checked_add(held).ok_or_else(|| {
            moruna_kernel::MorunaError::Config {
                name: "outside_memory",
                msg: "peak byte total overflow".into(),
            }
        })?;
        self.peak
            .fetch_max(sample.anon_bytes, std::sync::atomic::Ordering::AcqRel);
        Ok(sample)
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use moruna_kernel::Sample;
    use moruna_testkit::FakeSampler;

    use super::*;

    struct Held(AtomicU64);

    impl OutsideMemory for Held {
        fn in_use(&self) -> moruna_kernel::Result<u64> {
            Ok(self.0.load(Ordering::Acquire))
        }
    }

    /// SB-I9: every sample carries what the children hold, and the peak follows it.
    #[test]
    fn with_outside_adds_the_children() {
        let inner: Arc<dyn Sampler> = Arc::new(FakeSampler::new().scripted(vec![Sample {
            anon_bytes: 100,
            peak_anon_bytes: 100,
            ceiling_bytes: 1000,
            ..Sample::default()
        }]));
        let child = Arc::new(Held(AtomicU64::new(50)));
        let sampler = WithOutside::new(inner, vec![child.clone() as Arc<dyn OutsideMemory>]);
        let s = sampler.sample().unwrap();
        assert_eq!(
            (s.anon_bytes, s.peak_anon_bytes, s.ceiling_bytes),
            (150, 150, 1000)
        );
        child.0.store(400, Ordering::Release);
        assert_eq!(sampler.sample().unwrap().anon_bytes, 500);
        child.0.store(0, Ordering::Release);
        assert_eq!(sampler.sample().unwrap().anon_bytes, 100);
        let peak = sampler.process_peak();
        assert!(peak.bytes >= 500 && !peak.exact, "{peak:?}");
        sampler.reset_peak();
        let _ = sampler.process_usage();
    }

    #[test]
    fn a_wasm_package_binds_and_says_what_it_is() {
        let dir = std::env::temp_dir().join(format!("moruna-packaged-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let m = moruna_wasm::testing::manifest("echo", false, true);
        let bytes = moruna_wasm::testing::component(
            &m.to_json(),
            moruna_wasm::testing::IDENTITY,
            &[],
            "",
            "",
        );
        let package = moruna_wasm::testing::package_at(&dir, &bytes, &m);
        let packaged = open(
            &package.path,
            None,
            &PackageHost::default(),
            Inputs::default(),
        )
        .unwrap();
        assert_eq!(packaged.kind(), "wasm");
        assert!(packaged.outside().is_none());
        packaged.limits_changed();
        packaged.bind_here().unwrap();
        // A .pex with no sandbox host: refused naming what is missing (or, off Linux, Linux).
        let pex = dir.join("x.pex");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&pex).unwrap());
        zip.start_file("moruna.json", zip::write::SimpleFileOptions::default())
            .unwrap();
        std::io::Write::write_all(
            &mut zip,
            serde_json::json!({
                "name": "x", "entry": "function:x", "declaration": {"hints": {}},
                "abi": moruna_kernel::declare::ABI_VERSION, "moruna": "0", "format": "pex",
                "platforms": ["cp314-manylinux_2_28_x86_64"]
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        zip.finish().unwrap();
        let e = open(&pex, None, &PackageHost::default(), Inputs::default())
            .err()
            .expect("refused");
        let text = e.to_string();
        assert!(
            text.contains("--sandbox-python") || text.contains("needs Linux"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn outside_read_failure_is_terminal_evidence_not_a_zero_or_stale_sample() {
        struct Failed;
        impl OutsideMemory for Failed {
            fn in_use(&self) -> moruna_kernel::Result<u64> {
                Err(moruna_kernel::MorunaError::Io {
                    op: "read",
                    target: "memory.current".into(),
                    msg: "TEST-ONLY account unavailable".into(),
                })
            }
        }
        let inner: Arc<dyn Sampler> = Arc::new(FakeSampler::new().scripted(vec![Sample {
            anon_bytes: 100,
            ..Sample::default()
        }]));
        let sampler = WithOutside::new(inner, vec![Arc::new(Failed)]);
        assert!(
            matches!(sampler.sample(),Err(moruna_kernel::MorunaError::Io{target,msg,..})if target=="memory.current"&&msg.contains("account unavailable"))
        );
    }
    #[test]
    fn native_basis_and_child_reservation_are_forwarded_once() {
        struct Charged;
        impl moruna_kernel::Sampler for Charged {
            fn memory_basis(&self) -> moruna_kernel::MemoryBasis {
                moruna_kernel::MemoryBasis::NativeAccountCharge
            }
            fn sample(&self) -> moruna_kernel::Result<moruna_kernel::Sample> {
                Ok(moruna_kernel::Sample {
                    anon_bytes: 40,
                    ..Default::default()
                })
            }
            fn reset_peak(&self) {}
            fn process_peak(&self) -> moruna_kernel::ProcessPeak {
                Default::default()
            }
        }
        struct Child;
        impl moruna_kernel::OutsideMemory for Child {
            fn in_use(&self) -> moruna_kernel::Result<u64> {
                Ok(200)
            }
        }
        let sampler = WithOutside::new(Arc::new(Charged), vec![Arc::new(Child)]);
        assert_eq!(sampler.sample().unwrap().anon_bytes, 240);
        assert_eq!(
            sampler.memory_basis(),
            moruna_kernel::MemoryBasis::NativeAccountCharge
        );
        assert!(!sampler.process_usage().measured);
    }
}
