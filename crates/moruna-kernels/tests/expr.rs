//! `moruna.expr@1`: its corpus (conformance/expr) on the CPU reference, configured through the
//! catalogue by identity, and the `expr` standard kernel as a job document names it.

#![allow(clippy::result_large_err)]

use std::path::PathBuf;
use std::sync::Arc;

use moruna_kernel::arrow::array::{Array, ArrayRef, AsArray, RecordBatch};
use moruna_kernel::arrow::compute::concat_batches;
use moruna_kernel::arrow::datatypes::{DataType, Float64Type, Schema};
use moruna_kernel::{InitCtx, Kernel, KernelKind, Payload, SourceSchema};
use moruna_kernels::catalogue::{Params, configure};
use moruna_kernels::conformance::{Case, Expect, Step, read_corpus, read_source};
use moruna_kernels::tree::Tree;
use moruna_kernels::{NAMES, StdKernel};
use moruna_testkit::FakeAllocator;
use serde_json::{Value, json};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn ctx() -> InitCtx {
    InitCtx {
        instance: 0,
        device: None,
        alloc: Arc::new(FakeAllocator::new()),
    }
}

/// A case run on the CPU reference: its output, or the step that refused it and why.
fn run(
    dir: &std::path::Path,
    case: &Case,
) -> Result<(Arc<Schema>, Vec<RecordBatch>), (Step, String)> {
    let kernel = configure("moruna.expr@1", &Params::json(case.params.clone()))
        .map_err(|e| (Step::Configure, e.to_string()))?;
    let (schema, batches) = read_source(dir, &case.inputs[0]).expect("an input");
    let declared = kernel
        .output_schema_of(&[SourceSchema::Table(schema)])
        .map_err(|e| (Step::Schema, e.to_string()))?;
    let SourceSchema::Table(declared) = declared else {
        panic!("a table schema")
    };
    let mut state = kernel.init(&ctx()).expect("init");
    let mut out = Vec::new();
    for batch in batches {
        let got = kernel
            .apply_port(state.as_mut(), 0, Payload::table(batch).expect("payload"))
            .map_err(|e| (Step::Apply, e.to_string()))?;
        let Payload::Table(got, _) = got else {
            panic!("a table")
        };
        assert_eq!(
            got.schema().fields(),
            declared.fields(),
            "{}: the declared schema",
            case.name
        );
        out.push(got);
    }
    Ok((declared, out))
}

/// Whether `got` is `want` within `allowed` (absolute, per element), or bit for bit.
fn close(name: &str, got: &ArrayRef, want: &ArrayRef, allowed: Option<&ArrayRef>) {
    let Some(allowed) = allowed else {
        assert_eq!(got.to_data(), want.to_data(), "{name}");
        return;
    };
    assert_eq!(got.len(), want.len(), "{name}");
    let as_f64 = |a: &ArrayRef| -> Vec<Option<f64>> {
        moruna_kernel::arrow::compute::kernels::cast::cast(a, &DataType::Float64)
            .expect("as double")
            .as_primitive::<Float64Type>()
            .iter()
            .collect()
    };
    let (g, w, a) = (as_f64(got), as_f64(want), as_f64(allowed));
    for i in 0..g.len() {
        match (g[i], w[i], a[i]) {
            (None, None, _) => {}
            (Some(x), Some(y), Some(tol)) if !x.is_nan() && !y.is_nan() && x.is_finite() => {
                assert!(
                    (x - y).abs() <= tol,
                    "{name} row {i}: {x} vs {y} beyond {tol}"
                );
            }
            (Some(x), Some(y), _) => {
                assert!(
                    x.to_bits() == y.to_bits() || (x.is_nan() && y.is_nan()),
                    "{name} row {i}: {x} vs {y}"
                );
            }
            other => panic!("{name} row {i}: {other:?}"),
        }
    }
}

#[test]
fn the_expr_corpus_passes_on_the_cpu_reference() {
    let dir = root().join("conformance").join("expr");
    let corpus = read_corpus(&dir).unwrap_or_else(|p| panic!("{p:#?}"));
    assert_eq!(corpus.kernel, "moruna.expr");
    for case in &corpus.cases {
        let result = run(&dir, case);
        match (&case.expect, result) {
            (Expect::Output(file), Ok((schema, got))) => {
                let (want_schema, want) = read_source(
                    &dir,
                    &moruna_kernels::conformance::Source {
                        file: file.clone(),
                        slice: None,
                    },
                )
                .expect("expected");
                let names = |s: &Schema| -> Vec<(String, DataType, bool)> {
                    s.fields()
                        .iter()
                        .map(|f| (f.name().clone(), f.data_type().clone(), f.is_nullable()))
                        .collect()
                };
                assert_eq!(names(&schema), names(&want_schema), "{}: schema", case.name);
                let got = concat_batches(&schema, &got).expect("concat");
                let want = concat_batches(&want_schema, &want).expect("concat");
                assert_eq!(got.num_rows(), want.num_rows(), "{}", case.name);
                for (i, field) in schema.fields().iter().enumerate() {
                    let allowed = case.allowed.get(field.name()).map(|source| {
                        let (s, b) = read_source(&dir, source).expect("allowed");
                        Arc::clone(concat_batches(&s, &b).expect("concat").column(0))
                    });
                    close(
                        &format!("{} {}", case.name, field.name()),
                        got.column(i),
                        want.column(i),
                        allowed.as_ref(),
                    );
                }
            }
            (Expect::Error { at, contains }, Err((step, msg))) => {
                assert_eq!(*at, step, "{}: {msg}", case.name);
                if let Some(text) = contains {
                    assert!(msg.contains(text.as_str()), "{}: {msg}", case.name);
                }
            }
            (expect, got) => panic!(
                "{}: expected {expect:?}, got {:?}",
                case.name,
                got.map(|(_, b)| b.len())
            ),
        }
    }
}

