//! A job document version 2 with a fan-out and a join, killed with `SIGKILL` at seeded points
//! and resumed (SDD 10 f.13, 22 d.4): a fresh process builds the same document, finds the
//! manifest on the surviving staging disk (`resume: "auto"`, durable staging) and continues; each
//! of its two sinks ends up holding exactly what an uninterrupted run's does, every row once.
//!
//! The job: `rows` (Parquet) feeds `slow` (a kind registered here, which sleeps in every call so
//! a kill lands mid-run); `slow` fans out to `hash` (the standard `hash` of `id`), to the join
//! `both` (the standard `join`, port `slow`) and to the sink `copy`; `hash` feeds `only-h` (the
//! standard `select` of the hash), which is the join's port `only-h`; the join feeds the sink
//! `joined`.
//!
//! The run is a child process: this test binary run again with `MORUNA_JOB_RESUME_CHILD` set,
//! which makes `job_resume_child` below run the document and exit, and makes it a no-op
//! otherwise. Seeded: set `MORUNA_JOB_RESUME_SEED` to replay a failure.

#![allow(clippy::result_large_err)]

mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use moruna_job::{
    Declaration, Edge, Fact, Flow, Isolation, Job, KindDoc, Needs, Role, Task, settings_schema,
};
use moruna_kernel::arrow::array::{Array, Int64Array, StringArray};
use moruna_kernel::{CancelToken, Kernel};
use moruna_runtime::job::build::process_env;
use moruna_runtime::job::{BuildOptions, build};
use moruna_runtime::kinds::{Context, Entry, Registry, RunnableKind, Stage};
use moruna_runtime::{Components, RunId, Runtime};
use serde_json::json;
use support::{Sleeper, write_parquet};

const CHILD: &str = "MORUNA_JOB_RESUME_CHILD";
const ROWS: u64 = 60_000;
const GROUPS: u64 = 30;
const RUN_ID: RunId = RunId([7; 16]);

/// `{"kind": "test.sleep"}`: a function that sleeps 15 ms in every call and passes its batch on.
struct Sleep;

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Nothing {}

impl RunnableKind for Sleep {
    fn declaration(&self) -> Declaration {
        Declaration {
            name: "test.sleep".into(),
            role: Role::Kernel,
            summary: "sleeps in every call and passes its batch on".into(),
            settings: settings_schema::<Nothing>("test.sleep", vec![json!({})]),
            inputs: Flow::Batches,
            outputs: Flow::Batches,
            needs: Needs::nothing(),
            deterministic: Fact::Yes,
            preserves_columns: Fact::Yes,
            isolation: Isolation::Builtin,
            served: None,
            title: None,
            description: None,
            takes: None,
            gives: None,
            requires: Vec::new(),
            provenance: None,
        }
    }

    fn stage(&self, entry: &Entry<'_>, _cx: &mut Context) -> moruna_kernel::Result<Stage> {
        let Nothing {} = entry.settings()?;
        Ok(Stage::of(Arc::new(Sleeper::new(15)) as Arc<dyn Kernel>))
    }
}

fn entry(value: serde_json::Value) -> KindDoc {
    KindDoc::from_value(value).expect("an entry")
}

/// The paths one child's job uses: `input;staging;profiles;joined;copy`.
struct Paths {
    input: PathBuf,
    staging: PathBuf,
    profiles: PathBuf,
    outs: [PathBuf; 2],
}

impl Paths {
    fn encode(&self) -> String {
        [
            &self.input,
            &self.staging,
            &self.profiles,
            &self.outs[0],
            &self.outs[1],
        ]
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(";")
    }

    fn decode(text: &str) -> Paths {
        let parts: Vec<PathBuf> = text.split(';').map(PathBuf::from).collect();
        Paths {
            input: parts[0].clone(),
            staging: parts[1].clone(),
            profiles: parts[2].clone(),
            outs: [parts[3].clone(), parts[4].clone()],
        }
    }
}

