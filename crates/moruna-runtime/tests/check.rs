//! H13 through the Rust harness (15 k): a kernel that disagrees with its declaration is refused
//! naming the column (CK-T1); the profile row a check writes is read by a later run's sizer
//! (CK-T2); a check never overwrites a profile (CK-T4); every standard kernel passes (CK-T5).

#![allow(clippy::result_large_err)]

mod support;

use std::sync::{Arc, Mutex};

use moruna_kernel::arrow::datatypes::DataType;
use moruna_kernel::declare::{ColumnDecl, Declared, SchemaDecl};
use moruna_kernel::{
    CancelToken, Fingerprint, InitCtx, Kernel, KernelHints, KernelKind, KernelState, Payload,
    PayloadSpec, SourceSchema,
};
use moruna_runtime::check::{CheckOptions, Verdict, check};
use moruna_runtime::spec::{KernelEntry, build_kernels};
use moruna_runtime::{RunSpec, Runtime, SinkSpec, SourceSpec};
use serde_json::{Value, json};
use support::{Appender, Doubler, Scratch, one_run_at_a_time, write_parquet};

/// A kernel with a declaration, over one of the support kernels. It remembers the input schema
/// the run's chain walk hands it, so a test can say which schema the run keyed its profile by.
struct Declaring {
    inner: Arc<dyn Kernel>,
    declared: Declared,
    seen: Mutex<Option<SourceSchema>>,
}

impl Declaring {
    fn new(inner: Arc<dyn Kernel>, declared: Declared) -> Arc<Declaring> {
        Arc::new(Declaring {
            inner,
            declared,
            seen: Mutex::new(None),
        })
    }
}

impl Kernel for Declaring {
    fn fingerprint(&self) -> Fingerprint {
        self.inner.fingerprint()
    }
    fn kind(&self) -> KernelKind {
        self.inner.kind()
    }
    fn hints(&self) -> KernelHints {
        self.inner.hints()
    }
    fn declared(&self) -> Declared {
        self.declared.clone()
    }
    fn accepts(&self) -> PayloadSpec {
        self.inner.accepts()
    }
    fn output_schema(&self, input: &SourceSchema) -> moruna_kernel::Result<SourceSchema> {
        *self.seen.lock().expect("lock") = Some(input.clone());
        self.inner.output_schema(input)
    }
    fn init(&self, ctx: &InitCtx) -> moruna_kernel::Result<Box<dyn KernelState>> {
        self.inner.init(ctx)
    }
    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> moruna_kernel::Result<Payload> {
        self.inner.apply(state, input)
    }
}

fn unchanged() -> SchemaDecl {
    SchemaDecl::Relative {
        adds: Vec::new(),
        drops: Vec::new(),
        changes: Vec::new(),
    }
}

#[test]
fn ck_t1_h13_a_kernel_that_disagrees_is_refused_naming_the_column() {
    // The appender adds a boolean column; its author declared an integer one.
    let liar = Declaring::new(
        Arc::new(Appender::new("loud")),
        Declared {
            input: Some(SchemaDecl::from_schema(&support::schema())),
            output: Some(SchemaDecl::Relative {
                adds: vec![ColumnDecl::new("loud", DataType::Int64)],
                drops: Vec::new(),
                changes: Vec::new(),
            }),
        },
    );
    let report = check(liar, CheckOptions::new("liar")).expect("the harness runs");
    assert_eq!(report.verdict, Verdict::Refused);
    assert_eq!(report.exit_code(), 2);
    let refusals = report.refusals();
    assert!(!refusals.is_empty());
    for (_, d) in &refusals {
        assert_eq!(d.column(), "loud");
        assert_eq!(d.reason(), "type");
        let text = d.to_string();
        assert!(text.contains("int64") && text.contains("bool"), "{text}");
    }
    let json = report.to_json();
    assert_eq!(json["refusals"][0]["column"], "loud");
    assert_eq!(json["refusals"][0]["declared"], "int64");
    assert_eq!(json["refusals"][0]["produced"], "bool");
    assert!(
        report
            .summary()
            .contains("column `loud`: declared int64, produced bool")
    );
    assert!(report.profile.is_none(), "a refused kernel seeds nothing");

    // The same kernel, declared truthfully, agrees.
    let honest = Declaring::new(
        Arc::new(Appender::new("loud")),
        Declared {
            input: Some(SchemaDecl::from_schema(&support::schema())),
            output: Some(SchemaDecl::Relative {
                adds: vec![ColumnDecl::new("loud", DataType::Boolean)],
                drops: Vec::new(),
                changes: Vec::new(),
            }),
        },
    );
    let report = check(honest, CheckOptions::new("honest")).expect("the harness runs");
    assert_eq!(report.verdict, Verdict::Agreed, "{}", report.summary());
    assert_eq!(report.batches.len(), 5);
    assert_eq!(report.trace_records, 5, "one trace record per batch");
}

