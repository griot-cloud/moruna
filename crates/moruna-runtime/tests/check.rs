//! H13 through the Rust harness (15 k): a kernel that disagrees with its declaration is refused
//! naming the column (CK-T1); every standard kernel passes (CK-T5). What a check costs is
//! warm-up's, in `tests/warmup.rs`.

#![allow(clippy::result_large_err)]

mod support;

use std::sync::{Arc, Mutex};

use moruna_kernel::arrow::datatypes::DataType;
use moruna_kernel::declare::{ColumnDecl, Declared, SchemaDecl};
use moruna_kernel::{
    Fingerprint, InitCtx, Kernel, KernelHints, KernelKind, KernelState, Payload, PayloadSpec,
    SourceSchema,
};
use moruna_kernels::{StdKernel, fuse_chain};
use moruna_runtime::check::{CheckOptions, Verdict, check};
use serde_json::json;
use support::{Appender, Doubler, one_run_at_a_time};

/// A kernel with a declaration, over one of the support kernels.
struct Declaring {
    inner: Arc<dyn Kernel>,
    declared: Declared,
}

impl Declaring {
    fn new(inner: Arc<dyn Kernel>, declared: Declared) -> Arc<Declaring> {
        Arc::new(Declaring { inner, declared })
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
    // A check builds a real arena, which raises every other's baseline: one at a time.
    let _serial = one_run_at_a_time();
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
    assert!(
        report.warm_up.is_none(),
        "a refused kernel is not warmed up"
    );

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
    // PY-T21 (E13): a Rust kernel is not counted by the allocator guard, and says so.
    assert!(report.alloc.is_none() && report.memory_guard.is_none());
    assert!(report.to_json()["alloc"].is_null());
}

#[test]
fn ck_t5_every_standard_kernel_passes_check() {
    // A check builds a real arena, which raises every other's baseline: one at a time.
    let _serial = one_run_at_a_time();
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
    for (name, args) in chain {
        let kernel = StdKernel::new(name, &args).expect(name);
        let fingerprint = kernel.fingerprint();
        let kernel: Arc<dyn Kernel> = Arc::new(kernel);
        let mut opts = CheckOptions::new(name);
        opts.kind = "std";
        opts.fingerprint_scheme = "sha256";
        let report = check(kernel, opts).expect("the harness runs");
        assert_eq!(report.verdict, Verdict::Agreed, "{}", report.summary());
        assert_eq!(report.exit_code(), 0);
        assert_eq!(
            report.fingerprint,
            format!("sha256:{}", fingerprint.to_hex())
        );
    }
    // Fused chains are checkable too.
    let fused = fuse_chain(vec![
        StdKernel::new("cast", &json!({"columns": {"a": "double"}})).expect("cast"),
        StdKernel::new("select", &json!({"columns": ["a"]})).expect("select"),
        StdKernel::new("fill_null", &json!({"values": {"a": 0.5}})).expect("fill"),
        StdKernel::new("filter", &json!({"expr": "a > 0"})).expect("filter"),
    ]);
    assert_eq!(fused.len(), 2);
    for kernel in fused {
        let kernel: Arc<dyn Kernel> = Arc::new(kernel);
        let report = check(kernel, CheckOptions::new("fused")).expect("runs");
        assert_eq!(report.verdict, Verdict::Agreed, "{}", report.summary());
    }
}

#[test]
fn ck_not_checkable_and_plan_refusals() {
    // A check builds a real arena, which raises every other's baseline: one at a time.
    let _serial = one_run_at_a_time();
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
    assert!(report.summary().contains("function failed"));
    assert!(report.to_json()["batches"][1]["error"].is_string());
}

#[test]
fn ck_json_and_summary_carry_the_gil() {
    // A check builds a real arena, which raises every other's baseline: one at a time.
    let _serial = one_run_at_a_time();
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
    assert_eq!(json["moruna_check"], 2);
    assert_eq!(json["gil"], "free_threaded");
    assert!(json["warm_up"].is_null(), "no warm-up was asked for");
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
