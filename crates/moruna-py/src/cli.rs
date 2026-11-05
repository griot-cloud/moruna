//! `moruna` as a command, with the Python adapter compiled in (MH 4.2, 12 f.8).
//!
//! Two ways in, one function: the `moruna` binary of this crate (`src/bin/moruna.rs`), which
//! embeds an interpreter, and `python -m moruna` or the `moruna` console script, which call
//! `moruna._core.main` from an interpreter already running. Both run the facade's command line
//! ([`moruna_runtime::host::cli`]) with the runtime's kinds and the command's `python` kinds
//! ([`crate::kinds::Imported`], which import the code a task names, construct it and supply its
//! needs through the resolver `--resolver <module:attribute>` names), and both keep the thread roles of 12 f.5: the command runs on a helper thread and
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
    let (args, resolver) = match resolver_option(args) {
        Ok(split) => split,
        Err(line) => {
            eprintln!("{line}");
            return exit::SPEC_REFUSED;
        }
    };
    let kinds = match crate::kinds::command(resolver) {
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

/// `--resolver <module:attribute>` taken out of a command line: the resolver that supplies the
/// needs of the `python` tasks a document runs (`moruna.Resolver`, or a callable that returns
/// one). Given twice, or without its value, it is refused.
fn resolver_option(args: Vec<String>) -> Result<(Vec<String>, Option<String>), String> {
    let mut rest = Vec::with_capacity(args.len());
    let mut resolver = None;
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        if arg == "--resolver" {
            let value = it
                .next()
                .filter(|v| !v.starts_with("--"))
                .ok_or("moruna: --resolver needs <module:attribute>")?;
            if resolver.replace(value).is_some() {
                return Err("moruna: --resolver is given twice".into());
            }
        } else {
            rest.push(arg);
        }
    }
    Ok((rest, resolver))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `--resolver` is taken out of the command line wherever it is, once, with its value.
    #[test]
    fn the_resolver_option_is_taken_out() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let (rest, resolver) =
            resolver_option(args(&["run", "--resolver", "host:needs", "job.mjob"])).expect("ok");
        assert_eq!(rest, args(&["run", "job.mjob"]));
        assert_eq!(resolver.as_deref(), Some("host:needs"));
        assert_eq!(
            resolver_option(args(&["run", "job.mjob"])).expect("ok"),
            (args(&["run", "job.mjob"]), None)
        );
        assert!(resolver_option(args(&["run", "job.mjob", "--resolver"])).is_err());
        assert!(
            resolver_option(args(&[
                "run",
                "--resolver",
                "a:b",
                "--resolver",
                "c:d",
                "j"
            ]))
            .is_err()
        );
    }
}
