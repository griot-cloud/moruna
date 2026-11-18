//! `moruna.expr`'s reference: the JSON form, the checks, and every function on hand-written
//! batches whose answers are pyarrow 24's.

use std::sync::Arc;

use moruna_kernel::arrow::array::{
    Array, ArrayRef, AsArray, BooleanArray, Date32Array, Decimal128Array, DurationMicrosecondArray,
    Float32Array, Float64Array, Int8Array, Int16Array, Int32Array, Int64Array, LargeStringArray,
    StringArray, TimestampMicrosecondArray, TimestampNanosecondArray, TimestampSecondArray,
    UInt8Array, UInt16Array, UInt32Array, UInt64Array,
};
use moruna_kernel::arrow::datatypes::{
    DataType, Field, Float64Type, Int8Type, Int64Type, Schema, TimeUnit,
};
use moruna_kernel::arrow::record_batch::RecordBatch;
use serde_json::{Value, json};

use super::*;

fn batch(cols: Vec<(&str, ArrayRef)>) -> RecordBatch {
    let fields: Vec<Field> = cols
        .iter()
        .map(|(n, a)| Field::new(*n, a.data_type().clone(), true))
        .collect();
    RecordBatch::try_new(
        Arc::new(Schema::new(fields)),
        cols.into_iter().map(|(_, a)| a).collect(),
    )
    .expect("batch")
}

/// The standard test batch: integers, floats with NaN and -0.0, strings, timestamps.
fn sample() -> RecordBatch {
    batch(vec![
        (
            "i",
            Arc::new(Int64Array::from(vec![
                Some(1),
                Some(-7),
                None,
                Some(i64::MAX),
            ])) as ArrayRef,
        ),
        (
            "j",
            Arc::new(Int64Array::from(vec![Some(2), Some(2), Some(3), Some(1)])),
        ),
        (
            "f",
            Arc::new(Float64Array::from(vec![
                Some(1.5),
                Some(f64::NAN),
                Some(-0.0),
                None,
            ])),
        ),
        (
            "g",
            Arc::new(Float64Array::from(vec![
                Some(0.0),
                Some(1.0),
                Some(0.0),
                Some(2.0),
            ])),
        ),
        (
            "s",
            Arc::new(StringArray::from(vec![
                Some("a"),
                Some("b"),
                None,
                Some("é"),
            ])),
        ),
        (
            "b",
            Arc::new(BooleanArray::from(vec![
                Some(true),
                Some(false),
                None,
                Some(true),
            ])),
        ),
        (
            "t",
            Arc::new(TimestampMicrosecondArray::from(vec![
                Some(-1),
                Some(0),
                Some(1_704_067_200_000_000 + 17 * 3_600_000_000 + 123_456),
                None,
            ])),
        ),
    ])
}

/// One output `x` of `expr` over `b`.
fn one(expr: Value, b: &RecordBatch) -> ArrayRef {
    try_one(expr, b).expect("evaluates")
}

fn try_one(expr: Value, b: &RecordBatch) -> moruna_kernel::Result<ArrayRef> {
    let tree = Tree::from_json(&json!({"outputs": [{"name": "x", "expr": expr}]}))?;
    let out = tree.bind(&b.schema())?.eval(b)?;
    Ok(Arc::clone(out.column(0)))
}

fn err(expr: Value, b: &RecordBatch) -> String {
    try_one(expr, b).expect_err("refused").to_string()
}

fn f64s(a: &ArrayRef) -> Vec<Option<f64>> {
    a.as_primitive::<Float64Type>().iter().collect()
}

fn i64s(a: &ArrayRef) -> Vec<Option<i64>> {
    a.as_primitive::<Int64Type>().iter().collect()
}

fn bools(a: &ArrayRef) -> Vec<Option<bool>> {
    a.as_boolean().iter().collect()
}

fn c(name: &str) -> Value {
    json!({"column": name})
}

fn call(name: &str, args: Vec<Value>) -> Value {
    json!({"call": name, "args": args})
}

fn bits(v: Vec<Option<f64>>) -> Vec<Option<u64>> {
    v.into_iter().map(|x| x.map(f64::to_bits)).collect()
}

#[test]
fn the_json_form_round_trips_and_is_read_strictly() {
    let args = json!({"outputs": [
        {"name": "a", "expr": {"cast": {"column": "i"}, "to": "double", "safe": false}},
        {"name": "b", "expr": {"if": [{"when": {"column": "b"}, "then": {"literal": 1}}], "else": {"literal": null, "type": "int64"}}},
        {"name": "c", "expr": {"switch": {"column": "s"}, "cases": [{"match": {"literal": "a"}, "then": {"literal": 1.5}}], "else": {"literal": 2.5}}},
        {"name": "d", "expr": {"call": "round", "args": [{"column": "f"}], "options": {"ndigits": 1}}},
        {"name": "e", "expr": {"call": "add", "args": [{"literal": 1, "type": "duration[ms]"}, {"literal": 2, "type": "duration[ms]"}]}},
        {"name": "f", "expr": {"cast": {"column": "i"}, "to": "int32"}},
        {"name": "g", "expr": {"if": [{"when": {"column": "b"}, "then": {"literal": 1}}]}},
        {"name": "h", "expr": {"switch": {"column": "s"}, "cases": [{"match": {"literal": "a"}, "then": {"literal": 1}}]}},
        {"name": "k", "expr": {"call": "is_null", "args": [{"column": "f"}]}},
    ]});
    let tree = Tree::from_json(&args).expect("reads");
    assert_eq!(tree.to_json(), args);
    assert_eq!(Tree::from_json(&tree.to_json()).expect("again"), tree);
    assert_eq!(tree.columns(), vec!["i", "b", "s", "f"]);
    assert_eq!(
        spell_data_type(&DataType::Duration(TimeUnit::Second)),
        "duration[s]"
    );
    assert_eq!(
        spell_data_type(&DataType::Duration(TimeUnit::Nanosecond)),
        "duration[ns]"
    );
    assert_eq!(
        spell_data_type(&DataType::Duration(TimeUnit::Microsecond)),
        "duration[us]"
    );
    assert!(parse_data_type("duration[h]").is_err());
    assert!(parse_data_type("any").is_err());
    assert_eq!(
        Node::Scalar(Arc::new(Int64Array::from(vec![5]))).to_json(),
        json!({"literal": "5", "type": "int64"})
    );
    let bad = [
        (json!([]), "expected an object"),
        (json!({}), "`outputs` is required"),
        (json!({"outputs": []}), "lists one or more"),
        (json!({"outputs": [{"name": "a"}]}), "`expr` is required"),
        (
            json!({"outputs": [{"name": "", "expr": {"column": "i"}}]}),
            "non-empty",
        ),
        (
            json!({"outputs": [{"name": "a", "expr": {"column": "i"}}, {"name": "a", "expr": {"column": "j"}}]}),
            "two outputs",
        ),
        (
            json!({"outputs": [{"name": "a", "expr": {"column": "i"}, "x": 1}]}),
            "unknown key `x`",
        ),
        (json!({"outputs": [], "more": 1}), "unknown key `more`"),
        (
            json!({"outputs": [{"name": "a", "expr": {}}]}),
            "a node is one of",
        ),
        (
            json!({"outputs": [{"name": "a", "expr": {"column": "i", "literal": 1}}]}),
            "a node is one kind",
        ),
        (
            json!({"outputs": [{"name": "a", "expr": {"column": 1}}]}),
            "with a string",
        ),
        (
            json!({"outputs": [{"name": "a", "expr": {"literal": [1]}}]}),
            "a literal is",
        ),
        (
            json!({"outputs": [{"name": "a", "expr": {"literal": null}}]}),
            "needs a `type`",
        ),
        (
            json!({"outputs": [{"name": "a", "expr": {"literal": 1, "type": 5}}]}),
            "`type` is a string",
        ),
        (
            json!({"outputs": [{"name": "a", "expr": {"literal": 1, "type": "nope"}}]}),
            "nope",
        ),
        (
            json!({"outputs": [{"name": "a", "expr": {"cast": {"column": "i"}}}]}),
            "`to` is required",
        ),
        (
            json!({"outputs": [{"name": "a", "expr": {"cast": {"column": "i"}, "to": 1}}]}),
            "`to` is a type",
        ),
        (
            json!({"outputs": [{"name": "a", "expr": {"cast": {"column": "i"}, "to": "int64", "safe": 1}}]}),
            "`safe` is true or false",
        ),
        (
            json!({"outputs": [{"name": "a", "expr": {"if": []}}]}),
            "one or more",
        ),
        (
            json!({"outputs": [{"name": "a", "expr": {"if": [{"when": {"column": "b"}}]}}]}),
            "`then` is required",
        ),
        (
            json!({"outputs": [{"name": "a", "expr": {"switch": {"column": "s"}, "cases": [{"match": {"column": "s"}, "then": {"literal": 1}}]}}]}),
            "`match` is a literal",
        ),
        (
            json!({"outputs": [{"name": "a", "expr": {"switch": {"column": "s"}}}]}),
            "`cases` is required",
        ),
        (
            json!({"outputs": [{"name": "a", "expr": {"call": 3}}]}),
            "names a function",
        ),
        (
            json!({"outputs": [{"name": "a", "expr": {"call": "add", "args": 3}}]}),
            "`args` is a list",
        ),
        (
            json!({"outputs": [{"name": "a", "expr": {"call": "add", "options": 3}}]}),
            "`options` is an object",
        ),
    ];
    for (args, want) in bad {
        let e = Tree::from_json(&args)
            .expect_err(&args.to_string())
            .to_string();
        assert!(e.contains(want), "{args}: {e}");
    }
}

