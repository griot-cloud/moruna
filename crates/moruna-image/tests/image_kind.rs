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
use moruna_job::JobSpec;
use moruna_runtime::job::{BuildOptions, build};
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

/// A layout holding `arrow_echo` and `plane_echo` in `/bin` with what they need, one gzip layer,
/// named `echo`; its directory.
fn layout(scratch: &Scratch) -> PathBuf {
    let mut entries = vec![Entry::dir("bin"), Entry::dir("etc")];
    let mut libs = std::collections::BTreeSet::new();
    for (name, binary) in [
        ("bin/arrow_echo", env!("CARGO_BIN_EXE_arrow_echo")),
        ("bin/plane_echo", env!("CARGO_BIN_EXE_plane_echo")),
    ] {
        let binary = PathBuf::from(binary);
        entries.push(Entry::File(
            name,
            std::fs::read(&binary).expect("the fixture is built"),
            0o755,
        ));
        libs.extend(libraries(&binary));
    }
    for lib in libs {
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
    write_parquet_in(path, rows, 4);
}

/// `rows` rows in `groups` row groups: a batch for each, at the least.
fn write_parquet_in(path: &Path, rows: i64, groups: usize) {
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
        .set_max_row_group_row_count(Some((rows as usize).div_ceil(groups)))
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

/// How a document names the fixture's image, `echo` in `layout`: an image Moruna did not
/// build, by its manifest's digest and where it is.
fn image_of(layout: &Path) -> Value {
    let index: Value =
        serde_json::from_slice(&std::fs::read(layout.join("index.json")).unwrap()).unwrap();
    let digest = index["manifests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["annotations"]["org.opencontainers.image.ref.name"] == "echo")
        .expect("the fixture's image is named echo")["digest"]
        .clone();
    json!({"build_key": null, "digest": digest, "location": layout.display().to_string()})
}

/// The document: Parquet in, the image stage with `settings`, Parquet out.
fn document(scratch: &Path, layout: &Path, settings: Value) -> JobSpec {
    let input = scratch.join("in.parquet");
    write_parquet(&input, ROWS);
    job_of(scratch, layout, settings, Some(2.0))
}

/// The document over `in.parquet`, as written before: the image stage with `settings`, in a
/// job whose `budget.cpu` is `cpu`, or that names none.
fn job_of(scratch: &Path, layout: &Path, settings: Value, cpu: Option<f64>) -> JobSpec {
    let input = scratch.join("in.parquet");
    let out = scratch.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let staging = scratch.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let mut kernel = json!({"kind": "image", "image": image_of(layout),
                            "cache": scratch.join("images").display().to_string()});
    for (k, v) in settings.as_object().expect("settings are an object") {
        kernel[k] = v.clone();
    }
    let mut budget = json!({"memory_bytes": 1u64 << 30});
    if let Some(cpu) = cpu {
        budget["cpu"] = json!(cpu);
    }
    JobSpec::from_value(json!({
        "moruna_spec": 1,
        "source": {"kind": "parquet", "url": format!("file://{}", input.display())},
        "kernels": [kernel],
        "sink": {"kind": "parquet", "url": format!("file://{}", out.display())},
        "budget": budget,
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
    run_job(&document(scratch.path(), layout, settings))
}

fn run_job(job: &JobSpec) -> Result<moruna_runtime::RunReport, String> {
    let _serial = RUNS.lock().unwrap_or_else(|e| e.into_inner());
    let job = moruna_job::Job::from_v1(job);
    let plan = moruna_runtime::job::plan::default_plan(&job).map_err(|e| e.to_string())?;
    let built = build(
        &job,
        &plan,
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
            pulled_images: None,
            args: Default::default(),
            held: Vec::new(),
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
    // Every row exactly once. Across the sink's files the rows are in no order: Parquet encodes
    // into as many lanes as encodes ran at once (08 f.12, RB1), and on a four-CPU agent the
    // files' names and their rows no longer line up (2026-11-15).
    let mut every = id.clone();
    every.sort_unstable();
    assert_eq!(every, (0..ROWS).collect::<Vec<_>>(), "every row, once");
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
            "image": image_of(&layout),
            "cache": scratch.path().join("images").display().to_string(),
            "command": ["/bin/arrow_echo"],
            "env": {"A": "1"},
        })]);
    let passed = conformance::kernel(&image::Image, &harness).unwrap_or_else(|v| panic!("{v}"));
    assert!(passed.cases > 4, "{passed:?}");
}

/// IM-T23 the_process_is_alone: its user is the root of its own namespace, it is process 1, its
/// environment is the image's and the settings', with nothing of Moruna's but the image
/// contract's (`MORUNA_ROLE`, `MORUNA_PARAMS`, `MORUNA_ARGS`), its root is the image's, its
/// devices are three, and it sees no process but itself.
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
    // Under a coverage build the fixture is itself instrumented, and LLVM's profile runtime
    // sets this marker in the fixture's own environment before it reports: not the launcher's.
    let e = e.replace(
        " __LLVM_PROFILE_RT_INIT_ONCE=__LLVM_PROFILE_RT_INIT_ONCE",
        "",
    );
    assert!(e.contains("exited with status 9"), "{e}");
    assert!(
        e.contains("report: Uid:\t0\t0\t0\t0"),
        "root in its own user namespace: {e}"
    );
    assert!(e.contains("report: pid 1"), "{e}");
    // The job has 2 CPUs: each of its instances is told its share of them, and the thread pools
    // are set to it (6.3f); one instance when memory funds one.
    let line = e
        .lines()
        .find(|l| l.contains("report: env "))
        .expect("an environment line");
    let share = if line.contains("MORUNA_CPUS=1 ") {
        1
    } else {
        2
    };
    assert!(
        line.ends_with(&format!(
            "report: env EXTRA=2 FROM_IMAGE=3 MKL_NUM_THREADS={share} MORUNA_ARGS={{}} \
             MORUNA_CPUS={share} MORUNA_PARAMS={{}} MORUNA_ROLE=function \
             NUMEXPR_NUM_THREADS={share} OMP_NUM_THREADS={share} OPENBLAS_NUM_THREADS={share} \
             PATH=/bin RAYON_NUM_THREADS={share}"
        )),
        "exactly the image's environment with the settings', the image contract's and its \
         share of the CPUs added: {line}"
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

/// The `id` column of the Parquet files under `out`, sorted: across the sink's files rows are in
/// no order, since Parquet encodes into as many lanes as encodes ran at once (08 f.12, RB1).
fn ids(out: &Path) -> Vec<i64> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(out)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("parquet"))
        .collect();
    files.sort();
    let mut id = Vec::new();
    for file in files {
        let reader = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(&file).unwrap())
            .unwrap()
            .build()
            .unwrap();
        for batch in reader {
            let batch = batch.unwrap();
            let array = batch.column_by_name("id").expect("id");
            let array = array.as_any().downcast_ref::<Int64Array>().unwrap();
            id.extend((0..array.len()).map(|i| array.value(i)));
        }
    }
    id.sort_unstable();
    id
}

/// IM-T28 a_plane_program_is_a_stage (KA2): a command that makes the plane's calls itself on its
/// descriptor 3, under the image's filter, moves every row, in order, with no stream.
#[test]
#[ignore = "Linux, sandbox host: root, and MORUNA_SANDBOX_CGROUP"]
fn im_t28_a_plane_program_is_a_stage() {
    let scratch = Scratch::new("im_t28");
    let layout = layout(&scratch);
    let report = run_settings(
        &scratch,
        &layout,
        json!({"command": ["/bin/plane_echo"], "plane": true}),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        ids(&scratch.path().join("out")),
        (0..ROWS).collect::<Vec<_>>()
    );
    let json: Value = serde_json::from_str(&report.to_json()).unwrap();
    let stage = json["stages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["stage"] == 1)
        .expect("the image's stage");
    assert_eq!(stage["rows_out"], ROWS);
}

/// IM-T29 a_refused_record_ends_the_stage (KA2): a record the engine's check refuses ends the
/// run with the check's reason; a record more than was read is named.
#[test]
#[ignore = "Linux, sandbox host: root, and MORUNA_SANDBOX_CGROUP"]
fn im_t29_a_refused_record_ends_the_stage() {
    let scratch = Scratch::new("im_t29");
    let layout = layout(&scratch);
    let e = run_settings(
        &scratch,
        &layout,
        json!({"command": ["/bin/plane_echo", "--garbage"], "plane": true}),
    )
    .expect_err("the run fails");
    assert!(e.contains("refused by the engine's check"), "{e}");
    assert!(e.contains("continuation marker"), "the check's reason: {e}");
    let e = run_settings(
        &scratch,
        &layout,
        json!({"command": ["/bin/plane_echo", "--twice"], "plane": true}),
    )
    .expect_err("the run fails");
    assert!(e.contains("more record batches than"), "{e}");
}

/// IM-T30 a_source_the_filter_kills_is_named (0.6.4): a source image that makes a socket is
/// killed by the filter before it writes a byte, and the run says so (SDD 19 f.7), not only that
/// its stream was empty: 0.6.3 killed the process before looking at how it had ended, so every
/// Python image that died at its start was reported as "its output is not an Arrow IPC stream".
#[test]
#[ignore = "Linux, sandbox host: root, and MORUNA_SANDBOX_CGROUP"]
fn im_t30_a_source_the_filter_kills_is_named() {
    let _serial = RUNS.lock().unwrap_or_else(|e| e.into_inner());
    let scratch = Scratch::new("im_t30");
    let layout = layout(&scratch);
    let out = scratch.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let staging = scratch.path().join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let spec = JobSpec::from_value(json!({
        "moruna_spec": 1,
        "source": {"kind": "image", "image": image_of(&layout),
                   "cache": scratch.path().join("images").display().to_string(),
                   "command": ["/bin/arrow_echo", "--socket"]},
        "kernels": [],
        "sink": {"kind": "parquet", "url": format!("file://{}", out.display())},
        "budget": {"memory_bytes": 1u64 << 30, "cpu": 2.0},
        "staging": {"dir": staging.to_string_lossy(), "limit_bytes": 1u64 << 30},
        "profiles_dir": scratch.path().join("profiles").to_string_lossy(),
    }))
    .unwrap();
    let job = moruna_job::Job::from_v1(&spec);
    let plan = moruna_runtime::job::plan::default_plan(&job).unwrap();
    let built = build(
        &job,
        &plan,
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
            pulled_images: None,
            args: Default::default(),
            held: Vec::new(),
        },
    )
    .expect("the job builds");
    let e = Runtime::run(built.spec, CancelToken::new())
        .map(drop)
        .expect_err("the run fails")
        .to_string();
    assert!(
        e.contains("made a system call it is not allowed"),
        "SDD 19 f.7: {e}"
    );
    assert!(e.contains("SIGSYS"), "{e}");
    assert!(
        e.contains("its output is not an Arrow IPC stream"),
        "the stream's fault stays the cause: {e}"
    );
}

/// IM-T31 a_plane_task_that_ends_before_it_answers_is_named: a plane program that ends before
/// it answers its first batch ends its stage with its exit status and the batches it owed.
#[test]
#[ignore = "Linux, sandbox host: root, and MORUNA_SANDBOX_CGROUP"]
fn im_t31_a_plane_task_that_ends_before_it_answers_is_named() {
    let scratch = Scratch::new("im_t31");
    let first = layout(&scratch);
    let quit = run_settings(
        &scratch,
        &first,
        json!({"command": ["/bin/plane_echo", "--quit-first"], "plane": true}),
    )
    .expect_err("a task that ends before it answers fails its stage");
    assert!(
        quit.contains("kernels[0]") && quit.contains("exited with status 5"),
        "{quit}"
    );
    assert!(quit.contains("for 1 sent"), "{quit}");
}

/// What the run's report says of the image stage's instances (`ImageKernel::note`): how many
/// there were, the most that held a batch at once, and the batches each answered.
fn instances_of(report: &moruna_runtime::RunReport) -> (usize, usize, Vec<u64>) {
    let json: Value = serde_json::from_str(&report.to_json()).unwrap();
    let note = json["notes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .find(|n| n.starts_with("image `kernels[0]`: "))
        .unwrap_or_else(|| panic!("no note of the image's instances: {}", json["notes"]));
    instances_said(note)
}

/// The figures of an image stage's note.
fn instances_said(note: &str) -> (usize, usize, Vec<u64>) {
    fn number(text: &str) -> u64 {
        text.split_whitespace()
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("no number at `{text}`"))
    }
    fn after<'a>(note: &'a str, mark: &str) -> &'a str {
        let at = note
            .find(mark)
            .unwrap_or_else(|| panic!("no `{mark}` in: {note}"));
        &note[at + mark.len()..]
    }
    let batches = note
        .split("; ")
        .filter(|part| part.starts_with("instance "))
        .map(|part| number(part.split_once(": ").expect("its batches").1))
        .collect();
    (
        number(after(note, "`: ")) as usize,
        number(after(note, "busy_peak ")) as usize,
        batches,
    )
}

