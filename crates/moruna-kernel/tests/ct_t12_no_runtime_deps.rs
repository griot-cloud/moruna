//! CT-T12 no_runtime_deps: `cargo tree -p moruna-kernel` contains none of tokio, cudarc, pyo3,
//! parquet or object_store. Proves the boundary in section a and S7.

use std::process::Command;

/// The crates a kernel author's dependency on `moruna-kernel` must never pull in (section a).
const FORBIDDEN: [&str; 5] = ["tokio", "cudarc", "pyo3", "parquet", "object_store"];

#[test]
fn ct_t12_no_runtime_deps() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(cargo)
        .args([
            "tree",
            "-p",
            "moruna-kernel",
            "--edges",
            "normal",
            "--prefix",
            "none",
        ])
        .current_dir(root)
        .output()
        .expect("cargo tree runs");
    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tree = String::from_utf8_lossy(&output.stdout);
    assert!(
        tree.contains("moruna-kernel"),
        "cargo tree printed no tree:\n{tree}"
    );

    // Each line is "<name> v<version>"; a forbidden crate anywhere in the tree fails.
    let names: Vec<&str> = tree
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| !name.is_empty())
        .collect();
    for forbidden in FORBIDDEN {
        assert!(
            !names.contains(&forbidden),
            "moruna-kernel must not depend on {forbidden} (01 section a, CT-T12):\n{tree}"
        );
    }

    // The dependencies it does have are the four the preamble's table allows (6.1, 6.2).
    for expected in ["arrow", "dlpark", "thiserror", "blake3"] {
        assert!(names.contains(&expected), "{expected} is missing:\n{tree}");
    }
}