#[test]
fn the_json_schema_names_every_function_and_option() {
    let schema: Value = serde_json::from_str(SCHEMA).expect("the schema is JSON");
    let names: Vec<&str> = schema["$defs"]["call"]["properties"]["call"]["enum"]
        .as_array()
        .expect("enum")
        .iter()
        .map(|v| v.as_str().expect("name"))
        .collect();
    assert_eq!(names, FUNCTIONS);
    let modes: Vec<&str> =
        schema["$defs"]["call"]["properties"]["options"]["properties"]["round_mode"]["enum"]
            .as_array()
            .expect("modes")
            .iter()
            .map(|v| v.as_str().expect("mode"))
            .collect();
    let want: Vec<&str> = round::MODES.iter().map(|(n, _)| *n).collect();
    assert_eq!(modes, want);
    let temporal: Vec<&str> = FUNCTIONS[FUNCTIONS.len() - 14..].to_vec();
    assert_eq!(temporal, temporal::NAMES);
}

#[test]
fn binding_refuses_what_has_no_kernel_naming_where() {
    let b = sample();
    let cases = [
        (c("zz"), "no column `zz`"),
        (call("nope", vec![c("i")]), "no function `nope`"),
        (call("add", vec![c("i")]), "takes 2 argument(s)"),
        (
            call("add", vec![c("i"), c("s")]),
            "no kernel for (int64, string)",
        ),
        (call("divide", vec![c("t"), c("i")]), "no kernel"),
        (call("negate", vec![c("s")]), "no kernel"),
        (call("sign", vec![c("t")]), "no kernel"),
        (call("ln", vec![c("s")]), "no kernel"),
        (call("round", vec![c("s")]), "no kernel"),
        (call("equal", vec![c("i"), c("s")]), "no kernel"),
        (call("and", vec![c("i"), c("b")]), "no kernel"),
        (call("invert", vec![c("i")]), "no kernel"),
        (call("is_nan", vec![c("s")]), "no kernel"),
        (call("if_else", vec![c("i"), c("i"), c("j")]), "no kernel"),
        (
            call("if_else", vec![c("b"), c("i"), c("s")]),
            "no common type",
        ),
        (call("coalesce", vec![]), "one or more"),
        (call("fill_null", vec![c("i")]), "takes 2"),
        (call("max_element_wise", vec![c("s"), c("s")]), "no kernel"),
        (call("hour", vec![c("i")]), "no kernel"),
        (
            json!({"call": "round", "args": [c("f")], "options": {"ndigits": 1.5}}),
            "`ndigits` is an integer",
        ),
        (
            json!({"call": "round", "args": [c("f")], "options": {"round_mode": "nearest"}}),
            "no round mode",
        ),
        (
            json!({"call": "round", "args": [c("f")], "options": {"round_mode": 1}}),
            "is a string",
        ),
        (
            json!({"call": "round", "args": [c("f")], "options": {"digits": 1}}),
            "has no option `digits`",
        ),
        (
            json!({"call": "add", "args": [c("f"), c("f")], "options": {"x": 1}}),
            "takes no options",
        ),
        (
            json!({"call": "less", "args": [c("f"), c("f")], "options": {"order": "lexical"}}),
            "\"ieee\" or \"total\"",
        ),
        (
            json!({"call": "is_null", "args": [c("f")], "options": {"nan_is_null": 1}}),
            "true or false",
        ),
        (
            json!({"call": "day_of_week", "args": [c("t")], "options": {"week_start": 8}}),
            "1 (Monday) to 7",
        ),
        (
            json!({"if": [{"when": c("i"), "then": c("i")}]}),
            "a condition is a boolean",
        ),
        (
            json!({"cast": c("s"), "to": "int64"}),
            "no cast from string to int64",
        ),
        (
            json!({"cast": c("t"), "to": "timestamp[us, tz=Mars/Olympus]"}),
            "is not known",
        ),
        (json!({"literal": 300, "type": "int8"}), "is not a int8"),
        (json!({"literal": 1.5, "type": "int64"}), "is not a int64"),
        (json!({"literal": "x", "type": "double"}), "is not a double"),
        (json!({"literal": -1, "type": "uint8"}), "is not a uint8"),
        (json!({"literal": 1, "type": "bool"}), "is not a bool"),
        (json!({"literal": 1, "type": "string"}), "is not a string"),
        (
            json!({"literal": "x", "type": "decimal128(10, 2)"}),
            "is not a decimal128",
        ),
        (
            call(
                "negate_checked",
                vec![json!({"literal": 1, "type": "uint8"})],
            ),
            "no kernel",
        ),
    ];
    for (expr, want) in cases {
        let e = err(expr.clone(), &b);
        assert!(e.contains(want), "{expr}: {e}");
        assert!(e.contains("outputs[0] `x`"), "{expr}: {e}");
    }
    let zoned = batch(vec![
        (
            "z",
            Arc::new(TimestampMicrosecondArray::from(vec![1]).with_timezone("UTC")) as ArrayRef,
        ),
        ("n", Arc::new(TimestampMicrosecondArray::from(vec![1]))),
        (
            "d",
            Arc::new(
                Decimal128Array::from(vec![1])
                    .with_precision_and_scale(10, 2)
                    .expect("dec"),
            ),
        ),
    ]);
    assert!(err(call("less", vec![c("z"), c("n")]), &zoned).contains("cannot be compared"));
    assert!(err(call("subtract", vec![c("z"), c("n")]), &zoned).contains("ambiguous"));
    assert!(err(call("coalesce", vec![c("d"), c("d")]), &zoned).contains("not selected"));
    let tree = Tree::from_json(&json!({"outputs": [{"name": "x", "expr": c("d")}, {"name": "y", "expr": call("is_null", vec![c("d")])}]})).expect("tree");
    let out = tree
        .bind(&zoned.schema())
        .expect("binds")
        .eval(&zoned)
        .expect("evals");
    assert_eq!(out.column(0).data_type(), &DataType::Decimal128(10, 2));
    assert_eq!(bools(out.column(1)), vec![Some(false)]);
}

#[test]
fn arithmetic_wraps_or_checks_as_arrow_does() {
    let b = sample();
    // Wrapping: i64::MAX + 1 wraps; a null stays null.
    assert_eq!(
        i64s(&one(call("add", vec![c("i"), json!({"literal": 1})]), &b)),
        vec![Some(2), Some(-6), None, Some(i64::MIN)]
    );
    let e = err(call("add_checked", vec![c("i"), json!({"literal": 1})]), &b);
    assert!(
        e.contains("row 3") && e.contains("add_checked: overflow"),
        "{e}"
    );
    assert_eq!(
        i64s(&one(call("subtract", vec![c("i"), c("j")]), &b)),
        vec![Some(-1), Some(-9), None, Some(i64::MAX - 1)]
    );
    assert_eq!(
        i64s(&one(call("multiply_checked", vec![c("i"), c("j")]), &b)),
        vec![Some(2), Some(-14), None, Some(i64::MAX)]
    );
    // Integer division truncates toward zero.
    assert_eq!(
        i64s(&one(call("divide", vec![c("i"), c("j")]), &b)),
        vec![Some(0), Some(-3), None, Some(i64::MAX)]
    );
    // int + double is a double; int64 beyond 2^53 does not convert.
    assert!(err(call("add", vec![c("i"), json!({"literal": 1.0})]), &b).contains("not in range"));
    let small = batch(vec![
        (
            "a",
            Arc::new(Int32Array::from(vec![i32::MAX, -7, 0])) as ArrayRef,
        ),
        ("z", Arc::new(Int32Array::from(vec![1, 0, 0]))),
        ("u", Arc::new(UInt8Array::from(vec![1, 2, 255]))),
        ("v", Arc::new(Int8Array::from(vec![1, -128, 3]))),
    ]);
    let a = one(call("add", vec![c("a"), c("z")]), &small);
    assert_eq!(a.data_type(), &DataType::Int32);
    assert_eq!(
        a.as_primitive::<moruna_kernel::arrow::datatypes::Int32Type>()
            .value(0),
        i32::MIN
    );
    assert!(err(call("add_checked", vec![c("a"), c("z")]), &small).contains("overflow"));
    assert!(
        err(call("divide", vec![c("a"), c("z")]), &small).contains("row 1: divide: divide by zero")
    );
    // uint8 with int8 is an int16.
    let m = one(call("add", vec![c("u"), c("v")]), &small);
    assert_eq!(m.data_type(), &DataType::Int16);
    let u = one(
        call(
            "subtract",
            vec![c("u"), json!({"literal": 2, "type": "uint8"})],
        ),
        &small,
    );
    assert_eq!(
        u.as_primitive::<moruna_kernel::arrow::datatypes::UInt8Type>()
            .values()
            .to_vec(),
        vec![255, 0, 253]
    );
    assert!(
        err(
            call(
                "subtract_checked",
                vec![c("u"), json!({"literal": 2, "type": "uint8"})]
            ),
            &small
        )
        .contains("row 0")
    );
    let w = one(
        call(
            "multiply",
            vec![c("v"), json!({"literal": 100, "type": "int8"})],
        ),
        &small,
    );
    assert_eq!(
        w.as_primitive::<Int8Type>().values().to_vec(),
        vec![100, 0, 44]
    );
    // MIN / -1: 0 unchecked, an error checked.
    let d = one(
        call(
            "divide",
            vec![c("v"), json!({"literal": -1, "type": "int8"})],
        ),
        &small,
    );
    assert_eq!(
        d.as_primitive::<Int8Type>().values().to_vec(),
        vec![-1, 0, -3]
    );
    assert!(
        err(
            call(
                "divide_checked",
                vec![c("v"), json!({"literal": -1, "type": "int8"})]
            ),
            &small
        )
        .contains("overflow")
    );
    // Powers.
    let p = batch(vec![
        (
            "x",
            Arc::new(Int64Array::from(vec![2, 3, 0, -1, 2])) as ArrayRef,
        ),
        (
            "e",
            Arc::new(Int64Array::from(vec![
                10,
                2,
                0,
                4_611_686_018_427_387_905,
                64,
            ])),
        ),
        (
            "n",
            Arc::new(Int64Array::from(vec![
                Some(1),
                Some(-1),
                None,
                Some(1),
                Some(1),
            ])),
        ),
    ]);
    assert_eq!(
        i64s(&one(call("power", vec![c("x"), c("e")]), &p)),
        vec![Some(1024), Some(9), Some(1), Some(-1), Some(0)]
    );
    assert!(
        err(call("power_checked", vec![c("x"), c("e")]), &p)
            .contains("row 4: power_checked: overflow")
    );
    assert!(err(call("power", vec![c("x"), c("n")]), &p).contains("negative integer powers"));
    let up = batch(vec![
        ("x", Arc::new(UInt64Array::from(vec![2u64, 3])) as ArrayRef),
        ("e", Arc::new(UInt64Array::from(vec![64u64, 3]))),
    ]);
    let r = one(call("power", vec![c("x"), c("e")]), &up);
    assert_eq!(
        r.as_primitive::<moruna_kernel::arrow::datatypes::UInt64Type>()
            .values()
            .to_vec(),
        vec![0, 27]
    );
    assert!(err(call("power_checked", vec![c("x"), c("e")]), &up).contains("overflow"));
    assert!(
        err(
            call(
                "multiply_checked",
                vec![c("x"), json!({"literal": u64::MAX, "type": "uint64"})]
            ),
            &up
        )
        .contains("overflow")
    );
    assert!(
        err(
            call(
                "divide",
                vec![c("x"), json!({"literal": 0, "type": "uint64"})]
            ),
            &up
        )
        .contains("divide by zero")
    );
    let q = one(
        call(
            "divide",
            vec![c("x"), json!({"literal": 2, "type": "uint64"})],
        ),
        &up,
    );
    assert_eq!(
        q.as_primitive::<moruna_kernel::arrow::datatypes::UInt64Type>()
            .values()
            .to_vec(),
        vec![1, 1]
    );
}

