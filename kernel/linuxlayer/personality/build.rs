// SPDX-License-Identifier: MPL-2.0

// The image links inside the personality's region (personality.ld), far above where a
// Linux program links, as a static executable the core's loader takes.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("none") {
        let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        println!("cargo:rustc-link-arg-bins=-T{dir}/personality.ld");
        println!("cargo:rustc-link-arg-bins=-static");
        // The code is position-independent (built with `-C relocation-model=pie`, so it
        // reaches the region's addresses), but the image is linked at its address.
        println!("cargo:rustc-link-arg-bins=--no-pie");
        println!("cargo:rerun-if-changed=personality.ld");
    }
}
