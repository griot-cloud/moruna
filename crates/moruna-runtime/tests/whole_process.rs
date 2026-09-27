//! The whole process stays under the ceiling (F8.9): a run finishes inside its budget, measured
//! by the operating system for the whole process, or stops with a diagnostic; it is never
//! killed. Each run is its own process (`support::apart`), at 256 MiB and at 1 GiB, over data
//! larger than the smaller budget.

#![allow(clippy::result_large_err)]

mod support;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arrow::array::{Int64Array, RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use moruna_kernel::{
    BoxFuture, CancelToken, MorunaError, Payload, PayloadKind, PayloadSpec, Seq, Sink, SinkSummary,
    SourceSchema, TierPref,
};
use moruna_runtime::job::{BuildOptions, JobSpec, NoKernels, build};
use moruna_runtime::{RunSpec, Runtime, SinkSpec};
use serde_json::json;
use support::apart::{self, BUDGETS};
use support::{Scratch, one_run_at_a_time};

/// Rows of a kilobyte of text no codec shrinks much: 400 MB decoded, over the smaller budget.
const ROWS: i64 = 400_000;
const NOTE_BYTES: usize = 1000;

fn schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("note", DataType::Utf8, false),
    ]))
}

/// A kilobyte of text, the same for the same id.
fn note(id: i64) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut state = (id as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut out = String::with_capacity(NOTE_BYTES);
    while out.len() < NOTE_BYTES {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        for shift in (0..60).step_by(6) {
            if out.len() < NOTE_BYTES {
                out.push(ALPHABET[((state >> shift) & 63) as usize] as char);
            }
        }
    }
    out
}

/// `rows` rows as one Parquet file, written a row group (about 4 MB) at a time.
fn input(path: &Path, rows: i64) {
    let props = parquet::file::properties::WriterProperties::builder()
        .set_max_row_group_row_count(Some(4_000))
        .build();
    let file = std::fs::File::create(path).expect("the input file");
    let mut writer =
        parquet::arrow::ArrowWriter::try_new(file, schema(), Some(props)).expect("a writer");
    for from in (0..rows).step_by(4_000) {
        let ids: Vec<i64> = (from..(from + 4_000).min(rows)).collect();
        let batch = RecordBatch::try_new(
            schema(),
            vec![
                Arc::new(Int64Array::from(ids.clone())),
                Arc::new(StringArray::from(
                    ids.iter().map(|i| note(*i)).collect::<Vec<_>>(),
                )),
            ],
        )
        .expect("a batch");
        writer.write(&batch).expect("a batch");
    }
    writer.close().expect("closed");
}

/// A sink that keeps `hoard` bytes of its own memory, outside the arena, for every morsel it
/// is given, and never gives them back: what a sink that leaks does to the process, which no
/// share bounds.
struct Hoarder {
    hoard: usize,
    held: std::sync::Mutex<Vec<Vec<u8>>>,
    rows: AtomicU64,
}

impl Sink for Hoarder {
    fn open(&mut self, _schema: &SourceSchema) -> moruna_kernel::Result<()> {
        Ok(())
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    fn write(&self, _seq: Seq, payload: Payload) -> BoxFuture<'_, moruna_kernel::Result<()>> {
        Box::pin(async move {
            let Payload::Table(batch, _) = payload else {
                return Err(MorunaError::Sink("a table".into()));
            };
            self.rows
                .fetch_add(batch.num_rows() as u64, Ordering::SeqCst);
            // Touched, so it is resident; paced, so the controller's tick sees it grow.
            let kept = vec![1u8; self.hoard];
            self.held
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(kept);
            std::thread::sleep(std::time::Duration::from_millis(150));
            Ok(())
        })
    }

    fn finish(&mut self) -> moruna_kernel::Result<SinkSummary> {
        Ok(SinkSummary {
            rows: self.rows.load(Ordering::SeqCst),
            bytes: 0,
            files: Vec::new(),
        })
    }
}