#[test]
fn floats_follow_ieee_and_checked_division_refuses_zero() {
    let b = sample();
    let q = f64s(&one(call("divide", vec![c("f"), c("g")]), &b));
    assert_eq!(q[0], Some(f64::INFINITY));
    assert!(q[1].expect("nan").is_nan());
    assert!(q[2].expect("nan").is_nan());
    assert_eq!(q[3], None);
    let e = err(call("divide_checked", vec![c("f"), c("g")]), &b);
    assert!(e.contains("row 0: divide_checked: divide by zero"), "{e}");
    // add_checked of doubles does not refuse infinity, as Arrow.
    let big = batch(vec![(
        "x",
        Arc::new(Float64Array::from(vec![1.7e308])) as ArrayRef,
    )]);
    assert_eq!(
        f64s(&one(call("add_checked", vec![c("x"), c("x")]), &big)),
        vec![Some(f64::INFINITY)]
    );
    assert_eq!(
        f64s(&one(
            call("multiply", vec![c("x"), json!({"literal": 2})]),
            &big
        )),
        vec![Some(f64::INFINITY)]
    );
    let p = f64s(&one(
        call(
            "power",
            vec![json!({"literal": 2.0}), json!({"literal": 0.5})],
        ),
        &b,
    ));
    assert_eq!(p[0], Some(2f64.powf(0.5)));
    let f32s = batch(vec![(
        "x",
        Arc::new(Float32Array::from(vec![1.0f32, 2.0])) as ArrayRef,
    )]);
    let r = one(call("divide", vec![c("x"), json!({"literal": 3})]), &f32s);
    assert_eq!(r.data_type(), &DataType::Float32);
    assert_eq!(
        r.as_primitive::<moruna_kernel::arrow::datatypes::Float32Type>()
            .value(0),
        1.0f32 / 3.0
    );
    for f in [
        "add",
        "subtract",
        "multiply",
        "divide_checked",
        "power",
        "atan2",
    ] {
        let r = one(call(f, vec![c("x"), c("x")]), &f32s);
        assert_eq!(r.data_type(), &DataType::Float32, "{f}");
    }
    let r = one(
        call(
            "subtract",
            vec![c("x"), json!({"literal": 1.0, "type": "float"})],
        ),
        &f32s,
    );
    assert_eq!(
        r.as_primitive::<moruna_kernel::arrow::datatypes::Float32Type>()
            .value(1),
        1.0
    );
}

#[test]
fn unary_functions_and_their_checked_forms() {
    let x = batch(vec![
        (
            "x",
            Arc::new(Float64Array::from(vec![
                Some(-1.0),
                Some(0.0),
                Some(4.0),
                None,
            ])) as ArrayRef,
        ),
        (
            "i",
            Arc::new(Int64Array::from(vec![
                Some(i64::MIN),
                Some(-5),
                Some(0),
                Some(3),
            ])),
        ),
        ("u", Arc::new(UInt8Array::from(vec![0u8, 1, 2, 255]))),
        (
            "y",
            Arc::new(Float32Array::from(vec![-1.0f32, 0.0, 4.0, 9.0])),
        ),
        (
            "d",
            Arc::new(DurationMicrosecondArray::from(vec![-5, 5, 0, 1])),
        ),
    ]);
    let sqrt = f64s(&one(call("sqrt", vec![c("x")]), &x));
    assert!(sqrt[0].expect("nan").is_nan());
    assert_eq!(&sqrt[1..], &[Some(0.0), Some(2.0), None]);
    assert!(
        err(call("sqrt_checked", vec![c("x")]), &x)
            .contains("row 0: sqrt_checked: square root of negative number")
    );
    assert!(
        err(call("ln_checked", vec![c("x")]), &x)
            .contains("row 0: ln_checked: logarithm of negative number")
    );
    assert!(
        err(call("log10_checked", vec![call("abs", vec![c("x")])]), &x)
            .contains("row 1: log10_checked: logarithm of zero")
    );
    assert!(
        err(call("log1p_checked", vec![c("x")]), &x)
            .contains("row 0: log1p_checked: logarithm of zero")
    );
    assert!(
        err(call("log1p_checked", vec![json!({"literal": -2.0})]), &x)
            .contains("logarithm of negative number")
    );
    assert!(
        err(call("asin_checked", vec![c("x")]), &x).contains("row 2: asin_checked: domain error")
    );
    assert!(err(call("acos_checked", vec![json!({"literal": 2.0})]), &x).contains("domain error"));
    assert!(
        err(
            call(
                "sin_checked",
                vec![json!({"literal": "Infinity", "type": "double"})]
            ),
            &x
        )
        .contains("domain error")
    );
    assert_eq!(
        f64s(&one(
            call("ln", vec![c("i")]),
            &batch(vec![("i", Arc::new(Int64Array::from(vec![1])) as ArrayRef)])
        )),
        vec![Some(0.0)]
    );
    for (f, v) in [
        ("exp", 4f64.exp()),
        ("ln", 4f64.ln()),
        ("log10", 4f64.log10()),
        ("log2", 2.0),
        ("log1p", 4f64.ln_1p()),
        ("sin", 4f64.sin()),
        ("cos", 4f64.cos()),
        ("tan", 4f64.tan()),
        ("atan", 4f64.atan()),
        ("sqrt_checked", 2.0),
        ("ln_checked", 4f64.ln()),
        ("log2_checked", 2.0),
        ("sin_checked", 4f64.sin()),
        ("cos_checked", 4f64.cos()),
        ("tan_checked", 4f64.tan()),
    ] {
        // A checked form fails on the batch's negative row (above), so it is read on row 2 alone.
        let (input, at) = if f.ends_with("_checked") {
            (x.slice(2, 1), 0)
        } else {
            (x.clone(), 2)
        };
        assert_eq!(
            f64s(&one(call(f, vec![c("x")]), &input))[at],
            Some(v),
            "{f}"
        );
    }
    assert_eq!(
        f64s(&one(call("asin", vec![json!({"literal": 1.0})]), &x))[0],
        Some(1f64.asin())
    );
    assert_eq!(
        f64s(&one(
            call("acos_checked", vec![json!({"literal": 1.0})]),
            &x
        ))[0],
        Some(0.0)
    );
    assert_eq!(
        f64s(&one(
            call("asin_checked", vec![json!({"literal": 0.5})]),
            &x
        ))[0],
        Some(0.5f64.asin())
    );
    assert_eq!(
        f64s(&one(
            call(
                "atan2",
                vec![json!({"literal": 1.0}), json!({"literal": 1})]
            ),
            &x
        ))[0],
        Some(1f64.atan2(1.0))
    );
    let y = one(call("sqrt", vec![c("y")]), &x);
    assert_eq!(y.data_type(), &DataType::Float32);
    assert_eq!(
        y.as_primitive::<moruna_kernel::arrow::datatypes::Float32Type>()
            .value(3),
        3.0
    );
    // negate and abs wrap at MIN; checked refuse; unsigned negate wraps.
    assert_eq!(
        i64s(&one(call("negate", vec![c("i")]), &x)),
        vec![Some(i64::MIN), Some(5), Some(0), Some(-3)]
    );
    assert_eq!(
        i64s(&one(call("abs", vec![c("i")]), &x)),
        vec![Some(i64::MIN), Some(5), Some(0), Some(3)]
    );
    assert!(
        err(call("negate_checked", vec![c("i")]), &x).contains("row 0: negate_checked: overflow")
    );
    assert!(err(call("abs_checked", vec![c("i")]), &x).contains("row 0: abs_checked: overflow"));
    let u = one(call("negate", vec![c("u")]), &x);
    assert_eq!(
        u.as_primitive::<moruna_kernel::arrow::datatypes::UInt8Type>()
            .values()
            .to_vec(),
        vec![0, 255, 254, 1]
    );
    let u = one(call("abs_checked", vec![c("u")]), &x);
    assert_eq!(
        u.as_primitive::<moruna_kernel::arrow::datatypes::UInt8Type>()
            .values()
            .to_vec(),
        vec![0, 1, 2, 255]
    );
    let d = one(call("abs", vec![c("d")]), &x);
    assert_eq!(d.data_type(), &DataType::Duration(TimeUnit::Microsecond));
    let d = one(call("negate_checked", vec![c("d")]), &x);
    assert_eq!(
        d.as_primitive::<moruna_kernel::arrow::datatypes::DurationMicrosecondType>()
            .value(0),
        5
    );
    // sign: an int8 of an integer; the float's own type, with NaN kept and -0.0 as 0.0.
    let s = one(call("sign", vec![c("i")]), &x);
    assert_eq!(
        s.as_primitive::<Int8Type>().values().to_vec(),
        vec![-1, -1, 0, 1]
    );
    let s = one(call("sign", vec![c("u")]), &x);
    assert_eq!(
        s.as_primitive::<Int8Type>().values().to_vec(),
        vec![0, 1, 1, 1]
    );
    let s = f64s(&one(call("sign", vec![call("negate", vec![c("x")])]), &x));
    assert_eq!(bits(s), bits(vec![Some(1.0), Some(0.0), Some(-1.0), None]));
    let nan = batch(vec![(
        "n",
        Arc::new(Float64Array::from(vec![f64::NAN])) as ArrayRef,
    )]);
    assert!(
        f64s(&one(call("sign", vec![c("n")]), &nan))[0]
            .expect("nan")
            .is_nan()
    );
    let s = one(call("sign", vec![c("y")]), &x);
    assert_eq!(
        s.as_primitive::<moruna_kernel::arrow::datatypes::Float32Type>()
            .values()
            .to_vec(),
        vec![-1.0, 0.0, 1.0, 1.0]
    );
}

