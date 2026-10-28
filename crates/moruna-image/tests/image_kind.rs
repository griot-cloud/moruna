//! IM-T17 to IM-T24 (SDD 19 k): the `image` kind for real, on Linux. A program (`arrow_echo`) is
//! packed into an OCI layout and run as a stage between a Parquet source and a Parquet sink,
//! through the sandbox: user, mount, pid, network, ipc, uts and cgroup namespaces, no capabilities,
//! Landlock and seccomp.
//!
//! Each test is tagged and runs with `--ignored` where a sandbox host exists: `root` or a user
//! allowed to make namespaces, and `MORUNA_SANDBOX_CGROUP`, a cgroup v2 directory with `memory`
//! and `cpu` delegated (test configuration, never the runtime's: SDD 17 e.6).
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use arrow::array::{Array, ArrayRef, Int64Array, RecordBatch};
use arrow::datatypes::{DataType, Field, Schema};
use moruna_image::testing::{Entry, LayoutBuilder, Scratch, media};
use moruna_runtime::job::{BuildOptions, JobSpec, build};
use moruna_runtime::kinds::conformance::{self, Harness};
use moruna_runtime::kinds::{Context, Registry, Standalone, image};
use moruna_runtime::packaged::PackageHost;
use moruna_runtime::{CancelToken, Runtime};
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use serde_json::{Value, json};

const TAG: &str = "Linux, sandbox host: root, and MORUNA_SANDBOX_CGROUP";
const ROWS: i64 = 20_000;

/// Runs that build a real arena one at a time (the runtime's own tests do the same).
static RUNS: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn cgroup() -> PathBuf {
    PathBuf::from(std::env::var("MORUNA_SANDBOX_CGROUP").expect(TAG))
}

fn no_env(_: &str) -> Option<String> {
    None
}

/// The libraries and loader `binary` needs, by absolute path, from `ldd`.
fn libraries(binary: &Path) -> Vec<PathBuf> {
    let out = Command::new("ldd").arg(binary).output().expect("ldd runs");
    let text = String::from_utf8_lossy(&out.stdout);
    let mut found = Vec::new();
    for line in text.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        let path = match words.as_slice() {
            [_, "=>", path, ..] if path.starts_with('/') => *path,
            [path, ..] if path.starts_with('/') => *path,
            _ => continue,
        };
        found.push(PathBuf::from(path));
    }
    found
}

fn leak(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

/// A layout holding `arrow_echo` at `/bin/arrow_echo` with what it needs, one gzip layer, named
/// `echo`; its directory.
fn layout(scratch: &Scratch) -> PathBuf {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_arrow_echo"));
    let mut entries = vec![
        Entry::dir("bin"),
        Entry::dir("etc"),
        Entry::File(
            "bin/arrow_echo",
            std::fs::read(&binary).expect("the fixture is built"),
            0o755,
        ),
    ];
    for lib in libraries(&binary) {
        let path = leak(lib.to_string_lossy().trim_start_matches('/').to_string());
        entries.push(Entry::File(
            path,
            std::fs::read(&lib).unwrap_or_else(|e| panic!("{}: {e}", lib.display())),
            0o755,
        ));
    }
    let dir = scratch.path().join("layout");
    LayoutBuilder::new(&dir)
        .config(json!({"Entrypoint": ["/bin/arrow_echo"], "Env": ["PATH=/bin", "FROM_IMAGE=1"]}))
        .layer(media::GZIP, &entries)
        .finish(Some("echo"));
    dir
}

fn write_parquet(path: &Path, rows: i64) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("value", DataType::Int64, false),
    ]));
    let ids: Int64Array = (0..rows).collect::<Vec<i64>>().into();
    let values: Int64Array = (0..rows).map(|v| v * 2).collect::<Vec<i64>>().into();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(ids) as ArrayRef, Arc::new(values) as ArrayRef],
    )
    .expect("a batch");
    let props = parquet::file::properties::WriterProperties::builder()
        .set_max_row_group_row_count(Some((rows as usize).div_ceil(4)))
        .build();
    let mut writer = ArrowWriter::try_new(
        std::fs::File::create(path).expect("the input file"),
        schema,
        Some(props),
    )
    .expect("a writer");
    writer.write(&batch).expect("the batch is written");
    writer.close().expect("the footer is written");
}

/// The document: Parquet in, the image stage with `settings`, Parquet out.
fn document(scratch: &Path, layout: &Path, settings: Value) -> JobSpec {
    let input = scratch.join("in.parquet");
    write_parquet(&input, ROWS);
    let out = scratch.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let staging = scratch.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let mut kernel = json!({"kind": "image", "layout": layout.display().to_string(),
                            "reference": "echo",
                            "cache": scratch.join("images").display().to_string()});
    for (k, v) in settings.as_object().expect("settings are an object") {
        kernel[k] = v.clone();
    }
    JobSpec::from_value(json!({
        "moruna_spec": 1,
        "source": {"kind": "parquet", "url": format!("file://{}", input.display())},
        "kernels": [kernel],
        "sink": {"kind": "parquet", "url": format!("file://{}", out.display())},
        "budget": {"memory_bytes": 1u64 << 30, "cpu": 2.0},
        "staging": {"dir": staging.to_string_lossy(), "limit_bytes": 1u64 << 30},
        "profiles_dir": scratch.join("profiles").to_string_lossy(),
    }))
    .unwrap()
}

