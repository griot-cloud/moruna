//! Links the kernel with OSDK's x86_64 linker script (see the header of x86_64.ld).

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| String::from("."));
    println!("cargo:rustc-link-arg-bins=-T{dir}/x86_64.ld");
    println!("cargo:rerun-if-changed=x86_64.ld");
}
