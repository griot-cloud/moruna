//! SC-T16 and PL-T17 with real components (F4.7, MH H5): a run is killed with `SIGKILL` at
//! random points, a fresh process resumes it from the manifest on the surviving staging disk,
//! and the sink ends up holding what an uninterrupted run writes.
//!
//! The run is a child process: this test binary run again with `MORUNA_KILL_RESUME_CHILD` set,
//! which makes `kill_resume_child` below run one job and exit, and makes it a no-op otherwise.
//! Every child runs the same job with `resume_auto` and `staging_durable` set, exactly as a host
//! that restarts a destroyed machine with the same job does (MH 4.7): the first child of a
//! point finds no manifest and starts fresh, the second finds the killed run's and resumes it.
//!
//! The job reads a Parquet file whose row groups are read whole (the test wraps the source so
//! no split is sub-splittable), so morsel boundaries do not depend on the controller's timing
//! and an ordered sink's files are comparable byte for byte with an uninterrupted run's.
//!
//! Seeded: set `MORUNA_KILL_SEED` to replay a failure; the seed is printed on every run.

#![allow(clippy::result_large_err)]

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use moruna_kernel::{
    Allocator, BoxFuture, Kernel, Payload, Result as MResult, RowRange, SourceSchema, Split, Tier,
};
use moruna_runtime::{CancelToken, Components, RunId, RunSpec, Runtime, SinkSpec, SourceSpec};
use support::{Appender, Doubler, Sleeper, write_parquet};

const CHILD: &str = "MORUNA_KILL_RESUME_CHILD";
const ROWS: u64 = 240_000;
const GROUPS: u64 = 48;
/// One run id for every child, so an uninterrupted run and a resumed one write the same
/// footer metadata (the sink records the run id in every file, 08 e.2).
const RUN_ID: RunId = RunId([7; 16]);

/// Serialises the two tests of this file: each spawns processes that build a real arena.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

// ------------------------------------------------------------------------------------------
// The child.
// ------------------------------------------------------------------------------------------

/// The job one child runs, passed as `key=value` pairs separated by `;`.
struct Job {
    input: PathBuf,
    out: PathBuf,
    staging: PathBuf,
    profiles: PathBuf,
    reads: PathBuf,
    ordered: bool,
}

impl Job {
    fn encode(&self) -> String {
        format!(
            "input={};out={};staging={};profiles={};reads={};ordered={}",
            self.input.display(),
            self.out.display(),
            self.staging.display(),
            self.profiles.display(),
            self.reads.display(),
            self.ordered
        )
    }

    fn decode(text: &str) -> Job {
        let fields: BTreeMap<&str, &str> = text
            .split(';')
            .filter_map(|pair| pair.split_once('='))
            .collect();
        let path = |key: &str| PathBuf::from(fields.get(key).copied().unwrap_or_default());
        Job {
            input: path("input"),
            out: path("out"),
            staging: path("staging"),
            profiles: path("profiles"),
            reads: path("reads"),
            ordered: fields.get("ordered") == Some(&"true"),
        }
    }
}

/// A source that reads whole row groups and appends every read it makes to a file, so the
/// parent can count what a resumed run read again (PL-T17).
struct WholeSplits {
    inner: Arc<dyn moruna_kernel::Source>,
    log: std::sync::Mutex<std::fs::File>,
}

impl moruna_kernel::Source for WholeSplits {
    fn schema(&self) -> SourceSchema {
        self.inner.schema()
    }

    fn plan(&self) -> MResult<Vec<Split>> {
        Ok(self
            .inner
            .plan()?
            .into_iter()
            .map(|mut split| {
                split.sub_splittable = false;
                split
            })
            .collect())
    }

    fn read<'a>(
        &'a self,
        split: &'a Split,
        rows: Option<RowRange>,
        alloc: &'a dyn Allocator,
        tier: Tier,
    ) -> BoxFuture<'a, MResult<Payload>> {
        let range = rows.unwrap_or(RowRange {
            start: 0,
            end: split.rows,
        });
        if let Ok(mut log) = self.log.lock() {
            let _ = writeln!(log, "{} {} {}", split.id, range.start, range.end);
            let _ = log.flush();
        }
        self.inner.read(split, rows, alloc, tier)
    }

    fn repeatable(&self) -> bool {
        self.inner.repeatable()
    }
}

