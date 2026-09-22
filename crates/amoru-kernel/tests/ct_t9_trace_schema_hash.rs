//! CT-T9 trace_schema_hash: `TraceRecord::SCHEMA_HASH` equals the pinned constant;
//! `arrow_schema()` field names and types match d.13 in order. Proves CT-I8.

use amoru_kernel::{Outcome, TraceRecord};
use arrow::datatypes::DataType;

/// The pinned value (e.5): the BLAKE3 digest of the canonical field list, computed once by the
/// component 1 agent and asserted here, so a schema change is a deliberate edit of this test
/// and of the constant.
const PINNED: &str = "778b6e4dc4a77e035f406b66db860daded5667a1761f198d34894ce2946a814b";

#[test]
fn ct_t9_trace_schema_hash() {
    let recomputed = TraceRecord::schema_hash();
    assert_eq!(
        hex(&recomputed),
        PINNED,
        "the trace schema changed; update TraceRecord::SCHEMA_HASH and this test deliberately"
    );
    assert_eq!(TraceRecord::SCHEMA_HASH, recomputed);

    // The Arrow schema is the field list of d.13, in order, with the types of e.5.
    let schema = TraceRecord::arrow_schema();
    let expected: Vec<(&str, DataType)> = vec![
        ("seq", DataType::UInt64),
        ("stage", DataType::UInt16),
        ("worker", DataType::UInt16),
        ("instance", DataType::UInt16),
        ("t_start_ns", DataType::UInt64),
        ("t_end_ns", DataType::UInt64),
        ("rows_in", DataType::UInt64),
        ("bytes_in", DataType::UInt64),
        ("rows_out", DataType::UInt64),
        ("bytes_out", DataType::UInt64),
        ("tier_in", DataType::UInt8),
        ("tier_out", DataType::UInt8),
        ("feat_mean_string_len", DataType::Float32),
        ("feat_null_ratio", DataType::Float32),
        ("feat_column_bytes", list_u64()),
        ("knob_morsel_target", DataType::UInt64),
        ("knob_active_workers", DataType::UInt16),
        ("knob_read_ahead", DataType::UInt16),
        ("mem_anon_before", DataType::UInt64),
        ("mem_anon_peak", DataType::UInt64),
        ("dev_mem_peak", DataType::UInt64),
        ("cpu_time_us", DataType::UInt64),
        ("throttled_delta_us", DataType::UInt64),
        ("q_bytes_before", list_u64()),
        ("q_bytes_after", list_u64()),
        ("staging_bytes_delta", DataType::Int64),
        ("placement_miss_wait_us", DataType::UInt64),
        ("state_bytes", DataType::UInt64),
        ("sizer", DataType::UInt8),
        ("outcome", DataType::UInt8),
        ("error", DataType::Utf8),
    ];
    assert_eq!(schema.fields().len(), expected.len());
    for (field, (name, data_type)) in schema.fields().iter().zip(&expected) {
        assert_eq!(field.name(), name);
        assert_eq!(field.data_type(), data_type, "field {name}");
        assert_eq!(
            field.is_nullable(),
            *name == "error",
            "field {name} nullability"
        );
    }

    // The canonical field list the hash is taken over names the same fields in the same order.
    let names: Vec<&str> = TraceRecord::SCHEMA_FIELDS
        .split(',')
        .map(|f| f.split(':').next().expect("a field name"))
        .collect();
    let schema_names: Vec<&str> = expected.iter().map(|(n, _)| *n).collect();
    assert_eq!(names, schema_names);

    // The outcome codes of e.5.
    for (outcome, code) in [
        (Outcome::Ok, 0u8),
        (Outcome::Error, 1),
        (Outcome::Skipped, 2),
        (Outcome::Probe, 3),
    ] {
        assert_eq!(outcome.code(), code);
        assert_eq!(Outcome::from_code(code), Some(outcome));
    }
    assert_eq!(Outcome::from_code(4), None);
}

fn list_u64() -> DataType {
    DataType::List(std::sync::Arc::new(arrow::datatypes::Field::new(
        "item",
        DataType::UInt64,
        false,
    )))
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
