//! Links every test program as a static executable with the user linker script.

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| String::from("."));
    println!("cargo:rustc-link-arg-bins=-T{dir}/user.ld");
    println!("cargo:rustc-link-arg-bins=-static");
    println!("cargo:rerun-if-changed=user.ld");
}