fn job_spec(job: &Job) -> RunSpec {
    let input = format!("file://{}", job.input.display());
    let reads = job.reads.clone();
    let sink_url = format!("file://{}", job.out.display());
    let kernels: Vec<Arc<dyn Kernel>> = vec![
        Arc::new(Doubler::new()),
        Arc::new(Sleeper::new(25)),
        Arc::new(Appender::new("loud")),
    ];
    let mut spec = RunSpec::new(
        SourceSpec::Build(Box::new(move |ctx| {
            let inner = moruna_sources::ParquetSource::new(
                moruna_sources::ParquetSourceConfig {
                    urls: vec![input],
                    ..Default::default()
                },
                ctx.reactor.clone(),
                ctx.object_metadata()?,
            )?;
            let log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&reads)
                .map_err(|e| moruna_kernel::MorunaError::Io {
                    op: "open",
                    target: reads.display().to_string(),
                    msg: e.to_string(),
                })?;
            Ok(Arc::new(WholeSplits {
                inner: Arc::new(inner),
                log: std::sync::Mutex::new(log),
            }) as Arc<dyn moruna_kernel::Source>)
        })),
        kernels,
        SinkSpec::Build(Box::new(move |ctx| {
            moruna_sinks::ParquetSink::new(
                moruna_sinks::ParquetSinkConfig {
                    url: sink_url.clone(),
                    row_group_bytes: 64 << 10,
                    file_bytes: 256 << 10,
                    ..Default::default()
                },
                ctx.reactor.clone(),
                ctx.alloc.clone(),
            )
            .map(|sink| Box::new(sink.with_run_id(ctx.run_id)) as Box<dyn moruna_kernel::Sink>)
        })),
    );
    spec.budget = Some(512 << 20);
    spec.cpu = Some(2.0);
    spec.staging_dir = Some(job.staging.clone());
    spec.staging_limit = Some(256 << 20);
    spec.staging_durable = true;
    spec.profiles_dir = Some(job.profiles.clone());
    spec.checkpoint_interval_ms = 20;
    spec.ordered = job.ordered;
    spec.resume_auto = true;
    spec
}

/// The child's whole life: one job, run to its end or to the parent's SIGKILL. A no-op when
/// the variable is unset, which is how this binary's ordinary run sees it.
#[test]
fn kill_resume_child() {
    let Ok(text) = std::env::var(CHILD) else {
        return;
    };
    let job = Job::decode(&text);
    let components = Components {
        run_id: Some(RUN_ID),
        ..Components::default()
    };
    match Runtime::run_with(job_spec(&job), CancelToken::new(), components) {
        Ok(report) => {
            println!("CHILD-OK resumed={}", report.resumed);
        }
        Err(error) => panic!("CHILD-FAILED {error}"),
    }
}

// ------------------------------------------------------------------------------------------
// The parent.
// ------------------------------------------------------------------------------------------

/// xorshift64*, seeded from the OS or from `MORUNA_KILL_SEED`.
struct Rng(u64);

impl Rng {
    fn seeded() -> Rng {
        let seed = std::env::var("MORUNA_KILL_SEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| {
                let mut bytes = [0u8; 8];
                getrandom::fill(&mut bytes).expect("random bytes");
                u64::from_le_bytes(bytes) | 1
            });
        println!("MORUNA_KILL_SEED={seed}");
        Rng(seed)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) % n.max(1)
    }
}

