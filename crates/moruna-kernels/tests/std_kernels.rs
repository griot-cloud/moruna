//! CK-T6 std_output_schema_is_exact, and each standard kernel's behaviour on a batch whose
//! answer is written out by hand (15 e.6, CK-I6).

#![allow(clippy::result_large_err)]

use std::sync::Arc;

use moruna_kernel::arrow::array::{
    Array, AsArray, Date32Array, Float64Array, Int64Array, ListArray, RecordBatch, StringArray,
    TimestampSecondArray,
};
use moruna_kernel::arrow::datatypes::{DataType, Field, Int64Type, Schema, TimeUnit};
use moruna_kernel::{InitCtx, Kernel, KernelKind, NoState, Payload, ResumePolicy, SourceSchema};
use moruna_kernels::{NAMES, StdKernel, std_fingerprint};
use moruna_testkit::FakeAllocator;
use serde_json::{Value, json};

fn batch() -> RecordBatch {
    let list = ListArray::from_iter_primitive::<Int64Type, _, _>(vec![
        Some(vec![Some(1), Some(2)]),
        Some(vec![]),
        None,
        Some(vec![Some(3)]),
    ]);
    RecordBatch::try_new(
        Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, true),
            Field::new("name", DataType::Utf8, true),
            Field::new("score", DataType::Float64, true),
            Field::new("at", DataType::Timestamp(TimeUnit::Second, None), true),
            Field::new("tags", list.data_type().clone(), true),
            Field::new("day", DataType::Date32, true),
        ])),
        vec![
            Arc::new(Int64Array::from(vec![Some(1), Some(2), Some(2), None])),
            Arc::new(StringArray::from(vec![
                Some("alice"),
                None,
                Some("bob"),
                Some("carol"),
            ])),
            Arc::new(Float64Array::from(vec![
                Some(1.5),
                None,
                Some(20.0),
                Some(7.25),
            ])),
            // 2024-03-15T10:30:45Z, the epoch, 1969-12-31T23:59:59Z, null.
            Arc::new(TimestampSecondArray::from(vec![
                Some(1_710_498_645),
                Some(0),
                Some(-1),
                None,
            ])),
            Arc::new(list),
            Arc::new(Date32Array::from(vec![
                Some(19_797),
                Some(0),
                Some(-1),
                None,
            ])),
        ],
    )
    .expect("batch")
}

fn run(name: &str, args: Value, input: &RecordBatch) -> RecordBatch {
    let kernel = StdKernel::new(name, &args).expect("kernel");
    let alloc: Arc<dyn moruna_kernel::Allocator> = Arc::new(FakeAllocator::new());
    let mut state = kernel
        .init(&InitCtx {
            instance: 0,
            device: None,
            alloc,
        })
        .expect("init");
    let declared = kernel
        .output_schema(&SourceSchema::Table(input.schema()))
        .expect("output schema");
    let out = kernel
        .apply(
            state.as_mut(),
            Payload::table(input.clone()).expect("payload"),
        )
        .expect("apply");
    let Payload::Table(out, _) = out else {
        panic!("a table")
    };
    let SourceSchema::Table(declared) = declared else {
        panic!("a table schema")
    };
    assert_eq!(out.schema().fields(), declared.fields(), "{name}: CK-I6");
    out
}

fn strings(b: &RecordBatch, col: &str) -> Vec<Option<String>> {
    b.column_by_name(col)
        .expect("column")
        .as_string::<i32>()
        .iter()
        .map(|v| v.map(str::to_string))
        .collect()
}

fn ints(b: &RecordBatch, col: &str) -> Vec<Option<i64>> {
    b.column_by_name(col)
        .expect("column")
        .as_primitive::<Int64Type>()
        .iter()
        .collect()
}

fn names(b: &RecordBatch) -> Vec<String> {
    b.schema()
        .fields()
        .iter()
        .map(|f| f.name().clone())
        .collect()
}

#[test]
fn projections() {
    let b = batch();
    let cast = run(
        "cast",
        json!({"columns": {"id": "double", "score": "int32"}}),
        &b,
    );
    assert_eq!(cast.schema().field(0).data_type(), &DataType::Float64);
    assert_eq!(cast.schema().field(2).data_type(), &DataType::Int32);
    let renamed = run("rename", json!({"columns": {"id": "key"}}), &b);
    assert_eq!(names(&renamed)[0], "key");
    let selected = run("select", json!({"columns": ["score", "id"]}), &b);
    assert_eq!(names(&selected), vec!["score", "id"]);
    let dropped = run("drop", json!({"columns": ["tags", "day"]}), &b);
    assert_eq!(names(&dropped), vec!["id", "name", "score", "at"]);
}

