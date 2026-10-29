//! SC-T32 a_dag_resumes (SDD 10 f.13, placement f.13, contracts d.16): a DAG with a fan-out is
//! killed with `SIGKILL` at seeded points, a fresh process resumes it from the manifest on the
//! surviving staging disk, and each sink ends up holding exactly what an uninterrupted run's
//! does: every row once, none lost to the kill and none written twice by the replay.
//!
//! The run is a child process: this test binary run again with `MORUNA_DAG_RESUME_CHILD` set,
//! which makes `dag_resume_child` below run one job and exit, and makes it a no-op otherwise.
//! Every child runs the same job with `resume_auto` and `staging_durable`, as a host that
//! restarts a destroyed machine with the same job does (MH 4.7). Seeded: set
//! `MORUNA_DAG_RESUME_SEED` to replay a failure.

#![allow(clippy::result_large_err)]

mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use moruna_kernel::arrow::array::{Array, Int64Array};
use moruna_kernel::{Consumer, Kernel, Link, Producer, Topology};
use moruna_runtime::{CancelToken, Components, RunId, RunSpec, Runtime, SinkSpec, SourceSpec};
use support::{Doubler, Sleeper, write_parquet};

const CHILD: &str = "MORUNA_DAG_RESUME_CHILD";
const ROWS: u64 = 120_000;
const GROUPS: u64 = 30;
const RUN_ID: RunId = RunId([9; 16]);

/// The job one child runs: `input;staging;profiles;out0;out1`.
struct Job {
    input: PathBuf,
    staging: PathBuf,
    profiles: PathBuf,
    outs: [PathBuf; 2],
}

impl Job {
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

    fn decode(text: &str) -> Job {
        let parts: Vec<PathBuf> = text.split(';').map(PathBuf::from).collect();
        Job {
            input: parts[0].clone(),
            staging: parts[1].clone(),
            profiles: parts[2].clone(),
            outs: [parts[3].clone(), parts[4].clone()],
        }
    }
}

/// The source, a slow stage, then a fan-out to two stages each with its own sink.
fn job_spec(job: &Job) -> RunSpec {
    let input = format!("file://{}", job.input.display());
    let sink = |dir: &Path| {
        let url = format!("file://{}", dir.display());
        SinkSpec::Build(Box::new(move |ctx| {
            moruna_sinks::ParquetSink::new(
                moruna_sinks::ParquetSinkConfig {
                    url: url.clone(),
                    row_group_bytes: 64 << 10,
                    file_bytes: 256 << 10,
                    ..Default::default()
                },
                ctx.reactor.clone(),
                ctx.alloc.clone(),
            )
            .map(|sink| Box::new(sink.with_run_id(ctx.run_id)) as Box<dyn moruna_kernel::Sink>)
        }))
    };
    let kernels: Vec<Arc<dyn Kernel>> = vec![
        Arc::new(Sleeper::new(15)),
        Arc::new(Doubler::new()),
        Arc::new(
            moruna_kernels::StdKernel::new("filter", &serde_json::json!({"expr": "id >= 0"}))
                .expect("a filter"),
        ),
    ];
    let k = |stage, port| Consumer::Kernel { stage, port };
    let topology = Topology::new(
        1,
        3,
        2,
        vec![
            Link {
                from: Producer::Source(0),
                to: k(1, 0),
            },
            Link {
                from: Producer::Kernel(1),
                to: k(2, 0),
            },
            Link {
                from: Producer::Kernel(1),
                to: k(3, 0),
            },
            Link {
                from: Producer::Kernel(2),
                to: Consumer::Sink(0),
            },
            Link {
                from: Producer::Kernel(3),
                to: Consumer::Sink(1),
            },
        ],
    )
    .expect("a fan-out");
    let mut spec = RunSpec::graph(
        vec![SourceSpec::Build(Box::new(move |ctx| {
            Ok(Arc::new(moruna_sources::ParquetSource::new(
                moruna_sources::ParquetSourceConfig {
                    urls: vec![input],
                    ..Default::default()
                },
                ctx.reactor.clone(),
                ctx.object_metadata()?,
            )?) as Arc<dyn moruna_kernel::Source>)
        }))],
        kernels,
        vec![sink(&job.outs[0]), sink(&job.outs[1])],
        topology,
    );
    spec.budget = Some(512 << 20);
    spec.cpu = Some(2.0);
    spec.staging_dir = Some(job.staging.clone());
    spec.staging_limit = Some(256 << 20);
    spec.staging_durable = true;
    spec.profiles_dir = Some(job.profiles.clone());
    spec.checkpoint_interval_ms = 20;
    spec.resume_auto = true;
    spec
}

/// The child's whole life: one job, run to its end or to the parent's SIGKILL. A no-op when the
/// variable is unset.
#[test]
fn dag_resume_child() {
    let Ok(text) = std::env::var(CHILD) else {
        return;
    };
    let job = Job::decode(&text);
    let components = Components {
        run_id: Some(RUN_ID),
        ..Components::default()
    };
    match Runtime::run_with(job_spec(&job), CancelToken::new(), components) {
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

fn spawn(job: &Job) -> Child {
    Command::new(std::env::current_exe().expect("this test binary"))
        .args([
            "--exact",
            "dag_resume_child",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .env(CHILD, job.encode())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("a child process")
}

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

/// Every row of every Parquet file under `dir`, as `(id, value)`, with how many times each is
/// there.
fn rows(dir: &Path) -> BTreeMap<(i64, i64), u32> {
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
            for i in 0..batch.num_rows() {
                *out.entry((ids.value(i), values.value(i))).or_insert(0) += 1;
            }
        }
    }
    out
}

/// SC-T32 a_dag_resumes: an uninterrupted run, then runs killed at seeded points and resumed,
/// each sink compared with the uninterrupted run's row for row, each row once.
#[cfg(unix)]
#[test]
fn sc_t32_a_dag_killed_mid_run_resumes() {
    if std::env::var(CHILD).is_ok() {
        return;
    }
    let seed = std::env::var("MORUNA_DAG_RESUME_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x0d46_u64);
    println!("MORUNA_DAG_RESUME_SEED={seed}");
    let dir = Dir::new("sc-t32");
    let input = dir.0.join("in.parquet");
    write_parquet(&input, ROWS, GROUPS);
    let job = |name: &str| Job {
        input: input.clone(),
        staging: dir.join(&format!("{name}-staging")),
        profiles: dir.join(&format!("{name}-profiles")),
        outs: [
            dir.join(&format!("{name}-a")),
            dir.join(&format!("{name}-b")),
        ],
    };
    let whole = job("whole");
    finish(spawn(&whole), "the uninterrupted run");
    let want = [rows(&whole.outs[0]), rows(&whole.outs[1])];
    assert_eq!(want[0].len() as u64, ROWS);
    assert!(want.iter().all(|r| r.values().all(|&n| n == 1)));
    let mut state = seed;
    let mut resumed_any = false;
    for point in 0..3 {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let kill_after = Duration::from_millis(150 + (state >> 33) % 600);
        let run = job(&format!("point{point}"));
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
                "point {point} (killed after {kill_after:?}) sink {sink}: rows written twice {twice:?}"
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