/// A directory under the target directory (not `/tmp`, which may be a tmpfs, and discovery
/// refuses a tmpfs staging directory declared durable, 03 e.4), removed at the end.
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
    let exe = std::env::current_exe().expect("this test binary");
    Command::new(exe)
        .args([
            "--exact",
            "kill_resume_child",
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

/// How long a child that is meant to finish may take. A resume that lost a morsel under an
/// ordered sink does not fail, it waits for ever for the sequence number it will never see,
/// which is exactly the in-flight read gap this feature closes (10 l); the watchdog turns that
/// into a failure with the child's output rather than a hung suite.
const CHILD_LIMIT: Duration = Duration::from_secs(120);

/// Wait for a child that is meant to finish, and fail with its output when it does not.
fn finish(mut child: Child, what: &str) -> String {
    let done = wait_until(CHILD_LIMIT, || !matches!(child.try_wait(), Ok(None)));
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

/// Every `manifest.json` (and, with `tmp`, `manifest.json.tmp`) under the staging directory.
fn manifests(staging: &Path, tmp: bool) -> Vec<PathBuf> {
    let name = if tmp {
        "manifest.json.tmp"
    } else {
        "manifest.json"
    };
    std::fs::read_dir(staging)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path().join(name))
                .filter(|p| p.is_file())
                .collect()
        })
        .unwrap_or_default()
}

fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    false
}

/// What a kill left behind, read from the manifest the resume will pick: the reads a correct
/// resume makes, as `(split, row_start, row_end)`, and whether the manifest said durable.
fn expected_reads(manifest: &Path, plan: &[(u32, u64)]) -> (BTreeSet<(u32, u64, u64)>, bool) {
    let text = std::fs::read_to_string(manifest).expect("the manifest");
    let doc: serde_json::Value = serde_json::from_str(&text).expect("the manifest is JSON");
    let committed = doc["committed_seq"].as_u64();
    let above = |seq: u64| committed.is_none_or(|c| seq > c);
    let mut want = BTreeSet::new();
    let mut known = BTreeSet::new();
    for row in doc["lineage"].as_array().into_iter().flatten() {
        let seq = row["seq"].as_u64().unwrap_or_default();
        known.insert(seq);
        if above(seq) && row["disk"].is_null() {
            want.insert((
                row["split"].as_u64().unwrap_or_default() as u32,
                row["row_start"].as_u64().unwrap_or_default(),
                row["row_end"].as_u64().unwrap_or_default(),
            ));
        }
    }
    for row in doc["issued"].as_array().into_iter().flatten() {
        let seq = row["seq"].as_u64().unwrap_or_default();
        if above(seq) && !known.contains(&seq) {
            want.insert((
                row["split"].as_u64().unwrap_or_default() as u32,
                row["row_start"].as_u64().unwrap_or_default(),
                row["row_end"].as_u64().unwrap_or_default(),
            ));
        }
    }
    let cursor = doc["source_cursor"]["split_index"]
        .as_u64()
        .unwrap_or_default() as usize;
    for (id, rows) in plan.iter().skip(cursor) {
        want.insert((*id, 0, *rows));
    }
    (want, doc["durable_staging"].as_bool() == Some(true))
}

fn read_log(path: &Path) -> Vec<(u32, u64, u64)> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace().map(|p| p.parse::<u64>().ok());
            Some((parts.next()?? as u32, parts.next()??, parts.next()??))
        })
        .collect()
}

/// The Parquet files of an output directory, by name, as bytes.
fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(dir)
        .expect("the output directory")
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("parquet"))
        .map(|e| {
            (
                e.file_name().to_string_lossy().into_owned(),
                std::fs::read(e.path()).expect("an output file"),
            )
        })
        .collect()
}

/// Every row of every Parquet file, as `(id, value, loud)`, sorted: the multiset the sink holds.
fn rows(dir: &Path) -> Vec<(i64, i64, bool)> {
    use arrow::array::{BooleanArray, Int64Array};
    let mut out = Vec::new();
    for name in files(dir).into_keys() {
        let file = std::fs::File::open(dir.join(name)).expect("an output file");
        let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)
            .expect("a reader")
            .build()
            .expect("a batch reader");
        for batch in reader {
            let batch = batch.expect("a batch");
            let ids = batch
                .column_by_name("id")
                .and_then(|c| c.as_any().downcast_ref::<Int64Array>().cloned())
                .expect("id");
            let values = batch
                .column_by_name("value")
                .and_then(|c| c.as_any().downcast_ref::<Int64Array>().cloned())
                .expect("value");
            let loud = batch
                .column_by_name("loud")
                .and_then(|c| c.as_any().downcast_ref::<BooleanArray>().cloned())
                .expect("loud");
            for i in 0..batch.num_rows() {
                out.push((ids.value(i), values.value(i), loud.value(i)));
            }
        }
    }
    out.sort_unstable();
    out
}

