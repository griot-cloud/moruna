//! A run as a DAG (SDD 22 e.4, SDD 10 f.16, contracts d.16), over real components: real Parquet
//! files, real Parquet sources and sinks, the standard kernels, the real arena, reactor,
//! placement engine, scheduler and controller.
//!
//! SC-T27 fan-out shares the batch, SC-T28 fan-in, SC-T29 the diamond, SC-T30 a job document
//! version 2 run from its FlatBuffers, SC-T31 random DAGs against a reference computed directly,
//! JB-T38 a barrier the build refuses to go around.

#![allow(clippy::result_large_err)]

mod support;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use moruna_job::{Edge, Job, KindDoc, Task};
use moruna_kernel::arrow::array::{Array, Int64Array};
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{
    Allocator, BoxFuture, CancelToken, Consumer, Fingerprint, InitCtx, Kernel, KernelKind,
    KernelState, Link, NoState, Payload, PayloadKind, PayloadSpec, Producer, Result as MResult,
    RowRange, SourceSchema, Split, Tier, TierPref, Topology,
};
use moruna_runtime::job::{BuildOptions, build};
use moruna_runtime::kinds::Registry;
use moruna_runtime::{RunReport, RunSpec, Runtime, SinkSpec, SourceSpec};
use serde_json::json;
use support::{Scratch, one_run_at_a_time, write_parquet};

// ------------------------------------------------------------------------------------------
// Parts.
// ------------------------------------------------------------------------------------------

/// Every read a source made, as `(split, row_start, row_end)`.
type Reads = Arc<Mutex<Vec<(u32, u64, u64)>>>;

/// A Parquet source that records every row it reads, so a test can tell a source read once from
/// one read again for each consumer.
struct Counted {
    inner: Arc<dyn moruna_kernel::Source>,
    reads: Reads,
}

impl moruna_kernel::Source for Counted {
    fn schema(&self) -> SourceSchema {
        self.inner.schema()
    }

    fn plan(&self) -> MResult<Vec<Split>> {
        self.inner.plan()
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
        self.reads.lock().unwrap_or_else(|e| e.into_inner()).push((
            split.id,
            range.start,
            range.end,
        ));
        self.inner.read(split, rows, alloc, tier)
    }

    fn repeatable(&self) -> bool {
        self.inner.repeatable()
    }
}

fn parquet_source(path: &Path, reads: Option<Reads>) -> SourceSpec {
    let url = format!("file://{}", path.display());
    SourceSpec::Build(Box::new(move |ctx| {
        let inner = Arc::new(moruna_sources::ParquetSource::new(
            moruna_sources::ParquetSourceConfig {
                urls: vec![url],
                ..Default::default()
            },
            ctx.reactor.clone(),
            ctx.object_metadata()?,
        )?) as Arc<dyn moruna_kernel::Source>;
        Ok(match reads {
            Some(reads) => Arc::new(Counted { inner, reads }) as Arc<dyn moruna_kernel::Source>,
            None => inner,
        })
    }))
}

fn parquet_sink(dir: &Path) -> SinkSpec {
    std::fs::create_dir_all(dir).expect("the output directory");
    let url = format!("file://{}", dir.display());
    SinkSpec::Build(Box::new(move |ctx| {
        moruna_sinks::ParquetSink::new(
            moruna_sinks::ParquetSinkConfig {
                url: url.clone(),
                row_group_bytes: 1 << 20,
                file_bytes: 4 << 20,
                ..Default::default()
            },
            ctx.reactor.clone(),
            ctx.alloc.clone(),
        )
        .map(|sink| Box::new(sink.with_run_id(ctx.run_id)) as Box<dyn moruna_kernel::Sink>)
    }))
}

/// A kernel that hands its input on as it is and records where its first column's values are:
/// two consumers of one fanned-out batch see the same bytes at the same address.
struct Tap {
    seen: Arc<Mutex<BTreeSet<usize>>>,
}