#[test]
fn rounding_functions() {
    let x = batch(vec![
        (
            "x",
            Arc::new(Float64Array::from(vec![
                Some(7.095),
                Some(-2.5),
                Some(-0.001),
                None,
            ])) as ArrayRef,
        ),
        ("i", Arc::new(Int64Array::from(vec![15, 25, -15, i64::MAX]))),
        (
            "y",
            Arc::new(Float32Array::from(vec![2.675f32, -2.5, 0.125, 1.5])),
        ),
    ]);
    let r = f64s(&one(
        json!({"call": "round", "args": [c("x")], "options": {"ndigits": 2}}),
        &x,
    ));
    assert_eq!(
        bits(r),
        bits(vec![Some(7.09), Some(-2.5), Some(-0.0), None])
    );
    let r = f64s(&one(
        json!({"call": "round", "args": [c("x")], "options": {"round_mode": "half_up"}}),
        &x,
    ));
    assert_eq!(bits(r), bits(vec![Some(7.0), Some(-2.0), Some(-0.0), None]));
    assert_eq!(
        f64s(&one(call("floor", vec![c("x")]), &x)),
        vec![Some(7.0), Some(-3.0), Some(-1.0), None]
    );
    assert_eq!(f64s(&one(call("ceil", vec![c("x")]), &x))[1], Some(-2.0));
    assert_eq!(f64s(&one(call("trunc", vec![c("x")]), &x))[1], Some(-2.0));
    // An integer's floor is its double.
    assert_eq!(
        f64s(&one(call("floor", vec![c("j")]), &sample())),
        vec![Some(2.0), Some(2.0), Some(3.0), Some(1.0)]
    );
    let r = one(
        json!({"call": "round", "args": [c("i")], "options": {"ndigits": -1}}),
        &batch(vec![(
            "i",
            Arc::new(Int64Array::from(vec![15, 25, -15])) as ArrayRef,
        )]),
    );
    assert_eq!(i64s(&r), vec![Some(20), Some(20), Some(-20)]);
    assert_eq!(
        i64s(&one(
            json!({"call": "round", "args": [c("i")], "options": {"ndigits": 2}}),
            &x
        ))[0],
        Some(15)
    );
    let e = err(
        json!({"call": "round", "args": [c("i")], "options": {"ndigits": -1}}),
        &x,
    );
    assert!(e.contains("row 3") && e.contains("would overflow"), "{e}");
    let y = one(
        json!({"call": "round", "args": [c("y")], "options": {"ndigits": 2}}),
        &x,
    );
    assert_eq!(
        y.as_primitive::<moruna_kernel::arrow::datatypes::Float32Type>()
            .values()
            .to_vec(),
        vec![2.67f32, -2.5, 0.12, 1.5]
    );
    let y = one(call("floor", vec![c("y")]), &x);
    assert_eq!(
        y.as_primitive::<moruna_kernel::arrow::datatypes::Float32Type>()
            .value(1),
        -3.0
    );
}

#[test]
fn comparisons_in_ieee_and_in_total_order() {
    let b = batch(vec![
        (
            "x",
            Arc::new(Float64Array::from(vec![
                Some(f64::NAN),
                Some(-0.0),
                Some(1.0),
                None,
                Some(f64::NAN),
            ])) as ArrayRef,
        ),
        (
            "y",
            Arc::new(Float64Array::from(vec![
                Some(f64::NAN),
                Some(0.0),
                Some(f64::NAN),
                Some(1.0),
                Some(5.0),
            ])),
        ),
    ]);
    let run = |f: &str, order: &str| -> Vec<Option<bool>> {
        bools(&one(
            json!({"call": f, "args": [c("x"), c("y")], "options": {"order": order}}),
            &b,
        ))
    };
    let (t, f, n) = (Some(true), Some(false), None);
    // IEEE: NaN compares false (not_equal true); -0.0 equals 0.0.
    assert_eq!(run("equal", "ieee"), vec![f, t, f, n, f]);
    assert_eq!(run("not_equal", "ieee"), vec![t, f, t, n, t]);
    assert_eq!(run("less", "ieee"), vec![f, f, f, n, f]);
    assert_eq!(run("less_equal", "ieee"), vec![f, t, f, n, f]);
    assert_eq!(run("greater", "ieee"), vec![f, f, f, n, f]);
    assert_eq!(run("greater_equal", "ieee"), vec![f, t, f, n, f]);
    // Total order: NaN equals NaN and is above every number; -0.0 is below 0.0.
    assert_eq!(run("equal", "total"), vec![t, f, f, n, f]);
    assert_eq!(run("not_equal", "total"), vec![f, t, t, n, t]);
    assert_eq!(run("less", "total"), vec![f, t, t, n, f]);
    assert_eq!(run("less_equal", "total"), vec![t, t, t, n, f]);
    assert_eq!(run("greater", "total"), vec![f, f, f, n, t]);
    assert_eq!(run("greater_equal", "total"), vec![t, f, f, n, t]);
    let x32 = batch(vec![
        (
            "x",
            Arc::new(Float32Array::from(vec![f32::NAN, -0.0])) as ArrayRef,
        ),
        ("y", Arc::new(Float32Array::from(vec![1.0f32, 0.0]))),
    ]);
    let r = |order: &str| {
        bools(&one(
            json!({"call": "greater", "args": [c("x"), c("y")], "options": {"order": order}}),
            &x32,
        ))
    };
    assert_eq!(r("ieee"), vec![f, f]);
    assert_eq!(r("total"), vec![t, f]);
    let eq32 = bools(&one(
        json!({"call": "equal", "args": [c("x"), c("y")], "options": {"order": "total"}}),
        &x32,
    ));
    assert_eq!(eq32, vec![f, f]);
    // Strings by their bytes; Utf8 with LargeUtf8; booleans false < true; mixed numbers widen.
    let s = batch(vec![
        (
            "a",
            Arc::new(StringArray::from(vec!["a", "B", "é"])) as ArrayRef,
        ),
        ("b", Arc::new(LargeStringArray::from(vec!["b", "a", "z"]))),
        ("p", Arc::new(BooleanArray::from(vec![false, true, true]))),
        ("q", Arc::new(BooleanArray::from(vec![true, true, false]))),
        ("i", Arc::new(Int32Array::from(vec![1, 2, 3]))),
        ("u", Arc::new(UInt16Array::from(vec![1u16, 3, 2]))),
    ]);
    assert_eq!(
        bools(&one(call("less", vec![c("a"), c("b")]), &s)),
        vec![t, t, f]
    );
    assert_eq!(
        bools(&one(
            call("equal", vec![c("a"), json!({"literal": "B"})]),
            &s
        )),
        vec![f, t, f]
    );
    assert_eq!(
        bools(&one(call("less", vec![c("p"), c("q")]), &s)),
        vec![t, f, f]
    );
    assert_eq!(
        bools(&one(call("greater_equal", vec![c("i"), c("u")]), &s)),
        vec![t, f, t]
    );
    assert_eq!(
        bools(&one(
            call("equal", vec![c("i"), json!({"literal": 2.5})]),
            &s
        )),
        vec![f, f, f]
    );
    assert_eq!(
        bools(&one(
            json!({"call": "less", "args": [c("u"), c("i")], "options": {"order": "total"}}),
            &s
        )),
        vec![f, f, t]
    );
}

#[test]
fn boolean_logic_with_nulls() {
    // Every pair of true, false and null.
    let vals = [Some(true), Some(false), None];
    let mut l = Vec::new();
    let mut r = Vec::new();
    for a in vals {
        for b in vals {
            l.push(a);
            r.push(b);
        }
    }
    let bt = batch(vec![
        ("l", Arc::new(BooleanArray::from(l.clone())) as ArrayRef),
        ("r", Arc::new(BooleanArray::from(r.clone()))),
    ]);
    let k = |f: &str| bools(&one(call(f, vec![c("l"), c("r")]), &bt));
    let (t, f, n) = (Some(true), Some(false), None);
    assert_eq!(k("and"), vec![t, f, n, f, f, n, n, n, n]);
    assert_eq!(k("and_kleene"), vec![t, f, n, f, f, f, n, f, n]);
    assert_eq!(k("or"), vec![t, t, n, t, f, n, n, n, n]);
    assert_eq!(k("or_kleene"), vec![t, t, t, t, f, n, t, n, n]);
    assert_eq!(k("xor"), vec![f, t, n, t, f, n, n, n, n]);
    assert_eq!(k("and_not"), vec![f, t, n, f, f, n, n, n, n]);
    assert_eq!(k("and_not_kleene"), vec![f, t, n, f, f, f, f, n, n]);
    assert_eq!(
        bools(&one(call("invert", vec![c("l")]), &bt)),
        vec![f, f, f, t, t, t, n, n, n]
    );
    // With no nulls, the Kleene forms take their fast path.
    let full = batch(vec![
        (
            "l",
            Arc::new(BooleanArray::from(vec![true, true, false, false])) as ArrayRef,
        ),
        (
            "r",
            Arc::new(BooleanArray::from(vec![true, false, true, false])),
        ),
    ]);
    for (fname, want) in [
        ("and_kleene", vec![t, f, f, f]),
        ("or_kleene", vec![t, t, t, f]),
        ("and_not_kleene", vec![f, t, f, f]),
    ] {
        assert_eq!(
            bools(&one(call(fname, vec![c("l"), c("r")]), &full)),
            want,
            "{fname}"
        );
    }
}