/// The input's plan as `(split id, rows)`: one split per row group, in file order.
fn plan_of(input: &Path) -> Vec<(u32, u64)> {
    use parquet::file::reader::FileReader;
    let file = std::fs::File::open(input).expect("the input");
    let reader = parquet::file::reader::SerializedFileReader::new(file).expect("a reader");
    (0..reader.metadata().num_row_groups())
        .map(|g| (g as u32, reader.metadata().row_group(g).num_rows() as u64))
        .collect()
}

/// How one kill point chooses its moment.
#[derive(Copy, Clone, Debug)]
enum When {
    /// A random delay after the first manifest appears.
    After(u64),
    /// As soon as a manifest's temporary file is seen: mid-checkpoint (PL-I12).
    MidCheckpoint,
}

struct Outcome {
    killed: bool,
    mid_checkpoint: bool,
}

/// One kill point: run, kill, resume with the same job, and hand back what happened. The
/// caller compares the output.
fn kill_and_resume(
    root: &Dir,
    point: usize,
    input: &Path,
    ordered: bool,
    when: When,
    plan: &[(u32, u64)],
) -> (Job, Outcome) {
    let job = Job {
        input: input.to_path_buf(),
        out: root.join(&format!("out-{point}")),
        staging: root.join(&format!("staging-{point}")),
        profiles: root.join(&format!("profiles-{point}")),
        reads: root.0.join(format!("reads-{point}.log")),
        ordered,
    };
    let mut child = spawn(&job);
    assert!(
        wait_until(Duration::from_secs(30), || !manifests(&job.staging, false)
            .is_empty()
            || !matches!(child.try_wait(), Ok(None))),
        "point {point}: the first manifest never appeared"
    );
    let mut mid_checkpoint = false;
    match when {
        When::After(ms) => std::thread::sleep(Duration::from_millis(ms)),
        When::MidCheckpoint => {
            mid_checkpoint = wait_until(Duration::from_secs(5), || {
                !manifests(&job.staging, true).is_empty()
            });
        }
    }
    let killed = matches!(child.try_wait(), Ok(None));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&job.reads);
    if !killed {
        return (
            job,
            Outcome {
                killed,
                mid_checkpoint: false,
            },
        );
    }

    let found = manifests(&job.staging, false);
    assert_eq!(
        found.len(),
        1,
        "point {point}: one run directory, one manifest"
    );
    let (want, durable) = expected_reads(&found[0], plan);
    assert!(
        durable,
        "point {point}: staging.durable is recorded in the manifest"
    );

    let stdout = finish(spawn(&job), &format!("point {point}: the resume"));
    assert!(
        stdout.contains("resumed=true"),
        "point {point}: resume auto found the killed run's manifest"
    );
    let reads = read_log(&job.reads);
    if std::env::var("MORUNA_KILL_DEBUG").is_ok() {
        eprintln!("point {point}: reads {reads:?}\nwant {want:?}");
    }
    // A range may be read more than once: the engine evicts Q0 entries under pressure and the
    // drive reads them again (SC f.5, PL-I6), in an uninterrupted run as much as in this one.
    // What resume adds is the set of distinct ranges, and that must be exactly what the
    // manifest says was lost: the recomputed lineage, the issued reads and the rest of the
    // plan. Nothing the watermark covered is read again (PL-I13).
    let unique: BTreeSet<(u32, u64, u64)> = reads.iter().copied().collect();
    assert_eq!(
        unique, want,
        "point {point}: the resume read exactly the recomputed lineage, the issued reads and \
         the rest of the plan (PL-I13)"
    );
    (
        job,
        Outcome {
            killed,
            mid_checkpoint,
        },
    )
}