impl Kernel for Tap {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint::compute("moruna-runtime::tests::dag::Tap", b"v1")
    }

    fn kind(&self) -> KernelKind {
        KernelKind::Stateless
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    fn output_schema(&self, input: &SourceSchema) -> MResult<SourceSchema> {
        Ok(input.clone())
    }

    fn init(&self, _ctx: &InitCtx) -> MResult<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }

    fn apply(&self, _state: &mut dyn KernelState, input: Payload) -> MResult<Payload> {
        if let Payload::Table(batch, _) = &input {
            let at = batch.column(0).to_data().buffers()[0].as_ptr() as usize;
            self.seen
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(at);
        }
        Ok(input)
    }
}

fn std_kernel(name: &str, args: serde_json::Value) -> Arc<dyn Kernel> {
    Arc::new(moruna_kernels::StdKernel::new(name, &args).expect("a standard kernel"))
}

fn configure(spec: &mut RunSpec, scratch: &Scratch) {
    spec.budget = Some(1 << 30);
    spec.cpu = Some(4.0);
    spec.staging_dir = Some(scratch.path().join("staging"));
    spec.staging_limit = Some(1 << 30);
    spec.profiles_dir = Some(scratch.path().join("profiles"));
    std::fs::create_dir_all(scratch.path().join("staging")).expect("the staging directory");
}

fn run(spec: RunSpec) -> RunReport {
    match Runtime::run(spec, CancelToken::new()) {
        Ok(report) => report,
        Err(error) => panic!(
            "the run did not complete: {error}; notes: {:?}",
            error.shutdown_notes
        ),
    }
}

/// Every row of every Parquet file under `dir`, as `(id, value)`, sorted.
fn rows_in(dir: &Path) -> Vec<(i64, i64)> {
    let mut rows = Vec::new();
    for entry in std::fs::read_dir(dir).expect("the output directory") {
        let path = entry.expect("an entry").path();
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
            rows.extend(pairs(&batch));
        }
    }
    rows.sort_unstable();
    rows
}

fn pairs(batch: &RecordBatch) -> Vec<(i64, i64)> {
    let column = |name: &str| -> Int64Array {
        batch
            .column_by_name(name)
            .and_then(|c| c.as_any().downcast_ref::<Int64Array>().cloned())
            .expect("an Int64 column")
    };
    let (ids, values) = (column("id"), column("value"));
    (0..batch.num_rows())
        .map(|i| (ids.value(i), values.value(i)))
        .collect()
}

/// `rows` rows of the test schema: ids from `from`, values twice the id.
fn expected(from: i64, rows: i64) -> Vec<(i64, i64)> {
    (from..from + rows).map(|id| (id, id * 2)).collect()
}

fn k(stage: u16, port: u16) -> Consumer {
    Consumer::Kernel { stage, port }
}

// ------------------------------------------------------------------------------------------
// The shapes.
// ------------------------------------------------------------------------------------------

/// SC-T27 fan_out_shares_the_batch: one Parquet source to two kernels to two sinks. Each sink
/// has every row; the source is read once, every range of it exactly once; and the two kernels
/// were handed the same batches at the same addresses, so a fanned-out batch is resident once
/// and shared, never copied (contracts d.16, PL-I16).
#[test]
fn sc_t27_fan_out_shares_the_batch() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("sc_t27");
    let input = scratch.path().join("in.parquet");
    write_parquet(&input, 60_000, 6);
    let reads = Arc::new(Mutex::new(Vec::new()));
    let (seen_a, seen_b) = (Arc::default(), Arc::default());
    let kernels: Vec<Arc<dyn Kernel>> = vec![
        Arc::new(Tap {
            seen: Arc::clone(&seen_a),
        }),
        Arc::new(Tap {
            seen: Arc::clone(&seen_b),
        }),
    ];
    let topology = Topology::new(
        1,
        2,
        2,
        vec![
            Link {
                from: Producer::Source(0),
                to: k(1, 0),
            },
            Link {
                from: Producer::Source(0),
                to: k(2, 0),
            },
            Link {
                from: Producer::Kernel(1),
                to: Consumer::Sink(0),
            },
            Link {
                from: Producer::Kernel(2),
                to: Consumer::Sink(1),
            },
        ],
    )
    .expect("a fan-out");
    let (out_a, out_b) = (scratch.path().join("a"), scratch.path().join("b"));
    let mut spec = RunSpec::graph(
        vec![parquet_source(&input, Some(Arc::clone(&reads)))],
        kernels,
        vec![parquet_sink(&out_a), parquet_sink(&out_b)],
        topology,
    );
    configure(&mut spec, &scratch);
    let report = run(spec);
    assert_eq!(rows_in(&out_a), expected(0, 60_000), "sink a has every row");
    assert_eq!(rows_in(&out_b), expected(0, 60_000), "sink b has every row");
    let reads = reads.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let distinct: BTreeSet<_> = reads.iter().copied().collect();
    assert_eq!(
        distinct.len(),
        reads.len(),
        "no range was read twice: {reads:?}"
    );
    let read_rows: u64 = reads.iter().map(|(_, start, end)| end - start).sum();
    assert_eq!(read_rows, 60_000, "the source was read once");
    let (a, b) = (
        seen_a.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        seen_b.lock().unwrap_or_else(|e| e.into_inner()).clone(),
    );
    assert!(!a.is_empty());
    assert_eq!(a, b, "both consumers were handed the same bytes");
    assert_eq!(report.stages.len(), 2);
}