#[test]
fn ck_t2_h13_the_profile_row_a_check_writes_is_read_by_a_later_run() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("ck_t2");
    let profiles = scratch.path().join("profiles");
    let declared = Declared {
        input: Some(SchemaDecl::from_schema(&support::schema())),
        output: Some(unchanged()),
    };
    let kernel = Declaring::new(Arc::new(Doubler::new()), declared.clone());

    let mut opts = CheckOptions::new("doubler");
    opts.profiles_dir = Some(profiles.clone());
    let report = check(kernel.clone(), opts).expect("the harness runs");
    assert_eq!(report.verdict, Verdict::Agreed, "{}", report.summary());
    let row = report.profile.clone().expect("a profile");
    assert!(row.written);
    let path = row.path.clone().expect("a path");
    let written: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("the row")).expect("json");
    assert_eq!(written["runs"], 1);
    assert_eq!(written["source"], "moruna check");
    assert_eq!(written["fingerprint"], kernel.fingerprint().to_hex());
    let check_samples = written["a_k_samples"].as_u64().expect("samples");
    assert!(check_samples >= 1, "the preferred batch gives a sample");

    // A real run over a Parquet file of that schema, with the same store.
    let input = scratch.path().join("in.parquet");
    let out_dir = scratch.path().join("out");
    std::fs::create_dir_all(&out_dir).expect("out");
    std::fs::create_dir_all(scratch.path().join("staging")).expect("staging");
    // Large enough that the controller measures it and writes its profile (11 f.3: a dataset
    // that fits four times over in the budget is not measured).
    write_parquet(&input, 4_000_000, 16);
    let source_path = input.clone();
    let sink_url = format!("file://{}", out_dir.display());
    let mut spec = RunSpec::new(
        SourceSpec::Build(Box::new(move |ctx| {
            moruna_sources::ParquetSource::new(
                moruna_sources::ParquetSourceConfig {
                    urls: vec![format!("file://{}", source_path.display())],
                    ..Default::default()
                },
                ctx.reactor.clone(),
                ctx.object_metadata()?,
            )
            .map(|source| Arc::new(source) as Arc<dyn moruna_kernel::Source>)
        })),
        vec![kernel.clone() as Arc<dyn Kernel>],
        SinkSpec::Build(Box::new(move |ctx| {
            moruna_sinks::ParquetSink::new(
                moruna_sinks::ParquetSinkConfig {
                    url: sink_url.clone(),
                    row_group_bytes: 2 << 20,
                    file_bytes: 8 << 20,
                    ..Default::default()
                },
                ctx.reactor.clone(),
                ctx.alloc.clone(),
            )
            .map(|sink| Box::new(sink.with_run_id(ctx.run_id)) as Box<dyn moruna_kernel::Sink>)
        })),
    );
    spec.budget = Some(256 << 20);
    spec.cpu = Some(2.0);
    spec.staging_dir = Some(scratch.path().join("staging"));
    spec.staging_limit = Some(1 << 30);
    spec.profiles_dir = Some(profiles.clone());
    let report = match Runtime::run(spec, CancelToken::new()) {
        Ok(report) => report,
        Err(error) => panic!("the run did not complete: {error}"),
    };
    assert!(
        !report.notes.iter().any(|n| n.contains("small dataset")),
        "the dataset must be large enough for the controller to measure: {:?}",
        report.notes
    );

    // The run's stage input schema is the declared one, so it looked up this very row...
    let seen = kernel
        .seen
        .lock()
        .expect("lock")
        .clone()
        .expect("the chain walk");
    let SourceSchema::Table(seen) = seen else {
        panic!("a table")
    };
    assert_eq!(
        SourceSchema::Table(seen.clone()).hash(),
        SourceSchema::Table(support::schema()).hash(),
        "the run keyed its profile by {seen:?}"
    );
    // ...loaded it, and merged its own figures into it (11 e.3: runs adds one, samples add up).
    let merged: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("the row")).expect("json");
    assert_eq!(
        merged["runs"], 2,
        "the run read the row the check wrote: {merged}"
    );
    assert!(merged["a_k_samples"].as_u64().expect("samples") > check_samples);
}