#[test]
fn null_tests_and_selection() {
    let b = sample();
    let (t, f, n) = (Some(true), Some(false), None);
    assert_eq!(
        bools(&one(call("is_null", vec![c("f")]), &b)),
        vec![f, f, f, t]
    );
    assert_eq!(
        bools(&one(
            json!({"call": "is_null", "args": [c("f")], "options": {"nan_is_null": true}}),
            &b
        )),
        vec![f, t, f, t]
    );
    assert_eq!(
        bools(&one(call("is_valid", vec![c("s")]), &b)),
        vec![t, t, f, t]
    );
    assert_eq!(
        bools(&one(call("is_nan", vec![c("f")]), &b)),
        vec![f, t, f, n]
    );
    assert_eq!(
        bools(&one(call("is_finite", vec![c("f")]), &b)),
        vec![t, f, t, n]
    );
    assert_eq!(
        bools(&one(
            call("is_inf", vec![call("divide", vec![c("f"), c("g")])]),
            &b
        )),
        vec![t, f, f, n]
    );
    assert_eq!(
        bools(&one(call("is_nan", vec![c("i")]), &b)),
        vec![f, f, n, f]
    );
    assert_eq!(
        bools(&one(call("is_finite", vec![c("i")]), &b)),
        vec![t, t, n, t]
    );
    assert_eq!(
        bools(&one(call("is_inf", vec![c("i")]), &b)),
        vec![f, f, n, f]
    );
    // if_else: a null condition is null.
    assert_eq!(
        i64s(&one(call("if_else", vec![c("b"), c("i"), c("j")]), &b)),
        vec![Some(1), Some(2), None, Some(i64::MAX)]
    );
    // An `if` (case_when): a null condition counts as false.
    let w = one(
        json!({"if": [{"when": c("b"), "then": c("j")}], "else": {"literal": 0}}),
        &b,
    );
    assert_eq!(i64s(&w), vec![Some(2), Some(0), Some(0), Some(1)]);
    let w = one(
        json!({"if": [{"when": c("b"), "then": c("j")}, {"when": {"call": "is_null", "args": [c("b")]}, "then": {"literal": 9}}]}),
        &b,
    );
    assert_eq!(i64s(&w), vec![Some(2), None, Some(9), Some(1)]);
    // A switch: equality with each literal; a null argument matches none.
    let s = one(
        json!({"switch": c("s"), "cases": [
        {"match": {"literal": "a"}, "then": {"literal": "first"}},
        {"match": {"literal": "é"}, "then": c("s")}], "else": {"literal": "other"}}),
        &b,
    );
    let s: Vec<Option<&str>> = s.as_string::<i32>().iter().collect();
    assert_eq!(
        s,
        vec![Some("first"), Some("other"), Some("other"), Some("é")]
    );
    // coalesce and fill_null; the common type of int and double is double.
    let co = f64s(&one(call("coalesce", vec![c("f"), c("j")]), &b));
    assert_eq!(co[0], Some(1.5));
    assert!(co[1].expect("nan").is_nan());
    assert_eq!(bits(co[2..].to_vec()), bits(vec![Some(-0.0), Some(1.0)]));
    // int64 past 2^53 does not become a double to be selected.
    assert!(err(call("coalesce", vec![c("i"), c("f")]), &b).contains("row 3"));
    let filled = one(call("fill_null", vec![c("s"), json!({"literal": "?"})]), &b);
    let filled: Vec<Option<&str>> = filled.as_string::<i32>().iter().collect();
    assert_eq!(filled, vec![Some("a"), Some("b"), Some("?"), Some("é")]);
    let large = one(
        call(
            "coalesce",
            vec![c("s"), json!({"literal": "x", "type": "large_string"})],
        ),
        &b,
    );
    assert_eq!(large.data_type(), &DataType::LargeUtf8);
    assert_eq!(large.as_string::<i64>().value(2), "x");
    let bo = one(
        call("coalesce", vec![c("b"), json!({"literal": false})]),
        &b,
    );
    assert_eq!(bools(&bo), vec![t, f, f, t]);
    let dt = one(
        call(
            "coalesce",
            vec![c("t"), json!({"literal": 5, "type": "timestamp[s]"})],
        ),
        &b,
    );
    assert_eq!(
        dt.data_type(),
        &DataType::Timestamp(TimeUnit::Microsecond, None)
    );
    assert_eq!(
        dt.as_primitive::<moruna_kernel::arrow::datatypes::TimestampMicrosecondType>()
            .value(3),
        5_000_000
    );
    let uu = one(
        call(
            "coalesce",
            vec![
                json!({"literal": null, "type": "uint32"}),
                json!({"literal": 7, "type": "uint32"}),
            ],
        ),
        &b,
    );
    assert_eq!(
        uu.as_primitive::<moruna_kernel::arrow::datatypes::UInt32Type>()
            .values()
            .to_vec(),
        vec![7; 4]
    );
    let ff = one(
        call(
            "coalesce",
            vec![
                json!({"literal": null, "type": "float"}),
                json!({"literal": "NaN", "type": "float"}),
            ],
        ),
        &b,
    );
    assert!(
        ff.as_primitive::<moruna_kernel::arrow::datatypes::Float32Type>()
            .value(0)
            .is_nan()
    );
    let none = one(
        call("coalesce", vec![json!({"literal": null, "type": "date32"})]),
        &b,
    );
    assert_eq!(none.null_count(), 4);
}

#[test]
fn element_wise_extremes_ignore_nan_and_put_zero_above_minus_zero() {
    let b = batch(vec![
        (
            "x",
            Arc::new(Float64Array::from(vec![
                Some(f64::NAN),
                Some(1.0),
                Some(-0.0),
                Some(0.0),
                None,
                Some(f64::NAN),
            ])) as ArrayRef,
        ),
        (
            "y",
            Arc::new(Float64Array::from(vec![
                Some(0.0),
                Some(f64::NAN),
                Some(0.0),
                Some(-0.0),
                Some(2.0),
                Some(f64::NAN),
            ])),
        ),
        (
            "i",
            Arc::new(Int64Array::from(vec![
                Some(1),
                Some(5),
                None,
                Some(3),
                Some(2),
                Some(0),
            ])),
        ),
    ]);
    let max = f64s(&one(call("max_element_wise", vec![c("x"), c("y")]), &b));
    assert_eq!(
        bits(max[..5].to_vec()),
        bits(vec![Some(0.0), Some(1.0), Some(0.0), Some(0.0), Some(2.0)])
    );
    assert!(max[5].expect("nan").is_nan());
    let min = f64s(&one(call("min_element_wise", vec![c("x"), c("y")]), &b));
    assert_eq!(
        bits(min[..5].to_vec()),
        bits(vec![
            Some(0.0),
            Some(1.0),
            Some(-0.0),
            Some(-0.0),
            Some(2.0)
        ])
    );
    let strict = f64s(&one(
        json!({"call": "max_element_wise", "args": [c("x"), c("y")], "options": {"skip_nulls": false}}),
        &b,
    ));
    assert_eq!(strict[4], None);
    // An int with a double compares as doubles.
    assert_eq!(
        f64s(&one(
            call("max_element_wise", vec![c("i"), json!({"literal": 2.5})]),
            &b
        )),
        vec![
            Some(2.5),
            Some(5.0),
            Some(2.5),
            Some(3.0),
            Some(2.5),
            Some(2.5)
        ]
    );
    assert_eq!(
        i64s(&one(
            call("min_element_wise", vec![c("i"), json!({"literal": 2})]),
            &b
        )),
        vec![Some(1), Some(2), Some(2), Some(2), Some(2), Some(0)]
    );
    let u = batch(vec![
        ("u", Arc::new(UInt8Array::from(vec![1u8, 9])) as ArrayRef),
        ("v", Arc::new(UInt8Array::from(vec![3u8, 2]))),
    ]);
    let m = one(call("max_element_wise", vec![c("u"), c("v")]), &u);
    assert_eq!(
        m.as_primitive::<moruna_kernel::arrow::datatypes::UInt8Type>()
            .values()
            .to_vec(),
        vec![3, 9]
    );
    let y = batch(vec![(
        "y",
        Arc::new(Float32Array::from(vec![f32::NAN, -0.0])) as ArrayRef,
    )]);
    let m = one(
        call(
            "max_element_wise",
            vec![c("y"), json!({"literal": 0.0, "type": "float"})],
        ),
        &y,
    );
    assert_eq!(
        m.as_primitive::<moruna_kernel::arrow::datatypes::Float32Type>()
            .values()
            .iter()
            .map(|v| v.to_bits())
            .collect::<Vec<_>>(),
        vec![0.0f32.to_bits(), 0.0f32.to_bits()]
    );
}

