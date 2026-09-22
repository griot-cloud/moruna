//! The trace record, its Arrow schema and its schema hash (contracts d.13, e.5).

use std::sync::Arc;

use arrow::datatypes::{DataType, Field, Schema, SchemaRef};

use crate::ids::{Seq, StageId};

/// What happened to a morsel at a stage.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum Outcome {
    /// The kernel returned a payload.
    Ok,
    /// The kernel failed.
    Error,
    /// The error policy skipped the morsel.
    Skipped,
    /// The record is a probe's.
    Probe,
}

impl Outcome {
    /// The schema code (e.5): Ok=0, Error=1, Skipped=2, Probe=3.
    pub fn code(&self) -> u8 {
        match self {
            Outcome::Ok => 0,
            Outcome::Error => 1,
            Outcome::Skipped => 2,
            Outcome::Probe => 3,
        }
    }

    /// The outcome for a schema code; `None` for an unknown code.
    pub fn from_code(code: u8) -> Option<Outcome> {
        Some(match code {
            0 => Outcome::Ok,
            1 => Outcome::Error,
            2 => Outcome::Skipped,
            3 => Outcome::Probe,
            _ => return None,
        })
    }
}

/// One row per morsel per stage. Field order is the schema; see section e.5.
#[derive(Clone, Debug)]
pub struct TraceRecord {
    /// The morsel.
    pub seq: Seq,
    /// The stage.
    pub stage: StageId,
    /// The worker that ran it.
    pub worker: u16,
    /// Stateful instance index; `u16::MAX` for stateless.
    pub instance: u16,
    /// When `apply` started, nanoseconds.
    pub t_start_ns: u64,
    /// When `apply` ended, nanoseconds.
    pub t_end_ns: u64,
    /// Input rows.
    pub rows_in: u64,
    /// Input bytes.
    pub bytes_in: u64,
    /// Output rows.
    pub rows_out: u64,
    /// Output bytes.
    pub bytes_out: u64,
    /// `Tier::index` of the input payload.
    pub tier_in: u8,
    /// `Tier::index` of the output payload.
    pub tier_out: u8,
    /// Mean string length of the input.
    pub feat_mean_string_len: f32,
    /// Null ratio of the input.
    pub feat_null_ratio: f32,
    /// Bytes per input column.
    pub feat_column_bytes: Vec<u64>,
    /// The morsel target in force.
    pub knob_morsel_target: u64,
    /// The active worker count in force.
    pub knob_active_workers: u16,
    /// The read-ahead depth in force.
    pub knob_read_ahead: u16,
    /// Anonymous host bytes before `apply`.
    pub mem_anon_before: u64,
    /// Peak anonymous host bytes during `apply`.
    pub mem_anon_peak: u64,
    /// Peak device bytes during `apply`.
    pub dev_mem_peak: u64,
    /// CPU microseconds inside `apply`.
    pub cpu_time_us: u64,
    /// Microseconds of CPU throttling during `apply`.
    pub throttled_delta_us: u64,
    /// Queue bytes per tier before the task.
    pub q_bytes_before: Vec<u64>,
    /// Queue bytes per tier after the task.
    pub q_bytes_after: Vec<u64>,
    /// Change in staging bytes caused by the task.
    pub staging_bytes_delta: i64,
    /// Microseconds the pop waited on a move (PL-I9).
    pub placement_miss_wait_us: u64,
    /// `KernelState::footprint` after apply; 0 when None or stateless.
    pub state_bytes: u64,
    /// 0 rule, 1 learned.
    pub sizer: u8,
    /// What happened.
    pub outcome: Outcome,
    /// The error message, when the outcome is `Error`.
    pub error: Option<String>,
}

