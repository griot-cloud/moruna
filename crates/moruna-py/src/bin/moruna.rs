//! The `moruna` binary (MH 4.2): the `moruna` command (`moruna run JOB`, `moruna serve
//! --listen`, ...) with an embedded interpreter, which runs the package's command line
//! (`moruna.__main__.main`) exactly as the console script does.
//!
//! The binary registers its own `moruna._core` before anything imports `moruna`, so a kernel
//! module that says `import moruna` and decorates with `@moruna.kernel` gets this binary's
//! kernel type and not a second copy from an installed wheel, which the loader could not use.
//! The `moruna` package's Python files must be importable (an installed wheel, or `PYTHONPATH`).
//!
//! `moruna node --url <https://...> --token <token> [--pin <hex>] [--audit]` (system design 2E)
//! reads a node's document, or its audit, over the node's API. The API client answers it before
//! the interpreter starts, since it needs none.

use pyo3::prelude::*;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|command| command == "node") {
        let code = moruna_api_client::cli::node(
            &args[1..],
            &mut std::io::stdout().lock(),
            &mut std::io::stderr().lock(),
        );
        std::process::exit(code);
    }
    Python::initialize();
    let code = Python::attach(|py| {
        if let Err(error) = prepare(py) {
            eprintln!("moruna: the embedded interpreter could not be prepared: {error}");
            return moruna_runtime::host::exit::FAILED;
        }
        match py
            .import("moruna.__main__")
            .and_then(|m| m.getattr("main"))
            .and_then(|main| main.call1((args,)))
            .and_then(|code| code.extract::<i32>())
        {
            Ok(code) => code,
            Err(error) => {
                eprintln!("moruna: {error}");
                moruna_runtime::host::exit::FAILED
            }
        }
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