#[test]
fn temporal_parts_of_timestamps_and_dates() {
    let b = sample();
    let parts: Vec<Vec<Option<i64>>> = temporal::NAMES
        .iter()
        .map(|f| i64s(&one(call(f, vec![c("t")]), &b)))
        .collect();
    let row = |r: usize| parts.iter().map(|p| p[r]).collect::<Vec<_>>();
    let want = |v: [i64; 14]| v.into_iter().map(Some).collect::<Vec<_>>();
    assert_eq!(
        row(0),
        want([1969, 4, 12, 31, 2, 365, 1970, 1, 23, 59, 59, 999, 999, 0])
    );
    assert_eq!(
        row(2),
        want([2024, 1, 1, 1, 0, 1, 2024, 1, 17, 0, 0, 123, 456, 0])
    );
    assert_eq!(row(3), vec![None; 14]);
    let dow = i64s(&one(
        json!({"call": "day_of_week", "args": [c("t")], "options": {"count_from_zero": false, "week_start": 7}}),
        &b,
    ));
    assert_eq!(dow[..2], [Some(4), Some(5)]);
    let zoned = batch(vec![
        (
            "z",
            Arc::new(
                TimestampMicrosecondArray::from(vec![Some(0), None])
                    .with_timezone("America/New_York"),
            ) as ArrayRef,
        ),
        (
            "o",
            Arc::new(TimestampSecondArray::from(vec![Some(0), Some(-1)]).with_timezone("+05:30")),
        ),
        (
            "d",
            Arc::new(Date32Array::from(vec![Some(-1), Some(19_723)])),
        ),
        (
            "n",
            Arc::new(TimestampNanosecondArray::from(vec![-1, 1_234_567_891])),
        ),
    ]);
    assert_eq!(
        i64s(&one(call("hour", vec![c("z")]), &zoned)),
        vec![Some(19), None]
    );
    assert_eq!(
        i64s(&one(call("day", vec![c("z")]), &zoned)),
        vec![Some(31), None]
    );
    assert_eq!(
        i64s(&one(call("minute", vec![c("o")]), &zoned)),
        vec![Some(30), Some(29)]
    );
    assert_eq!(
        i64s(&one(call("year", vec![c("d")]), &zoned)),
        vec![Some(1969), Some(2024)]
    );
    assert_eq!(
        i64s(&one(call("iso_week", vec![c("d")]), &zoned)),
        vec![Some(1), Some(1)]
    );
    assert_eq!(
        i64s(&one(call("nanosecond", vec![c("n")]), &zoned)),
        vec![Some(999), Some(891)]
    );
    assert!(err(call("hour", vec![c("d")]), &zoned).contains("no kernel"));
    let far = batch(vec![(
        "z",
        Arc::new(TimestampSecondArray::from(vec![i64::MAX]).with_timezone("America/New_York"))
            as ArrayRef,
    )]);
    assert!(err(call("hour", vec![c("z")]), &far).contains("out of the calendar's range"));
}

#[test]
fn temporal_arithmetic() {
    let b = batch(vec![
        (
            "a",
            Arc::new(TimestampMicrosecondArray::from(vec![1, 5_000_000])) as ArrayRef,
        ),
        ("s", Arc::new(TimestampSecondArray::from(vec![5, 1]))),
        ("d", Arc::new(DurationMicrosecondArray::from(vec![5, -5]))),
        ("x", Arc::new(Date32Array::from(vec![5, 3]))),
        ("y", Arc::new(Date32Array::from(vec![3, 5]))),
        (
            "m",
            Arc::new(TimestampMicrosecondArray::from(vec![i64::MAX, 0])),
        ),
    ]);
    let diff = one(call("subtract", vec![c("a"), c("s")]), &b);
    assert_eq!(diff.data_type(), &DataType::Duration(TimeUnit::Microsecond));
    assert_eq!(
        i64s(&one(
            json!({"cast": call("subtract", vec![c("a"), c("s")]), "to": "int64"}),
            &b
        )),
        vec![Some(-4_999_999), Some(4_000_000)]
    );
    let plus = one(call("add", vec![c("s"), c("d")]), &b);
    assert_eq!(
        plus.data_type(),
        &DataType::Timestamp(TimeUnit::Microsecond, None)
    );
    let plus2 = one(call("add", vec![c("d"), c("s")]), &b);
    assert_eq!(plus.to_data(), plus2.to_data());
    assert_eq!(
        one(call("subtract", vec![c("a"), c("d")]), &b).data_type(),
        &DataType::Timestamp(TimeUnit::Microsecond, None)
    );
    assert_eq!(
        one(call("add", vec![c("d"), c("d")]), &b).data_type(),
        &DataType::Duration(TimeUnit::Microsecond)
    );
    let days = one(call("subtract", vec![c("x"), c("y")]), &b);
    assert_eq!(days.data_type(), &DataType::Duration(TimeUnit::Second));
    assert_eq!(
        i64s(&one(
            json!({"cast": call("subtract", vec![c("x"), c("y")]), "to": "int64"}),
            &b
        )),
        vec![Some(172_800), Some(-172_800)]
    );
    let times = one(call("multiply", vec![c("d"), json!({"literal": 3})]), &b);
    assert_eq!(
        times.data_type(),
        &DataType::Duration(TimeUnit::Microsecond)
    );
    let times = one(
        call(
            "multiply",
            vec![json!({"literal": 3, "type": "int8"}), c("d")],
        ),
        &b,
    );
    assert_eq!(
        times
            .as_primitive::<moruna_kernel::arrow::datatypes::DurationMicrosecondType>()
            .values()
            .to_vec(),
        vec![15, -15]
    );
    assert!(
        err(call("add_checked", vec![c("m"), c("d")]), &b).contains("row 0: add_checked: overflow")
    );
    assert!(err(call("add", vec![c("a"), c("a")]), &b).contains("no kernel"));
    assert!(err(call("multiply", vec![c("d"), c("d")]), &b).contains("no kernel"));
    assert_eq!(
        bools(&one(call("less", vec![c("s"), c("a")]), &b)),
        vec![Some(false), Some(true)]
    );
    assert_eq!(
        bools(&one(call("less", vec![c("x"), c("y")]), &b)),
        vec![Some(false), Some(true)]
    );
    assert_eq!(
        bools(&one(
            call(
                "less",
                vec![c("d"), json!({"literal": 0, "type": "duration[s]"})]
            ),
            &b
        )),
        vec![Some(false), Some(true)]
    );
    let zz = batch(vec![
        (
            "p",
            Arc::new(TimestampMicrosecondArray::from(vec![5]).with_timezone("UTC")) as ArrayRef,
        ),
        (
            "q",
            Arc::new(TimestampMicrosecondArray::from(vec![5]).with_timezone("America/New_York")),
        ),
    ]);
    assert_eq!(
        bools(&one(call("equal", vec![c("p"), c("q")]), &zz)),
        vec![Some(true)]
    );
    assert!(err(call("coalesce", vec![c("p"), c("q")]), &zz).contains("no common type"));
}