impl TraceRecord {
    /// The canonical field list the schema hash is taken over (e.5): `"name:type,..."` in
    /// field order, with the field types as d.13 declares them (`u8`, `u16`, `u64`, `i64`,
    /// `f32`, `list<u64>`, `string`; `outcome` is the `u8` code of e.5).
    pub const SCHEMA_FIELDS: &'static str = concat!(
        "seq:u64,stage:u16,worker:u16,instance:u16,",
        "t_start_ns:u64,t_end_ns:u64,",
        "rows_in:u64,bytes_in:u64,rows_out:u64,bytes_out:u64,",
        "tier_in:u8,tier_out:u8,",
        "feat_mean_string_len:f32,feat_null_ratio:f32,feat_column_bytes:list<u64>,",
        "knob_morsel_target:u64,knob_active_workers:u16,knob_read_ahead:u16,",
        "mem_anon_before:u64,mem_anon_peak:u64,dev_mem_peak:u64,",
        "cpu_time_us:u64,throttled_delta_us:u64,",
        "q_bytes_before:list<u64>,q_bytes_after:list<u64>,",
        "staging_bytes_delta:i64,placement_miss_wait_us:u64,state_bytes:u64,",
        "sizer:u8,outcome:u8,error:string"
    );

    /// BLAKE3 of the canonical field list (CT-I8). A compile-time constant: the digest of
    /// `SCHEMA_FIELDS`, computed once and pinned here, because BLAKE3 cannot be evaluated in a
    /// `const` at the pinned `blake3` version (see this pull request's escalations). Test
    /// CT-T9 recomputes the digest from `SCHEMA_FIELDS` and asserts this value, so a schema
    /// change is a deliberate edit of the test and of this constant.
    pub const SCHEMA_HASH: [u8; 32] = [
        0x77, 0x8b, 0x6e, 0x4d, 0xc4, 0xa7, 0x7e, 0x03, 0x5f, 0x40, 0x6b, 0x66, 0xdb, 0x86, 0x0d,
        0xad, 0xed, 0x56, 0x67, 0xa1, 0x76, 0x1f, 0x19, 0x8d, 0x34, 0x89, 0x4c, 0xe2, 0x94, 0x6a,
        0x81, 0x4b,
    ];

    /// The digest of `SCHEMA_FIELDS`, recomputed. Equal to `SCHEMA_HASH` (CT-T9).
    pub fn schema_hash() -> [u8; 32] {
        *blake3::hash(Self::SCHEMA_FIELDS.as_bytes()).as_bytes()
    }

    /// The Arrow schema of the trace file: field order and types exactly as in d.13 (e.5).
    pub fn arrow_schema() -> SchemaRef {
        let list_u64 = || DataType::List(Arc::new(Field::new("item", DataType::UInt64, false)));
        let fields = vec![
            Field::new("seq", DataType::UInt64, false),
            Field::new("stage", DataType::UInt16, false),
            Field::new("worker", DataType::UInt16, false),
            Field::new("instance", DataType::UInt16, false),
            Field::new("t_start_ns", DataType::UInt64, false),
            Field::new("t_end_ns", DataType::UInt64, false),
            Field::new("rows_in", DataType::UInt64, false),
            Field::new("bytes_in", DataType::UInt64, false),
            Field::new("rows_out", DataType::UInt64, false),
            Field::new("bytes_out", DataType::UInt64, false),
            Field::new("tier_in", DataType::UInt8, false),
            Field::new("tier_out", DataType::UInt8, false),
            Field::new("feat_mean_string_len", DataType::Float32, false),
            Field::new("feat_null_ratio", DataType::Float32, false),
            Field::new("feat_column_bytes", list_u64(), false),
            Field::new("knob_morsel_target", DataType::UInt64, false),
            Field::new("knob_active_workers", DataType::UInt16, false),
            Field::new("knob_read_ahead", DataType::UInt16, false),
            Field::new("mem_anon_before", DataType::UInt64, false),
            Field::new("mem_anon_peak", DataType::UInt64, false),
            Field::new("dev_mem_peak", DataType::UInt64, false),
            Field::new("cpu_time_us", DataType::UInt64, false),
            Field::new("throttled_delta_us", DataType::UInt64, false),
            Field::new("q_bytes_before", list_u64(), false),
            Field::new("q_bytes_after", list_u64(), false),
            Field::new("staging_bytes_delta", DataType::Int64, false),
            Field::new("placement_miss_wait_us", DataType::UInt64, false),
            Field::new("state_bytes", DataType::UInt64, false),
            Field::new("sizer", DataType::UInt8, false),
            Field::new("outcome", DataType::UInt8, false),
            Field::new("error", DataType::Utf8, true),
        ];
        Arc::new(Schema::new(fields))
    }
}

/// Where a trace record goes (component 4, write side).
pub trait TraceSink: Send + Sync {
    /// Bounded, non-blocking beyond a channel push; drops nothing (backpressure is on the
    /// writer, not the caller).
    fn record(&self, r: TraceRecord);
    /// Push everything buffered to its destination.
    fn flush(&self) -> crate::Result<()>;
}

/// Read-side of the trace for the controller; implemented by the trace writer.
pub trait TraceTail: Send + Sync {
    /// The last `n` records of `stage`, oldest first, from the in-memory chunks.
    fn tail(&self, stage: StageId, n: usize) -> Vec<TraceRecord>;
}