/// SC-T28 fan_in: two Parquet sources into the two ports of a `union`, then a sink. The sink
/// has every row of both, of sources of different lengths, so the numbers one source's morsels
/// would have had past the other's end are skipped and the run completes.
#[test]
fn sc_t28_fan_in_of_two_sources() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("sc_t28");
    let (left, right) = (
        scratch.path().join("l.parquet"),
        scratch.path().join("r.parquet"),
    );
    write_parquet(&left, 50_000, 5);
    write_parquet(&right, 7_000, 1);
    let topology = Topology::new(
        2,
        1,
        1,
        vec![
            Link {
                from: Producer::Source(0),
                to: k(1, 0),
            },
            Link {
                from: Producer::Source(1),
                to: k(1, 1),
            },
            Link {
                from: Producer::Kernel(1),
                to: Consumer::Sink(0),
            },
        ],
    )
    .expect("a fan-in");
    let out = scratch.path().join("out");
    let mut spec = RunSpec::graph(
        vec![parquet_source(&left, None), parquet_source(&right, None)],
        vec![std_kernel("union", json!({}))],
        vec![parquet_sink(&out)],
        topology,
    );
    configure(&mut spec, &scratch);
    spec.ordered = true;
    run(spec);
    let mut want = expected(0, 50_000);
    want.extend(expected(0, 7_000));
    want.sort_unstable();
    assert_eq!(rows_in(&out), want);
}

/// SC-T29 diamond: a source to A (`id < 30000`) and B (`id >= 30000`), both into C's two
/// ports, C to a sink: every row once, through one branch or the other.
#[test]
fn sc_t29_diamond() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("sc_t29");
    let input = scratch.path().join("in.parquet");
    write_parquet(&input, 60_000, 6);
    let topology = Topology::new(
        1,
        3,
        1,
        vec![
            Link {
                from: Producer::Source(0),
                to: k(1, 0),
            },
            Link {
                from: Producer::Source(0),
                to: k(2, 0),
            },
            Link {
                from: Producer::Kernel(1),
                to: k(3, 0),
            },
            Link {
                from: Producer::Kernel(2),
                to: k(3, 1),
            },
            Link {
                from: Producer::Kernel(3),
                to: Consumer::Sink(0),
            },
        ],
    )
    .expect("a diamond");
    let out = scratch.path().join("out");
    let mut spec = RunSpec::graph(
        vec![parquet_source(&input, None)],
        vec![
            std_kernel("filter", json!({"expr": "id < 30000"})),
            std_kernel("filter", json!({"expr": "id >= 30000"})),
            std_kernel("union", json!({})),
        ],
        vec![parquet_sink(&out)],
        topology,
    );
    configure(&mut spec, &scratch);
    let report = run(spec);
    assert_eq!(rows_in(&out), expected(0, 60_000));
    assert_eq!(report.stages.len(), 3);
}

// ------------------------------------------------------------------------------------------
// The document.
// ------------------------------------------------------------------------------------------

fn entry(value: serde_json::Value) -> KindDoc {
    KindDoc::from_value(value).expect("an entry")
}