#[test]
fn a_strict_cast_fails_where_a_lenient_one_nulls() {
    let b = batch();
    let lenient = run("cast", json!({"columns": {"name": "int64"}}), &b);
    assert_eq!(lenient.column(1).null_count(), 4);
    let strict = StdKernel::new(
        "cast",
        &json!({"columns": {"name": "int64"}, "strict": true}),
    )
    .expect("kernel");
    assert!(
        strict
            .apply(&mut NoState, Payload::table(b).expect("payload"))
            .is_err()
    );
}

#[test]
fn filter_fill_and_dedupe() {
    let b = batch();
    let filtered = run(
        "filter",
        json!({"expr": "score > 5 and is_not_null(name)"}),
        &b,
    );
    assert_eq!(ints(&filtered, "id"), vec![Some(2), None]);
    let filled = run(
        "fill_null",
        json!({"values": {"name": "?", "score": 0}}),
        &b,
    );
    assert_eq!(strings(&filled, "name")[1].as_deref(), Some("?"));
    assert_eq!(
        filled
            .column(2)
            .as_primitive::<moruna_kernel::arrow::datatypes::Float64Type>()
            .value(1),
        0.0
    );
    let deduped = run("dedupe", json!({"keys": ["id"]}), &b);
    assert_eq!(ints(&deduped, "id"), vec![Some(1), Some(2), None]);
}

#[test]
fn dedupe_remembers_across_batches_and_across_a_checkpoint() {
    let kernel = StdKernel::new("dedupe", &json!({"keys": "id"})).expect("kernel");
    assert!(matches!(kernel.kind(), KernelKind::Stateful { .. }));
    assert_eq!(kernel.hints().resume, ResumePolicy::Checkpoint);
    let alloc: Arc<dyn moruna_kernel::Allocator> = Arc::new(FakeAllocator::new());
    let ctx = InitCtx {
        instance: 0,
        device: None,
        alloc,
    };
    let mut state = kernel.init(&ctx).expect("init");
    let first = kernel
        .apply(state.as_mut(), Payload::table(batch()).expect("p"))
        .expect("apply");
    assert_eq!(first.rows(), 3);
    assert!(state.footprint().expect("footprint") > 0);
    let saved = state.checkpoint().expect("checkpoint").expect("bytes");
    let mut restored = kernel.restore(&ctx, &saved).expect("restore");
    let again = kernel
        .apply(restored.as_mut(), Payload::table(batch()).expect("p"))
        .expect("apply");
    assert_eq!(again.rows(), 0, "every key was seen before the checkpoint");
    assert!(kernel.restore(&ctx, &[9, 0, 0, 0, 1]).is_err());
    let select = StdKernel::new("select", &json!({"columns": ["id"]})).expect("select");
    assert!(select.restore(&ctx, &[]).is_err());
}

#[test]
fn hash_mask_and_concat() {
    let b = batch();
    let hashed = run("hash", json!({"columns": ["id", "name"]}), &b);
    let digests = strings(&hashed, "hash");
    assert_eq!(digests.len(), 4);
    assert!(
        digests
            .iter()
            .all(|d| d.as_ref().is_some_and(|d| d.len() == 64))
    );
    // Rows 1 and 2 share an id and differ in name.
    assert_ne!(digests[1], digests[2]);
    let blake = run(
        "hash",
        json!({"columns": ["id"], "algo": "blake3", "output": "h"}),
        &b,
    );
    let sha = run("hash", json!({"columns": ["id"]}), &b);
    assert_ne!(strings(&blake, "h")[0], strings(&sha, "hash")[0]);

    let redacted = run("mask", json!({"columns": ["name"]}), &b);
    assert_eq!(strings(&redacted, "name")[0].as_deref(), Some("*****"));
    let partial = run(
        "mask",
        json!({"columns": ["name"], "mode": "partial", "keep": 2}),
        &b,
    );
    assert_eq!(strings(&partial, "name")[3].as_deref(), Some("***ol"));
    let nulled = run("mask", json!({"columns": ["id"], "mode": "null"}), &b);
    assert_eq!(nulled.column(0).null_count(), 4);
    let hashed_mask = run("mask", json!({"columns": ["name"], "mode": "hash"}), &b);
    assert_eq!(
        strings(&hashed_mask, "name")[0].as_ref().map(String::len),
        Some(64)
    );

    let joined = run(
        "concat_str",
        json!({"columns": ["name", "id"], "separator": "-", "output": "k"}),
        &b,
    );
    assert_eq!(
        strings(&joined, "k"),
        vec![Some("alice-1".into()), None, Some("bob-2".into()), None]
    );
}

