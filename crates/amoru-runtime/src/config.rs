//! The rows of the preamble's configuration table (section 5) whose owner is `compile` and
//! which the facade, not a component, has to supply. Every one is the table's default; the
//! facade clamps nothing (PY-I10: the surface clamps user arguments, discovery clamps the
//! budget, the scheduler clamps its own knobs).

/// `budget.reserve_fraction`.
pub const RESERVE_FRACTION: f32 = 0.10;
/// `morsel.min_bytes`.
pub const MORSEL_MIN_BYTES: u64 = 4 << 20;
/// `morsel.max_bytes`.
pub const MORSEL_MAX_BYTES: u64 = 512 << 20;
/// `morsel.probe_bytes`.
pub const MORSEL_PROBE_BYTES: u64 = 16 << 20;
/// `controller.target_fraction`.
pub const TARGET_FRACTION: f32 = 0.85;
/// `controller.safety_initial`.
pub const SAFETY_INITIAL: f32 = 1.5;
/// `controller.safety_floor`.
pub const SAFETY_FLOOR: f32 = 1.2;
/// `controller.increase_step`.
pub const INCREASE_STEP: f32 = 0.10;
/// `controller.tick_ms`.
pub const TICK_MS: u64 = 250;
/// `controller.oscillation_flips`.
pub const OSCILLATION_FLIPS: u32 = 5;
/// `controller.freeze_morsels`.
pub const FREEZE_MORSELS: u32 = 100;
/// `readahead.splits`.
pub const READAHEAD_SPLITS: u16 = 2;
/// `reactor.threads`.
pub const REACTOR_THREADS: usize = 2;
/// `reactor.object_concurrency`.
pub const REACTOR_OBJECT_CONCURRENCY: usize = 8;
/// `reactor.file_depth`.
pub const REACTOR_FILE_DEPTH: usize = 32;
/// `trace.channel_capacity`.
pub const TRACE_CHANNEL_CAPACITY: usize = 4096;
/// `trace.memory_limit`.
pub const TRACE_MEMORY_LIMIT: u64 = 64 << 20;
/// `staging.segment_bytes`.
pub const STAGING_SEGMENT_BYTES: u64 = 128 << 20;
/// `ordering.buffer_bytes`.
pub const ORDERING_BUFFER_BYTES: u64 = 256 << 20;
/// `sink.concurrency`.
pub const SINK_CONCURRENCY: u16 = 2;
/// `checkpoint.interval_ms`.
pub const CHECKPOINT_INTERVAL_MS: u64 = 5000;
/// `sizer.fallback_error_ratio`.
pub const SIZER_FALLBACK_ERROR_RATIO: f32 = 2.0;
/// The scheduler's heartbeat cadence (10 d.1, `heartbeat_interval_ms`).
pub const HEARTBEAT_INTERVAL_MS: u64 = 1000;
/// The fraction of a device's free memory the run may hold (`budget.device`).
pub const DEVICE_BUDGET_FRACTION: f64 = 0.9;

/// The resolved configuration table, as the placement engine records it in the manifest
/// (09 e.5, `PlacementConfig::config`). It is the run's settings, not a schema: a resumed run
/// whose table differs is still resumed, and the difference is visible in the two manifests.
#[allow(clippy::too_many_arguments)]
pub fn resolved(
    budget_host: u64,
    budget_disk: u64,
    workers_max: u16,
    read_ahead: u16,
    error_policy: &amoru_kernel::ErrorPolicy,
    ordered: bool,
    sizer: amoru_kernel::SizerKind,
    checkpoint_enabled: bool,
    checkpoint_interval_ms: u64,
) -> serde_json::Value {
    serde_json::json!({
        "budget.host": budget_host,
        "budget.reserve_fraction": RESERVE_FRACTION,
        "budget.disk": budget_disk,
        "staging.segment_bytes": STAGING_SEGMENT_BYTES,
        "morsel.min_bytes": MORSEL_MIN_BYTES,
        "morsel.max_bytes": MORSEL_MAX_BYTES,
        "morsel.probe_bytes": MORSEL_PROBE_BYTES,
        "controller.target_fraction": TARGET_FRACTION,
        "controller.safety_initial": SAFETY_INITIAL,
        "controller.safety_floor": SAFETY_FLOOR,
        "controller.increase_step": INCREASE_STEP,
        "controller.tick_ms": TICK_MS,
        "controller.oscillation_flips": OSCILLATION_FLIPS,
        "controller.freeze_morsels": FREEZE_MORSELS,
        "workers.max": workers_max,
        "readahead.splits": read_ahead,
        "reactor.threads": REACTOR_THREADS,
        "reactor.object_concurrency": REACTOR_OBJECT_CONCURRENCY,
        "reactor.file_depth": REACTOR_FILE_DEPTH,
        "trace.channel_capacity": TRACE_CHANNEL_CAPACITY,
        "trace.memory_limit": TRACE_MEMORY_LIMIT,
        "errors.policy": format!("{error_policy:?}"),
        "ordering.required": ordered,
        "ordering.buffer_bytes": ORDERING_BUFFER_BYTES,
        "sink.concurrency": SINK_CONCURRENCY,
        "sizer": format!("{sizer:?}"),
        "sizer.fallback_error_ratio": SIZER_FALLBACK_ERROR_RATIO,
        "checkpoint.enabled": checkpoint_enabled,
        "checkpoint.interval_ms": checkpoint_interval_ms,
    })
}