/// The rows of the instance tests' input: batches of half a mebibyte and more, so what a
/// process holds whatever its batch (its program, a few MiB) is not read as bytes a byte of
/// morsel by the controller's fit of the run's first morsels (6.3f: with 5 KB batches it read
/// 115 to 215 bytes a byte and kept the run at one worker).
const ROWS_WIDE: i64 = 1 << 21;

/// IM-T32 an_image_function_runs_one_instance_per_cpu_of_the_job (system design 3E, item 6.3f):
/// an image function that declares nothing, in a job with 4 CPUs, runs 4 instances and its
/// batches go to all of them. It ran one process, however many CPUs the job had. Nothing
/// measured it, so its stage warms up on its first instance and the fit funds all four.
#[test]
#[ignore = "Linux, sandbox host: root, and MORUNA_SANDBOX_CGROUP"]
fn im_t32_an_image_function_runs_one_instance_per_cpu_of_the_job() {
    const BATCHES: usize = 32;
    const ROWS: i64 = ROWS_WIDE;
    let scratch = Scratch::new("im_t32");
    let layout = layout(&scratch);
    write_parquet_in(&scratch.path().join("in.parquet"), ROWS, BATCHES);
    // A function that takes its time over a batch, so the batches are there for every
    // instance: 32 of them, 150 milliseconds each, long enough for the controller to come back
    // to four workers after the warm-up, one a quarter second.
    let job = job_of(
        scratch.path(),
        &layout,
        json!({"command": ["/bin/arrow_echo", "--sleep-ms", "150"]}),
        Some(4.0),
    );
    let report = run_job(&job).unwrap_or_else(|e| panic!("{e}"));
    let (instances, busy_peak, batches) = instances_of(&report);
    assert_eq!(instances, 4, "one instance for each CPU of the job");
    assert_eq!(batches.len(), 4);
    assert!(
        batches.iter().all(|answered| *answered > 0),
        "its batches go to all of them: {batches:?}; notes: {:#?}",
        report.notes
    );
    assert!(batches.iter().sum::<u64>() >= BATCHES as u64, "{batches:?}");
    assert_eq!(
        busy_peak, 4,
        "all four held a batch at once: {batches:?}; notes: {:#?}",
        report.notes
    );
    // The first instance measured the function on the job's first five batches, and the fit
    // funded every instance.
    let note = report
        .notes
        .iter()
        .find(|n| n.starts_with("image `kernels[0]`: "))
        .expect("the stage's note");
    assert!(
        note.contains("; warmed up on its first instance over 5 pieces in ")
            && note.contains("; its 4 instances funded at pieces of "),
        "{note}"
    );
    // Every row, once, whichever instance answered its batch.
    assert_eq!(
        ids(&scratch.path().join("out")),
        (0..ROWS).collect::<Vec<_>>()
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
}

/// The test's process in a cgroup of its own with a CPU quota, beside the stages' cgroups,
/// until it is dropped: what a container or a service unit gives a Linux node's engine.
struct Quota {
    dir: PathBuf,
    back: PathBuf,
}

impl Quota {
    /// With `cpu_max` as the cgroup's `cpu.max`: a quota and its period, in microseconds.
    fn enter(cpu_max: &str) -> Quota {
        let own = std::fs::read_to_string("/proc/self/cgroup").expect("/proc/self/cgroup");
        let path = own
            .lines()
            .find_map(|l| l.strip_prefix("0::"))
            .expect("a cgroup v2 path")
            .trim()
            .trim_start_matches('/');
        let back = Path::new("/sys/fs/cgroup").join(path).join("cgroup.procs");
        let dir = cgroup().join(format!("quota-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
        std::fs::write(dir.join("cpu.max"), cpu_max).expect("cpu.max");
        std::fs::write(dir.join("cgroup.procs"), std::process::id().to_string())
            .expect("the test's process joins its quota's cgroup");
        Quota { dir, back }
    }
}

impl Drop for Quota {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.back, std::process::id().to_string());
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// IM-T33 under_a_cpu_quota_the_instances_are_the_engine_s_workers (item 6.3f): a job that
/// names no `budget.cpu`, on a node whose engine a cgroup gives 2 CPUs, runs an image function
/// as 2 instances, the workers the engine starts for that run; and with a quota of 2.5 CPUs,
/// as 3 and 3. Both come from the run's CPU quota as the engine's discovery found it, rounded
/// up.
#[test]
#[ignore = "Linux, sandbox host: root, and MORUNA_SANDBOX_CGROUP"]
fn im_t33_under_a_cpu_quota_the_instances_are_the_engine_s_workers() {
    const ROWS: i64 = ROWS_WIDE;
    assert!(
        std::env::var_os("MORUNA_CPU").is_none(),
        "MORUNA_CPU would stand in for the quota this test sets"
    );
    for (name, cpu_max, quota, expected) in [
        ("im_t33_whole", "200000 100000", 2.0, 2),
        ("im_t33_fraction", "250000 100000", 2.5, 3),
    ] {
        let scratch = Scratch::new(name);
        let layout = layout(&scratch);
        write_parquet_in(&scratch.path().join("in.parquet"), ROWS, 16);
        let job = job_of(
            scratch.path(),
            &layout,
            json!({"command": ["/bin/arrow_echo", "--sleep-ms", "20"]}),
            None,
        );
        let held = Quota::enter(cpu_max);
        let ran = run_job(&job);
        drop(held);
        let report = ran.unwrap_or_else(|e| panic!("{cpu_max}: {e}"));
        let json: Value = serde_json::from_str(&report.to_json()).unwrap();
        // The engine's workers for the run: its CPU quota as discovery found it, rounded up.
        let limits = &json["limits_initial"];
        assert_eq!(limits["cpu_quota"].as_f64(), Some(quota), "{limits}");
        let workers = quota.ceil() as usize;
        assert_eq!(workers, expected);
        let (instances, _, batches) = instances_of(&report);
        assert_eq!(
            instances, workers,
            "{cpu_max}: an instance for each of the engine's workers"
        );
        assert!(
            batches.iter().all(|answered| *answered > 0),
            "{batches:?}; notes: {:#?}",
            report.notes
        );
        assert_eq!(
            ids(&scratch.path().join("out")),
            (0..ROWS).collect::<Vec<_>>()
        );
    }
}

const GIB: u64 = 1 << 30;

/// The fixture's image unpacked, as the `image` kind unpacks it.
fn unpacked(scratch: &Scratch, layout: &Path) -> moruna_image::Image {
    let digest = image_of(layout)["digest"]
        .as_str()
        .expect("a digest")
        .to_string();
    let source = moruna_image::OciLayout::open(layout).expect("the layout");
    moruna_image::unpack(&source, &digest, &scratch.path().join("images")).expect("unpacked")
}

/// The kernel of the fixture's `arrow_echo`, in a job of `cpus` CPUs.
fn echo_kernel(image: &moruna_image::Image, cpus: usize) -> moruna_sandbox::ImageKernel {
    moruna_sandbox::ImageKernel::new(
        "echo",
        image,
        Some(vec!["/bin/arrow_echo".into()]),
        &Default::default(),
        cgroup(),
        false,
        cpus,
    )
    .expect("the kernel")
}

/// A run whose sampler says it holds `anon` bytes under `ceiling`, with a quota of `cpu`.
fn sampler(anon: u64, ceiling: u64, cpu: f64) -> Arc<dyn moruna_kernel::Sampler> {
    Arc::new(
        moruna_testkit::FakeSampler::new().scripted(vec![moruna_kernel::Sample {
            anon_bytes: anon,
            ceiling_bytes: ceiling,
            cpu_limit: cpu,
            ..moruna_kernel::Sample::default()
        }]),
    )
}

fn init_ctx(instance: usize) -> moruna_kernel::InitCtx {
    moruna_kernel::InitCtx {
        instance,
        device: None,
        alloc: Arc::new(moruna_testkit::FakeAllocator::new()),
    }
}

fn state_of(state: &mut Box<dyn moruna_kernel::KernelState>) -> &mut moruna_sandbox::ImageState {
    state
        .as_any_mut()
        .downcast_mut::<moruna_sandbox::ImageState>()
        .expect("an image's state")
}

fn cgroup_file(dir: &Path, file: &str) -> String {
    std::fs::read_to_string(dir.join(file))
        .unwrap_or_else(|e| panic!("{}/{file}: {e}", dir.display()))
        .trim()
        .to_string()
}

/// One batch through an instance; the rows it answered.
fn echo(
    kernel: &moruna_sandbox::ImageKernel,
    state: &mut dyn moruna_kernel::KernelState,
    rows: i64,
) -> usize {
    use moruna_kernel::Kernel;
    let batch = RecordBatch::try_new(
        Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)])),
        vec![Arc::new(Int64Array::from_iter_values(0..rows)) as ArrayRef],
    )
    .expect("a batch");
    let input = moruna_kernel::Payload::Table(batch, moruna_kernel::Tier::Host);
    match kernel.apply(state, input).unwrap_or_else(|e| panic!("{e}")) {
        moruna_kernel::Payload::Table(out, _) => out.num_rows(),
        moruna_kernel::Payload::Tensor(..) => panic!("a table"),
    }
}

