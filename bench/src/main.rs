//! The `amoru-bench` binary: parse the command line, run it, print the error and
//! exit non zero if it fails (preamble 6.7 leaves `main` that one liberty). The
//! body lives in the library, where it is tested.

fn main() -> std::process::ExitCode {
    amoru_bench::install_tracing();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = amoru_bench::main_with(
        &args,
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
    );
    std::process::ExitCode::from(code)
}