/// Run `command`'s arguments (after the image's entrypoint) as the stage; the run's result.
fn run(scratch: &Scratch, args: &[&str]) -> Result<moruna_runtime::RunReport, String> {
    let layout = layout(scratch);
    let mut command = vec!["/bin/arrow_echo"];
    command.extend_from_slice(args);
    run_settings(scratch, &layout, json!({"command": command}))
}

fn run_settings(
    scratch: &Scratch,
    layout: &Path,
    settings: Value,
) -> Result<moruna_runtime::RunReport, String> {
    let _serial = RUNS.lock().unwrap_or_else(|e| e.into_inner());
    let job = document(scratch.path(), layout, settings);
    let built = build(
        &job,
        &Registry::standard(),
        BuildOptions {
            strict: true,
            env: &no_env,
            notes: Vec::new(),
            packages: PackageHost {
                sandbox_python: None,
                sandbox_cgroup: Some(cgroup()),
            },
            host_ports: None,
        },
    )
    .map_err(|e| e.to_string())?;
    Runtime::run(built.spec, CancelToken::new()).map_err(|e| e.to_string())
}

/// What the Parquet files under `out` hold: the `n` and `id` columns, in file order.
fn read_back(out: &Path) -> (Vec<i64>, Vec<i64>) {
    let mut files: Vec<PathBuf> = std::fs::read_dir(out)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("parquet"))
        .collect();
    files.sort();
    let (mut n, mut id) = (Vec::new(), Vec::new());
    for file in files {
        let reader = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(&file).unwrap())
            .unwrap()
            .build()
            .unwrap();
        for batch in reader {
            let batch = batch.unwrap();
            let col = |name: &str| -> Vec<i64> {
                let array = batch
                    .column_by_name(name)
                    .unwrap_or_else(|| panic!("{}: no column `{name}`", file.display()));
                let array = array.as_any().downcast_ref::<Int64Array>().unwrap();
                (0..array.len()).map(|i| array.value(i)).collect()
            };
            n.extend(col("n"));
            id.extend(col("id"));
        }
    }
    (n, id)
}

