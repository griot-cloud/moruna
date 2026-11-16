// SPDX-License-Identifier: MPL-2.0

// The server links as every Moruna program does (kernel/sys/user.ld), with the Linux
// personality's image in it: LINUXLAYER_PERSONALITY names the built image
// (kernel/tools/build.sh builds it first).
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("none") {
        let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        println!("cargo:rustc-link-arg-bins=-T{dir}/../../sys/user.ld");
        println!("cargo:rustc-link-arg-bins=-static");
        println!("cargo:rerun-if-changed=../../sys/user.ld");
        println!("cargo:rerun-if-env-changed=LINUXLAYER_PERSONALITY");
        let image = std::env::var("LINUXLAYER_PERSONALITY")
            .expect("LINUXLAYER_PERSONALITY names the personality's built image");
        println!("cargo:rerun-if-changed={image}");
    }
}