#[test]
fn casts_between_every_lane() {
    let b = batch(vec![
        (
            "f",
            Arc::new(Float64Array::from(vec![
                Some(2.5),
                Some(-2.5),
                Some(f64::NAN),
                Some(1e30),
                Some(-1.0),
                None,
            ])) as ArrayRef,
        ),
        (
            "i",
            Arc::new(Int64Array::from(vec![
                Some(300),
                Some(-1),
                Some(0),
                Some(1 << 60),
                Some(5),
                None,
            ])),
        ),
        (
            "u",
            Arc::new(UInt64Array::from(vec![
                Some(u64::MAX),
                Some(1),
                Some(0),
                Some(2),
                Some(5),
                None,
            ])),
        ),
        (
            "b",
            Arc::new(BooleanArray::from(vec![
                Some(true),
                Some(false),
                Some(true),
                Some(false),
                Some(true),
                None,
            ])),
        ),
        (
            "t",
            Arc::new(TimestampMicrosecondArray::from(vec![
                Some(1_500_000),
                Some(-1_500_000),
                Some(0),
                Some(-1),
                Some(86_400_000_000),
                None,
            ])),
        ),
        (
            "g",
            Arc::new(Float32Array::from(vec![
                Some(0.1f32),
                Some(1.0),
                Some(2.0),
                Some(3.0),
                Some(4.0),
                None,
            ])),
        ),
    ]);
    let cast = |what: &str, to: &str, safe: bool| -> moruna_kernel::Result<ArrayRef> {
        try_one(json!({"cast": c(what), "to": to, "safe": safe}), &b)
    };
    let e = cast("f", "int64", true).expect_err("truncates").to_string();
    assert!(
        e.contains("row 0: cast: float value 2.5 was truncated converting to Int64"),
        "{e}"
    );
    let u = cast("f", "int64", false).expect("unsafe");
    assert_eq!(
        i64s(&u),
        vec![Some(2), Some(-2), Some(0), Some(i64::MAX), Some(-1), None]
    );
    let u8s = cast("f", "uint8", false).expect("unsafe");
    assert_eq!(
        u8s.as_primitive::<moruna_kernel::arrow::datatypes::UInt8Type>()
            .values()[..5]
            .to_vec(),
        vec![2, 0, 0, 255, 0]
    );
    let e = cast("i", "int8", true).expect_err("range").to_string();
    assert!(
        e.contains("row 0: cast: integer value 300 not in range: -128 to 127"),
        "{e}"
    );
    let w = cast("i", "int8", false).expect("wraps");
    assert_eq!(
        w.as_primitive::<Int8Type>().values()[..3].to_vec(),
        vec![44, -1, 0]
    );
    assert!(
        cast("i", "double", true)
            .expect_err("2^60")
            .to_string()
            .contains("row 3")
    );
    assert_eq!(
        f64s(&cast("i", "double", false).expect("as"))[3],
        Some((1u64 << 60) as f64)
    );
    assert!(
        cast("i", "float", true)
            .expect_err("2^24")
            .to_string()
            .contains("row 3")
    );
    assert!(
        cast("u", "int64", true)
            .expect_err("u64 max")
            .to_string()
            .contains("row 0")
    );
    assert_eq!(
        i64s(&cast("u", "int64", false).expect("wraps"))[0],
        Some(-1)
    );
    assert!(
        cast("i", "uint32", true)
            .expect_err("negative")
            .to_string()
            .contains("row 1")
    );
    assert_eq!(
        cast("u", "uint16", false)
            .expect("wraps")
            .as_primitive::<moruna_kernel::arrow::datatypes::UInt16Type>()
            .value(0),
        u16::MAX
    );
    assert_eq!(
        f64s(&cast("u", "double", false).expect("as"))[0],
        Some(u64::MAX as f64)
    );
    assert_eq!(
        f64s(&cast("b", "double", true).expect("bool"))[..2],
        [Some(1.0), Some(0.0)]
    );
    assert_eq!(
        i64s(&cast("b", "int64", true).expect("bool"))[..2],
        [Some(1), Some(0)]
    );
    assert_eq!(
        bools(&cast("f", "bool", true).expect("nonzero"))[..3],
        [Some(true), Some(true), Some(true)]
    );
    assert_eq!(
        bools(&cast("i", "bool", true).expect("nonzero"))[..3],
        [Some(true), Some(true), Some(false)]
    );
    let g = cast("g", "double", true).expect("widen");
    assert_eq!(f64s(&g)[0], Some(f64::from(0.1f32)));
    let narrowed = cast("f", "float", true).expect("narrow");
    assert_eq!(
        narrowed
            .as_primitive::<moruna_kernel::arrow::datatypes::Float32Type>()
            .value(3),
        f32::INFINITY.min(1e30f64 as f32)
    );
    assert!(
        cast("g", "int32", true)
            .expect_err("0.1")
            .to_string()
            .contains("row 0")
    );
    // Timestamps: units, ints, dates.
    let e = cast("t", "timestamp[s]", true)
        .expect_err("loses")
        .to_string();
    assert!(e.contains("would lose data: 1500000"), "{e}");
    let s = cast("t", "timestamp[s]", false).expect("truncates");
    assert_eq!(
        s.as_primitive::<moruna_kernel::arrow::datatypes::TimestampSecondType>()
            .values()[..2]
            .to_vec(),
        vec![1, -1]
    );
    let ns = cast("t", "timestamp[ns]", true).expect("finer");
    assert_eq!(
        ns.as_primitive::<moruna_kernel::arrow::datatypes::TimestampNanosecondType>()
            .value(0),
        1_500_000_000
    );
    let big = batch(vec![(
        "s",
        Arc::new(TimestampSecondArray::from(vec![1i64 << 62])) as ArrayRef,
    )]);
    let e = try_one(json!({"cast": c("s"), "to": "timestamp[us]"}), &big)
        .expect_err("overflow")
        .to_string();
    assert!(e.contains("out of bounds timestamp"), "{e}");
    let w = try_one(
        json!({"cast": c("s"), "to": "timestamp[us]", "safe": false}),
        &big,
    )
    .expect("wraps");
    assert_eq!(
        w.as_primitive::<moruna_kernel::arrow::datatypes::TimestampMicrosecondType>()
            .value(0),
        0
    );
    let d = cast("t", "date32", true).expect("date");
    assert_eq!(
        d.as_primitive::<moruna_kernel::arrow::datatypes::Date32Type>()
            .values()[..5]
            .to_vec(),
        vec![0, -1, 0, -1, 1]
    );
    assert_eq!(
        i64s(&cast("t", "int64", true).expect("relabel"))[3],
        Some(-1)
    );
    let back = try_one(
        json!({"cast": {"cast": c("t"), "to": "date32"}, "to": "timestamp[ms]"}),
        &b,
    )
    .expect("midnight");
    assert_eq!(
        back.as_primitive::<moruna_kernel::arrow::datatypes::TimestampMillisecondType>()
            .value(4),
        86_400_000
    );
    let z = try_one(json!({"cast": c("t"), "to": "timestamp[us, tz=UTC]"}), &b).expect("zone");
    assert_eq!(
        z.data_type(),
        &DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
    );
    let ny = batch(vec![(
        "z",
        Arc::new(
            TimestampMicrosecondArray::from(vec![0, 23 * 3_600_000_000])
                .with_timezone("America/New_York"),
        ) as ArrayRef,
    )]);
    let local = try_one(json!({"cast": c("z"), "to": "date32"}), &ny).expect("local date");
    assert_eq!(
        local
            .as_primitive::<moruna_kernel::arrow::datatypes::Date32Type>()
            .values()
            .to_vec(),
        vec![-1, 0]
    );
    let i32d = try_one(
        json!({"cast": {"literal": 5, "type": "int32"}, "to": "date32"}),
        &b,
    )
    .expect("int32 date");
    assert_eq!(i32d.data_type(), &DataType::Date32);
    let dur =
        try_one(json!({"cast": {"literal": 5}, "to": "duration[s]"}), &b).expect("int duration");
    let dur_us = try_one(
        json!({"cast": {"cast": {"literal": 5}, "to": "duration[s]"}, "to": "duration[us]"}),
        &b,
    )
    .expect("units");
    assert_eq!(dur.data_type(), &DataType::Duration(TimeUnit::Second));
    assert_eq!(
        dur_us
            .as_primitive::<moruna_kernel::arrow::datatypes::DurationMicrosecondType>()
            .value(0),
        5_000_000
    );
    let large = batch(vec![(
        "s",
        Arc::new(StringArray::from(vec![Some("a"), None])) as ArrayRef,
    )]);
    let l = try_one(json!({"cast": c("s"), "to": "large_string"}), &large).expect("large");
    assert_eq!(
        l.as_string::<i64>().iter().collect::<Vec<_>>(),
        vec![Some("a"), None]
    );
    assert!(
        try_one(
            json!({"cast": c("t"), "to": "date32"}),
            &batch(vec![(
                "t",
                Arc::new(TimestampSecondArray::from(vec![i64::MAX])) as ArrayRef
            )])
        )
        .is_err()
    );
    let w = try_one(
        json!({"cast": c("t"), "to": "date32", "safe": false}),
        &batch(vec![(
            "t",
            Arc::new(TimestampSecondArray::from(vec![i64::MAX])) as ArrayRef,
        )]),
    )
    .expect("wraps");
    assert_eq!(w.len(), 1);
    let narrow = batch(vec![(
        "x",
        Arc::new(Int16Array::from(vec![300i16])) as ArrayRef,
    )]);
    assert!(try_one(json!({"cast": c("x"), "to": "uint8"}), &narrow).is_err());
    assert_eq!(
        try_one(json!({"cast": c("x"), "to": "int32"}), &narrow)
            .expect("widen")
            .data_type(),
        &DataType::Int32
    );
    let u32s = batch(vec![(
        "x",
        Arc::new(UInt32Array::from(vec![7u32])) as ArrayRef,
    )]);
    assert_eq!(
        f64s(&try_one(json!({"cast": c("x"), "to": "double"}), &u32s).expect("u32"))[0],
        Some(7.0)
    );
}

#[test]
fn chunks_constants_and_shared_subtrees() {
    // Several chunks, with nulls, so registers are reused and constants kept across chunks.
    let n = super::eval::CHUNK * 3 + 17;
    let x: Vec<Option<i64>> = (0..n as i64)
        .map(|v| if v % 7 == 0 { None } else { Some(v) })
        .collect();
    let y: Vec<Option<f64>> = (0..n)
        .map(|v| {
            if v % 5 == 0 {
                None
            } else {
                Some(v as f64 / 4.0)
            }
        })
        .collect();
    let s: Vec<Option<String>> = (0..n)
        .map(|v| {
            if v % 3 == 0 {
                None
            } else {
                Some(format!("s{v}"))
            }
        })
        .collect();
    let b = batch(vec![
        ("x", Arc::new(Int64Array::from(x.clone())) as ArrayRef),
        ("y", Arc::new(Float64Array::from(y.clone()))),
        ("s", Arc::new(StringArray::from(s.clone()))),
    ]);
    let sum = call(
        "add",
        vec![
            call("multiply", vec![c("x"), json!({"literal": 2})]),
            json!({"literal": 1}),
        ],
    );
    let args = json!({"outputs": [
        {"name": "a", "expr": sum},
        {"name": "b", "expr": call("coalesce", vec![c("y"), call("add", vec![json!({"literal": 1.0}), json!({"literal": 2.0})])])},
        {"name": "c", "expr": call("greater", vec![sum, json!({"literal": 100})])},
        {"name": "d", "expr": call("fill_null", vec![c("s"), json!({"literal": "-"})])},
        {"name": "e", "expr": json!({"literal": 3})},
        {"name": "x", "expr": c("x")},
        {"name": "n", "expr": json!({"literal": null, "type": "bool"})},
    ]});
    let tree = Tree::from_json(&args).expect("tree");
    let program = tree.bind(&b.schema()).expect("binds");
    // 23 nodes; `sum` and the column `x` are computed once however often they are named.
    assert_eq!(program.instructions(), 17);
    assert!(program.registers() <= program.instructions());
    assert_eq!(program.input().fields().len(), 3);
    let out = program.eval(&b).expect("evals");
    assert_eq!(out.num_rows(), n);
    let want_a: Vec<Option<i64>> = x.iter().map(|v| v.map(|v| v * 2 + 1)).collect();
    assert_eq!(i64s(out.column(0)), want_a);
    let want_b: Vec<Option<f64>> = y.iter().map(|v| Some(v.unwrap_or(3.0))).collect();
    assert_eq!(f64s(out.column(1)), want_b);
    let want_c: Vec<Option<bool>> = x.iter().map(|v| v.map(|v| v * 2 + 1 > 100)).collect();
    assert_eq!(bools(out.column(2)), want_c);
    let d: Vec<Option<&str>> = out.column(3).as_string::<i32>().iter().collect();
    let want_d: Vec<Option<&str>> = s
        .iter()
        .map(|v| Some(v.as_deref().unwrap_or("-")))
        .collect();
    assert_eq!(d, want_d);
    assert_eq!(i64s(out.column(4)), vec![Some(3); n]);
    // A column passed through is the input's array, not a copy.
    assert!(Arc::ptr_eq(out.column(5), b.column(0)));
    assert_eq!(out.schema().field(5).name(), "x");
    assert_eq!(out.column(6).null_count(), n);
    // An empty batch gives empty outputs of the right types.
    let empty = b.slice(0, 0);
    let out = program.eval(&empty).expect("empty");
    assert_eq!(out.num_rows(), 0);
    assert_eq!(out.column(1).data_type(), &DataType::Float64);
    // A sliced batch reads from its offset.
    let sliced = b.slice(super::eval::CHUNK - 3, 10);
    let out = program.eval(&sliced).expect("sliced");
    assert_eq!(
        i64s(out.column(0)),
        want_a[super::eval::CHUNK - 3..super::eval::CHUNK + 7].to_vec()
    );
    // A batch of another schema is refused, not misread.
    let other = batch(vec![(
        "x",
        Arc::new(Float64Array::from(vec![1.0])) as ArrayRef,
    )]);
    assert!(
        program
            .eval(&other)
            .expect_err("schema")
            .to_string()
            .contains("checked against")
    );
    let compiled = Compiled::new(tree);
    assert_eq!(compiled.eval(&b).expect("cached").num_rows(), n);
    assert_eq!(compiled.eval(&b).expect("again").num_columns(), 7);
    assert!(compiled.eval(&other.clone()).is_err());
}