/// IM-T34 a_stage_s_cgroup_holds_one_for_each_instance (item 6.3f): the stage has one cgroup,
/// which carries the stage's ceiling and the run's CPU quota, and one under it for each
/// instance with its share; what the stage holds outside the engine is the sum over them; a
/// change of the limits is written to all of them; and they go with their instances, the
/// stage's last.
#[test]
#[ignore = "Linux, sandbox host: root, and MORUNA_SANDBOX_CGROUP"]
fn im_t34_a_stage_s_cgroup_holds_one_for_each_instance() {
    use moruna_kernel::Kernel;
    let scratch = Scratch::new("im_t34");
    let layout = layout(&scratch);
    let image = unpacked(&scratch, &layout);
    let k = echo_kernel(&image, 1);
    assert_eq!(k.instances(), 1, "as the build found the job's CPUs");
    // The run's CPUs are 4: an instance for each.
    k.bind_run(sampler(0, GIB, 4.0), GIB);
    assert_eq!(k.instances(), 4);
    let outside = k.outside();
    assert_eq!(outside.in_use().unwrap(), 0, "no instance yet");

    let mut states: Vec<_> = (0..4)
        .map(|i| k.init(&init_ctx(i)).unwrap_or_else(|e| panic!("{e}")))
        .collect();
    let stage = state_of(&mut states[0]).stage_cgroup();
    assert_eq!(stage.parent(), Some(cgroup().as_path()));
    assert_eq!(cgroup_file(&stage, "memory.max"), GIB.to_string());
    assert_eq!(cgroup_file(&stage, "cpu.max"), "400000 100000");
    assert_eq!(cgroup_file(&stage, "cgroup.subtree_control"), "cpu memory");
    assert_eq!(
        cgroup_file(&stage, "cgroup.procs"),
        "",
        "it holds no process itself"
    );
    let mut dirs = Vec::new();
    for state in &mut states {
        let st = state_of(state);
        assert_eq!(st.stage_cgroup(), stage, "one cgroup for the stage");
        let dir = st.cgroup();
        assert_eq!(
            dir.parent(),
            Some(stage.as_path()),
            "an instance's is under it"
        );
        assert_eq!(cgroup_file(&dir, "memory.max"), (GIB / 4).to_string());
        assert_eq!(cgroup_file(&dir, "memory.swap.max"), "0");
        assert_eq!(
            cgroup_file(&dir, "cpu.max"),
            "max 100000",
            "the stage's holds it"
        );
        dirs.push(dir);
    }
    dirs.sort();
    dirs.dedup();
    assert_eq!(dirs.len(), 4, "a cgroup for each instance");

    // Every instance answers its own batches, from a process in its own cgroup.
    for (i, state) in states.iter_mut().enumerate() {
        assert_eq!(echo(&k, state.as_mut(), 100 + i as i64), 100 + i);
        let dir = state_of(state).cgroup();
        assert!(
            cgroup_file(&dir, "cgroup.events").contains("populated 1"),
            "its process is in it"
        );
    }
    // What the stage holds is the sum over its instances: more than any one of them, and
    // what the stage's cgroup counts for them.
    let held = outside.in_use().unwrap();
    let each: Vec<u64> = dirs
        .iter()
        .map(|d| cgroup_file(d, "memory.current").parse().unwrap())
        .collect();
    assert!(each.iter().all(|m| *m > 0), "{each:?}");
    assert!(held > *each.iter().max().unwrap(), "{held} of {each:?}");
    let all: u64 = cgroup_file(&stage, "memory.current").parse().unwrap();
    assert!(
        held <= all + (16 << 20) && all <= held + (16 << 20),
        "{held} and {all}"
    );

    // The limits moved: the stage's ceiling and quota, and every instance's share. The
    // instances stay those the stage started with.
    k.bind_run(sampler(0, GIB / 2, 1.0), GIB / 2);
    assert_eq!(k.instances(), 4);
    k.limits_changed();
    assert_eq!(cgroup_file(&stage, "memory.max"), (GIB / 2).to_string());
    assert_eq!(cgroup_file(&stage, "cpu.max"), "100000 100000");
    for dir in &dirs {
        assert_eq!(cgroup_file(dir, "memory.max"), (GIB / 8).to_string());
    }
    // And at an instance's next batch: the rest of the run grew by a quarter of a gibibyte.
    k.bind_run(sampler(GIB / 4, GIB / 2, 1.0), GIB / 2);
    assert_eq!(echo(&k, states[2].as_mut(), 7), 7);
    let left: u64 = cgroup_file(&stage, "memory.max").parse().unwrap();
    assert!(left >= GIB / 4 && left < GIB / 2, "{left}");
    for dir in &dirs {
        // An equal share, in whole pages of this machine's.
        let share: u64 = cgroup_file(dir, "memory.max").parse().unwrap();
        assert!(
            share <= left / 4 && share + (64 << 10) > left / 4,
            "{share} of {left}"
        );
    }

    for state in &mut states {
        assert!(k.finish(state.as_mut()).unwrap().is_none());
    }
    let (instances, busy_peak, batches) = instances_said(&k.note());
    assert_eq!((instances, busy_peak), (4, 1), "one at a time, here");
    assert_eq!(batches, vec![1, 1, 2, 1]);

    // An instance gone takes its cgroup, and the stage's stays while one lives.
    let first = state_of(&mut states[0]).cgroup();
    drop(states.remove(0));
    assert!(!first.exists());
    assert!(stage.is_dir());
    drop(states);
    assert!(
        !stage.exists(),
        "the stage's cgroup goes with its last instance"
    );
    assert_eq!(outside.in_use().unwrap(), 0, "the instances are gone");

    // A stage started again takes a new cgroup.
    let mut again = k.init(&init_ctx(0)).unwrap_or_else(|e| panic!("{e}"));
    let second = state_of(&mut again).stage_cgroup();
    assert_ne!(second, stage);
    drop(again);
    assert!(!second.exists());
}