fn tree_file(name: &str) -> Value {
    let text = std::fs::read_to_string(root().join("tests").join("expr").join(name)).expect("tree");
    serde_json::from_str(&text).expect("json")
}

#[test]
fn audit_and_enrich_are_one_kernel_each() {
    for (file, outputs) in [("audit.json", 5), ("enrich.json", 24)] {
        let args = tree_file(file);
        let kernel = StdKernel::new("expr", &args).expect(file);
        assert_eq!(kernel.name(), "expr");
        assert!(matches!(kernel.kind(), KernelKind::Stateless));
        let declared = kernel.declared();
        // Its column types are checked at plan against the real input, not on synthetic batches.
        assert!(
            declared.input.is_none() && !declared.is_checkable(),
            "{file}"
        );
        assert_eq!(kernel.hints().releases_gil, Some(true));
        let Some(moruna_kernel::declare::SchemaDecl::Exact(cols)) = declared.output else {
            panic!("{file}: an exact output declaration")
        };
        assert_eq!(cols.len(), outputs, "{file}");
        let tree = Tree::from_json(&args).expect("tree");
        assert_eq!(Tree::from_json(&tree.to_json()).expect("again"), tree);
    }
    // The corpus's Audit and Enrich cases carry these same trees.
    let corpus = read_corpus(&root().join("conformance").join("expr")).expect("corpus");
    let audit = corpus
        .cases
        .iter()
        .find(|c| c.name == "audit_rule")
        .expect("audit");
    assert_eq!(audit.params, tree_file("audit.json"));
    let enrich = corpus
        .cases
        .iter()
        .find(|c| c.name == "enrich_columns")
        .expect("enrich");
    assert_eq!(enrich.params, tree_file("enrich.json"));
}

#[test]
fn expr_is_a_standard_kernel_a_document_names() {
    assert_eq!(NAMES.last(), Some(&"expr"));
    let args = json!({"outputs": [
        {"name": "x2", "expr": {"call": "multiply", "args": [{"column": "x"}, {"literal": 2}]}},
        {"name": "x", "expr": {"column": "x"}}]});
    let kernel = StdKernel::new("expr", &args).expect("kernel");
    // The fingerprint is of the canonical arguments: key order does not change it.
    let reordered: Value = serde_json::from_str(
        r#"{"outputs": [{"expr": {"args": [{"column": "x"}, {"literal": 2}], "call": "multiply"}, "name": "x2"},
                        {"expr": {"column": "x"}, "name": "x"}]}"#,
    )
    .expect("json");
    assert_eq!(
        StdKernel::new("expr", &reordered)
            .expect("same")
            .fingerprint(),
        kernel.fingerprint()
    );
    let schema = Arc::new(Schema::new(vec![
        moruna_kernel::arrow::datatypes::Field::new("x", DataType::Int64, false),
    ]));
    let SourceSchema::Table(out) = kernel
        .output_schema(&SourceSchema::Table(Arc::clone(&schema)))
        .expect("schema")
    else {
        panic!("table")
    };
    assert_eq!(out.field(0).data_type(), &DataType::Int64);
    assert!(out.field(0).is_nullable());
    assert!(
        !out.field(1).is_nullable(),
        "a column passed through keeps its field"
    );
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![Arc::new(moruna_kernel::arrow::array::Int64Array::from(
            vec![1, 2, 3],
        ))],
    )
    .expect("batch");
    let mut state = kernel.init(&ctx()).expect("init");
    let Payload::Table(got, _) = kernel
        .apply(state.as_mut(), Payload::table(batch).expect("payload"))
        .expect("apply")
    else {
        panic!("table")
    };
    assert_eq!(
        got.column(0)
            .as_primitive::<moruna_kernel::arrow::datatypes::Int64Type>()
            .values()
            .to_vec(),
        vec![2, 4, 6]
    );
    for bad in [json!(null), json!({"outputs": []}), json!({"expr": 1})] {
        assert!(StdKernel::new("expr", &bad).is_err(), "{bad}");
    }
    let refused = kernel
        .output_schema(&SourceSchema::Table(Arc::new(Schema::empty())))
        .expect_err("no x")
        .to_string();
    assert!(refused.contains("no column `x`"), "{refused}");
    assert!(configure("moruna.expr@2", &Params::json(args.clone())).is_err());
    let mut with_arrays = Params::json(args);
    with_arrays.arrays.insert(
        "a".into(),
        RecordBatch::new_empty(Arc::new(Schema::empty())),
    );
    assert!(configure("moruna.expr@1", &with_arrays).is_err());
}