/// The input, and the uninterrupted run everything is compared with.
fn reference(root: &Dir, ordered: bool) -> (PathBuf, PathBuf) {
    let input = root.0.join("in.parquet");
    if !input.exists() {
        write_parquet(&input, ROWS, GROUPS);
    }
    let name = if ordered { "ref-ordered" } else { "ref" };
    let job = Job {
        input: input.clone(),
        out: root.join(&format!("{name}-out")),
        staging: root.join(&format!("{name}-staging")),
        profiles: root.join(&format!("{name}-profiles")),
        reads: root.0.join(format!("{name}-reads.log")),
        ordered,
    };
    let stdout = finish(spawn(&job), "the uninterrupted run");
    assert!(
        stdout.contains("resumed=false"),
        "a fresh run with resume auto"
    );
    assert!(
        manifests(&job.staging, false).is_empty(),
        "a completed run leaves no manifest"
    );
    (input, job.out)
}

/// SC-T16 with real components: an unordered run killed with SIGKILL and resumed holds the
/// same multiset of rows as an uninterrupted run. f.13, S17, MH H5.
#[test]
fn sc_t16_sigkill_resume_equivalence() {
    if std::env::var(CHILD).is_ok() {
        return;
    }
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Dir::new("sc_t16_kill");
    let mut rng = Rng::seeded();
    let (input, reference_out) = reference(&root, false);
    let plan = plan_of(&input);
    let want = rows(&reference_out);
    assert_eq!(want.len() as u64, ROWS);
    let mut killed = 0;
    for point in 0..4 {
        let delay = rng.below(600);
        let (job, outcome) =
            kill_and_resume(&root, point, &input, false, When::After(delay), &plan);
        killed += usize::from(outcome.killed);
        assert_eq!(
            rows(&job.out),
            want,
            "point {point} (killed {} after {delay} ms): the row multiset differs",
            outcome.killed
        );
    }
    assert!(killed >= 2, "only {killed} of 4 points landed mid-run");
}

/// PL-T17 with real components: an ordered run killed at ten points, one of them while the
/// manifest's temporary file exists, resumes to files byte-identical to an uninterrupted run's,
/// and re-reads exactly the recomputed lineage (PL-I12, PL-I13, S17, MH H5).
#[test]
fn pl_t17_kill_and_resume() {
    if std::env::var(CHILD).is_ok() {
        return;
    }
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Dir::new("pl_t17_kill");
    let mut rng = Rng::seeded();
    let (input, reference_out) = reference(&root, true);
    let plan = plan_of(&input);
    let want = files(&reference_out);
    assert!(want.len() > 2, "the uninterrupted run rolled several files");
    let mut killed = 0;
    let mut mid = 0;
    for point in 0..10 {
        let when = if point == 0 {
            When::MidCheckpoint
        } else {
            When::After(rng.below(800))
        };
        let (job, outcome) = kill_and_resume(&root, point, &input, true, when, &plan);
        killed += usize::from(outcome.killed);
        mid += usize::from(outcome.mid_checkpoint);
        // File for file, in order, byte for byte. The names may skip an index: the file that
        // was open at the kill had taken one, and a resumed sink numbers past it rather than
        // reuse a name (08 e.5).
        let got: Vec<Vec<u8>> = files(&job.out).into_values().collect();
        let reference: Vec<&Vec<u8>> = want.values().collect();
        assert_eq!(
            got.len(),
            reference.len(),
            "point {point} ({when:?}): {} files against {}",
            got.len(),
            reference.len()
        );
        for (index, (got, want)) in got.iter().zip(reference).enumerate() {
            assert!(
                got == want,
                "point {point} ({when:?}): file {index} is not byte-identical"
            );
        }
    }
    assert!(killed >= 5, "only {killed} of 10 points landed mid-run");
    println!("pl_t17: {killed} kills, {mid} of them mid-checkpoint");
}
