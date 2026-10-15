//! PK-T13 check_through_the_runtime and PK-T15 aot (16 k), over components in the text format.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use moruna_kernel::arrow::array::Int64Array;
use moruna_kernel::arrow::datatypes::{DataType, Field, Schema};
use moruna_kernel::arrow::ipc::writer::StreamWriter;
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_runtime::check::Verdict;
use moruna_runtime::check::package::check_package;
use moruna_runtime::packaged::{self, PackageHost};
use moruna_wasm::testing::{IDENTITY, component, constant, manifest, package_at};

static SCRATCH: AtomicU64 = AtomicU64::new(0);

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "moruna-runtime-package-{}-{}",
        std::process::id(),
        SCRATCH.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn sha256_hex(bytes: &[u8]) -> String {
    let package = moruna_package::fingerprint_of(bytes);
    moruna_package::hex(&package)
}

#[test]
fn pk_t13_check_through_the_runtime() {
    let dir = scratch();
    let m = manifest("echo", false, true);
    let package = package_at(&dir, &component(&m.to_json(), IDENTITY, &[], "", ""), &m);
    let path = package.path.clone();
    let profiles = dir.join("profiles");
    let report = check_package(&path, 7, Some(profiles.clone()), &PackageHost::default()).unwrap();
    assert_eq!(report.verdict, Verdict::Agreed, "{}", report.summary());
    assert_eq!(report.kind, "wasm");
    let file = std::fs::read(&path).unwrap();
    assert_eq!(report.fingerprint, format!("sha256:{}", sha256_hex(&file)));
    assert_eq!(report.exit_code(), 0);
    assert!(report.notes.iter().any(|n| n.contains("compiled")));
    let row = report.profile.as_ref().expect("a profile row");
    let written = row.path.as_ref().expect("written to the directory");
    assert!(
        written.display().to_string().contains(&sha256_hex(&file)),
        "the row is keyed by the package's fingerprint: {}",
        written.display()
    );
    assert!(report.to_json()["alloc"]["linear"].is_object());
    assert!(report.summary().contains("linear"));

    // A component whose output disagrees with its declaration: refused naming the column.
    let other = RecordBatch::try_new(
        Arc::new(Schema::new(vec![Field::new(
            "other",
            DataType::Int64,
            true,
        )])),
        vec![Arc::new(Int64Array::from(vec![1, 2]))],
    )
    .unwrap();
    let mut ipc = Vec::new();
    let mut w = StreamWriter::try_new(&mut ipc, &other.schema()).unwrap();
    w.write(&other).unwrap();
    w.finish().unwrap();
    drop(w);
    let m2 = manifest("wrong", false, true);
    let wrong = package_at(
        &dir,
        &component(&m2.to_json(), &constant(ipc.len()), &ipc, "", ""),
        &m2,
    );
    let report = check_package(&wrong.path, 0, None, &PackageHost::default()).unwrap();
    assert_eq!(report.verdict, Verdict::Refused);
    let columns: Vec<&str> = report.refusals().iter().map(|(_, d)| d.column()).collect();
    assert!(
        columns.contains(&"other") || columns.contains(&"n"),
        "{columns:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn pk_t2_the_pin_holds_or_the_package_is_refused() {
    let dir = scratch();
    let m = manifest("echo", false, true);
    let package = package_at(&dir, &component(&m.to_json(), IDENTITY, &[], "", ""), &m);
    let pin = package.fingerprint_string();
    let opened = packaged::open(&package.path, Some(&pin), &PackageHost::default()).unwrap();
    assert_eq!(opened.kind(), "wasm");
    assert_eq!(opened.kernel().fingerprint(), package.kernel_fingerprint());
    let e = packaged::open(
        &package.path,
        Some(&format!("sha256:{}", "0".repeat(64))),
        &PackageHost::default(),
    )
    .err()
    .expect("a mismatched pin is refused");
    assert!(e.to_string().contains("the document pins"), "{e}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn pk_t15_aot_then_no_compile() {
    let dir = scratch();
    let m = manifest("echo", false, true);
    let package = package_at(&dir, &component(&m.to_json(), IDENTITY, &[], "", ""), &m);
    let written = moruna_wasm::aot(&package).unwrap();
    assert_eq!(written, dir.join("echo.cwasm"));
    let opened = packaged::open(&package.path, None, &PackageHost::default()).unwrap();
    assert!(opened.notes().is_empty(), "{:?}", opened.notes());
    let _ = std::fs::remove_dir_all(dir);
}
