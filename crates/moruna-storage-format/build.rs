//! Enforce the actual dependency-wide upstream zeroizing POLYVAL backend selection.
mod build_policy;
fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_CRYPTO");
    println!("cargo:rerun-if-env-changed=CARGO_ENCODED_RUSTFLAGS");
    if std::env::var_os("CARGO_FEATURE_CRYPTO").is_none() {
        return;
    }
    let flags = std::env::var("CARGO_ENCODED_RUSTFLAGS").unwrap_or_default();
    if !build_policy::allows(&flags) {
        panic!(
            "SDD32 crypto requires dependency-wide --cfg polyval_force_soft in Cargo global rustflags; pinned polyval 0.6.2 autodetect/PMULL do not zeroize retained keyed state. Preserve .cargo/config.toml or include this exact cfg in overriding RUSTFLAGS/CARGO_ENCODED_RUSTFLAGS; a per-crate rustc argument does not select the dependency backend."
        );
    }
}