#[test]
fn explode_and_date_trunc() {
    let b = batch();
    let exploded = run("explode", json!({"column": "tags"}), &b);
    assert_eq!(
        ints(&exploded, "id"),
        vec![Some(1), Some(1), Some(2), Some(2), None]
    );
    assert_eq!(
        ints(&exploded, "tags"),
        vec![Some(1), Some(2), None, None, Some(3)]
    );

    let month = run("date_trunc", json!({"column": "at", "unit": "month"}), &b);
    let at: Vec<Option<i64>> = month
        .column(3)
        .as_primitive::<moruna_kernel::arrow::datatypes::TimestampSecondType>()
        .iter()
        .collect();
    // 2024-03-01, the epoch, 1969-12-01, null.
    assert_eq!(
        at,
        vec![Some(1_709_251_200), Some(0), Some(-2_678_400), None]
    );
    let hour = run(
        "date_trunc",
        json!({"column": "at", "unit": "hour", "output": "h"}),
        &b,
    );
    assert_eq!(hour.num_columns(), 7);
    let days = run("date_trunc", json!({"column": "day", "unit": "year"}), &b);
    let days: Vec<Option<i32>> = days
        .column(5)
        .as_primitive::<moruna_kernel::arrow::datatypes::Date32Type>()
        .iter()
        .collect();
    assert_eq!(days, vec![Some(19_723), Some(0), Some(-365), None]);
}

#[test]
fn bad_arguments_and_bad_inputs_are_plan_errors_naming_the_kernel() {
    for (name, args) in [
        ("nope", json!({})),
        ("cast", json!({"columns": {"id": "tensor"}})),
        ("cast", json!({"colums": {"id": "int64"}})),
        ("rename", json!({"columns": {"id": 3}})),
        ("fill_null", json!({"values": {"id": null}})),
        ("hash", json!({"columns": ["id"], "algo": "md5"})),
        ("mask", json!({"columns": ["name"], "mode": "blur"})),
        ("mask", json!({"columns": ["name"], "keep": 2})),
        ("date_trunc", json!({"column": "at", "unit": "week"})),
        ("filter", json!({"expr": "id >"})),
        ("select", json!([1])),
    ] {
        let err = StdKernel::new(name, &args).expect_err(name);
        assert!(err.to_string().contains("moruna.std"), "{name}: {err}");
    }
    let b = batch();
    let schema = SourceSchema::Table(b.schema());
    for (name, args) in [
        ("cast", json!({"columns": {"missing": "int64"}})),
        ("cast", json!({"columns": {"tags": "date32"}})),
        ("rename", json!({"columns": {"id": "name"}})),
        ("select", json!({"columns": ["missing"]})),
        ("drop", json!({"columns": ["missing"]})),
        ("filter", json!({"expr": "name > 3"})),
        ("fill_null", json!({"values": {"id": "x"}})),
        ("hash", json!({"columns": ["id"], "output": "name"})),
        ("mask", json!({"columns": ["id"]})),
        ("explode", json!({"column": "id"})),
        ("date_trunc", json!({"column": "id", "unit": "day"})),
        ("dedupe", json!({"keys": ["missing"]})),
    ] {
        let kernel = StdKernel::new(name, &args).expect(name);
        assert!(kernel.output_schema(&schema).is_err(), "{name} {args}");
    }
    let tensor = SourceSchema::Tensor {
        dtype: moruna_kernel::DType::F32,
        shape: vec![-1, 4],
    };
    let select = StdKernel::new("select", &json!({"columns": ["id"]})).expect("select");
    assert!(select.output_schema(&tensor).is_err());
}

#[test]
fn every_kernel_declares_both_halves_releases_the_gil_and_has_a_stable_fingerprint() {
    let args = [
        json!({"columns": {"a": "double"}}),
        json!({"columns": {"a": "b"}}),
        json!({"columns": ["a"]}),
        json!({"columns": ["a"]}),
        json!({"expr": "a > 1"}),
        json!({"values": {"a": 0}}),
        json!({"keys": ["a"]}),
        json!({"columns": ["a"]}),
        json!({"columns": ["a"]}),
        json!({"column": "a"}),
        json!({"columns": ["a"]}),
        json!({"column": "a", "unit": "day"}),
    ];
    for (name, args) in NAMES.iter().zip(args) {
        let kernel = StdKernel::new(name, &args).expect(name);
        assert!(kernel.declared().is_checkable(), "{name}");
        assert_eq!(kernel.hints().releases_gil, Some(true));
        assert_eq!(kernel.name(), *name);
        assert!(!kernel.is_fused());
        assert_eq!(kernel.fingerprint(), std_fingerprint(name, &args));
        assert_eq!(kernel.args(), &args);
    }
    // Key order does not change the fingerprint; a value does.
    let a = std_fingerprint("hash", &json!({"columns": ["a"], "algo": "sha256"}));
    let b = std_fingerprint("hash", &json!({"algo": "sha256", "columns": ["a"]}));
    let c = std_fingerprint("hash", &json!({"algo": "blake3", "columns": ["a"]}));
    assert_eq!(a, b);
    assert_ne!(a, c);
}
