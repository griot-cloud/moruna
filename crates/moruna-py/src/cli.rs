//! `moruna` as a command, with the Python adapter compiled in (MH 4.2, 12 f.8).
//!
//! Two ways in, one function: the `moruna` binary of this crate (`src/bin/moruna.rs`), which
//! embeds an interpreter, and `python -m moruna` or the `moruna` console script, which call
//! `moruna._core.main` from an interpreter already running. Both run the facade's command line
//! ([`moruna_runtime::host::cli`]) with the runtime's kinds and the command's `python` kind
//! ([`crate::kinds::Imported`], which imports the module a kernel entry names and finds its
//! callable), and both keep the thread roles of 12 f.5: the command runs on a helper thread and
//! the main thread only polls for signals, so Ctrl-C cancels the run and the process still
//! writes its report and exits 130.

use std::time::Duration;

use moruna_kernel::CancelToken;
use moruna_runtime::host::{cli, exit};
use pyo3::exceptions::PyKeyboardInterrupt;
use pyo3::prelude::*;

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
    let kinds = match crate::kinds::command() {
        Ok(kinds) => kinds,
        Err(error) => {
            eprintln!("moruna: {error}");
            return exit::FAILED;
        }
    };
    // The command's process is the run's: what a Python kernel prints is its stage's.
    crate::output::install(py);
    let cancel = CancelToken::new();
    let thread_cancel = cancel.clone();
    let handle = match std::thread::Builder::new()
        .name("moruna-command".to_string())
        .spawn(move || cli::main(&args, &kinds, thread_cancel))
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