/// IM-T35 memory_bounds_an_image_s_instances (item 6.3f): the stage starts as many of the
/// instances the rule asks for as its ceiling funds at 16 MiB each; when it funds not one, one
/// instance has the whole of it, as before; and an image that declares a ceiling runs no more.
#[test]
#[ignore = "Linux, sandbox host: root, and MORUNA_SANDBOX_CGROUP"]
fn im_t35_memory_bounds_an_image_s_instances() {
    use moruna_kernel::Kernel;
    const MIB: u64 = 1 << 20;
    let scratch = Scratch::new("im_t35");
    let layout = layout(&scratch);
    let image = unpacked(&scratch, &layout);

    // 80 MiB are left of the run's ceiling: two of four instances, 40 MiB each.
    let k = echo_kernel(&image, 4);
    k.bind_run(sampler(GIB - 80 * MIB, GIB, 4.0), GIB);
    assert_eq!(k.instances(), 4, "80 MiB funds four at 16 MiB");
    k.bind_run(sampler(GIB - 40 * MIB, GIB, 4.0), GIB);
    assert_eq!(k.instances(), 2);
    let mut states: Vec<_> = (0..2)
        .map(|i| k.init(&init_ctx(i)).unwrap_or_else(|e| panic!("{e}")))
        .collect();
    let stage = state_of(&mut states[0]).stage_cgroup();
    assert_eq!(cgroup_file(&stage, "memory.max"), (40 * MIB).to_string());
    for state in &mut states {
        let dir = state_of(state).cgroup();
        assert_eq!(cgroup_file(&dir, "memory.max"), (20 * MIB).to_string());
    }
    for state in &mut states {
        assert_eq!(echo(&k, state.as_mut(), 10), 10);
        assert!(k.finish(state.as_mut()).unwrap().is_none());
    }
    let note = k.note();
    assert!(
        note.ends_with("; echo: 2 of 4 instances: the share funds 2"),
        "{note}"
    );
    drop(states);

    // 12 MiB are left, less than the least: one instance, in all of it.
    let k = echo_kernel(&image, 4);
    k.bind_run(sampler(GIB - 12 * MIB, GIB, 4.0), GIB);
    assert_eq!(k.instances(), 1);
    let mut state = k.init(&init_ctx(0)).unwrap_or_else(|e| panic!("{e}"));
    let st = state_of(&mut state);
    assert_eq!(
        cgroup_file(&st.stage_cgroup(), "memory.max"),
        (12 * MIB).to_string()
    );
    assert_eq!(
        cgroup_file(&st.cgroup(), "memory.max"),
        (12 * MIB).to_string()
    );
    drop(state);

    // An image that declares a ceiling runs no more than it, whatever the job's CPUs.
    let mut one = image.clone();
    one.config.env.push("MORUNA_PROCESSES=1".into());
    let k = echo_kernel(&one, 4);
    k.bind_run(sampler(0, GIB, 4.0), GIB);
    assert_eq!(k.instances(), 1);
    let mut state = k.init(&init_ctx(0)).unwrap_or_else(|e| panic!("{e}"));
    let st = state_of(&mut state);
    assert_eq!(cgroup_file(&st.cgroup(), "memory.max"), GIB.to_string());
    assert_eq!(echo(&k, state.as_mut(), 3), 3);
    assert!(k.finish(state.as_mut()).unwrap().is_none());
    assert_eq!(instances_said(&k.note()), (1, 1, vec![1]));
}

