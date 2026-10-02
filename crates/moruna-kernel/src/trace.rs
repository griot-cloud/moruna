//! The trace record, its Arrow schema and its schema hash (contracts d.13, e.5).

use std::sync::Arc;

use arrow::datatypes::{DataType, Field, Schema, SchemaRef};

use crate::ids::{Seq, StageId};
use crate::kernel::{AllocCounts, KernelAlloc};
use crate::limits::Limits;

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
    /// What the call asked for, per source (contracts d.7, d.13; E13): what
    /// `take_kernel_alloc` returned after `apply`, the default when the kernel counted nothing.
    pub alloc: KernelAlloc,
}

/// The sources of `KernelAlloc`, in schema order (e.5).
pub const ALLOC_SOURCES: [&str; 3] = ["python", "numpy", "arrow"];
/// The counts of each source, in schema order (e.5).
pub const ALLOC_COUNTS: [&str; 5] = ["bytes", "requests", "largest", "peak", "refused"];

impl KernelAlloc {
    /// The source's counts by its schema index (0 python, 1 numpy, 2 arrow).
    pub fn source(&self, index: usize) -> &AllocCounts {
        match index {
            0 => &self.python,
            1 => &self.numpy,
            _ => &self.arrow,
        }
    }

    /// As `source`, mutably.
    pub fn source_mut(&mut self, index: usize) -> &mut AllocCounts {
        match index {
            0 => &mut self.python,
            1 => &mut self.numpy,
            _ => &mut self.arrow,
        }
    }
}

impl AllocCounts {
    /// The count by its schema index (`ALLOC_COUNTS`).
    pub fn count(&self, index: usize) -> u64 {
        match index {
            0 => self.bytes,
            1 => self.requests,
            2 => self.largest,
            3 => self.peak,
            _ => self.refused,
        }
    }

    /// As `count`, mutably.
    pub fn count_mut(&mut self, index: usize) -> &mut u64 {
        match index {
            0 => &mut self.bytes,
            1 => &mut self.requests,
            2 => &mut self.largest,
            3 => &mut self.peak,
            _ => &mut self.refused,
        }
    }
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
        "sizer:u8,outcome:u8,error:string,",
        "alloc_measured:u8,alloc_refusal_on:u8,",
        "alloc_python_bytes:u64,alloc_python_requests:u64,alloc_python_largest:u64,",
        "alloc_python_peak:u64,alloc_python_refused:u64,",
        "alloc_numpy_bytes:u64,alloc_numpy_requests:u64,alloc_numpy_largest:u64,",
        "alloc_numpy_peak:u64,alloc_numpy_refused:u64,",
        "alloc_arrow_bytes:u64,alloc_arrow_requests:u64,alloc_arrow_largest:u64,",
        "alloc_arrow_peak:u64,alloc_arrow_refused:u64"
    );

    /// BLAKE3 of the canonical field list (CT-I8). A compile-time constant: the digest of
    /// `SCHEMA_FIELDS`, computed once and pinned here, because BLAKE3 cannot be evaluated in a
    /// `const` at the pinned `blake3` version (see this pull request's escalations). Test
    /// CT-T9 recomputes the digest from `SCHEMA_FIELDS` and asserts this value, so a schema
    /// change is a deliberate edit of the test and of this constant.
    pub const SCHEMA_HASH: [u8; 32] = [
        0xb2, 0x76, 0x85, 0x66, 0xc9, 0xb6, 0x93, 0xc4, 0x1b, 0x00, 0x05, 0x89, 0x8a, 0x72, 0x30,
        0x8d, 0xd7, 0x4f, 0x98, 0xe4, 0xc0, 0x43, 0x04, 0xfd, 0xb6, 0x40, 0xc1, 0x2c, 0x70, 0xa1,
        0xca, 0x91,
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
            Field::new("alloc_measured", DataType::UInt8, false),
            Field::new("alloc_refusal_on", DataType::UInt8, false),
        ];
        let mut fields = fields;
        for source in ALLOC_SOURCES {
            for count in ALLOC_COUNTS {
                fields.push(Field::new(
                    format!("alloc_{source}_{count}"),
                    DataType::UInt64,
                    false,
                ));
            }
        }
        Arc::new(Schema::new(fields))
    }
}

/// Which of the limits moved. A change smaller than one huge page of memory and one
/// CPU is not a change and never produces one of these.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum LimitsChangeReason {
    /// The memory ceiling or the kill line moved.
    Memory,
    /// The CPU quota moved.
    Cpu,
    /// Both moved in the same reading.
    MemoryAndCpu,
}

impl LimitsChangeReason {
    /// The name the host protocol and the report use: `"memory"`, `"cpu"` or `"memory,cpu"`.
    pub fn name(&self) -> &'static str {
        match self {
            LimitsChangeReason::Memory => "memory",
            LimitsChangeReason::Cpu => "cpu",
            LimitsChangeReason::MemoryAndCpu => "memory,cpu",
        }
    }
}

/// A change of the machine's limits while the run is in progress.
///
/// It is not a `TraceRecord`: a record is one row per morsel per stage under a pinned schema
/// (CT-I8, e.5), and a limits change belongs to no morsel. It travels beside the records through
/// the same `TraceSink`, and the run report reads the timeline back from the trace, so the report
/// stays a function of the trace, the limits and the meta (TR-I3).
#[derive(Clone, Debug)]
pub struct LimitsChanged {
    /// When the watcher saw the change, nanoseconds since the Unix epoch.
    pub at_ns: u64,
    /// The limits in force before the change.
    pub old: Limits,
    /// The limits in force from now on.
    pub new: Limits,
    /// What moved.
    pub reason: LimitsChangeReason,
}

/// Where a trace record goes (component 4, write side).
pub trait TraceSink: Send + Sync {
    /// Bounded, non-blocking beyond a channel push; drops nothing (backpressure is on the
    /// writer, not the caller).
    fn record(&self, r: TraceRecord);
    /// Push everything buffered to its destination.
    fn flush(&self) -> crate::Result<()>;
    /// A change of the limits. Called by the facade's watcher, never by a worker, and
    /// rarely: once per change the watcher accepts. Default: nothing, so a sink that keeps no
    /// timeline (a fake, a test double) is still a valid sink; the trace writer keeps them for
    /// the run report's `limits_timeline`.
    fn limits_changed(&self, change: LimitsChanged) {
        let _ = change;
    }
}

/// Read-side of the trace for the controller; implemented by the trace writer.
pub trait TraceTail: Send + Sync {
    /// The last `n` records of `stage`, oldest first, from the in-memory chunks.
    fn tail(&self, stage: StageId, n: usize) -> Vec<TraceRecord>;
}
