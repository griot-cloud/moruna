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

mod support;

use moruna_runtime::job::{BuildOptions, JobSpec, NoKernels, build};
use moruna_runtime::{CancelToken, Runtime};
use serde_json::json;
use support::{Scratch, one_run_at_a_time, read_back_rows, write_parquet};

fn no_env(_: &str) -> Option<String> {
    None
}

/// A hosted document (strict, no environment) over `rows` rows with one package stage.
fn package_document(scratch: &std::path::Path, rows: u64, package: &str, pin: &str) -> JobSpec {
    let input = scratch.join("in.parquet");
    write_parquet(&input, rows, 4);
    let out = scratch.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let staging = scratch.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    JobSpec::from_value(json!({
        "moruna_spec": 1,
        "source": {"kind": "parquet", "url": format!("file://{}", input.display())},
        "kernels": [{"kind": "package", "path": package, "fingerprint": pin}],
        "sink": {"kind": "parquet", "url": format!("file://{}", out.display())},
        "budget": {"memory_bytes": 1u64 << 30, "cpu": 2.0},
        "staging": {"dir": staging.to_string_lossy(), "limit_bytes": 1u64 << 30},
        "profiles_dir": scratch.join("profiles").to_string_lossy(),
    }))
    .unwrap()
}

fn run_document(job: &JobSpec, host: PackageHost) -> moruna_runtime::RunReport {
    let built = build(
        job,
        &NoKernels,
        BuildOptions {
            strict: true,
            env: &no_env,
            notes: Vec::new(),
            packages: host,
        },
    )
    .expect("the document builds");
    Runtime::run(built.spec, CancelToken::new()).expect("the run completes")
}

/// PK-T16 run_a_package (16 f.8): a hosted document with a `.wasm` stage writes every row of
/// the source, and the report's stage is the package's fingerprint.
#[test]
fn pk_t16_run_a_wasm_package() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("pk_t16_wasm");
    let m = manifest("echo", false, true);
    let package = package_at(
        scratch.path(),
        &component(&m.to_json(), IDENTITY, &[], "", ""),
        &m,
    );
    let rows = 20_000;
    let job = package_document(
        scratch.path(),
        rows,
        &package.path.display().to_string(),
        &package.fingerprint_string(),
    );
    let report = run_document(&job, PackageHost::default());
    assert_eq!(read_back_rows(&scratch.path().join("out")), rows);
    let text = report.to_json();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(
        text.contains("compiled"),
        "the compile note is in the report"
    );
    let stage = json["stages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["stage"] == 1)
        .expect("the package's stage");
    assert_eq!(stage["rows_in"], rows);
    assert!(stage["alloc"]["linear"].is_object(), "{stage}");
}

/// PK-T16 for a `.pex` (Linux, sandbox host): the same through the confined child, the child's
/// memory in the run's samples.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "Linux, sandbox host: MORUNA_SANDBOX_PYTHON and MORUNA_SANDBOX_CGROUP"]
fn pk_t16_run_a_pex_package() {
    use std::io::Write;
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("pk_t16_pex");
    let arch = std::env::consts::ARCH;
    let manifest = json!({
        "name": "doubled",
        "entry": "function:doubled",
        "declaration": {"input_schema": {"id": "int64", "value": "int64"},
                        "output_schema": {"adds": {"twice": "int64"}}, "hints": {}},
        "abi": moruna_kernel::declare::ABI_VERSION,
        "moruna": moruna_package::MORUNA_VERSION,
        "format": "pex",
        "platforms": [format!("cp314-manylinux_2_28_{arch}")],
    });
    let path = scratch.path().join("doubled.pex");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("moruna.json", opts).unwrap();
    zip.write_all(manifest.to_string().as_bytes()).unwrap();
    zip.start_file("src/function.py", opts).unwrap();
    zip.write_all(
        b"import pyarrow.compute as pc\n\ndef doubled(batch):\n    return batch.append_column(\"twice\", pc.multiply(batch.column(\"value\"), 2))\n",
    )
    .unwrap();
    zip.finish().unwrap();
    let pin = format!("sha256:{}", sha256_hex(&std::fs::read(&path).unwrap()));
    let rows = 50_000;
    let job = package_document(scratch.path(), rows, &path.display().to_string(), &pin);
    let host = PackageHost {
        sandbox_python: std::env::var("MORUNA_SANDBOX_PYTHON")
            .ok()
            .map(PathBuf::from),
        sandbox_cgroup: std::env::var("MORUNA_SANDBOX_CGROUP")
            .ok()
            .map(PathBuf::from),
    };
    let report = run_document(&job, host);
    assert_eq!(read_back_rows(&scratch.path().join("out")), rows);
    assert!(support::output_has_column(
        &scratch.path().join("out"),
        "twice"
    ));
    let json: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();
    let stage = json["stages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["stage"] == 1)
        .expect("the package's stage");
    assert_eq!(stage["rows_in"], rows);
    assert!(
        stage["alloc"]["arrow"]["requests"].as_u64().unwrap() > 0,
        "{stage}"
    );
}

/// PK-T14 check_a_pex (Linux, sandbox host): a packaged Polars kernel agrees through its child,
/// and the allocation figures are the child's.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "Linux, sandbox host: MORUNA_SANDBOX_PYTHON and MORUNA_SANDBOX_CGROUP"]
fn pk_t14_check_a_pex() {
    use std::io::Write;
    let scratch = Scratch::new("pk_t14");
    let arch = std::env::consts::ARCH;
    let manifest = json!({
        "name": "shout",
        "entry": "function:shout",
        "declaration": {"input_schema": {"n": "int64", "s": "string"},
                        "output_schema": {"adds": {"loud": "string"}}, "hints": {}},
        "abi": moruna_kernel::declare::ABI_VERSION,
        "moruna": moruna_package::MORUNA_VERSION,
        "format": "pex",
        "platforms": [format!("cp314-manylinux_2_28_{arch}")],
    });
    let path = scratch.path().join("shout.pex");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("moruna.json", opts).unwrap();
    zip.write_all(manifest.to_string().as_bytes()).unwrap();
    zip.start_file("src/function.py", opts).unwrap();
    zip.write_all(
        b"import polars as pl\n\ndef shout(df: pl.DataFrame) -> pl.DataFrame:\n    return df.with_columns(pl.col(\"s\").str.to_uppercase().alias(\"loud\"))\n",
    )
    .unwrap();
    zip.finish().unwrap();
    let host = PackageHost {
        sandbox_python: std::env::var("MORUNA_SANDBOX_PYTHON")
            .ok()
            .map(PathBuf::from),
        sandbox_cgroup: std::env::var("MORUNA_SANDBOX_CGROUP")
            .ok()
            .map(PathBuf::from),
    };
    let report = check_package(&path, 0, None, &host).unwrap();
    assert_eq!(report.verdict, Verdict::Agreed, "{}", report.summary());
    assert_eq!(report.kind, "pex");
    let json = report.to_json();
    assert!(
        json["alloc"]["python"]["requests"].as_u64().unwrap() > 0,
        "{json}"
    );
    assert_eq!(
        report.fingerprint,
        format!("sha256:{}", sha256_hex(&std::fs::read(&path).unwrap()))
    );
}