fn opts() -> BuildOptions<'static> {
    BuildOptions {
        strict: false,
        env: &|_| None,
        notes: Vec::new(),
        packages: Default::default(),
        host_ports: None,
    }
}

fn configured(mut job: Job, scratch: &Scratch) -> Job {
    let staging = scratch.path().join("staging");
    std::fs::create_dir_all(&staging).expect("the staging directory");
    job.budget.memory_bytes = Some(1 << 30);
    job.budget.cpu = Some(4.0);
    job.staging.dir = Some(staging.display().to_string());
    job.staging.limit_bytes = Some(1 << 30);
    job.profiles_dir = Some(scratch.path().join("profiles").display().to_string());
    job
}

/// SC-T30 a_document_runs: a job document version 2, a fan-out and a diamond in one DAG, written
/// as FlatBuffers, read back from its bytes and built through the registry of kinds: each sink
/// has what its branch gives.
#[test]
fn sc_t30_a_version_2_document_runs() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("sc_t30");
    let input = scratch.path().join("in.parquet");
    write_parquet(&input, 40_000, 4);
    let url = |p: &Path| format!("file://{}", p.display());
    let (all, low) = (scratch.path().join("all"), scratch.path().join("low"));
    std::fs::create_dir_all(&all).expect("dir");
    std::fs::create_dir_all(&low).expect("dir");
    let job = configured(
        Job::new(
            vec![
                Task::source("in", entry(json!({"kind": "parquet", "url": url(&input)}))),
                Task::kernel(
                    "low",
                    entry(json!({"kind": "std", "name": "filter", "args": {"expr": "id < 1000"}})),
                ),
                Task::kernel(
                    "high",
                    entry(json!({"kind": "std", "name": "filter", "args": {"expr": "id >= 1000"}})),
                ),
                Task::kernel_with_inputs(
                    "join",
                    entry(json!({"kind": "std", "name": "union"})),
                    &["a", "b"],
                ),
                Task::sink("all", entry(json!({"kind": "parquet", "url": url(&all)}))),
                Task::sink(
                    "low-only",
                    entry(json!({"kind": "parquet", "url": url(&low)})),
                ),
            ],
            vec![
                Edge::between("in", "low"),
                Edge::between("in", "high"),
                Edge::new("low", "out", "join", "a"),
                Edge::new("high", "out", "join", "b"),
                Edge::between("join", "all"),
                Edge::between("low", "low-only"),
            ],
        ),
        &scratch,
    );
    let bytes = job.to_flatbuffer().expect("encodes");
    let read = Job::from_flatbuffer(&bytes).expect("reads back");
    assert_eq!(read, job);
    let built = build(&read, &Registry::standard(), opts()).expect("builds");
    assert_eq!(built.parts.sources, vec![0]);
    assert_eq!(built.parts.sinks, vec![4, 5]);
    assert_eq!(built.parts.stages, vec![vec![1], vec![2], vec![3]]);
    run(built.spec);
    assert_eq!(rows_in(&all), expected(0, 40_000));
    assert_eq!(rows_in(&low), expected(0, 1_000));
}

/// JB-T38 a_barrier_is_not_gone_around: the build refuses a document whose edge goes around a
/// barrier, naming the edge, before any kind is built; the same document through the barrier
/// builds.
#[test]
fn jb_t38_the_build_refuses_to_go_around_a_barrier() {
    let scratch = Scratch::new("jb_t38");
    let input = scratch.path().join("in.parquet");
    write_parquet(&input, 10, 1);
    let url = format!("file://{}", input.display());
    let out = format!("file://{}", scratch.path().join("out").display());
    let mut job = Job::new(
        vec![
            Task::source("in", entry(json!({"kind": "parquet", "url": url}))),
            Task::kernel("mask", entry(json!({"kind": "std", "name": "mask", "args": {"columns": ["value"], "mode": "redact"}}))).barrier(),
            Task::kernel_with_inputs("join", entry(json!({"kind": "std", "name": "union"})), &["a", "b"]),
            Task::sink("out", entry(json!({"kind": "parquet", "url": out}))),
        ],
        vec![
            Edge::between("in", "mask"),
            Edge::new("mask", "out", "join", "a"),
            Edge::new("in", "out", "join", "b"),
            Edge::between("join", "out"),
        ],
    );
    let refused = build(&job, &Registry::standard(), opts())
        .err()
        .expect("refused")
        .to_string();
    assert!(
        refused.contains("edges[2]") && refused.contains("around the barrier `mask`"),
        "{refused}"
    );
    job.tasks[2] = Task::kernel_with_inputs(
        "join",
        entry(json!({"kind": "std", "name": "union"})),
        &["a"],
    );
    job.edges.remove(2);
    assert!(build(&job, &Registry::standard(), opts()).is_ok());
}

