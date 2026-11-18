//! The reference's behaviour, case by case, with the answers written out by hand.

use std::sync::Arc;

use moruna_kernel::arrow::array::{
    Array, ArrayRef, AsArray, BooleanArray, Date32Array, Decimal128Array, Float32Array,
    Float64Array, Int8Array, Int32Array, Int64Array, RecordBatch, StringArray, UInt8Array,
    UInt64Array,
};
use moruna_kernel::arrow::datatypes::{DataType, Field, Schema};
use moruna_kernel::{
    InitCtx, Kernel, KernelKind, KernelState, MorunaError, Payload, ResumePolicy, SourceSchema,
};
use moruna_testkit::FakeAllocator;
use serde_json::{Value, json};

use super::*;

fn batch(columns: Vec<(&str, ArrayRef)>) -> RecordBatch {
    let fields: Vec<Field> = columns
        .iter()
        .map(|(n, a)| Field::new(*n, a.data_type().clone(), true))
        .collect();
    RecordBatch::try_new(
        Arc::new(Schema::new(fields)),
        columns.into_iter().map(|(_, a)| a).collect(),
    )
    .unwrap()
}

fn i64s(v: Vec<Option<i64>>) -> ArrayRef {
    Arc::new(Int64Array::from(v))
}

fn f64s(v: Vec<Option<f64>>) -> ArrayRef {
    Arc::new(Float64Array::from(v))
}

fn kernel(args: Value) -> GroupAggregate {
    GroupAggregate::new(&args).unwrap()
}

fn refusal(args: Value) -> String {
    match GroupAggregate::new(&args) {
        Err(MorunaError::Plan(msg)) => msg,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn col<'a>(b: &'a RecordBatch, name: &str) -> &'a ArrayRef {
    b.column_by_name(name)
        .unwrap_or_else(|| panic!("no column {name}"))
}

fn ints(b: &RecordBatch, name: &str) -> Vec<Option<i64>> {
    col(b, name).as_primitive::<Int64Type>().iter().collect()
}

fn floats(b: &RecordBatch, name: &str) -> Vec<Option<f64>> {
    col(b, name).as_primitive::<Float64Type>().iter().collect()
}

use moruna_kernel::arrow::datatypes::{Float64Type, Int64Type};

/// Two keys, one with nulls, a value with nulls: the shared input of several tests.
fn trips() -> Vec<RecordBatch> {
    vec![
        batch(vec![
            ("zone", i64s(vec![Some(2), Some(1), Some(2), None, Some(1)])),
            (
                "minute",
                i64s(vec![Some(5), Some(5), Some(5), Some(0), Some(7)]),
            ),
            (
                "fare",
                f64s(vec![Some(1.5), Some(2.0), None, Some(4.0), Some(0.25)]),
            ),
            ("n", i64s(vec![Some(10), Some(-3), Some(7), None, Some(1)])),
        ]),
        batch(vec![
            ("zone", i64s(vec![Some(1), None, Some(3)])),
            ("minute", i64s(vec![Some(5), Some(0), None])),
            ("fare", f64s(vec![Some(-1.0), None, Some(f64::NAN)])),
            ("n", i64s(vec![None, Some(6), Some(2)])),
        ]),
    ]
}

fn every_aggregate() -> Value {
    json!([
        {"op": "count", "as": "rows"},
        {"op": "count", "column": "fare", "as": "fares"},
        {"op": "sum", "column": "n", "as": "n_sum"},
        {"op": "min", "column": "fare", "as": "fare_min"},
        {"op": "max", "column": "fare", "as": "fare_max"},
        {"op": "mean", "column": "n", "as": "n_mean"},
        {"op": "sum", "column": "fare", "as": "fare_sum"},
        {"op": "mean", "column": "fare", "as": "fare_mean"},
    ])
}

/// The answer to `trips` under `every_aggregate`, worked by hand, in key order with nulls last:
/// (1,5) (1,7) (2,5) (3,null) (null,0).
fn check_trips(out: &RecordBatch) {
    assert_eq!(
        ints(out, "zone"),
        vec![Some(1), Some(1), Some(2), Some(3), None]
    );
    assert_eq!(
        ints(out, "minute"),
        vec![Some(5), Some(7), Some(5), None, Some(0)]
    );
    assert_eq!(
        ints(out, "rows"),
        vec![Some(2), Some(1), Some(2), Some(1), Some(2)]
    );
    assert_eq!(
        ints(out, "fares"),
        vec![Some(2), Some(1), Some(1), Some(1), Some(1)]
    );
    assert_eq!(
        ints(out, "n_sum"),
        vec![Some(-3), Some(1), Some(17), Some(2), Some(6)]
    );
    let min = floats(out, "fare_min");
    assert_eq!(min[..3], [Some(-1.0), Some(0.25), Some(1.5)]);
    assert_eq!(min[4], Some(4.0));
    // The NaN of group (3, null) is its min and max: NaN is greatest, and alone.
    assert!(floats(out, "fare_min")[3].unwrap().is_nan());
    assert!(floats(out, "fare_max")[3].unwrap().is_nan());
    assert_eq!(
        floats(out, "fare_max")[..3],
        [Some(2.0), Some(0.25), Some(1.5)]
    );
    assert_eq!(floats(out, "fare_max")[4], Some(4.0));
    assert_eq!(
        floats(out, "n_mean"),
        vec![Some(-3.0), Some(1.0), Some(8.5), Some(2.0), Some(6.0)]
    );
    assert_eq!(
        floats(out, "fare_sum")[..3],
        [Some(1.0), Some(0.25), Some(1.5)]
    );
    assert!(floats(out, "fare_sum")[3].unwrap().is_nan());
    assert_eq!(floats(out, "fare_sum")[4], Some(4.0));
    assert_eq!(
        floats(out, "fare_mean")[..3],
        [Some(0.5), Some(0.25), Some(1.5)]
    );
    assert_eq!(floats(out, "fare_mean")[4], Some(4.0));
}