#[test]
fn ck_t4_a_check_never_overwrites_a_profile() {
    let scratch = Scratch::new("ck_t4");
    let kernel = Declaring::new(
        Arc::new(Doubler::new()),
        Declared {
            input: Some(SchemaDecl::from_schema(&support::schema())),
            output: Some(unchanged()),
        },
    );
    let run = |notes_expected: bool| {
        let mut opts = CheckOptions::new("doubler");
        opts.profiles_dir = Some(scratch.path().to_path_buf());
        opts.seed = 7;
        let report = check(kernel.clone(), opts).expect("the harness runs");
        assert_eq!(report.verdict, Verdict::Agreed);
        let row = report.profile.expect("row");
        assert_eq!(row.written, !notes_expected);
        assert_eq!(!report.notes.is_empty(), notes_expected);
        row.path.expect("path")
    };
    let path = run(false);
    std::fs::write(&path, "{\"evidence\": true}").expect("overwrite by hand");
    let again = run(true);
    assert_eq!(path, again);
    assert_eq!(
        std::fs::read_to_string(&path).expect("row"),
        "{\"evidence\": true}"
    );
}

#[test]
fn ck_t5_every_standard_kernel_passes_check() {
    let chain = [
        ("cast", json!({"columns": {"a": "double"}})),
        ("rename", json!({"columns": {"a": "b"}})),
        ("select", json!({"columns": ["a"]})),
        ("drop", json!({"columns": ["a"]})),
        (
            "filter",
            json!({"expr": "a > 1 and s != 'x' or is_null(b)"}),
        ),
        ("fill_null", json!({"values": {"a": 0, "s": "?"}})),
        ("dedupe", json!({"keys": ["a"]})),
        ("hash", json!({"columns": ["a", "b"], "algo": "blake3"})),
        (
            "mask",
            json!({"columns": ["s"], "mode": "partial", "keep": 2}),
        ),
        ("explode", json!({"column": "l"})),
        (
            "concat_str",
            json!({"columns": ["a", "b"], "separator": "|"}),
        ),
        ("date_trunc", json!({"column": "t", "unit": "month"})),
    ];
    let scratch = Scratch::new("ck_t5");
    for (name, args) in chain {
        let entry = KernelEntry::std(name, args.clone()).expect(name);
        let fingerprint = entry.fingerprint();
        let kernel = build_kernels(vec![entry]).pop().expect("one kernel");
        let mut opts = CheckOptions::new(name);
        opts.kind = "std";
        opts.fingerprint_scheme = "sha256";
        opts.profiles_dir = Some(scratch.path().to_path_buf());
        let report = check(kernel, opts).expect("the harness runs");
        assert_eq!(report.verdict, Verdict::Agreed, "{}", report.summary());
        assert_eq!(report.exit_code(), 0);
        assert_eq!(
            report.fingerprint,
            format!("sha256:{}", fingerprint.to_hex())
        );
        assert!(report.profile.expect("row").written, "{name}");
    }
    // Fused chains are checkable too.
    let fused = build_kernels(vec![
        KernelEntry::std("cast", json!({"columns": {"a": "double"}})).expect("cast"),
        KernelEntry::std("select", json!({"columns": ["a"]})).expect("select"),
        KernelEntry::std("fill_null", json!({"values": {"a": 0.5}})).expect("fill"),
        KernelEntry::std("filter", json!({"expr": "a > 0"})).expect("filter"),
    ]);
    assert_eq!(fused.len(), 2);
    for kernel in fused {
        let report = check(kernel, CheckOptions::new("fused")).expect("runs");
        assert_eq!(report.verdict, Verdict::Agreed, "{}", report.summary());
    }
}