// ------------------------------------------------------------------------------------------
// SC-T31: random DAGs.
// ------------------------------------------------------------------------------------------

/// SplitMix64, seeded, so a failing DAG is named by its seed.
struct Draw(u64);

impl Draw {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// One kernel of a random DAG: a standard kernel that keeps the schema, so any output may feed
/// any input, and what it does to rows, to compute the reference.
#[derive(Clone, Debug)]
enum Op {
    /// `filter id < n`.
    Below(i64),
    /// `filter id >= n`.
    AtLeast(i64),
    /// `fill_null`, which keeps every row of a column with no nulls.
    Fill,
    /// `select` of both columns.
    Both,
    /// `union` of its inputs.
    Union(usize),
}

impl Op {
    fn kernel(&self) -> Arc<dyn Kernel> {
        match self {
            Op::Below(n) => std_kernel("filter", json!({"expr": format!("id < {n}")})),
            Op::AtLeast(n) => std_kernel("filter", json!({"expr": format!("id >= {n}")})),
            Op::Fill => std_kernel("fill_null", json!({"values": {"value": 0}})),
            Op::Both => std_kernel("select", json!({"columns": ["id", "value"]})),
            Op::Union(_) => std_kernel("union", json!({})),
        }
    }

    fn rows(&self, inputs: &[Vec<(i64, i64)>]) -> Vec<(i64, i64)> {
        let all: Vec<(i64, i64)> = inputs.iter().flatten().copied().collect();
        match self {
            Op::Below(n) => all.into_iter().filter(|(id, _)| id < n).collect(),
            Op::AtLeast(n) => all.into_iter().filter(|(id, _)| id >= n).collect(),
            Op::Fill | Op::Both | Op::Union(_) => all,
        }
    }
}

/// A random DAG of at most eight tasks: one or two sources, kernels in an order in which every
/// edge goes forward, each kernel's inputs drawn from what is before it, every output read, and
/// one or two sinks reading what nothing else reads.
struct Random {
    sources: Vec<i64>,
    ops: Vec<Op>,
    /// Each kernel's inputs, port order: `Producer`s.
    inputs: Vec<Vec<Producer>>,
    /// Each sink's input.
    sinks: Vec<Producer>,
}

fn random_dag(draw: &mut Draw) -> Random {
    let sources: Vec<i64> = (0..=draw.below(2))
        .map(|_| 200 + draw.below(3_000) as i64)
        .collect();
    let kernels = draw.below(8 - sources.len() as u64 - 1) as usize;
    let mut producers: Vec<Producer> = (0..sources.len() as u16).map(Producer::Source).collect();
    let mut ops: Vec<Op> = Vec::new();
    let mut inputs: Vec<Vec<Producer>> = Vec::new();
    for at in 0..kernels {
        let stage = at as u16 + 1;
        let op = match draw.below(5) {
            0 => Op::Below(draw.below(3_000) as i64),
            1 => Op::AtLeast(draw.below(3_000) as i64),
            2 => Op::Fill,
            3 => Op::Both,
            _ => Op::Union(2 + draw.below(2) as usize),
        };
        let ports = match op {
            Op::Union(n) => n,
            _ => 1,
        };
        let mine: Vec<Producer> = (0..ports)
            .map(|_| producers[draw.below(producers.len() as u64) as usize])
            .collect();
        inputs.push(mine);
        ops.push(op);
        producers.push(Producer::Kernel(stage));
    }
    // Every producer nothing reads goes to a sink; one more sink reads anything, when there is
    // room in the eight tasks.
    let read: BTreeSet<Producer> = inputs.iter().flatten().copied().collect();
    let mut sinks: Vec<Producer> = producers
        .iter()
        .copied()
        .filter(|p| !read.contains(p))
        .collect();
    if sources.len() + kernels + sinks.len() < 8 && draw.below(2) == 0 {
        sinks.push(producers[draw.below(producers.len() as u64) as usize]);
    }
    Random {
        sources,
        ops,
        inputs,
        sinks,
    }
}

impl Random {
    fn topology(&self) -> Topology {
        let mut links = Vec::new();
        for (at, ports) in self.inputs.iter().enumerate() {
            for (port, from) in ports.iter().enumerate() {
                links.push(Link {
                    from: *from,
                    to: k(at as u16 + 1, port as u16),
                });
            }
        }
        for (at, from) in self.sinks.iter().enumerate() {
            links.push(Link {
                from: *from,
                to: Consumer::Sink(at as u16),
            });
        }
        Topology::new(
            self.sources.len() as u16,
            self.ops.len() as u16,
            self.sinks.len() as u16,
            links,
        )
        .expect("a random DAG is a topology")
    }

