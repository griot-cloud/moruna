//! `moruna-engine` (SDD 30 d.4): the engine's command line, `moruna run` and `moruna serve`
//! (MH 4.2), with the standard kinds and no embedded interpreter. It is what runs on the Moruna
//! kernel, a static Linux program the personality starts with granted handles, where a Python
//! stage is an `image` whose interpreter is in the image; the `moruna` binary of `moruna-py` is
//! the same command line with the Python adapter compiled in.

/// The engine's Rust heap: counted while a warm-up runs, and its large buffers kept for reuse
/// while a run lasts (12 l), the same allocator the `moruna` binary and the Python extension
/// install, so the engine behaves the same on a host, in its own VM and on the kernel.
#[global_allocator]
static ALLOCATOR: moruna_runtime::check::warmup::CountingAllocator =
    moruna_runtime::check::warmup::CountingAllocator;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let kinds = moruna_runtime::kinds::Registry::standard();
    let cancel = moruna_kernel::CancelToken::new();
    std::process::exit(moruna_runtime::host::cli::main(&args, &kinds, cancel));
}