#[test]
fn opaque_columns_are_compared_through_arrows_kernels() {
    let dec = Decimal128Array::from(vec![Some(125), None, Some(-5)])
        .with_precision_and_scale(10, 2)
        .expect("dec");
    let b = batch(vec![
        ("d", Arc::new(dec.clone()) as ArrayRef),
        ("e", Arc::new(dec)),
        (
            "h",
            moruna_kernel::arrow::compute::kernels::cast::cast(
                &Float32Array::from(vec![1.0f32, f32::NAN, -1.0]),
                &DataType::Float16,
            )
            .expect("float16"),
        ),
    ]);
    let lit = json!({"literal": "1.25", "type": "decimal128(10, 2)"});
    assert_eq!(
        bools(&one(call("equal", vec![c("d"), lit.clone()]), &b)),
        vec![Some(true), None, Some(false)]
    );
    assert_eq!(
        bools(&one(call("less", vec![c("d"), c("e")]), &b)),
        vec![Some(false), None, Some(false)]
    );
    assert_eq!(
        bools(&one(call("greater_equal", vec![lit.clone(), c("d")]), &b)),
        vec![Some(true), None, Some(true)]
    );
    for f in ["not_equal", "less_equal", "greater"] {
        assert_eq!(one(call(f, vec![c("d"), lit.clone()]), &b).len(), 3, "{f}");
    }
    assert_eq!(
        bools(&one(call("equal", vec![lit.clone(), lit.clone()]), &b)),
        vec![Some(true); 3]
    );
    assert_eq!(
        bools(&one(call("is_valid", vec![c("d")]), &b)),
        vec![Some(true), Some(false), Some(true)]
    );
    let null_dec = json!({"literal": null, "type": "decimal128(10, 2)"});
    assert_eq!(
        bools(&one(call("is_null", vec![null_dec.clone()]), &b)),
        vec![Some(true); 3]
    );
    assert_eq!(one(null_dec, &b).null_count(), 3);
    assert_eq!(one(lit, &b).len(), 3);
    assert!(err(call("less", vec![c("h"), c("h")]), &b).contains("total order"));
    let total = one(
        json!({"call": "greater", "args": [c("h"), {"literal": 0.5, "type": "halffloat"}], "options": {"order": "total"}}),
        &b,
    );
    assert_eq!(bools(&total), vec![Some(true), Some(true), Some(false)]);
    assert!(err(call("add", vec![c("d"), c("d")]), &b).contains("no kernel"));
    let tree = Tree::from_json(
        &json!({"outputs": [{"name": "x", "expr": {"cast": c("d"), "to": "double"}}]}),
    )
    .expect("tree");
    assert!(
        tree.bind(&b.schema())
            .expect_err("no cast")
            .to_string()
            .contains("no cast")
    );
}

/// The old evaluator of the filter language, as it was: arrow-rs comparisons and Kleene logic.
fn old_eval(e: &crate::expr::Expr, b: &RecordBatch) -> BooleanArray {
    use crate::expr::{Expr, Op, Operand};
    use moruna_kernel::arrow::array::Datum;
    use moruna_kernel::arrow::compute::kernels::{boolean, cmp};
    let datum = |this: &Operand, other: &Operand| -> Box<dyn Datum> {
        match this {
            Operand::Column(c) => Box::new(Arc::clone(b.column_by_name(c).expect("column"))),
            Operand::Literal(lit) => {
                let Operand::Column(c) = other else {
                    panic!("two literals")
                };
                let ty = b.column_by_name(c).expect("column").data_type().clone();
                Box::new(lit.scalar_of(&ty).expect("scalar"))
            }
        }
    };
    match e {
        Expr::Cmp(l, op, r) => {
            let (x, y) = (datum(l, r), datum(r, l));
            let (x, y): (&dyn Datum, &dyn Datum) = (x.as_ref(), y.as_ref());
            match op {
                Op::Eq => cmp::eq(x, y),
                Op::Ne => cmp::neq(x, y),
                Op::Lt => cmp::lt(x, y),
                Op::Le => cmp::lt_eq(x, y),
                Op::Gt => cmp::gt(x, y),
                Op::Ge => cmp::gt_eq(x, y),
            }
            .expect("cmp")
        }
        Expr::IsNull(c) => {
            boolean::is_null(b.column_by_name(c).expect("c").as_ref()).expect("null")
        }
        Expr::IsNotNull(c) => {
            boolean::is_not_null(b.column_by_name(c).expect("c").as_ref()).expect("nn")
        }
        Expr::And(x, y) => boolean::and_kleene(&old_eval(x, b), &old_eval(y, b)).expect("and"),
        Expr::Or(x, y) => boolean::or_kleene(&old_eval(x, b), &old_eval(y, b)).expect("or"),
        Expr::Not(x) => boolean::not(&old_eval(x, b)).expect("not"),
    }
}

#[test]
fn the_filter_language_keeps_its_rows_through_the_translation() {
    let specials = [
        f64::NAN,
        -0.0,
        0.0,
        1.0,
        -1.0,
        f64::INFINITY,
        f64::NEG_INFINITY,
        2.5,
    ];
    let n = 300;
    let f: Vec<Option<f64>> = (0..n)
        .map(|i| {
            if i % 11 == 0 {
                None
            } else {
                Some(specials[(i * 7) % specials.len()])
            }
        })
        .collect();
    let g: Vec<Option<f64>> = (0..n)
        .map(|i| Some(specials[(i * 3 + 1) % specials.len()]))
        .collect();
    let k: Vec<Option<i32>> = (0..n)
        .map(|i| {
            if i % 13 == 0 {
                None
            } else {
                Some(i as i32 % 9 - 4)
            }
        })
        .collect();
    let s: Vec<Option<String>> = (0..n)
        .map(|i| {
            if i % 5 == 0 {
                None
            } else {
                Some(["a", "b", "x"][i % 3].to_string())
            }
        })
        .collect();
    let t: Vec<Option<i64>> = (0..n)
        .map(|i| Some(i as i64 * 1_000_000 - 50_000_000))
        .collect();
    let dec = Decimal128Array::from((0..n).map(|i| Some(i as i128 - 100)).collect::<Vec<_>>())
        .with_precision_and_scale(10, 2)
        .expect("dec");
    let b = batch(vec![
        ("f", Arc::new(Float64Array::from(f)) as ArrayRef),
        ("g", Arc::new(Float64Array::from(g))),
        ("k", Arc::new(Int32Array::from(k))),
        ("s", Arc::new(StringArray::from(s))),
        ("t", Arc::new(TimestampMicrosecondArray::from(t))),
        ("d", Arc::new(dec)),
        (
            "h",
            Arc::new(Float32Array::from(
                (0..n)
                    .map(|i| if i % 4 == 0 { f32::NAN } else { -0.0 })
                    .collect::<Vec<_>>(),
            )),
        ),
    ]);
    for text in [
        "f > 1.0",
        "f >= 0",
        "f == 0.0",
        "f != f",
        "f < g",
        "f <= g or g == 2.5",
        "f == g",
        "not (f > g) and k >= -2",
        "k > 2.7",
        "k == 1 or s == 'x'",
        "s >= 'b' and is_not_null(f)",
        "is_null(f) or is_null(k)",
        "t > 0 and t <= 100000000",
        "d > 0.5",
        "d <= -0.25 or d == 1",
        "h < 0.0",
        "h == h",
        "-1 < f and f < 1",
        "`f` > -1e400",
    ] {
        let e = crate::expr::Expr::parse(text).expect(text);
        let want = old_eval(&e, &b);
        let got = e.eval(&b).expect(text);
        assert_eq!(got, want, "{text}");
    }
    // The translation as a document would write it.
    let e = crate::expr::Expr::parse("f > 1 and not is_null(k)").expect("parse");
    let tree = legacy::to_json(&e, &b.schema()).expect("translates");
    assert_eq!(
        tree,
        json!({"call": "and_kleene", "args": [
            {"call": "greater", "args": [{"column": "f"}, {"literal": "1.0", "type": "double"}], "options": {"order": "total"}},
            {"call": "invert", "args": [{"call": "is_null", "args": [{"column": "k"}]}]}]})
    );
    let e = crate::expr::Expr::parse("k < g").expect("parse");
    assert!(
        legacy::translate(&e, &b.schema())
            .expect_err("types")
            .to_string()
            .contains("one type")
    );
    assert!(
        legacy::translate(&crate::expr::Expr::parse("zz > 1").expect("p"), &b.schema()).is_err()
    );
    assert!(
        legacy::translate(
            &crate::expr::Expr::parse("is_null(zz)").expect("p"),
            &b.schema()
        )
        .is_err()
    );
    assert!(
        legacy::translate(
            &crate::expr::Expr::parse("is_not_null(zz)").expect("p"),
            &b.schema()
        )
        .is_err()
    );
    assert!(
        legacy::translate(
            &crate::expr::Expr::parse("k > 'x'").expect("p"),
            &b.schema()
        )
        .is_err()
    );
}