/// The document, as version 2's FlatBuffers read back: a fan-out, a join and two sinks.
fn document(paths: &Paths) -> Job {
    let url = |p: &Path| format!("file://{}", p.display());
    let mut job = Job::new(
        vec![
            Task::source(
                "rows",
                entry(json!({"kind": "parquet", "url": url(&paths.input)})),
            ),
            Task::kernel("slow", entry(json!({"kind": "test.sleep"}))),
            Task::kernel(
                "hash",
                entry(json!({"kind": "std", "name": "hash",
                             "args": {"columns": ["id"], "output": "h"}})),
            ),
            Task::kernel(
                "only-h",
                entry(json!({"kind": "std", "name": "select", "args": {"columns": ["h"]}})),
            ),
            Task::kernel_with_inputs(
                "both",
                entry(json!({"kind": "std", "name": "join"})),
                &["slow", "only-h"],
            ),
            Task::sink(
                "joined",
                entry(json!({"kind": "parquet", "url": url(&paths.outs[0])})),
            ),
            Task::sink(
                "copy",
                entry(json!({"kind": "parquet", "url": url(&paths.outs[1])})),
            ),
        ],
        vec![
            Edge::between("rows", "slow"),
            Edge::between("slow", "hash"),
            Edge::between("hash", "only-h"),
            Edge::new("slow", "out", "both", "slow"),
            Edge::new("only-h", "out", "both", "only-h"),
            Edge::between("both", "joined"),
            Edge::between("slow", "copy"),
        ],
    );
    job.name = Some("job-resume".into());
    job.budget.memory_bytes = Some(512 << 20);
    job.budget.cpu = Some(2.0);
    job.staging.dir = Some(paths.staging.display().to_string());
    job.staging.limit_bytes = Some(256 << 20);
    job.staging.durable = Some(true);
    job.profiles_dir = Some(paths.profiles.display().to_string());
    job.resume = Some("auto".into());
    let bytes = job.to_flatbuffer().expect("encodes");
    let read = Job::from_flatbuffer(&bytes).expect("reads back");
    assert_eq!(read, job);
    read
}

/// The child's whole life: the document built and run to its end or to the parent's SIGKILL.
/// A no-op when the variable is unset.
#[test]
fn job_resume_child() {
    let Ok(text) = std::env::var(CHILD) else {
        return;
    };
    let job = document(&Paths::decode(&text));
    let mut kinds = Registry::standard();
    kinds.register_kernel(Arc::new(Sleep)).expect("the kind");
    let plan = moruna_runtime::job::plan::default_plan(&job).expect("a plan");
    let mut built = build(
        &job,
        &plan,
        &kinds,
        BuildOptions {
            strict: false,
            env: &process_env,
            notes: Vec::new(),
            packages: Default::default(),
            host_ports: None,
            pulled_images: None,
            args: Default::default(),
            held: Vec::new(),
        },
    )
    .expect("builds");
    // A manifest every few morsels, so a kill finds one; a document's interval is at least
    // 500 ms, which the test would have to outwait.
    built.spec.checkpoint_interval_ms = 20;
    let components = Components {
        run_id: Some(RUN_ID),
        ..Components::default()
    };
    match Runtime::run_with(built.spec, CancelToken::new(), components) {
        Ok(report) => println!("CHILD-OK resumed={}", report.resumed),
        Err(error) => panic!("CHILD-FAILED {error}"),
    }
}

struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("moruna-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("a scratch directory");
        Dir(path)
    }

    fn join(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::create_dir_all(&path).expect("a subdirectory");
        path
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn spawn(paths: &Paths) -> Child {
    Command::new(std::env::current_exe().expect("this test binary"))
        .args([
            "--exact",
            "job_resume_child",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .env(CHILD, paths.encode())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("a child process")
}

/// The child's output once it has exited; a child still running after three minutes is killed
/// and the test fails.
fn finish(mut child: Child, what: &str) -> String {
    let start = Instant::now();
    let mut done = false;
    while start.elapsed() < Duration::from_secs(180) {
        if !matches!(child.try_wait(), Ok(None)) {
            done = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    if !done {
        let _ = child.kill();
    }
    let out = child.wait_with_output().expect("the child's output");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        done && out.status.success() && stdout.contains("CHILD-OK"),
        "{what} failed ({:?}, finished in time: {done}):\n{stdout}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

/// Every row of every Parquet file under `dir`, as `(id, value, h)` (`h` empty where the file
/// has none), with how many times each is there.
fn rows(dir: &Path) -> BTreeMap<(i64, i64, String), u32> {
    let mut out = BTreeMap::new();
    for entry in std::fs::read_dir(dir)
        .expect("the output directory")
        .flatten()
    {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("parquet") {
            continue;
        }
        let file = std::fs::File::open(&path).expect("an output file");
        let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)
            .expect("a reader")
            .build()
            .expect("batches");
        for batch in reader {
            let batch = batch.expect("a batch");
            let column = |name: &str| -> Int64Array {
                batch
                    .column_by_name(name)
                    .and_then(|c| c.as_any().downcast_ref::<Int64Array>().cloned())
                    .expect("an Int64 column")
            };
            let (ids, values) = (column("id"), column("value"));
            let hashes = batch
                .column_by_name("h")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>().cloned());
            for i in 0..batch.num_rows() {
                let h = hashes
                    .as_ref()
                    .map_or_else(String::new, |h| h.value(i).to_string());
                *out.entry((ids.value(i), values.value(i), h)).or_insert(0) += 1;
            }
        }
    }
    out
}

/// A version 2 job with a fan-out, a join and two sinks: an uninterrupted run, then runs killed
/// at seeded points and resumed, each sink compared with the uninterrupted run's row for row,
/// each row once, and at least one kill landing mid-run.
#[cfg(unix)]
#[test]
fn a_version_2_job_with_a_join_resumes_after_preemption() {
    if std::env::var(CHILD).is_ok() {
        return;
    }
    let seed = std::env::var("MORUNA_JOB_RESUME_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x2a62_u64);
    println!("MORUNA_JOB_RESUME_SEED={seed}");
    let dir = Dir::new("job-resume");
    let input = dir.0.join("in.parquet");
    write_parquet(&input, ROWS, GROUPS);
    let paths = |name: &str| Paths {
        input: input.clone(),
        staging: dir.join(&format!("{name}-staging")),
        profiles: dir.join(&format!("{name}-profiles")),
        outs: [
            dir.join(&format!("{name}-joined")),
            dir.join(&format!("{name}-copy")),
        ],
    };
    let whole = paths("whole");
    finish(spawn(&whole), "the uninterrupted run");
    let want = [rows(&whole.outs[0]), rows(&whole.outs[1])];
    assert_eq!(want[0].len() as u64, ROWS);
    assert_eq!(want[1].len() as u64, ROWS);
    assert!(
        want[0].keys().all(|(_, _, h)| h.len() == 64),
        "every joined row has its hash"
    );
    assert!(want.iter().all(|r| r.values().all(|&n| n == 1)));
    let mut state = seed;
    let mut resumed_any = false;
    for point in 0..3 {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let kill_after = Duration::from_millis(150 + (state >> 33) % 400);
        let run = paths(&format!("point{point}"));
        let mut child = spawn(&run);
        std::thread::sleep(kill_after);
        let killed = matches!(child.try_wait(), Ok(None));
        let _ = child.kill();
        let _ = child.wait();
        let stdout = finish(spawn(&run), &format!("the resume after {kill_after:?}"));
        resumed_any |= killed && stdout.contains("resumed=true");
        for (sink, wanted) in want.iter().enumerate() {
            let got = rows(&run.outs[sink]);
            let twice: Vec<_> = got.iter().filter(|(_, n)| **n > 1).take(5).collect();
            assert!(
                twice.is_empty(),
                "point {point} (killed after {kill_after:?}) sink {sink}: rows written twice \
                 {twice:?}"
            );
            assert_eq!(
                &got, wanted,
                "point {point} (killed after {kill_after:?}) sink {sink} differs from the \
                 uninterrupted run"
            );
        }
    }
    assert!(
        resumed_any,
        "at least one kill landed mid-run and was resumed"
    );
}