    /// What each sink holds, computed directly: each source's rows, each kernel's function of
    /// its inputs' rows, once per edge.
    fn reference(&self) -> Vec<Vec<(i64, i64)>> {
        let mut out: Vec<Vec<(i64, i64)>> = Vec::new();
        let of = |p: &Producer, out: &Vec<Vec<(i64, i64)>>| -> Vec<(i64, i64)> {
            match p {
                Producer::Source(s) => expected(0, self.sources[usize::from(*s)]),
                Producer::Kernel(stage) => out[usize::from(*stage) - 1].clone(),
            }
        };
        for (at, op) in self.ops.iter().enumerate() {
            let given: Vec<Vec<(i64, i64)>> = self.inputs[at].iter().map(|p| of(p, &out)).collect();
            out.push(op.rows(&given));
        }
        self.sinks
            .iter()
            .map(|p| {
                let mut rows = of(p, &out);
                rows.sort_unstable();
                rows
            })
            .collect()
    }
}

/// SC-T31 random_dags_run_to_their_reference: seeded random DAGs of at most eight tasks, of one
/// or two Parquet sources, standard kernels that keep the schema (`filter`, `fill_null`,
/// `select`, `union` of two or three inputs) and one or more Parquet sinks, run to completion,
/// and each sink holds exactly the rows a direct computation says it does.
#[test]
fn sc_t31_random_dags_run_to_their_reference() {
    let _serial = one_run_at_a_time();
    let seed = std::env::var("MORUNA_DAG_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5c_7310_u64);
    let cases = std::env::var("MORUNA_DAG_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(12u64);
    println!("MORUNA_DAG_SEED={seed} MORUNA_DAG_CASES={cases}");
    let mut draw = Draw(seed);
    for case in 0..cases {
        let scratch = Scratch::new(&format!("sc_t31_{case}"));
        let dag = random_dag(&mut draw);
        let inputs: Vec<PathBuf> = dag
            .sources
            .iter()
            .enumerate()
            .map(|(at, rows)| {
                let path = scratch.path().join(format!("in{at}.parquet"));
                write_parquet(&path, *rows as u64, 1 + (*rows as u64 / 700));
                path
            })
            .collect();
        let outs: Vec<PathBuf> = (0..dag.sinks.len())
            .map(|at| scratch.path().join(format!("out{at}")))
            .collect();
        let mut spec = RunSpec::graph(
            inputs.iter().map(|p| parquet_source(p, None)).collect(),
            dag.ops.iter().map(Op::kernel).collect(),
            outs.iter().map(|p| parquet_sink(p)).collect(),
            dag.topology(),
        );
        configure(&mut spec, &scratch);
        run(spec);
        for (at, want) in dag.reference().into_iter().enumerate() {
            assert_eq!(
                rows_in(&outs[at]),
                want,
                "case {case} (seed {seed}) sink {at} of {:?} {:?} {:?} {:?}",
                dag.sources,
                dag.ops,
                dag.inputs,
                dag.sinks
            );
        }
    }
}
