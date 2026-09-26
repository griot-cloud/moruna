//! The guest recipe's OCI assembler (guest/oci.py) and the monitor's image resolver agree on
//! the layout: what the release publishes is what `moruna-vmm boot --image` reads.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn vm_t32_recipe_layout_resolves() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let script = root.join("guest/oci.py");
    let dir = std::env::temp_dir().join(format!("mvmm-oci-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (name, body) in [("k", "kernel"), ("i", "initramfs"), ("r", "rootfs")] {
        std::fs::write(dir.join(name), body).unwrap();
    }
    let arch = moruna_vmm::image::OCI_ARCH;
    let out = dir.join("layout");
    let run = |out: &PathBuf| {
        Command::new("python3")
            .arg(&script)
            .arg(out)
            .arg(arch)
            .args([dir.join("k"), dir.join("i"), dir.join("r")])
            .output()
    };
    let Ok(first) = run(&out) else {
        eprintln!("python3 is not installed; the recipe check is not run");
        return;
    };
    assert!(first.status.success(), "{first:?}");
    let digest = String::from_utf8(first.stdout).unwrap().trim().to_string();
    let img = moruna_vmm::image::resolve(&out).unwrap();
    assert_eq!(img.digest.as_deref(), Some(digest.as_str()));
    assert_eq!(std::fs::read(&img.kernel).unwrap(), b"kernel");
    assert_eq!(std::fs::read(img.initramfs.unwrap()).unwrap(), b"initramfs");
    assert_eq!(std::fs::read(img.rootfs.unwrap()).unwrap(), b"rootfs");
    // Reproducible: the same inputs give the same digest.
    let again = run(&dir.join("layout2")).unwrap();
    assert_eq!(String::from_utf8(again.stdout).unwrap().trim(), digest);
    // Bad arguments are refused.
    let bad = Command::new("python3")
        .arg(&script)
        .arg("x")
        .output()
        .unwrap();
    assert_eq!(bad.status.code(), Some(2));
    let _ = std::fs::remove_dir_all(&dir);
}
