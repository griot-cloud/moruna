//! `moruna-vmm`: boot the Moruna guest, or resize, inspect or stop a running one.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = moruna_vmm::cli::run(&args, &mut std::io::stdout(), &mut std::io::stderr());
    std::process::exit(code);
}
