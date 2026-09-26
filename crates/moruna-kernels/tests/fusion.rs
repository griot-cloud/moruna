//! CK-T7 fusion_is_invisible: a fused chain produces exactly what the unfused chain produces
//! (15 f.6, CK-I7), in fewer stages.

#![allow(clippy::result_large_err)]

use std::sync::Arc;

use moruna_kernel::arrow::array::{Float64Array, Int64Array, RecordBatch, StringArray};
use moruna_kernel::arrow::datatypes::{DataType, Field, Schema};
use moruna_kernel::{Kernel, NoState, Payload, SourceSchema};
use moruna_kernels::{StdKernel, fuse, fuse_chain};
use serde_json::json;

fn batch() -> RecordBatch {
    RecordBatch::try_new(
        Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, true),
            Field::new("name", DataType::Utf8, true),
            Field::new("score", DataType::Float64, true),
            Field::new("big", DataType::Int64, true),
        ])),
        vec![
            Arc::new(Int64Array::from(vec![
                Some(1),
                None,
                Some(3),
                Some(4),
                Some(5),
            ])),
            Arc::new(StringArray::from(vec![
                Some("a"),
                Some("b"),
                None,
                Some("d"),
                None,
            ])),
            Arc::new(Float64Array::from(vec![
                Some(1.5),
                None,
                Some(20.0),
                None,
                Some(-2.0),
            ])),
            Arc::new(Int64Array::from(vec![
                Some(1),
                Some(i64::MAX),
                None,
                Some(-7),
                Some(9),
            ])),
        ],
    )
    .expect("batch")
}

fn k(name: &str, args: serde_json::Value) -> StdKernel {
    StdKernel::new(name, &args).expect(name)
}

fn apply_all(kernels: &[StdKernel], input: &RecordBatch) -> RecordBatch {
    let mut current = input.clone();
    for kernel in kernels {
        let out = kernel
            .apply(&mut NoState, Payload::table(current).expect("payload"))
            .expect("apply");
        let Payload::Table(out, _) = out else {
            panic!("table")
        };
        current = out;
    }
    current
}

fn same(chain: Vec<StdKernel>, expect_stages: usize) {
    let input = batch();
    let sequential = apply_all(&chain, &input);
    let fused = fuse_chain(chain.clone());
    assert_eq!(fused.len(), expect_stages, "stages after fusion");
    assert!(fused.len() < chain.len());
    let together = apply_all(&fused, &input);
    assert_eq!(sequential, together, "CK-I7");
    // The fused kernel's schema answer is its output's.
    let mut schema = SourceSchema::Table(input.schema());
    for kernel in &fused {
        schema = kernel.output_schema(&schema).expect("schema");
    }
    let SourceSchema::Table(schema) = schema else {
        panic!("table")
    };
    assert_eq!(schema.fields(), together.schema().fields());
    for kernel in &fused {
        assert!(kernel.declared().is_checkable(), "{}", kernel.name());
    }
}

#[test]
fn select_after_cast_is_one_projection() {
    same(
        vec![
            k("cast", json!({"columns": {"id": "double", "big": "int32"}})),
            k("select", json!({"columns": ["id", "name"]})),
        ],
        1,
    );
}

#[test]
fn a_run_of_projections_is_one() {
    same(
        vec![
            k("rename", json!({"columns": {"id": "key"}})),
            k("cast", json!({"columns": {"key": "string"}})),
            k("drop", json!({"columns": ["big"]})),
            k("select", json!({"columns": ["score", "key"]})),
        ],
        1,
    );
}

#[test]
fn filter_after_fill_null_is_one_pass() {
    same(
        vec![
            k("fill_null", json!({"values": {"score": 0, "name": "?"}})),
            k("filter", json!({"expr": "score >= 0 and name != 'd'"})),
        ],
        1,
    );
    // The predicate reads a column the fill does not touch.
    same(
        vec![
            k("fill_null", json!({"values": {"name": "?"}})),
            k("filter", json!({"expr": "is_not_null(score)"})),
        ],
        1,
    );
}

#[test]
fn unfusable_neighbours_stay_apart_and_mixed_chains_fuse_where_they_can() {
    let filter = k("filter", json!({"expr": "id > 1"}));
    let fill = k("fill_null", json!({"values": {"id": 0}}));
    assert!(
        fuse(&filter, &fill).is_none(),
        "fill_null after filter is two passes"
    );
    let hash = k("hash", json!({"columns": ["id"]}));
    let select = k("select", json!({"columns": ["id", "hash"]}));
    assert!(fuse(&hash, &select).is_none());
    same(
        vec![
            k("cast", json!({"columns": {"score": "int64"}})),
            k("select", json!({"columns": ["id", "score", "name"]})),
            k("hash", json!({"columns": ["id"]})),
            k("fill_null", json!({"values": {"score": 0}})),
            k("filter", json!({"expr": "score > 1"})),
        ],
        3,
    );
}

#[test]
fn a_fused_kernel_has_its_own_name_arguments_and_fingerprint() {
    let cast = k("cast", json!({"columns": {"id": "double"}}));
    let select = k("select", json!({"columns": ["id"]}));
    let fused = fuse(&cast, &select).expect("fusable");
    assert_eq!(fused.name(), "cast+select");
    assert!(fused.is_fused());
    assert_eq!(fused.args()["stages"].as_array().map(Vec::len), Some(2));
    assert_ne!(fused.fingerprint(), cast.fingerprint());
    let again = fuse(&cast, &select).expect("fusable");
    assert_eq!(fused.fingerprint(), again.fingerprint());
    // A fused projection fuses again, and its stage list flattens.
    let drop = k("drop", json!({"columns": ["id"]}));
    let three = fuse(&fused, &drop).expect("fusable");
    assert_eq!(three.args()["stages"].as_array().map(Vec::len), Some(3));
}
