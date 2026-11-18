//! The C the tests drive, compiled with the host's compilers and linked into the integration
//! tests alone (`cargo::rustc-link-arg-tests`): examples/double.c, its `main` renamed, and
//! tests/c. The library itself holds no C, and a build for another target compiles none.
//!
//! `MORUNA_C_SANITIZE=address` compiles that C with AddressSanitizer and links its runtime into
//! the tests, which then catch a double release or a read of a released batch.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Each source, its compiler, and its own flags.
const SOURCES: [(&str, &str, &[&str]); 4] = [
    (
        "examples/double.c",
        "CC",
        &["-std=c11", "-Dmain=moruna_example_double"],
    ),
    ("tests/c/support.c", "CC", &["-std=c11"]),
    ("tests/c/scenarios.c", "CC", &["-std=c11"]),
    ("tests/c/cplusplus.cpp", "CXX", &["-std=c++11"]),
];

fn main() {
    println!("cargo::rerun-if-changed=moruna_function.h");
    println!("cargo::rerun-if-changed=tests/c/support.h");
    println!("cargo::rerun-if-env-changed=MORUNA_C_SANITIZE");
    println!("cargo::rerun-if-env-changed=CC");
    println!("cargo::rerun-if-env-changed=CXX");
    let (target, host) = (env::var("TARGET"), env::var("HOST"));
    if target.is_err() || target != host {
        return;
    }
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let sanitize = env::var("MORUNA_C_SANITIZE").ok().filter(|s| !s.is_empty());
    // On a Mac, the macOS the Rust side links for (rustc's default for the target, or
    // MACOSX_DEPLOYMENT_TARGET), so the linker takes the C objects without a warning.
    println!("cargo::rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");
    let macos = target
        .as_deref()
        .is_ok_and(|t| t.ends_with("apple-darwin"))
        .then(|| {
            let default = if target.as_deref().is_ok_and(|t| t.starts_with("aarch64")) {
                "11.0"
            } else {
                "10.12"
            };
            let v = env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| String::from(default));
            format!("-mmacosx-version-min={v}")
        });
    for (source, compiler, flags) in SOURCES {
        println!("cargo::rerun-if-changed={source}");
        let default = if compiler == "CC" { "cc" } else { "c++" };
        let compiler = env::var(compiler).unwrap_or_else(|_| String::from(default));
        let name = Path::new(source).file_name().expect("a file name");
        let object = out.join(name).with_extension("o");
        let mut cc = Command::new(&compiler);
        cc.args(flags)
            .args([
                "-Wall",
                "-Wextra",
                "-Wpedantic",
                "-Werror",
                "-g",
                "-O1",
                "-fPIC",
            ])
            .arg("-I")
            .arg(&root)
            .arg("-c")
            .arg(root.join(source))
            .arg("-o")
            .arg(&object)
            .args(&macos);
        if let Some(s) = &sanitize {
            cc.arg(format!("-fsanitize={s}"))
                .arg("-fno-omit-frame-pointer");
        }
        let status = cc
            .status()
            .unwrap_or_else(|e| panic!("{compiler} could not run: {e}"));
        assert!(status.success(), "{compiler} failed on {source}: {status}");
        println!("cargo::rustc-link-arg-tests={}", object.display());
    }
    if sanitize.is_some() {
        link_asan(target.as_deref().unwrap_or_default());
    }
}

/// Links AddressSanitizer's runtime into the tests by its path: rustc links with
/// `-nodefaultlibs`, under which `-fsanitize=address` adds no runtime.
fn link_asan(target: &str) {
    assert_eq!(
        env::var("MORUNA_C_SANITIZE").as_deref(),
        Ok("address"),
        "MORUNA_C_SANITIZE takes `address` only"
    );
    let name = if target.ends_with("apple-darwin") {
        "libclang_rt.asan_osx_dynamic.dylib"
    } else {
        "libasan.so"
    };
    let compiler = env::var("CC").unwrap_or_else(|_| String::from("cc"));
    let printed = Command::new(&compiler)
        .arg(format!("-print-file-name={name}"))
        .output()
        .unwrap_or_else(|e| panic!("{compiler} could not run: {e}"));
    let runtime = PathBuf::from(String::from_utf8_lossy(&printed.stdout).trim());
    assert!(runtime.is_absolute(), "{compiler} has no {name}");
    let dir = runtime.parent().expect("the runtime's directory");
    println!("cargo::rustc-link-arg-tests={}", runtime.display());
    println!("cargo::rustc-link-arg-tests=-Wl,-rpath,{}", dir.display());
}
