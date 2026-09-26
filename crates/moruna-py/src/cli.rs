//! `moruna` as a command, with the Python adapter compiled in (MH 4.2, 12 f.8).
//!
//! Two ways in, one function: the `moruna` binary of this crate (`src/bin/moruna.rs`), which
//! embeds an interpreter, and `python -m moruna` or the `moruna` console script, which call
//! `moruna._core.main` from an interpreter already running. Both run the facade's command line
//! ([`moruna_runtime::host::cli`]) with [`ModuleLoader`], which imports the module a kernel entry
//! names and finds its callable, and both keep the thread roles of 12 f.5: the command runs on a
//! helper thread and the main thread only polls for signals, so Ctrl-C cancels the run and the
//! process still writes its report and exits 130.

use std::sync::Arc;
use std::time::Duration;

use moruna_kernel::{CancelToken, Kernel};
use moruna_runtime::host::{cli, exit};
use moruna_runtime::job::{KernelDoc, KernelLoader, LoadedKernel, SpecError};
use pyo3::exceptions::PyKeyboardInterrupt;
use pyo3::prelude::*;
use pyo3::types::PyAny;

use crate::kernel::{PyKernelHandle, build};

/// Imports the kernels a document names (MH 4.2).
pub struct ModuleLoader;

impl KernelLoader for ModuleLoader {
    fn load(&self, index: usize, doc: &KernelDoc) -> moruna_kernel::Result<LoadedKernel> {
        let field = |name: &str| format!("kernels[{index}].{name}");
        let module = doc
            .module
            .as_deref()
            .filter(|m| !m.is_empty())
            .ok_or_else(|| SpecError::new(field("module"), "a Python kernel names its module"))?;
        let callable = doc
            .callable
            .as_deref()
            .filter(|c| !c.is_empty())
            .ok_or_else(|| {
                SpecError::new(field("callable"), "a Python kernel names its callable")
            })?;
        Python::attach(|py| {
            let object = import(py, module)
                .map_err(|e| SpecError::new(field("module"), format!("`{module}`: {e}")))?;
            let mut target = object;
            for part in callable.split('.') {
                target = target.getattr(part).map_err(|e| {
                    SpecError::new(
                        field("callable"),
                        format!("`{callable}` in `{module}`: {e}"),
                    )
                })?;
            }
            if let Ok(handle) = target.cast::<PyKernelHandle>() {
                let set = doc.hints_set();
                if !set.is_empty() {
                    return Err(SpecError::new(
                        field(set[0]),
                        format!(
                            "`{callable}` is already decorated with @moruna.kernel, whose \
                             arguments are its hints; the document sets {}",
                            set.join(", ")
                        ),
                    )
                    .into());
                }
                let kernel = Arc::clone(&handle.get().kernel);
                return Ok(LoadedKernel {
                    kernel: Arc::clone(&kernel) as Arc<dyn Kernel>,
                    python: Some(kernel),
                });
            }
            let handle = build(
                py,
                &target,
                doc.stateful.unwrap_or(false),
                usize::from(doc.instances.unwrap_or(1)),
                doc.device_memory.unwrap_or(false),
                doc.accepts.as_deref().unwrap_or("table"),
                doc.tier.as_deref().unwrap_or("host"),
                doc.releases_gil,
                doc.expected_amplification,
                doc.preferred_rows,
                doc.resume.as_deref().unwrap_or("reinit"),
                doc.state_bytes,
            )
            .map_err(|e| SpecError::new(format!("kernels[{index}]"), e.to_string()))?;
            let kernel = handle.kernel;
            Ok(LoadedKernel {
                kernel: Arc::clone(&kernel) as Arc<dyn Kernel>,
                python: Some(kernel),
            })
        })
    }
}

/// A module by name, or a `.py` file by path, imported once per path.
fn import<'py>(py: Python<'py>, module: &str) -> PyResult<Bound<'py, PyAny>> {
    let is_file = module.ends_with(".py") || module.contains('/');
    if !is_file {
        return Ok(py.import(module)?.into_any());
    }
    let sys_modules = py.import("sys")?.getattr("modules")?;
    let name: String = format!(
        "_moruna_job_{}",
        module
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect::<String>()
    );
    if let Ok(loaded) = sys_modules.get_item(&name) {
        return Ok(loaded);
    }
    let util = py.import("importlib.util")?;
    let spec = util.call_method1("spec_from_file_location", (&name, module))?;
    if spec.is_none() {
        return Err(pyo3::exceptions::PyImportError::new_err(format!(
            "`{module}` is not a Python file"
        )));
    }
    let loaded = util.call_method1("module_from_spec", (&spec,))?;
    sys_modules.set_item(&name, &loaded)?;
    if let Err(e) = spec
        .getattr("loader")?
        .call_method1("exec_module", (&loaded,))
    {
        let _ = sys_modules.del_item(&name);
        return Err(e);
    }
    Ok(loaded)
}

/// How often the main thread wakes to check for a signal (12 f.5).
const SIGNAL_POLL: Duration = Duration::from_millis(100);

/// Run a command line on a helper thread while this thread, which must be attached to the
/// interpreter, polls for signals; a `KeyboardInterrupt` cancels the run (12 f.5).
pub fn run_command(py: Python<'_>, args: Vec<String>) -> i32 {
    let _guard = match crate::run::RunningGuard::acquire() {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("moruna: {error}");
            return exit::FAILED;
        }
    };
    let cancel = CancelToken::new();
    let thread_cancel = cancel.clone();
    let handle = match std::thread::Builder::new()
        .name("moruna-command".to_string())
        .spawn(move || cli::main(&args, &ModuleLoader, thread_cancel))
    {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("moruna: no thread for the command: {error}");
            return exit::FAILED;
        }
    };
    let mut interrupted = false;
    while !handle.is_finished() {
        if let Err(e) = Python::check_signals(py) {
            if e.is_instance_of::<PyKeyboardInterrupt>(py) && !interrupted {
                interrupted = true;
                cancel.cancel();
                eprintln!("moruna: cancelling the run");
            } else if !e.is_instance_of::<PyKeyboardInterrupt>(py) {
                cancel.cancel();
            }
        }
        py.detach(|| std::thread::sleep(SIGNAL_POLL));
    }
    match py.detach(|| handle.join()) {
        Ok(code) => code,
        Err(_) => {
            eprintln!("moruna: the command panicked");
            exit::FAILED
        }
    }
}

/// `moruna._core.main(argv)`: the console script's entry (`python -m moruna`, `moruna`).
#[pyfunction]
pub fn main(py: Python<'_>, argv: Vec<String>) -> i32 {
    run_command(py, argv)
}

/// Register [`main`] on the module.
pub fn register(m: &Bound<'_, pyo3::types::PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(main, m)?)?;
    Ok(())
}