#[test]
fn ck_not_checkable_and_plan_refusals() {
    // No declarations at all: not checkable, exit 2, and the reason says what is missing.
    let bare: Arc<dyn Kernel> = Arc::new(Doubler::new());
    let report = check(bare, CheckOptions::new("bare")).expect("runs");
    assert_eq!(report.verdict, Verdict::NotCheckable);
    assert_eq!(report.exit_code(), 2);
    assert!(
        report
            .reason
            .as_deref()
            .unwrap_or("")
            .contains("input_schema and output_schema")
    );
    assert_eq!(report.to_json()["verdict"], "not_checkable");

    // Only an input: the output is what is missing.
    let half = Declaring::new(
        Arc::new(Doubler::new()),
        Declared {
            input: Some(SchemaDecl::from_schema(&support::schema())),
            output: None,
        },
    );
    let report = check(half, CheckOptions::new("half")).expect("runs");
    assert!(
        report
            .reason
            .as_deref()
            .unwrap_or("")
            .contains("output_schema")
    );

    // A type the generator does not know.
    let odd = Declaring::new(
        Arc::new(Doubler::new()),
        Declared {
            input: Some(SchemaDecl::Subset(vec![ColumnDecl::new(
                "t",
                DataType::Time32(moruna_kernel::arrow::datatypes::TimeUnit::Second),
            )])),
            output: Some(unchanged()),
        },
    );
    let report = check(odd, CheckOptions::new("odd")).expect("runs");
    assert_eq!(report.verdict, Verdict::NotCheckable);
    assert!(report.summary().contains("column `t`"));

    // A relative input declaration describes no input.
    let relative = Declaring::new(
        Arc::new(Doubler::new()),
        Declared {
            input: Some(unchanged()),
            output: Some(unchanged()),
        },
    );
    assert_eq!(
        check(relative, CheckOptions::new("r"))
            .expect("runs")
            .verdict,
        Verdict::NotCheckable
    );

    // A standard kernel whose own declaration its output_schema rejects is refused before any
    // batch: a declared output that names a column the input lacks.
    let drops_ghost = Declaring::new(
        Arc::new(Doubler::new()),
        Declared {
            input: Some(SchemaDecl::from_schema(&support::schema())),
            output: Some(SchemaDecl::Relative {
                adds: Vec::new(),
                drops: vec!["ghost".into()],
                changes: Vec::new(),
            }),
        },
    );
    let report = check(drops_ghost, CheckOptions::new("ghost")).expect("runs");
    assert_eq!(report.verdict, Verdict::Refused);
    assert!(report.reason.as_deref().unwrap_or("").contains("ghost"));

    // The doubler needs a `value` column; declaring an input without one fails every batch
    // with the kernel's own error, which is a refusal carrying it.
    let wrong_input = Declaring::new(
        Arc::new(Doubler::new()),
        Declared {
            input: Some(SchemaDecl::Subset(vec![ColumnDecl::new(
                "id",
                DataType::Int64,
            )])),
            output: Some(unchanged()),
        },
    );
    let report = check(wrong_input, CheckOptions::new("wrong")).expect("runs");
    assert_eq!(report.verdict, Verdict::Refused);
    assert!(report.batches.iter().any(|b| b.error.is_some()));
    assert!(report.summary().contains("kernel failed"));
    assert!(report.to_json()["batches"][1]["error"].is_string());
}

#[test]
fn ck_json_and_summary_carry_the_profile_and_the_gil() {
    let kernel = Declaring::new(
        Arc::new(Doubler::new()),
        Declared {
            input: Some(SchemaDecl::from_schema(&support::schema())),
            output: Some(SchemaDecl::Exact(vec![
                ColumnDecl::new("id", DataType::Int64),
                ColumnDecl::new("value", DataType::Int64),
            ])),
        },
    );
    let mut opts = CheckOptions::new("doubler");
    opts.gil = Some(moruna_kernel::GilState::FreeThreaded);
    let bound = Arc::new(Mutex::new(false));
    let flag = bound.clone();
    opts.bind = Some(Box::new(move |_alloc| *flag.lock().expect("lock") = true));
    let report = check(kernel, opts).expect("runs");
    assert!(
        *bound.lock().expect("lock"),
        "bind is called with the arena"
    );
    let json = report.to_json();
    assert_eq!(json["moruna_check"], 1);
    assert_eq!(json["profile"]["gil"], "free_threaded");
    assert!(json["profile"]["path"].is_null());
    assert!(report.summary().contains("gil free_threaded"));
    assert!(report.fingerprint.starts_with("blake3:"));

    // A position disagreement under an exact declaration.
    let swapped = Declaring::new(
        Arc::new(Doubler::new()),
        Declared {
            input: Some(SchemaDecl::from_schema(&support::schema())),
            output: Some(SchemaDecl::Exact(vec![
                ColumnDecl::new("value", DataType::Int64),
                ColumnDecl::new("id", DataType::Int64),
            ])),
        },
    );
    let report = check(swapped, CheckOptions::new("swapped")).expect("runs");
    assert_eq!(report.verdict, Verdict::Refused);
    assert_eq!(report.to_json()["refusals"][0]["reason"], "position");
}