/// IM-T36 a_batch_past_an_instance_s_cap_goes_in_slices (system design 3A, item 6.3f): an
/// instance takes at most what its cgroup's share holds by its function's measured line, which
/// the kernel says to the controller; a batch past that is cut into slices of its rows, each
/// given to the process and answered in turn, and the answers are joined in order into one
/// batch: every row once. A batch that fits goes whole.
#[test]
#[ignore = "Linux, sandbox host: root, and MORUNA_SANDBOX_CGROUP"]
fn im_t36_a_batch_past_an_instance_s_cap_goes_in_slices() {
    use moruna_kernel::Kernel;
    const MIB: u64 = 1 << 20;
    const ROWS_IN: i64 = 100_000;
    let scratch = Scratch::new("im_t36");
    let layout = layout(&scratch);
    let image = unpacked(&scratch, &layout);
    let k = echo_kernel(&image, 4);
    // A function measured to hold a KiB beside each byte of its batch: an instance's quarter of
    // a gibibyte holds 256 KiB of input.
    let line = moruna_cost::Memory {
        fixed_bytes: 0.0,
        per_input_byte: 1024.0,
        output_per_input_byte: None,
        spread: 0.0,
        units: 6,
        input_bytes_min: 4096,
        input_bytes_max: 65536,
    };
    k.bind_room(1 << 44, 512 * MIB, 4 * MIB, 4.0, Some(line));
    k.bind_run(sampler(0, GIB, 4.0), GIB);
    assert_eq!(
        k.instances(),
        4,
        "the line at the least morsel funds all four"
    );
    assert_eq!(k.morsel_bytes_max(), Some(256 << 10));
    let mut state = k.init(&init_ctx(0)).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        cgroup_file(&state_of(&mut state).cgroup(), "memory.max"),
        (GIB / 4).to_string()
    );

    // 100,000 rows of 8 bytes: past the cap, four slices of 25,000 rows.
    let batch = RecordBatch::try_new(
        Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)])),
        vec![Arc::new(Int64Array::from_iter_values(0..ROWS_IN)) as ArrayRef],
    )
    .expect("a batch");
    let input = moruna_kernel::Payload::Table(batch, moruna_kernel::Tier::Host);
    let out = match k.apply(state.as_mut(), input) {
        Ok(moruna_kernel::Payload::Table(out, _)) => out,
        Ok(_) => panic!("a table"),
        Err(e) => panic!("{e}"),
    };
    let column = |name: &str| -> Vec<i64> {
        out.column_by_name(name)
            .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
            .unwrap_or_else(|| panic!("no column {name}"))
            .values()
            .to_vec()
    };
    assert_eq!(
        column("id"),
        (0..ROWS_IN).collect::<Vec<_>>(),
        "every row once, in order"
    );
    // The process answered each slice as a batch of its own: its index restarts at each.
    let starts: Vec<usize> = column("n")
        .iter()
        .enumerate()
        .filter(|(_, n)| **n == 0)
        .map(|(at, _)| at)
        .collect();
    assert_eq!(starts, vec![0, 25_000, 50_000, 75_000]);

    // A batch that fits goes whole.
    assert_eq!(echo(&k, state.as_mut(), 1000), 1000);
    assert!(k.finish(state.as_mut()).unwrap().is_none());
    let note = k.note();
    assert_eq!(instances_said(&note).2, vec![5, 0, 0, 0], "{note}");
    assert!(
        note.contains("; 1 batches past an instance's cap cut into 4 slices"),
        "{note}"
    );
    assert!(
        note.contains(
            "measured: batches of at most 262144 bytes, what an instance's share of 268435456 \
             bytes holds"
        ),
        "{note}"
    );
}
