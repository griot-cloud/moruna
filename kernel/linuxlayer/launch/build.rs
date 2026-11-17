// SPDX-License-Identifier: MPL-2.0

// The booted test program links as every Moruna user program does (kernel/sys/user.ld).
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("none") {
        let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        println!("cargo:rustc-link-arg-bins=-T{dir}/../../sys/user.ld");
        println!("cargo:rustc-link-arg-bins=-static");
        println!("cargo:rerun-if-changed=../../sys/user.ld");
    }
}