/// IM-T17 an_image_is_a_stage: every row of the source arrives, each with `n`, the row's index
/// in the batch it travelled in; the stage reports every row in and out.
#[test]
#[ignore = "Linux, sandbox host: root, and MORUNA_SANDBOX_CGROUP"]
fn im_t17_an_image_is_a_stage() {
    let scratch = Scratch::new("im_t17");
    let report = run(&scratch, &[]).unwrap_or_else(|e| panic!("{e}"));
    let (n, id) = read_back(&scratch.path().join("out"));
    assert_eq!(id, (0..ROWS).collect::<Vec<_>>(), "every row, in order");
    assert_eq!(n.len() as i64, ROWS);
    assert_eq!(n[0], 0);
    assert!(
        n.iter().all(|v| *v >= 0 && *v < ROWS),
        "n is an index in a batch"
    );
    let json: Value = serde_json::from_str(&report.to_json()).unwrap();
    let stage = json["stages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["stage"] == 1)
        .expect("the image's stage");
    assert_eq!(stage["rows_in"], ROWS);
    assert_eq!(stage["rows_out"], ROWS);
    // The unpacked root is in the cache the settings named, beside nothing else.
    assert_eq!(
        std::fs::read_dir(scratch.path().join("images"))
            .unwrap()
            .count(),
        1
    );
}

/// IM-T18 a_socket_kills_the_image: opening a TCP socket is a system call the filter does not
/// allow, and the run says so.
#[test]
#[ignore = "Linux, sandbox host: root, and MORUNA_SANDBOX_CGROUP"]
fn im_t18_a_socket_kills_the_image() {
    let scratch = Scratch::new("im_t18");
    let e = run(&scratch, &["--socket"]).expect_err("the run fails");
    assert!(
        e.contains("made a system call it is not allowed"),
        "the sentence of SDD 19 f.7: {e}"
    );
    assert!(e.contains("SIGSYS"), "{e}");
}

/// IM-T19 the_root_is_read_only: writing under `/etc` fails, the process says so on its standard
/// error and exits with status 1; the run quotes both.
#[test]
#[ignore = "Linux, sandbox host: root, and MORUNA_SANDBOX_CGROUP"]
fn im_t19_the_root_is_read_only() {
    let scratch = Scratch::new("im_t19");
    let e = run(&scratch, &["--write-root"]).expect_err("the run fails");
    assert!(e.contains("exited with status 1"), "{e}");
    assert!(
        e.contains("creating /etc/x"),
        "its standard error is quoted: {e}"
    );
    assert!(
        e.contains("Permission denied") || e.contains("Read-only file system"),
        "/etc exists in the image and is not the process's to write: {e}"
    );
    assert!(
        !e.contains("writing /tmp/arrow_echo"),
        "/tmp was writable, so the failure is /etc's: {e}"
    );
}

/// IM-T20 output_that_is_not_arrow: bytes that are no stream end the run with the reason.
#[test]
#[ignore = "Linux, sandbox host: root, and MORUNA_SANDBOX_CGROUP"]
fn im_t20_output_that_is_not_arrow() {
    let scratch = Scratch::new("im_t20");
    let e = run(&scratch, &["--garbage"]).expect_err("the run fails");
    assert!(e.contains("is not an Arrow IPC stream"), "{e}");
}

/// IM-T21 a_nonzero_exit_is_named: a process that exits with status 3 where it should have ended
/// its stream ends the run naming 3, though every batch was answered.
#[test]
#[ignore = "Linux, sandbox host: root, and MORUNA_SANDBOX_CGROUP"]
fn im_t21_a_nonzero_exit_is_named() {
    let scratch = Scratch::new("im_t21");
    let e = run(&scratch, &["--exit", "3"]).expect_err("the run fails");
    assert!(e.contains("exited with status 3"), "{e}");
}

/// IM-T22 the_kind_passes_its_suite: the runnable-kind suite over a real layout.
#[test]
#[ignore = "Linux, sandbox host: root, and MORUNA_SANDBOX_CGROUP"]
fn im_t22_the_kind_passes_its_suite() {
    let scratch = Scratch::new("im_t22");
    let layout = layout(&scratch);
    let host = PackageHost {
        sandbox_python: None,
        sandbox_cgroup: Some(cgroup()),
    };
    let harness = Harness::new()
        .context(move || Context::new(Arc::new(Standalone)).packages(host.clone()))
        .examples(vec![json!({
            "layout": layout.display().to_string(),
            "reference": "echo",
            "cache": scratch.path().join("images").display().to_string(),
            "command": ["/bin/arrow_echo"],
            "env": {"A": "1"},
        })]);
    let passed = conformance::kernel(&image::Image, &harness).unwrap_or_else(|v| panic!("{v}"));
    assert!(passed.cases > 4, "{passed:?}");
}

/// IM-T23 the_process_is_alone: its user is the root of its own namespace, it is process 1, its
/// environment is the image's and the settings' and nothing of Moruna's, its root is the image's,
/// its devices are three, and it sees no process but itself.
#[test]
#[ignore = "Linux, sandbox host: root, and MORUNA_SANDBOX_CGROUP"]
fn im_t23_the_process_is_alone() {
    let scratch = Scratch::new("im_t23");
    let layout = layout(&scratch);
    let e = run_settings(
        &scratch,
        &layout,
        json!({"command": ["/bin/arrow_echo", "--report"], "env": {"EXTRA": "2", "FROM_IMAGE": "3"}}),
    )
    .expect_err("the report exits with status 9");
    assert!(e.contains("exited with status 9"), "{e}");
    assert!(
        e.contains("report: Uid:\t0\t0\t0\t0"),
        "root in its own user namespace: {e}"
    );
    assert!(e.contains("report: pid 1"), "{e}");
    assert!(
        e.contains("report: env EXTRA=2 FROM_IMAGE=3 PATH=/bin\n"),
        "exactly the image's environment with the settings' added: {e}"
    );
    assert!(e.contains("report: dev null,urandom,zero"), "{e}");
    assert!(
        e.contains("report: processes 1") && !e.contains("report: processes 1,"),
        "{e}"
    );
    let root_line = e
        .lines()
        .find(|l| l.contains("report: root"))
        .expect("a root listing");
    for outside in ["home", "Users", "var", "w", "target"] {
        assert!(
            !root_line.split([' ', ',']).any(|n| n == outside),
            "`/{outside}` is not in the image: {root_line}"
        );
    }
    assert!(root_line.contains("bin"), "{root_line}");
}

/// IM-T24 a_missing_command_is_named: a command that is not in the image ends the run with
/// where it was looked for, and a user that is not numeric is refused when the kernel is built.
#[test]
#[ignore = "Linux, sandbox host: root, and MORUNA_SANDBOX_CGROUP"]
fn im_t24_a_missing_command_is_named() {
    let scratch = Scratch::new("im_t24");
    let layout = layout(&scratch);
    let e = run_settings(&scratch, &layout, json!({"command": ["nothing-like-it"]}))
        .expect_err("the run fails");
    assert!(e.contains("exited with status 127"), "{e}");
    assert!(e.contains("/bin/nothing-like-it"), "{e}");

    let by_name = scratch.path().join("named");
    LayoutBuilder::new(&by_name)
        .config(json!({"Cmd": ["/bin/x"], "User": "app"}))
        .layer(media::TAR, &[Entry::file("x", b"")])
        .finish(Some("echo"));
    let e = run_settings(&scratch, &by_name, json!({})).expect_err("the build is refused");
    assert!(e.contains("user `app` is not numeric"), "{e}");
}