/// The child: one run, from the job its parent left. A job document runs as `moruna run`
/// would run it; `{"hoard": ...}` runs a Parquet file into a `Hoarder`.
#[test]
#[ignore = "run by the tests below in a process of their own"]
fn child_run() {
    let Some(job) = apart::job() else {
        return;
    };
    let outcome = if let Some(hoard) = job.get("hoard") {
        let mut spec = RunSpec::new(
            moruna_runtime::SourceSpec::Build(Box::new({
                let url = job["url"].as_str().expect("a url").to_string();
                move |ctx| {
                    Ok(Arc::new(moruna_sources::ParquetSource::new(
                        moruna_sources::ParquetSourceConfig {
                            urls: vec![format!("file://{url}")],
                            ..Default::default()
                        },
                        ctx.reactor.clone(),
                        ctx.object_metadata()?,
                    )?) as Arc<dyn moruna_kernel::Source>)
                }
            })),
            Vec::new(),
            SinkSpec::Built(Box::new(Hoarder {
                hoard: hoard.as_u64().expect("bytes") as usize,
                held: std::sync::Mutex::new(Vec::new()),
                rows: AtomicU64::new(0),
            })),
        );
        spec.budget = job["budget"].as_u64();
        spec.cpu = Some(2.0);
        spec.staging_dir = job["staging"].as_str().map(Into::into);
        match Runtime::run(spec, CancelToken::new()) {
            Ok(report) => json!({"report": report}),
            Err(error) => json!({"error": error.error.to_string(), "report": error.report}),
        }
    } else {
        // What the process holds before the run, touched so it is resident.
        let held = vec![1u8; job.get("hold").and_then(|h| h.as_u64()).unwrap_or(0) as usize];
        let mut job = job;
        if let Some(doc) = job.as_object_mut() {
            doc.remove("hold");
        }
        let doc = JobSpec::from_value(job).expect("the document parses");
        let env = |_: &str| None;
        let outcome = match build(
            &doc,
            &NoKernels,
            BuildOptions {
                strict: true,
                env: &env,
                notes: Vec::new(),
            },
        ) {
            Err(e) => json!({"error": e.to_string()}),
            Ok(built) => match Runtime::run(built.spec, CancelToken::new()) {
                Ok(report) => json!({"report": report}),
                Err(e) => json!({"error": e.to_string()}),
            },
        };
        drop(std::hint::black_box(held));
        outcome
    };
    apart::answer(outcome);
}

/// A Parquet file into Parquet with no kernel in between, over data larger than the smaller
/// budget: the run's report states the whole process's peak (it said 0 for a chain with no
/// kernel until F8.9), and that peak, and the operating system's, are under the ceiling.
#[test]
fn a_copy_with_no_kernel_stays_under_the_ceiling() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("whole_copy");
    let file = scratch.path().join("in.parquet");
    input(&file, ROWS);
    assert!(std::fs::metadata(&file).expect("the file").len() > BUDGETS[0]);
    for budget in BUDGETS {
        let out = scratch.path().join(format!("out-{budget}"));
        let outcome = apart::run(
            "child_run",
            &json!({
                "moruna_spec": 1,
                "source": {"kind": "parquet", "url": file},
                "sink": {"kind": "parquet", "url": out},
                "budget": {"memory_bytes": budget, "cpu": 2.0},
                "staging": {"dir": scratch.path().join("staging"), "limit_bytes": 1u64 << 30},
                "profiles_dir": scratch.path().join("profiles"),
            }),
        );
        apart::within(&outcome, "a copy with no kernel", budget);
        assert_eq!(support::read_back_rows(&out), ROWS as u64);
    }
}

/// A run whose sink keeps memory outside the arena that no share bounds grows the process toward
/// its ceiling; the controller, seeing the whole process, stops the run with a diagnostic before
/// the ceiling rather than let the operating system stop it, and the report and the operating
/// system agree that the process never reached the ceiling. The sink keeps a mebibyte a morsel,
/// slower than a controller tick can miss.
#[test]
fn the_controller_stops_a_process_outgrowing_its_ceiling() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("whole_hoard");
    let file = scratch.path().join("in.parquet");
    input(&file, ROWS);
    let budget = BUDGETS[0];
    let outcome = apart::run(
        "child_run",
        &json!({
            "hoard": 1u64 << 20,
            "url": file,
            "budget": budget,
            "staging": scratch.path().join("staging"),
        }),
    );
    let error = outcome["error"].as_str().unwrap_or_else(|| {
        panic!("the run was stopped: {outcome}");
    });
    assert!(error.starts_with("budget:"), "{error}");
    let notes = outcome["report"]["notes"].as_array().expect("a report");
    assert!(
        notes
            .iter()
            .filter_map(|n| n.as_str())
            .any(|n| n.contains("rather than let the operating system stop it")),
        "{notes:?}"
    );
    apart::peaks_within(&outcome, "a sink that keeps what it writes", budget);
}

/// N-7: a budget under the run's floor is refused before anything is built, and the refusal
/// names the floor: the least budget the run can be honoured in. The floor counts what the
/// process already holds, so a process holding most of its budget before the run meets it at
/// the smallest budget there is.
#[test]
fn a_budget_under_the_floor_is_refused_with_the_floor() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("whole_floor");
    let file = scratch.path().join("in.parquet");
    input(&file, 4_000);
    let budget = BUDGETS[0];
    let outcome = apart::run(
        "child_run",
        &json!({
            "hold": 200u64 << 20,
            "moruna_spec": 1,
            "source": {"kind": "parquet", "url": file},
            "sink": {"kind": "parquet", "url": scratch.path().join("out")},
            "budget": {"memory_bytes": budget, "cpu": 1.0},
        }),
    );
    let message = outcome["error"].as_str().expect("refused");
    assert!(message.starts_with("config budget.host:"), "{message}");
    let floor: u64 = message
        .split("floor of ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("the floor is named: {message}"));
    assert!(floor > budget, "{message}");
    assert!(message.contains("an arena of"), "{message}");
    assert!(!scratch.path().join("out").exists(), "nothing was written");
}