#[test]
fn every_aggregate_on_the_hash_path() {
    let k = kernel(json!({"keys": ["zone", "minute"], "aggregates": every_aggregate()}));
    assert_eq!(k.path(), Path::Hash);
    let out = run(&k, &trips()).unwrap().unwrap();
    check_trips(&out);
    assert!(!out.schema().field_with_name("rows").unwrap().is_nullable());
    assert_eq!(col(&out, "n_sum").data_type(), &DataType::Int64);
}

#[test]
fn every_aggregate_on_the_dense_path_gives_the_same_rows() {
    let k = kernel(json!({
        "keys": ["zone", "minute"],
        "aggregates": every_aggregate(),
        "domain": {"zone": [0, 9], "minute": [0, 1439]},
    }));
    assert_eq!(k.path(), Path::Dense);
    let st = run_state(&k, &trips()).unwrap();
    assert_eq!(st.dense(), Some(true));
    assert_eq!(st.groups(), 11 * 1441);
    assert!(st.declined().is_none());
    let out = finish_state(&k, &st).unwrap().unwrap();
    check_trips(&out);
    let hash = run(
        &kernel(json!({"keys": ["zone", "minute"], "aggregates": every_aggregate()})),
        &trips(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(format!("{out:?}"), format!("{hash:?}"));
}

#[test]
fn a_value_outside_the_domain_moves_the_instance_to_the_hash_path() {
    let k = kernel(json!({
        "keys": ["zone", "minute"],
        "aggregates": every_aggregate(),
        "domain": {"zone": [0, 2], "minute": [0, 1439]},
    }));
    // Zone 3 arrives in the second batch, after the first was counted densely.
    let st = run_state(&k, &trips()).unwrap();
    assert_eq!(st.dense(), Some(false));
    let why = st.declined().unwrap();
    assert!(
        why.contains("dense path declined: value 3 outside [0, 2]"),
        "{why}"
    );
    check_trips(&finish_state(&k, &st).unwrap().unwrap());
    // Declined on the very first batch, before anything was counted.
    let k = kernel(json!({
        "keys": ["zone"],
        "aggregates": [{"op": "count", "as": "n"}],
        "domain": {"zone": [5, 6]},
    }));
    let b = batch(vec![("zone", i64s(vec![Some(1), Some(5), Some(1)]))]);
    let st = run_state(&k, &[b]).unwrap();
    assert_eq!(st.dense(), Some(false));
    let out = finish_state(&k, &st).unwrap().unwrap();
    assert_eq!(ints(&out, "zone"), vec![Some(1), Some(5)]);
    assert_eq!(ints(&out, "n"), vec![Some(2), Some(1)]);
}

#[test]
fn unsigned_keys_past_i64_decline_and_narrow_keys_fill_their_domain() {
    let k = kernel(json!({
        "keys": ["u"],
        "aggregates": [{"op": "count", "as": "n"}],
        "domain": {"u": [0, 10]},
    }));
    let b = batch(vec![(
        "u",
        Arc::new(UInt64Array::from(vec![Some(u64::MAX), Some(3), None])) as ArrayRef,
    )]);
    let st = run_state(&k, &[b]).unwrap();
    assert!(st.declined().unwrap().contains(&u64::MAX.to_string()));
    let out = finish_state(&k, &st).unwrap().unwrap();
    assert_eq!(
        col(&out, "u")
            .as_primitive::<moruna_kernel::arrow::datatypes::UInt64Type>()
            .iter()
            .collect::<Vec<_>>(),
        vec![Some(3), Some(u64::MAX), None]
    );
    // Every integer width, dense, with nulls.
    let k = kernel(json!({
        "keys": ["a", "b"],
        "aggregates": [{"op": "count", "as": "n"}],
        "domain": {"a": [-128, 127], "b": [0, 255]},
    }));
    let b = batch(vec![
        (
            "a",
            Arc::new(Int8Array::from(vec![
                Some(-128),
                Some(127),
                None,
                Some(-128),
            ])) as ArrayRef,
        ),
        (
            "b",
            Arc::new(UInt8Array::from(vec![
                Some(255),
                Some(0),
                Some(1),
                Some(255),
            ])) as ArrayRef,
        ),
    ]);
    let st = run_state(&k, &[b]).unwrap();
    assert_eq!(st.dense(), Some(true));
    let out = finish_state(&k, &st).unwrap().unwrap();
    assert_eq!(col(&out, "a").data_type(), &DataType::Int8);
    assert_eq!(
        col(&out, "a")
            .as_primitive::<moruna_kernel::arrow::datatypes::Int8Type>()
            .iter()
            .collect::<Vec<_>>(),
        vec![Some(-128), Some(127), None]
    );
    assert_eq!(ints(&out, "n"), vec![Some(2), Some(1), Some(1)]);
}

#[test]
fn split_across_instances_and_merged_gives_the_single_result() {
    let single = kernel(json!({"keys": ["zone", "minute"], "aggregates": every_aggregate()}));
    let reference = run(&single, &trips()).unwrap().unwrap();
    // Each row its own batch, so every instance sees a different part.
    let mut rows = Vec::new();
    for b in trips() {
        for i in 0..b.num_rows() {
            rows.push(b.slice(i, 1));
        }
    }
    for instances in [1, 2, 3, 8] {
        let (out, states) = run_split(&single, &rows, instances).unwrap();
        let out = out.unwrap();
        assert_eq!(states.len(), instances);
        check_trips(&out);
        assert_eq!(format!("{out:?}"), format!("{reference:?}"), "{instances}");
    }
    // And on the dense path.
    let dense = kernel(json!({
        "keys": ["zone", "minute"],
        "aggregates": every_aggregate(),
        "domain": {"zone": [0, 9], "minute": [0, 1439]},
    }));
    let (out, _) = run_split(&dense, &rows, 3).unwrap();
    assert_eq!(format!("{:?}", out.unwrap()), format!("{reference:?}"));
}

#[test]
fn the_partial_form_is_stated() {
    let k = kernel(json!({
        "keys": ["zone"],
        "aggregates": [
            {"op": "count", "as": "rows"},
            {"op": "sum", "column": "n", "as": "n_sum"},
            {"op": "sum", "column": "u", "as": "u_sum"},
            {"op": "sum", "column": "fare", "as": "fare_sum"},
            {"op": "mean", "column": "n", "as": "n_mean"},
            {"op": "min", "column": "day", "as": "first"},
        ],
        "mode": "partial",
        "instances": 4,
    }));
    assert!(matches!(k.kind(), KernelKind::Stateful { max_instances } if max_instances.get() == 4));
    let b = batch(vec![
        ("zone", i64s(vec![Some(1), Some(1), Some(2)])),
        ("n", i64s(vec![Some(i64::MAX), Some(i64::MAX), None])),
        (
            "u",
            Arc::new(UInt64Array::from(vec![Some(1), Some(2), Some(3)])) as ArrayRef,
        ),
        ("fare", f64s(vec![Some(0.1), Some(0.2), None])),
        (
            "day",
            Arc::new(Date32Array::from(vec![Some(19_000), Some(18_000), None])) as ArrayRef,
        ),
    ]);
    let out = run(&k, &[b]).unwrap().unwrap();
    let schema = out.schema();
    let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    assert_eq!(
        names,
        [
            "zone",
            "rows",
            "n_sum",
            "u_sum",
            "fare_sum",
            "n_mean.sum",
            "n_mean.count",
            "first"
        ]
    );
    let n_sum = col(&out, "n_sum")
        .as_any()
        .downcast_ref::<Decimal128Array>()
        .unwrap();
    // A partial sum past i64 is kept exactly; only the final result checks the range.
    assert_eq!(n_sum.value(0), 2 * i64::MAX as i128);
    assert!(n_sum.is_null(1));
    let meta = out
        .schema()
        .field_with_name("u_sum")
        .unwrap()
        .metadata()
        .clone();
    assert_eq!(meta.get(SUM_TYPE).map(String::as_str), Some("uint64"));
    assert_eq!(col(&out, "fare_sum").data_type(), &DataType::Binary);
    assert_eq!(col(&out, "first").data_type(), &DataType::Date32);
    assert_eq!(
        col(&out, "first")
            .as_primitive::<moruna_kernel::arrow::datatypes::Date32Type>()
            .value(0),
        18_000
    );
    // The final result of that state overflows Int64, and says which group.
    let single = k.in_mode(Mode::Single, 1).unwrap();
    let b = batch(vec![
        ("zone", i64s(vec![Some(1), Some(1)])),
        ("n", i64s(vec![Some(i64::MAX), Some(1)])),
        (
            "u",
            Arc::new(UInt64Array::from(vec![Some(1), Some(2)])) as ArrayRef,
        ),
        ("fare", f64s(vec![Some(0.1), Some(0.2)])),
        (
            "day",
            Arc::new(Date32Array::from(vec![Some(1), Some(2)])) as ArrayRef,
        ),
    ]);
    match run(&single, std::slice::from_ref(&b)) {
        Err(MorunaError::Kernel { msg, .. }) => {
            assert!(msg.contains("the sum `n_sum` of the group (1)"), "{msg}");
        }
        other => panic!("expected an overflow, got {other:?}"),
    }
    // An overflow of a partial sum that comes back into range is no overflow.
    let b2 = batch(vec![
        ("zone", i64s(vec![Some(1)])),
        ("n", i64s(vec![Some(-5)])),
        (
            "u",
            Arc::new(UInt64Array::from(vec![Some(u64::MAX - 3)])) as ArrayRef,
        ),
        ("fare", f64s(vec![None])),
        (
            "day",
            Arc::new(Date32Array::from(vec![None::<i32>])) as ArrayRef,
        ),
    ]);
    let (out, _) = run_split(&single, &[b, b2], 2).unwrap();
    let out = out.unwrap();
    assert_eq!(ints(&out, "n_sum"), vec![Some(i64::MAX - 4)]);
    assert_eq!(
        col(&out, "u_sum")
            .as_primitive::<moruna_kernel::arrow::datatypes::UInt64Type>()
            .value(0),
        u64::MAX
    );
    assert_eq!(floats(&out, "fare_sum"), vec![Some(0.30000000000000004)]);
}

#[test]
fn keys_of_every_kind_group_and_order() {
    // Strings, in byte order, null last.
    let k = kernel(json!({"keys": ["s"], "aggregates": [{"op": "count", "as": "n"}]}));
    let b = batch(vec![(
        "s",
        Arc::new(StringArray::from(vec![
            Some("b"),
            None,
            Some("a"),
            Some("é"),
            Some("b"),
            Some("B"),
        ])) as ArrayRef,
    )]);
    let out = run(&k, &[b]).unwrap().unwrap();
    let s: Vec<Option<&str>> = col(&out, "s").as_string::<i32>().iter().collect();
    assert_eq!(s, vec![Some("B"), Some("a"), Some("b"), Some("é"), None]);
    assert_eq!(
        ints(&out, "n"),
        vec![Some(1), Some(1), Some(2), Some(1), Some(1)]
    );

    // Float keys: −0.0 with 0.0, every NaN one group, total order, null last.
    let k = kernel(json!({"keys": ["x"], "aggregates": [{"op": "count", "as": "n"}]}));
    let neg_nan = f64::from_bits(f64::NAN.to_bits() | (1 << 63) | 1);
    let b = batch(vec![(
        "x",
        f64s(vec![
            Some(0.0),
            Some(-0.0),
            Some(f64::NAN),
            Some(neg_nan),
            None,
            Some(f64::NEG_INFINITY),
            Some(f64::INFINITY),
        ]),
    )]);
    let out = run(&k, &[b]).unwrap().unwrap();
    let x = floats(&out, "x");
    assert_eq!(x[0], Some(f64::NEG_INFINITY));
    assert_eq!(x[1].unwrap().to_bits(), 0.0f64.to_bits());
    assert_eq!(x[2], Some(f64::INFINITY));
    assert!(x[3].unwrap().is_nan());
    assert_eq!(x[4], None);
    assert_eq!(
        ints(&out, "n"),
        vec![Some(1), Some(2), Some(1), Some(2), Some(1)]
    );

    // Float32, Boolean and Int32 keys together pack; a string and a decimal key do not.
    let k = kernel(json!({"keys": ["f", "b", "i"], "aggregates": [{"op": "count", "as": "n"}]}));
    let b = batch(vec![
        (
            "f",
            Arc::new(Float32Array::from(vec![
                Some(-0.0f32),
                Some(0.0),
                Some(1.5),
            ])) as ArrayRef,
        ),
        (
            "b",
            Arc::new(BooleanArray::from(vec![Some(true), Some(true), None])) as ArrayRef,
        ),
        (
            "i",
            Arc::new(Int32Array::from(vec![Some(7), Some(7), Some(7)])) as ArrayRef,
        ),
    ]);
    let out = run(&k, &[b]).unwrap().unwrap();
    assert_eq!(out.num_rows(), 2);
    assert_eq!(ints(&out, "n"), vec![Some(2), Some(1)]);

    let k = kernel(
        json!({"keys": ["s", "d"], "aggregates": [{"op": "sum", "column": "v", "as": "t"}]}),
    );
    let d = Decimal128Array::from(vec![Some(5), Some(5), Some(-1)])
        .with_precision_and_scale(10, 2)
        .unwrap();
    let b = batch(vec![
        (
            "s",
            Arc::new(StringArray::from(vec![Some("x"), Some("x"), Some("x")])) as ArrayRef,
        ),
        ("d", Arc::new(d) as ArrayRef),
        (
            "v",
            Arc::new(Int32Array::from(vec![Some(1), Some(2), Some(4)])) as ArrayRef,
        ),
    ]);
    let out = run(&k, &[b.clone(), b]).unwrap().unwrap();
    assert_eq!(ints(&out, "t"), vec![Some(8), Some(6)]);
}

#[test]
fn empty_input_gives_nothing() {
    let k = kernel(json!({"keys": ["zone"], "aggregates": [{"op": "count", "as": "n"}]}));
    assert!(run(&k, &[]).unwrap().is_none());
    let empty = batch(vec![("zone", i64s(vec![]))]);
    assert!(run(&k, std::slice::from_ref(&empty)).unwrap().is_none());
    let (out, _) = run_split(&k, &[empty], 3).unwrap();
    assert!(out.is_none());
    let dense = kernel(json!({
        "keys": ["zone"],
        "aggregates": [{"op": "count", "as": "n"}],
        "domain": {"zone": [0, 3]},
    }));
    assert!(
        run(&dense, &[batch(vec![("zone", i64s(vec![]))])])
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_group_of_only_nulls_has_null_aggregates_and_a_count() {
    let k = kernel(json!({"keys": ["k"], "aggregates": every_aggregate_on("v")}));
    let b = batch(vec![
        ("k", i64s(vec![Some(1), Some(1)])),
        ("v", f64s(vec![None, None])),
    ]);
    let out = run(&k, &[b]).unwrap().unwrap();
    assert_eq!(ints(&out, "rows"), vec![Some(2)]);
    assert_eq!(ints(&out, "values"), vec![Some(0)]);
    for c in ["sum", "min", "max", "mean"] {
        assert!(col(&out, c).is_null(0), "{c}");
    }
}

fn every_aggregate_on(c: &str) -> Value {
    json!([
        {"op": "count", "as": "rows"},
        {"op": "count", "column": c, "as": "values"},
        {"op": "sum", "column": c, "as": "sum"},
        {"op": "min", "column": c, "as": "min"},
        {"op": "max", "column": c, "as": "max"},
        {"op": "mean", "column": c, "as": "mean"},
    ])
}

#[test]
fn min_and_max_of_floats_use_total_order() {
    let k = kernel(json!({"keys": ["k"], "aggregates": every_aggregate_on("v")}));
    let b = batch(vec![
        ("k", i64s(vec![Some(1), Some(1), Some(2), Some(2)])),
        (
            "v",
            f64s(vec![
                Some(0.0),
                Some(-0.0),
                Some(f64::NEG_INFINITY),
                Some(-1.0),
            ]),
        ),
    ]);
    let out = run(&k, &[b]).unwrap().unwrap();
    let min = floats(&out, "min");
    let max = floats(&out, "max");
    assert_eq!(min[0].unwrap().to_bits(), (-0.0f64).to_bits());
    assert_eq!(max[0].unwrap().to_bits(), 0.0f64.to_bits());
    assert_eq!(min[1], Some(f64::NEG_INFINITY));
    assert_eq!(max[1], Some(-1.0));
    assert_eq!(floats(&out, "sum")[0].unwrap().to_bits(), 0.0f64.to_bits());
    assert_eq!(floats(&out, "mean")[1], Some(f64::NEG_INFINITY));
}

#[test]
fn unsigned_and_narrow_values_keep_their_types() {
    let k = kernel(json!({"keys": ["k"], "aggregates": every_aggregate_on("v")}));
    let b = batch(vec![
        ("k", i64s(vec![Some(1), Some(1)])),
        (
            "v",
            Arc::new(UInt8Array::from(vec![Some(200), Some(100)])) as ArrayRef,
        ),
    ]);
    let out = run(&k, &[b]).unwrap().unwrap();
    assert_eq!(col(&out, "sum").data_type(), &DataType::UInt64);
    assert_eq!(col(&out, "min").data_type(), &DataType::UInt8);
    assert_eq!(
        col(&out, "min")
            .as_primitive::<moruna_kernel::arrow::datatypes::UInt8Type>()
            .value(0),
        100
    );
    assert_eq!(
        col(&out, "max")
            .as_primitive::<moruna_kernel::arrow::datatypes::UInt8Type>()
            .value(0),
        200
    );
    assert_eq!(floats(&out, "mean"), vec![Some(150.0)]);
}

#[test]
fn configuration_is_checked() {
    assert!(refusal(json!([])).contains("arguments must be an object"));
    assert!(refusal(json!({"keys": ["a"], "bogus": 1})).contains("unknown argument `bogus`"));
    assert!(refusal(json!({})).contains("`keys` is required"));
    assert!(refusal(json!({"keys": []})).contains("names no column"));
    assert!(refusal(json!({"keys": "a"})).contains("must be a list"));
    assert!(refusal(json!({"keys": [1]})).contains("must list column names"));
    assert!(refusal(json!({"keys": ["a"], "aggregates": {}})).contains("must be a list"));
    assert!(refusal(json!({"keys": ["a"], "aggregates": [1]})).contains("must be an object"));
    assert!(
        refusal(json!({"keys": ["a"], "aggregates": [{"op": "count", "as": "n", "x": 1}]}))
            .contains("not `x`")
    );
    assert!(refusal(json!({"keys": ["a"], "aggregates": [{"as": "n"}]})).contains("needs `op`"));
    assert!(
        refusal(json!({"keys": ["a"], "aggregates": [{"op": "median", "as": "n"}]}))
            .contains("is not an aggregate")
    );
    assert!(
        refusal(json!({"keys": ["a"], "aggregates": [{"op": "sum", "as": "n"}]}))
            .contains("needs a `column`")
    );
    assert!(
        refusal(json!({"keys": ["a"], "aggregates": [{"op": "count"}]})).contains("needs `as`")
    );
    assert!(
        refusal(json!({"keys": ["a"], "aggregates": [{"op": "count", "as": 3}]}))
            .contains("must be a name")
    );
    assert!(
        refusal(json!({"keys": ["a"], "aggregates": [{"op": "count", "as": "a"}]}))
            .contains("named twice")
    );
    assert!(refusal(json!({"keys": ["a", "a"]})).contains("named twice"));
    assert!(refusal(json!({
        "keys": ["a"],
        "aggregates": [{"op": "mean", "column": "v", "as": "m"}, {"op": "count", "as": "m.sum"}],
        "mode": "partial",
    }))
    .contains("named twice"));
    assert!(refusal(json!({"keys": ["a"], "mode": "both"})).contains("single, partial or final"));
    assert!(refusal(json!({"keys": ["a"], "mode": 1})).contains("must be a string"));
    assert!(refusal(json!({"keys": ["a"], "instances": 0})).contains("whole number"));
    assert!(
        refusal(json!({"keys": ["a"], "instances": 65, "mode": "partial"}))
            .contains("whole number")
    );
    assert!(refusal(json!({"keys": ["a"], "instances": 2})).contains("only `partial` mode"));
    assert!(refusal(json!({"keys": ["a"], "domain": []})).contains("must map each key"));
    assert!(refusal(json!({"keys": ["a"], "domain": {"b": [0, 1]}})).contains("not a key"));
    assert!(refusal(json!({"keys": ["a", "b"], "domain": {"a": [0, 1]}})).contains("`b` has none"));
    assert!(refusal(json!({"keys": ["a"], "domain": {"a": [0]}})).contains("must be [lo, hi]"));
    assert!(
        refusal(json!({"keys": ["a"], "domain": {"a": [0, "x"]}})).contains("must be an integer")
    );
    assert!(refusal(json!({"keys": ["a"], "domain": {"a": [2, 1]}})).contains("is empty"));
    // A domain too large for the dense path is aggregated on the hash path.
    let big = kernel(json!({"keys": ["a", "b"], "domain": {"a": [0, 99_999], "b": [0, 99_999]}}));
    assert_eq!(big.path(), Path::Hash);
    assert!(big.hints().state_bytes.is_none());
    let huge = kernel(json!({"keys": ["a", "b", "c"], "domain": {
        "a": [i64::MIN, i64::MAX], "b": [i64::MIN, i64::MAX], "c": [0, 1]}}));
    assert_eq!(huge.dense_bytes(), None);
    let small = kernel(
        json!({"keys": ["a"], "aggregates": [{"op": "count", "as": "n"}], "domain": {"a": [0, 9]}}),
    );
    assert_eq!(small.path(), Path::Dense);
    assert_eq!(small.hints().state_bytes, Some(11 * 16));
    assert_eq!(small.keys(), ["a"]);
    assert_eq!(small.aggregates().len(), 1);
    assert_eq!(small.mode(), Mode::Single);
    assert_eq!(small.args()["domain"]["a"], json!([0, 9]));
}

#[test]
fn inputs_are_checked_against_the_configuration() {
    let schema = |fields: Vec<Field>| SourceSchema::Table(Arc::new(Schema::new(fields)));
    let plan_err = |k: &GroupAggregate, s: SourceSchema| match k.output_schema(&s) {
        Err(MorunaError::Plan(msg)) => msg,
        other => panic!("expected a refusal, got {other:?}"),
    };
    let k = kernel(json!({"keys": ["a"], "aggregates": [{"op": "sum", "column": "v", "as": "t"}]}));
    assert!(
        plan_err(&k, schema(vec![Field::new("v", DataType::Int64, true)]))
            .contains("no column `a`")
    );
    assert!(
        plan_err(
            &k,
            schema(vec![
                Field::new("a", DataType::new_list(DataType::Int64, true), true),
                Field::new("v", DataType::Int64, true),
            ])
        )
        .contains("keys are fixed-width")
    );
    assert!(
        plan_err(
            &k,
            schema(vec![
                Field::new("a", DataType::Int64, true),
                Field::new("v", DataType::Utf8, true)
            ])
        )
        .contains("does not take `v`")
    );
    let dense = kernel(json!({"keys": ["a"], "domain": {"a": [0, 1]}}));
    assert!(
        plan_err(&dense, schema(vec![Field::new("a", DataType::Utf8, true)]))
            .contains("integer keys")
    );
    let tensor = SourceSchema::Tensor {
        dtype: moruna_kernel::DType::F32,
        shape: vec![1],
    };
    assert!(k.output_schema(&tensor).is_err());
    // Final mode reads partial columns by the aggregate's name.
    let fin = kernel(json!({"keys": ["a"], "mode": "final", "aggregates": [
        {"op": "count", "as": "n"}, {"op": "sum", "column": "v", "as": "t"},
        {"op": "max", "column": "v", "as": "m"}, {"op": "mean", "column": "v", "as": "avg"}]}));
    let good = vec![
        Field::new("a", DataType::Int64, true),
        Field::new("n", DataType::Int64, false),
        Field::new("t", DataType::Binary, true),
        Field::new("m", DataType::Float32, true),
        Field::new("avg.sum", DataType::Decimal128(38, 0), false),
        Field::new("avg.count", DataType::Int64, false),
    ];
    let SourceSchema::Table(out) = fin.output_schema(&schema(good.clone())).unwrap() else {
        panic!()
    };
    let types: Vec<&DataType> = out.fields().iter().map(|f| f.data_type()).collect();
    assert_eq!(
        types,
        [
            &DataType::Int64,
            &DataType::Int64,
            &DataType::Float64,
            &DataType::Float32,
            &DataType::Float64
        ]
    );
    let with = |i: usize, f: Field| {
        let mut g = good.clone();
        g[i] = f;
        schema(g)
    };
    assert!(plan_err(&fin, with(1, Field::new("n", DataType::Int32, false))).contains("is Int64"));
    assert!(
        plan_err(&fin, with(2, Field::new("t", DataType::Int64, true)))
            .contains("Decimal128(38, 0) or Binary")
    );
    assert!(
        plan_err(&fin, with(3, Field::new("m", DataType::Utf8, true)))
            .contains("a number or a time")
    );
    assert!(
        plan_err(
            &fin,
            with(5, Field::new("avg.count", DataType::UInt64, true))
        )
        .contains("is Int64")
    );
}

#[test]
fn a_batch_of_another_schema_is_refused() {
    let k = kernel(json!({"keys": ["a"], "aggregates": [{"op": "sum", "column": "v", "as": "t"}]}));
    let one = batch(vec![("a", i64s(vec![Some(1)])), ("v", i64s(vec![Some(1)]))]);
    let two = batch(vec![
        ("a", i64s(vec![Some(1)])),
        ("v", f64s(vec![Some(1.0)])),
    ]);
    match run(&k, &[one, two]) {
        Err(MorunaError::Kernel { msg, .. }) => {
            assert!(msg.contains("differ from the first"), "{msg}")
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn ctx() -> InitCtx {
    InitCtx {
        instance: 0,
        device: None,
        alloc: Arc::new(FakeAllocator::new()),
    }
}

#[test]
fn the_kernel_contract() {
    let k = kernel(json!({
        "keys": ["zone", "minute"],
        "aggregates": every_aggregate(),
        "domain": {"zone": [0, 9], "minute": [0, 1439]},
    }));
    assert!(matches!(k.kind(), KernelKind::Stateful { max_instances } if max_instances.get() == 1));
    let hints = k.hints();
    assert_eq!(hints.resume, ResumePolicy::Checkpoint);
    assert_eq!(hints.state_bytes, k.dense_bytes());
    assert_eq!(k.fingerprint(), crate::std_fingerprint(IDENTITY, k.args()));
    let id = crate::catalogue::KernelId::parse(IDENTITY).unwrap();
    assert_eq!(id.name, NAME);
    assert_eq!(
        crate::catalogue::fingerprint(&id, &Params::json(k.args().clone())).unwrap(),
        k.fingerprint()
    );
    let configured =
        crate::catalogue::configure(IDENTITY, &Params::json(k.args().clone())).unwrap();
    assert_eq!(configured.fingerprint(), k.fingerprint());
    assert!(configure(2, &Params::json(k.args().clone())).is_err());
    let mut with_arrays = Params::json(k.args().clone());
    with_arrays.arrays.insert("x".into(), trips()[0].clone());
    assert!(configure(1, &with_arrays).is_err());
    assert!(k.declared().is_checkable());
    assert_eq!(k.accepts().kind, moruna_kernel::PayloadKind::Table);
    let mut state = k.init(&ctx()).unwrap();
    assert_eq!(state.footprint().unwrap(), Some(0));
    for b in trips() {
        let out = k.apply(state.as_mut(), Payload::table(b).unwrap()).unwrap();
        assert_eq!(out.rows(), 0);
    }
    assert!(state.footprint().unwrap().unwrap() >= 11 * 1441 * 8);
    let Some(Payload::Table(out, _)) = k.finish(state.as_mut()).unwrap() else {
        panic!("finish gives a table")
    };
    check_trips(&out);
    // A state the kernel did not make is refused.
    let mut other = moruna_kernel::NoState;
    assert!(
        k.apply(&mut other, Payload::table(trips()[0].clone()).unwrap())
            .is_err()
    );
    assert!(k.finish(&mut other).is_err());
    // The declarations of the partial and final forms.
    let partial = k.in_mode(Mode::Partial, 3).unwrap();
    let declared = partial.declared();
    let Some(SchemaDecl::Exact(out)) = declared.output else {
        panic!()
    };
    assert!(out.iter().any(|c| c.name == "n_mean.sum"));
    let fin = k.in_mode(Mode::Final, 1).unwrap();
    let Some(SchemaDecl::Subset(input)) = fin.declared().input else {
        panic!()
    };
    assert!(input.iter().any(|c| c.name == "fare_mean.count"));
    assert_eq!(fin.in_mode(Mode::Single, 1).unwrap().mode(), Mode::Single);
}

#[test]
fn checkpoints_restore_in_every_mode() {
    let single = kernel(json!({"keys": ["zone", "minute"], "aggregates": every_aggregate()}));
    let rows = trips();
    for k in [
        single.clone(),
        single.in_mode(Mode::Partial, 2).unwrap(),
        kernel(json!({
            "keys": ["zone", "minute"],
            "aggregates": every_aggregate(),
            "domain": {"zone": [0, 9], "minute": [0, 1439]},
        })),
    ] {
        let mut state = k.init(&ctx()).unwrap();
        // Before any batch, a checkpoint is empty and restores to an empty state.
        let empty = state.checkpoint().unwrap().unwrap();
        assert!(empty.is_empty());
        let fresh = k.restore(&ctx(), &empty).unwrap();
        drop(fresh);
        k.apply(state.as_mut(), Payload::table(rows[0].clone()).unwrap())
            .unwrap();
        let bytes = state.checkpoint().unwrap().unwrap();
        let mut restored = k.restore(&ctx(), &bytes).unwrap();
        k.apply(restored.as_mut(), Payload::table(rows[1].clone()).unwrap())
            .unwrap();
        let Some(Payload::Table(out, _)) = k.finish(restored.as_mut()).unwrap() else {
            panic!()
        };
        let expect = run(&k, &rows).unwrap().unwrap();
        assert_eq!(format!("{out:?}"), format!("{expect:?}"));
    }
    // A final instance restores what it had merged.
    let partial = single.in_mode(Mode::Partial, 2).unwrap();
    let fin = single.in_mode(Mode::Final, 1).unwrap();
    let p0 = run(&partial, &rows[..1]).unwrap().unwrap();
    let p1 = run(&partial, &rows[1..]).unwrap().unwrap();
    let mut state = fin.init(&ctx()).unwrap();
    fin.apply(state.as_mut(), Payload::table(p0).unwrap())
        .unwrap();
    let bytes = state.checkpoint().unwrap().unwrap();
    let mut restored = fin.restore(&ctx(), &bytes).unwrap();
    fin.apply(restored.as_mut(), Payload::table(p1).unwrap())
        .unwrap();
    let Some(Payload::Table(out, _)) = fin.finish(restored.as_mut()).unwrap() else {
        panic!()
    };
    check_trips(&out);
    // Malformed checkpoints are refused.
    for bad in [vec![1u8], vec![9, 0, 0, 0, 0, 0, 0, 0, 1], {
        let mut b = (2u64).to_le_bytes().to_vec();
        b.extend_from_slice(&[0, 0]);
        b
    }] {
        assert!(matches!(
            single.restore(&ctx(), &bad),
            Err(MorunaError::Resume(_))
        ));
    }
    // A checkpoint of one configuration does not restore into another.
    let mut state = single.init(&ctx()).unwrap();
    single
        .apply(state.as_mut(), Payload::table(rows[0].clone()).unwrap())
        .unwrap();
    let bytes = state.checkpoint().unwrap().unwrap();
    let other =
        kernel(json!({"keys": ["zone", "minute"], "aggregates": [{"op": "count", "as": "rows"}]}));
    assert!(other.restore(&ctx(), &bytes).is_err());
}

#[test]
fn keys_only_is_distinct_keys_in_order() {
    let k = kernel(json!({"keys": ["a"]}));
    let b = batch(vec![("a", i64s(vec![Some(3), Some(1), Some(3), None]))]);
    let out = run(&k, &[b]).unwrap().unwrap();
    assert_eq!(out.num_columns(), 1);
    assert_eq!(ints(&out, "a"), vec![Some(1), Some(3), None]);
}

#[test]
fn a_sliced_input_is_read_where_it_lies() {
    let k = kernel(json!({"keys": ["a"], "aggregates": every_aggregate_on("v")}));
    let b = batch(vec![
        ("a", i64s(vec![Some(9), Some(1), Some(1), Some(9)])),
        ("v", i64s(vec![Some(100), Some(2), None, Some(100)])),
    ]);
    let out = run(&k, &[b.slice(1, 2)]).unwrap().unwrap();
    assert_eq!(ints(&out, "a"), vec![Some(1)]);
    assert_eq!(ints(&out, "sum"), vec![Some(2)]);
    assert_eq!(ints(&out, "values"), vec![Some(1)]);
    let dense = kernel(
        json!({"keys": ["a"], "aggregates": every_aggregate_on("v"), "domain": {"a": [0, 9]}}),
    );
    let out2 = run(&dense, &[b.slice(1, 2)]).unwrap().unwrap();
    assert_eq!(format!("{out:?}"), format!("{out2:?}"));
}

#[test]
fn measurement_float_sums_are_single_mode_only() {
    let k = kernel(json!({"keys": ["k"], "aggregates": [{"op": "sum", "column": "v", "as": "s"}]}));
    let b = batch(vec![
        ("k", i64s(vec![Some(1), Some(1), Some(2), Some(2)])),
        ("v", f64s(vec![Some(0.5), Some(0.25), Some(1e16), None])),
    ]);
    let exact = run(&k, std::slice::from_ref(&b)).unwrap().unwrap();
    for how in [FloatSum::Exact, FloatSum::Binned, FloatSum::Plain] {
        let m = k.measure_float_sum(how);
        let st = run_state(&m, std::slice::from_ref(&b)).unwrap();
        assert!(st.footprint().unwrap().unwrap() > 0);
        let out = finish_state(&m, &st).unwrap().unwrap();
        assert_eq!(floats(&out, "s"), floats(&exact, "s"), "{how:?}");
        // A group that spans the dense path's decline keeps its sum.
        let mut d = m.clone();
        d.domain = Some(vec![KeyRange { lo: 1, hi: 1 }]);
        d.path = Path::Dense;
        let st = run_state(&d, std::slice::from_ref(&b)).unwrap();
        assert!(st.declined().is_some());
        assert_eq!(
            floats(&finish_state(&d, &st).unwrap().unwrap(), "s"),
            floats(&exact, "s")
        );
        if how != FloatSum::Exact {
            let partial = m.in_mode(Mode::Partial, 1).unwrap().measure_float_sum(how);
            assert!(run(&partial, std::slice::from_ref(&b)).is_err());
            let fin = m.in_mode(Mode::Final, 1).unwrap().measure_float_sum(how);
            let p = run(
                &k.in_mode(Mode::Partial, 1).unwrap(),
                std::slice::from_ref(&b),
            )
            .unwrap()
            .unwrap();
            assert!(run(&fin, &[p]).is_err());
        }
    }
}
