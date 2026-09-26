//! The `moruna` binary (MH 4.2): `moruna run <spec.json>` and `moruna serve --listen`,
//! with an embedded interpreter so a document's Python kernels can be imported.
//!
//! The binary registers its own `moruna._core` before anything imports `moruna`, so a kernel
//! module that says `import moruna` and decorates with `@moruna.kernel` gets this binary's
//! kernel type and not a second copy from an installed wheel, which the loader could not use.
//! The `moruna` package's Python files must be importable (an installed wheel, or `PYTHONPATH`).

use pyo3::prelude::*;

fn main() {
    Python::initialize();
    let code = Python::attach(|py| {
        if let Err(error) = prepare(py) {
            eprintln!("moruna: the embedded interpreter could not be prepared: {error}");
            return moruna_runtime::host::exit::FAILED;
        }
        _core::cli::run_command(py, std::env::args().skip(1).collect())
    });
    std::process::exit(code);
}

/// `moruna._core` is this binary's module, and SIGINT raises `KeyboardInterrupt` on this, the
/// main thread (12 f.5); `Python::initialize` leaves signal handling off.
fn prepare(py: Python<'_>) -> PyResult<()> {
    let module = pyo3::wrap_pymodule!(_core::module::_core)(py);
    py.import("sys")?
        .getattr("modules")?
        .set_item("moruna._core", module)?;
    let signal = py.import("signal")?;
    signal.call_method1(
        "signal",
        (
            signal.getattr("SIGINT")?,
            signal.getattr("default_int_handler")?,
        ),
    )?;
    Ok(())
}
