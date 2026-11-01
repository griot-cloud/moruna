//! The run report: a pure function of the trace, the discovered limits and the run meta
//! (d.1, f.2, TR-I3, G-I4). Every report field is computed here and nowhere else.

use std::collections::BTreeMap;
use std::path::PathBuf;

use moruna_kernel::{
    GilState, IoPaths, Limits, Outcome, ProcessPeak, ProcessUsage, RunId, Seq, SinkWrite,
    SourceRead, StageId, TraceRecord,
};
use serde::{Serialize, Serializer};

use crate::TraceView;
use crate::run_id_hex;

/// How the run ended (d.1). The scheduler's outcome, as the facade translates it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum ExitReason {
    /// Source exhausted, every queue drained, sink finished.
    Completed,
    /// The runtime ended the run itself, with this diagnostic (G-I8).
    Terminated {
        /// The diagnostic the runtime produced.
        diagnostic: String,
    },
    /// SIGINT or `KeyboardInterrupt`, forwarded by the surface.
    Cancelled,
}

/// What the report needs that is not in the trace; assembled by the facade (12 f.2).
#[derive(Clone, Debug)]
pub struct RunMeta {
    /// The run's identity (contracts d.1).
    pub run_id: RunId,
    /// How the run ended.
    pub exit: ExitReason,
    /// When the run started, nanoseconds.
    pub start_ns: u64,
    /// When the run ended, nanoseconds.
    pub end_ns: u64,
    /// The run continued from a manifest (12 f.7).
    pub resumed: bool,
    /// The last manifest written, when one was.
    pub manifest: Option<PathBuf>,
    /// Discovery notes, facade clamps (12 f.3), runtime notes.
    pub notes: Vec<String>,
    /// One entry per Python stage (05 d.1 `PyKernel::gil_state`).
    pub gil: Vec<(StageId, GilState)>,
    /// The direct paths the reactor selected (`Reactor::paths`).
    pub io_paths: IoPaths,
    /// Which decision function sized morsels.
    pub sizer: &'static str,
    /// Where a learned sizer fell back to the rule sizer.
    pub sizer_fallback_at: Option<Seq>,
    /// `(seconds, classification)` from `ControllerSummary` (11 d.1).
    pub bottleneck_timeline: Vec<(f64, String)>,
    /// `ControllerSummary.notes`, appended to `notes` in the report.
    pub controller_notes: Vec<String>,
    /// Every shrink the facade's watcher started, and how long its drain took.
    pub drains: Vec<DrainSummary>,
    /// The sampler's high-water mark in `memory_basis`, including a chain with no
    /// kernel. Native Account marks are observed pressure, never exact RSS.
    pub process_peak: ProcessPeak,
    /// Sampler pressure basis; serialized as an additive plain metadata label.
    pub memory_basis: moruna_kernel::MemoryBasis,
    /// What the whole process consumed over the run, measured (`Sampler::process_usage`).
    pub process_usage: ProcessUsage,
    /// What the engine behind the sink said about the write it committed
    /// (`SinkSummary::write`); `None` for a sink with no such engine, and for a run that did
    /// not complete.
    pub write: Option<SinkWrite>,
    /// What the engine behind the source said about the read it planned
    /// (`Source::read_report`); `None` for a source with no such engine.
    pub read: Option<SourceRead>,
    /// The placement engine's buffer catalog at the end of the run (placement f.18); `None` for
    /// a run given a placement of its own.
    pub buffers: Option<BufferReport>,
    /// What the scheduling policy decided and left unused (10 f.3, adaptive-runtime 9.7);
    /// `None` when the run never started its scheduler.
    pub policy: Option<PolicyReport>,
}

/// What the scheduling policy did over a run (adaptive-runtime 9.7, 10), for the benchmark
/// matrix to compare policies by.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PolicyReport {
    /// The policy in use.
    pub name: String,
    /// Decisions made, and the nanoseconds they took.
    pub decisions: u64,
    /// Nanoseconds spent deciding.
    pub decision_ns: u64,
    /// Decisions over their budget, and those the baseline made in their place.
    pub over_budget: u64,
    /// Decisions the baseline made because the last was over budget.
    pub fallbacks: u64,
    /// Units run, and units chosen whose claim was not granted.
    pub admitted: u64,
    /// Units chosen whose claim was not granted.
    pub not_claimed: u64,
    /// Seconds workers idled while a unit was ready.
    pub idle_ready_s: f64,
    /// Budget bytes neither resident nor claimed while a unit was ready, times seconds.
    pub unused_ready_byte_s: f64,
    /// Pushes memory could not admit, written straight to disk.
    pub overflow_spills: u64,
    /// Pushes that waited for room.
    pub push_waits: u64,
    /// Decisions the policy made with a shallower procedure to keep to its budget (lookahead
    /// falling to greedy).
    pub shallower: u64,
    /// Simulations the policy ran, and those that could not be run, with the first reason.
    pub simulations: u64,
    /// Simulations that could not be run.
    pub simulation_errors: u64,
    /// Why the first could not be.
    pub simulation_error: Option<String>,
    /// Units run whose prediction came from the cost model, and from measured ratios.
    pub predicted_by_model: u64,
    /// Units run predicted from measured ratios.
    pub predicted_by_ratio: u64,
    /// What the run's learning did (adaptive-runtime 9.6); `None` when no cost model was set.
    pub learning: Option<LearningReport>,
    /// What deciding and learning cost, part by part (adaptive-runtime 0, G9).
    pub overhead: OverheadReport,
}

/// What the adaptive machinery cost a run (adaptive-runtime 0, G9), seconds and counts.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OverheadReport {
    /// Units run and their wall time summed.
    pub units: u64,
    /// The units' wall time summed.
    pub unit_s: f64,
    /// Gathering what decisions read, part of their budget.
    pub gather_s: f64,
    /// Of it, asking the cost model, how often, and how often a stage's cache answered.
    pub predict_s: f64,
    /// Times the model was asked.
    pub predictions: u64,
    /// Times a stage's cache answered.
    pub cached: u64,
    /// Of the gathering, the stages' snapshot.
    pub snapshot_s: f64,
    /// The policy's decisions.
    pub decide_s: f64,
    /// Settling units into the models, on the workers, and how many.
    pub learn_s: f64,
    /// Units settled.
    pub learned: u64,
    /// The share of the units' time the three above took: what the run paid for adapting.
    pub share: f64,
    /// Appending to the store and writing the fitted state, on the learning thread.
    pub store_s: f64,
    /// Writing the fitted state.
    pub save_s: f64,
    /// The run's end waiting for the learning thread, after its output was complete.
    pub drain_s: f64,
}

/// What the run's cost model learned (adaptive-runtime 9.6, 9.7).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LearningReport {
    /// The deciding model, `name@version`.
    pub model: String,
    /// The model in shadow.
    pub shadow: Option<String>,
    /// Units observed, written to the store, and not written.
    pub observed: u64,
    /// Observations written to the store.
    pub written: u64,
    /// Observations not written.
    pub unwritten: u64,
    /// Why the first was not written.
    pub store_error: Option<String>,
    /// Outcomes refused as no measurement.
    pub refused: u64,
    /// Why the first was refused.
    pub refusal: Option<String>,
    /// Every spell of drift: the stage, the target, the unit it began at, how many units it
    /// lasted and whether it settled.
    pub drifts: Vec<DriftReport>,
}

/// One spell of a stage's drift.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DriftReport {
    /// The stage.
    pub stage: StageId,
    /// The target whose outcomes left the spread.
    pub target: String,
    /// The stage's units observed when it began.
    pub at_unit: u64,
    /// Units observed while it drifted.
    pub units: u64,
    /// Whether it settled before the run ended.
    pub settled: bool,
}

impl PolicyReport {
    /// The report of the scheduler's figures.
    pub fn of(stats: &moruna_kernel::PolicyStats) -> PolicyReport {
        PolicyReport {
            name: stats.name.clone(),
            decisions: stats.decisions,
            decision_ns: stats.decision_ns,
            over_budget: stats.over_budget,
            fallbacks: stats.fallbacks,
            admitted: stats.admitted,
            not_claimed: stats.not_claimed,
            idle_ready_s: stats.idle_ready_ns as f64 / 1e9,
            unused_ready_byte_s: stats.unused_ready_byte_ns as f64 / 1e9,
            overflow_spills: stats.overflow_spills,
            push_waits: stats.push_waits,
            shallower: stats.counters.shallower,
            simulations: stats.counters.simulations,
            simulation_errors: stats.counters.simulation_errors,
            simulation_error: stats.counters.simulation_error.clone(),
            predicted_by_model: stats.predicted_by_model,
            predicted_by_ratio: stats.predicted_by_ratio,
            overhead: {
                let o = &stats.overhead;
                let s = |ns: u64| ns as f64 / 1e9;
                OverheadReport {
                    units: o.units,
                    unit_s: s(o.unit_ns),
                    gather_s: s(o.gather_ns),
                    predict_s: s(o.predict_ns),
                    predictions: o.predictions,
                    cached: o.cached,
                    snapshot_s: s(o.snapshot_ns),
                    decide_s: s(o.decide_ns),
                    learn_s: s(o.learn_ns),
                    learned: o.learned,
                    share: o.share(),
                    store_s: s(o.store_ns),
                    save_s: s(o.save_ns),
                    drain_s: s(o.drain_ns),
                }
            },
            learning: stats.learning.as_ref().map(|l| LearningReport {
                model: l.model.clone(),
                shadow: l.shadow.clone(),
                observed: l.observed,
                written: l.written,
                unwritten: l.unwritten,
                store_error: l.store_error.clone(),
                refused: l.refused,
                refusal: l.refusal.clone(),
                drifts: l
                    .drifts
                    .iter()
                    .map(|d| DriftReport {
                        stage: d.stage,
                        target: d.target.clone(),
                        at_unit: d.at_unit,
                        units: d.units,
                        settled: d.settled,
                    })
                    .collect(),
            }),
        }
    }
}

/// What the buffer catalog did over a run (placement f.18, adaptive-runtime 9.7): the figures the
/// benchmark matrix compares eviction orders and policies by. Per-tier arrays are indexed by
/// `TierKind::index` (Device, PinnedHost, Host, Disk, Remote).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BufferReport {
    /// The eviction order the catalog ran with.
    pub eviction: String,
    /// The highest bytes resident per tier, every batch once.
    pub peak_by_tier: Vec<u64>,
    /// The budget per tier at the end of the run, beside which the peaks are read.
    pub budget_by_tier: Vec<u64>,
    /// Bytes written to disk to free memory, and how many batches.
    pub spilled_bytes: u64,
    /// Batches spilled.
    pub spills: u64,
    /// Bytes read back from disk, and how many batches.
    pub reloaded_bytes: u64,
    /// Batches reloaded.
    pub reloads: u64,
    /// Batches sent out of memory, by the eviction order or by a policy's spill.
    pub evictions: u64,
    /// Times a consumer reached a batch eviction had taken out of memory.
    pub misses: u64,
    /// The misses weighted by reload cost: the disk bytes bringing them back moved.
    pub miss_reload_bytes: u64,
    /// Shares consumers took and released.
    pub released: u64,
    /// Batches freed.
    pub freed: u64,
    /// Bytes batches were pushed beyond their reservations.
    pub overrun_bytes: u64,
    /// Batches memory could not admit, written straight to disk, and their bytes.
    pub overflows: u64,
    /// Their bytes.
    pub overflow_bytes: u64,
}

impl BufferReport {
    /// The report of a catalog that ran with the order named `eviction`.
    pub fn of(eviction: &str, stats: &moruna_kernel::CatalogStats) -> BufferReport {
        BufferReport {
            eviction: eviction.to_string(),
            peak_by_tier: stats.peak_by_tier.to_vec(),
            budget_by_tier: stats.budget_by_tier.to_vec(),
            spilled_bytes: stats.spilled_bytes,
            spills: stats.spills,
            reloaded_bytes: stats.reloaded_bytes,
            reloads: stats.reloads,
            evictions: stats.evictions,
            misses: stats.misses,
            miss_reload_bytes: stats.miss_reload_bytes,
            released: stats.released,
            freed: stats.freed,
            overrun_bytes: stats.overrun_bytes,
            overflows: stats.overflows,
            overflow_bytes: stats.overflow_bytes,
        }
    }
}

/// One shrink of the arena and its drain: when the watcher marked regions draining, how
/// many bytes, and how long until the last of them was unmapped. The host paces its next
/// lowering on this (MH 7.1).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DrainSummary {
    /// Milliseconds from the start of the run to the shrink.
    pub at_ms: u64,
    /// Bytes marked draining.
    pub bytes: u64,
    /// Milliseconds until the drain completed; `None` when the run ended first.
    pub drain_ms: Option<u64>,
}

/// One accelerator, as the report names it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DeviceSummary {
    /// `DeviceId`.
    pub id: u8,
    /// The device name discovery reported.
    pub name: String,
    /// Device memory in total.
    pub total_bytes: u64,
    /// Device memory free when discovery ran.
    pub free_bytes: u64,
}

/// The discovered limits, as the report shows them (d.1: ceiling, kill, cpu quota, source,
/// devices).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LimitsSummary {
    /// The host ceiling the run was sized against.
    pub memory_ceiling: u64,
    /// Where the host would kill the process, when that is known.
    pub memory_kill: Option<u64>,
    /// CPUs the run was allowed.
    pub cpu_quota: f64,
    /// `LimitSource` by name: `Cgroup`, `Os` or `Explicit`.
    pub source: String,
    /// The accelerators discovery found.
    pub devices: Vec<DeviceSummary>,
}

impl LimitsSummary {
    fn of(limits: &Limits) -> LimitsSummary {
        LimitsSummary {
            memory_ceiling: limits.memory_ceiling,
            memory_kill: limits.memory_kill,
            cpu_quota: limits.cpu_quota,
            source: format!("{:?}", limits.source),
            devices: limits
                .devices
                .iter()
                .map(|d| DeviceSummary {
                    id: d.id.0,
                    name: d.name.clone(),
                    total_bytes: d.total_bytes,
                    free_bytes: d.free_bytes,
                })
                .collect(),
        }
    }
}

/// One stage's row of the report (d.1, f.2).
#[derive(Clone, Debug, Serialize)]
pub struct StageReport {
    /// The stage.
    pub stage: StageId,
    /// Records for this stage.
    pub morsels: u64,
    /// Input rows.
    pub rows_in: u64,
    /// Output rows.
    pub rows_out: u64,
    /// Input bytes.
    pub bytes_in: u64,
    /// Output bytes.
    pub bytes_out: u64,
    /// `max(t_end) - min(t_start)` over the stage's records.
    pub wall_s: f64,
    /// Time inside `apply` for records that produced a payload.
    pub kernel_busy_s: f64,
    /// `rows_out` over the stage's wall time.
    pub rows_per_s: f64,
    /// `bytes_out` over the stage's wall time.
    pub bytes_per_s: f64,
    /// Median of `(mem_anon_peak - mem_anon_before) / bytes_in`.
    pub amplification_p50: f64,
    /// 95th percentile of the same, by nearest rank.
    pub amplification_p95: f64,
    /// Seconds a pop waited on a move.
    pub placement_miss_wait_s: f64,
    /// Records whose outcome was `Error`.
    pub errors: u64,
    /// Records whose outcome was `Skipped`.
    pub skipped: u64,
    /// The largest `state_bytes` any instance of the stage held (f.2).
    pub state_bytes_max: u64,
    /// The largest growth of one instance's state over the run: its last `state_bytes`
    /// less its first non-zero one (f.2, RC f.6 StateGrowth).
    pub state_growth: i64,
    /// What the stage's kernel asked for, per source (f.2; E13); `None` when no record of the
    /// stage was measured.
    pub alloc: Option<crate::alloc::StageAlloc>,
}

/// What the user reads and what S1 to S4 are measured from (d.1). Pure in the trace, the
/// limits and the meta (TR-I3).
#[derive(Clone, Debug, Serialize)]
pub struct RunReport {
    /// `meta.run_id` as 32 lowercase hex characters.
    pub run_id: String,
    /// How the run ended.
    pub exit: ExitReason,
    /// The run continued from a manifest.
    pub resumed: bool,
    /// The manifest, when the run is resumable.
    pub manifest: Option<String>,
    /// `meta.end_ns - meta.start_ns` in seconds.
    pub wall_s: f64,
    /// The limits the run started with, as discovery found them.
    pub limits_initial: LimitsSummary,
    /// Every change of the limits while the run was in progress, as `(milliseconds from the start
    /// of the run, the limits from then on)`, oldest first. Empty for a run whose machine
    /// never moved. This is the host's sizing input: it tells "90% of 8 GB" from "90% of 8 GB
    /// for one minute and of 3 GB the rest of the time" (MH 2.1).
    pub limits_timeline: Vec<(u64, LimitsSummary)>,
    /// Every shrink of the arena and how long its drain took.
    pub drains: Vec<DrainSummary>,
    /// The direct paths taken (G-I7).
    #[serde(serialize_with = "io_paths_json")]
    pub io_paths: IoPaths,
    /// Legacy pressure-peak field in `memory_basis`: process anonymous residency
    /// by default, native Account charges with registered outside reservations
    /// when explicitly labeled. Native peaks are observed, never exact RSS marks.
    pub peak_anon_bytes: u64,
    /// Meaning of legacy memory-pressure/peak fields; absent means legacy
    /// ProcessAnon. An explicit native string labels charges, never RSS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_basis: Option<String>,
    /// The process's CPU time over the run, user plus system, nanoseconds, as the operating
    /// system accounted it. Zero when `usage_measured` is false.
    pub cpu_ns: u64,
    /// The process's anonymous memory integrated over the run, byte-seconds. Zero when
    /// `usage_measured` is false.
    pub mem_byte_seconds: u64,
    /// True when `cpu_ns` and `mem_byte_seconds` were measured; a host never bills otherwise.
    pub usage_measured: bool,
    /// That over the memory ceiling in force when the peak was reached (S1): the initial
    /// ceiling, or the one the last limits change before the peak set.
    pub peak_fraction_of_ceiling: f64,
    /// The ceiling `peak_fraction_of_ceiling` was taken against.
    pub peak_ceiling_bytes: u64,
    /// Kernel busy time over wall time times the mean active worker count (S4).
    pub worker_busy_fraction: f64,
    /// Throttled microseconds over the CPU the run was allowed.
    pub cpu_throttled_fraction: f64,
    /// Stage 1 input bytes over stage 1's wall time.
    pub source_bytes_per_s: f64,
    /// Stage 1 input bytes over the whole run's wall time (f.2).
    pub source_bandwidth: f64,
    /// Bytes moved to and from staging over the whole run's wall time (f.2).
    pub staging_bandwidth: f64,
    /// Bytes demoted to staging.
    pub staging_bytes_written: u64,
    /// Whether staging was used at all.
    pub staging_engaged: bool,
    /// The interpreter state of each Python stage (05 d.1).
    #[serde(serialize_with = "gil_json")]
    pub gil: Vec<(StageId, GilState)>,
    /// Whether any stage ran serialised.
    pub gil_serialised: bool,
    /// Which decision function sized morsels.
    pub sizer_used: String,
    /// Where a learned sizer fell back.
    pub sizer_fallback_at: Option<Seq>,
    /// `(seconds, classification)` from the controller's records.
    pub bottleneck_timeline: Vec<(f64, String)>,
    /// One entry per stage the trace saw.
    pub stages: Vec<StageReport>,
    /// `meta.notes` followed by `meta.controller_notes`.
    pub notes: Vec<String>,
    /// A chunk could not be written to the overflow file (04 h, failures).
    pub overflow_failed: bool,
    /// Records that arrived after `finish` or after the writer failed (04 e.1, h).
    pub late_records: u64,
    /// The engine's own report of the write the run's sink committed (a peQL contract's
    /// verdict over the rows written, the rows that broke each rule, and the snapshot
    /// committed), as `{"engine", "report"}`, with `"excluded": {"url", "files", "rows"}`
    /// when the write set rows aside. Absent from the JSON for every other run.
    #[serde(serialize_with = "write_json", skip_serializing_if = "Option::is_none")]
    pub write: Option<SinkWrite>,
    /// The engine's own report of the read the run's source planned (a peQL plan's
    /// resolution of every contract it read, with the rows each view left out by rule), as
    /// `{"engine", "report"}`. Absent from the JSON for every other run.
    #[serde(serialize_with = "read_json", skip_serializing_if = "Option::is_none")]
    pub read: Option<SourceRead>,
    /// What the buffer catalog did (placement f.18): bytes spilled and reloaded, evictions, misses
    /// weighted by reload cost, peaks beside budgets. Absent from the JSON for a run given a
    /// placement of its own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buffers: Option<BufferReport>,
    /// What the scheduling policy decided and left unused (10 f.3). Absent from the JSON when
    /// the run never started its scheduler.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<PolicyReport>,
}

/// `SinkWrite` is the contracts' type (d.8); serialised here as `{"engine", "report"}`, the
/// report exactly as the engine serialised it, and `"excluded"` when the sink kept rows the
/// engine handed back.
fn write_json<S: Serializer>(v: &Option<SinkWrite>, s: S) -> core::result::Result<S::Ok, S::Error> {
    use serde::ser::SerializeStruct;
    match v {
        None => s.serialize_none(),
        Some(write) => {
            let fields = if write.excluded.is_some() { 3 } else { 2 };
            let mut st = s.serialize_struct("SinkWrite", fields)?;
            st.serialize_field("engine", &write.engine)?;
            st.serialize_field("report", &write.report)?;
            if let Some(excluded) = &write.excluded {
                st.serialize_field(
                    "excluded",
                    &serde_json::json!({
                        "url": excluded.url,
                        "files": excluded.files,
                        "rows": excluded.rows,
                    }),
                )?;
            }
            st.end()
        }
    }
}

/// `SourceRead` is the contracts' type (d.6); serialised here as `{"engine", "report"}`, the
/// report exactly as the engine serialised it.
fn read_json<S: Serializer>(v: &Option<SourceRead>, s: S) -> core::result::Result<S::Ok, S::Error> {
    use serde::ser::SerializeStruct;
    match v {
        None => s.serialize_none(),
        Some(read) => {
            let mut st = s.serialize_struct("SourceRead", 2)?;
            st.serialize_field("engine", &read.engine)?;
            st.serialize_field("report", &read.report)?;
            st.end()
        }
    }
}

fn io_paths_json<S: Serializer>(p: &IoPaths, s: S) -> core::result::Result<S::Ok, S::Error> {
    use serde::ser::SerializeStruct;
    let mut st = s.serialize_struct("IoPaths", 5)?;
    st.serialize_field("direct_io", &p.direct_io)?;
    st.serialize_field("io_uring", &p.io_uring)?;
    st.serialize_field("gds", &p.gds)?;
    st.serialize_field("pinned", &p.pinned)?;
    st.serialize_field("rdma", &p.rdma)?;
    st.end()
}

/// `GilState` is the contracts' type (d.7); this crate serialises it by name and defines
/// no type of its own (d.1).
pub(crate) fn gil_name(g: GilState) -> &'static str {
    match g {
        GilState::FreeThreaded => "FreeThreaded",
        GilState::Serialised => "Serialised",
    }
}

fn gil_json<S: Serializer>(
    v: &[(StageId, GilState)],
    s: S,
) -> core::result::Result<S::Ok, S::Error> {
    use serde::ser::SerializeSeq;
    let mut seq = s.serialize_seq(Some(v.len()))?;
    for (stage, state) in v {
        seq.serialize_element(&(stage, gil_name(*state)))?;
    }
    seq.end()
}

/// How a note that judged the peak across a limits change or a drain begins; `Display` shows
/// it beside the memory line (04 f.4, amended 2026-09-30).
pub const PEAK_WINDOW_NOTE: &str = "the peak rose between";

/// A record's place in the walk: `(t_start_ns, seq)`, which orders the records of one stage
/// the same way whichever order the chunks were read in.
type OrderKey = (u64, u64);
/// The first non-zero `state_bytes` one instance reported and its last one, each with the
/// record's order key (f.2, state growth).
type StateSpan = (Option<(OrderKey, u64)>, Option<(OrderKey, u64)>);

/// One stage's running totals while the trace is walked.
#[derive(Default)]
struct StageAcc {
    morsels: u64,
    rows_in: u64,
    rows_out: u64,
    bytes_in: u64,
    bytes_out: u64,
    t_min: Option<u64>,
    t_max: u64,
    busy_ns: u64,
    amplification: Vec<f64>,
    miss_us: u64,
    errors: u64,
    skipped: u64,
    state_max: u64,
    /// Per instance: the first non-zero `state_bytes` and the last one, each with the
    /// record's order key so the walk is independent of the order the chunks are read in.
    state: BTreeMap<u16, StateSpan>,
    alloc: crate::alloc::AllocAcc,
}

impl StageAcc {
    fn add(&mut self, r: &TraceRecord) {
        self.morsels += 1;
        self.rows_in += r.rows_in;
        self.rows_out += r.rows_out;
        self.bytes_in += r.bytes_in;
        self.bytes_out += r.bytes_out;
        self.t_min = Some(match self.t_min {
            Some(t) => t.min(r.t_start_ns),
            None => r.t_start_ns,
        });
        self.t_max = self.t_max.max(r.t_end_ns);
        if matches!(r.outcome, Outcome::Ok | Outcome::Probe) {
            self.busy_ns += r.t_end_ns.saturating_sub(r.t_start_ns);
        }
        if r.bytes_in > 0 {
            let delta = r.mem_anon_peak.saturating_sub(r.mem_anon_before);
            self.amplification.push(delta as f64 / r.bytes_in as f64);
        }
        self.miss_us += r.placement_miss_wait_us;
        match r.outcome {
            Outcome::Error => self.errors += 1,
            Outcome::Skipped => self.skipped += 1,
            Outcome::Ok | Outcome::Probe => {}
        }
        self.state_max = self.state_max.max(r.state_bytes);
        self.alloc.add(r);
        let key = (r.t_start_ns, r.seq);
        let entry = self.state.entry(r.instance).or_default();
        if r.state_bytes > 0 && entry.0.is_none_or(|(k, _)| key < k) {
            entry.0 = Some((key, r.state_bytes));
        }
        if entry.1.is_none_or(|(k, _)| key > k) {
            entry.1 = Some((key, r.state_bytes));
        }
    }

    fn finish(self, stage: StageId) -> StageReport {
        let wall_s = match self.t_min {
            Some(t) => (self.t_max.saturating_sub(t)) as f64 / 1e9,
            None => 0.0,
        };
        let per_s = |n: u64| if wall_s > 0.0 { n as f64 / wall_s } else { 0.0 };
        let mut amp = self.amplification;
        amp.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
        let growth = self
            .state
            .values()
            .map(|(first, last)| match (first, last) {
                (Some((_, f)), Some((_, l))) => *l as i64 - *f as i64,
                _ => 0,
            })
            .max()
            .unwrap_or(0);
        StageReport {
            stage,
            morsels: self.morsels,
            rows_in: self.rows_in,
            rows_out: self.rows_out,
            bytes_in: self.bytes_in,
            bytes_out: self.bytes_out,
            wall_s,
            kernel_busy_s: self.busy_ns as f64 / 1e9,
            rows_per_s: per_s(self.rows_out),
            bytes_per_s: per_s(self.bytes_out),
            amplification_p50: percentile(&amp, 50),
            amplification_p95: percentile(&amp, 95),
            placement_miss_wait_s: self.miss_us as f64 / 1e6,
            errors: self.errors,
            skipped: self.skipped,
            state_bytes_max: self.state_max,
            state_growth: growth,
            alloc: self.alloc.finish(),
        }
    }
}

/// Nearest rank: the smallest value at or above which `p` percent of the sample lies.
fn percentile(sorted: &[f64], p: u32) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((p as f64 / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted[rank.min(sorted.len()) - 1]
}

impl RunReport {
    /// TR-I3. Deterministic in `(trace, limits, meta)`: two calls on the same inputs
    /// produce equal structs, and no field is computed anywhere but here (l).
    pub fn compute(trace: &TraceView, limits: &Limits, meta: &RunMeta) -> RunReport {
        let mut stages: BTreeMap<StageId, StageAcc> = BTreeMap::new();
        let mut peak_anon = 0u64;
        let mut peak_at = 0u64;
        // Where the peak could have been reached from: a record's peak anywhere in its
        // `apply`, a kernel's mark anywhere since the sampler last read it (04 f.4).
        let mut peak_since = 0u64;
        let mut throttled_us = 0u64;
        let mut staging_written = 0u64;
        let mut staging_abs = 0u128;
        // `(t_start, seq, stage, active workers)`: the piecewise constant worker signal,
        // sorted so the time weighting is independent of the order chunks are read in.
        let mut workers: Vec<(u64, u64, u16, u16)> = Vec::new();
        let mut t_end_max = 0u64;

        for batch in trace.batches() {
            for row in 0..batch.num_rows() {
                let Some(r) = crate::view::record_at(&batch, row) else {
                    continue;
                };
                if r.mem_anon_peak > peak_anon {
                    peak_anon = r.mem_anon_peak;
                    peak_at = r.t_end_ns;
                    peak_since = r.t_start_ns;
                }
                throttled_us += r.throttled_delta_us;
                staging_written += r.staging_bytes_delta.max(0) as u64;
                staging_abs += r.staging_bytes_delta.unsigned_abs() as u128;
                workers.push((r.t_start_ns, r.seq, r.stage, r.knob_active_workers));
                t_end_max = t_end_max.max(r.t_end_ns);
                stages.entry(r.stage).or_default().add(&r);
            }
        }

        let wall_s = (meta.end_ns.saturating_sub(meta.start_ns)) as f64 / 1e9;
        let stage_reports: Vec<StageReport> = stages
            .into_iter()
            .map(|(stage, acc)| acc.finish(stage))
            .collect();

        let busy_total: f64 = stage_reports.iter().map(|s| s.kernel_busy_s).sum();
        let mean_workers = mean_active_workers(&mut workers, t_end_max);
        let worker_busy_fraction = if wall_s > 0.0 && mean_workers > 0.0 {
            busy_total / (wall_s * mean_workers)
        } else {
            0.0
        };
        let cpu_throttled_fraction = if wall_s > 0.0 && limits.cpu_quota > 0.0 {
            throttled_us as f64 / (wall_s * limits.cpu_quota * 1e6)
        } else {
            0.0
        };
        let stage_one = stage_reports.iter().find(|s| s.stage == 1);
        let source_bytes_per_s = stage_one.map_or(0.0, |s| {
            if s.wall_s > 0.0 {
                s.bytes_in as f64 / s.wall_s
            } else {
                0.0
            }
        });
        let source_bandwidth = match (stage_one, wall_s > 0.0) {
            (Some(s), true) => s.bytes_in as f64 / wall_s,
            _ => 0.0,
        };
        let staging_bandwidth = if wall_s > 0.0 {
            staging_abs as f64 / wall_s
        } else {
            0.0
        };
        // The process's own high-water mark is the peak; a record that saw more (a kernel peak
        // inside one `apply`) raises it.
        if meta.process_peak.bytes > peak_anon {
            peak_anon = meta.process_peak.bytes;
            peak_at = meta.process_peak.at_ns;
            peak_since = if meta.process_peak.exact {
                meta.process_peak.since_ns
            } else {
                peak_at
            };
        }
        // The ceiling in force at the peak, which is the initial one unless the machine
        // changed before it.
        let mut changes = trace.limits_changes();
        changes.sort_by_key(|change| change.at_ns);
        let latest = changes.iter().rev().find(|change| change.at_ns <= peak_at);
        let mut peak_ceiling_bytes =
            latest.map_or(limits.memory_ceiling, |change| change.new.memory_ceiling);
        // Unless the peak came while the arena was still giving memory back after a lowered
        // ceiling. That memory was taken under the ceiling before the change: Moruna never
        // allocates past the ceiling in force, and returns what it holds as the buffers in it
        // are freed, which the hosted design accepts takes as long as it takes (MH 4.4, 7.1).
        // Judging such a peak against the lowered ceiling reported a run that had done
        // nothing wrong at 1.78 of its ceiling, and whether it did depended on whether the
        // peak sample landed just before the change or just after it (2026-09-29).
        let peak_ms = peak_at.saturating_sub(meta.start_ns) / 1_000_000;
        let draining_at_peak = meta.drains.iter().any(|drain| {
            drain.at_ms <= peak_ms && drain.drain_ms.is_none_or(|ms| peak_ms <= drain.at_ms + ms)
        });
        if let Some(change) = latest
            && draining_at_peak
            && change.new.memory_ceiling < change.old.memory_ceiling
        {
            peak_ceiling_bytes = change.old.memory_ceiling;
        }
        // A peak is stamped with the end of the window it could have been reached in: a kernel's
        // own mark anywhere since the sampler last read it, a record's peak anywhere in its
        // `apply`. When the ceiling moved, or the arena began a drain, inside the window, the peak
        // may have been reached under any ceiling in force in it; it is judged against the highest
        // of them, and the report says so, naming both ceilings and the window, so a real breach
        // is never absorbed without a word. A sample's own reading has no window and is judged as
        // before. Measured: a machine lowered from 809 MB to 272 MB whose grown
        // region was empty and unmapped at once still had its 494 MB mark stamped 8 ms after the
        // lowering, and the run was reported at 1.82 of a ceiling it never ran under (2026-09-30).
        let mut window_note = None;
        let since = peak_since.min(peak_at);
        if since < peak_at {
            let ms = |ns: u64| ns.saturating_sub(meta.start_ns) / 1_000_000;
            let (since_ms, at_ms) = (ms(since), ms(peak_at));
            let inside: Vec<_> = changes
                .iter()
                .filter(|change| change.at_ns > since && change.at_ns <= peak_at)
                .collect();
            let drains_inside: Vec<_> = meta
                .drains
                .iter()
                .filter(|drain| drain.at_ms >= since_ms && drain.at_ms <= at_ms)
                .collect();
            if !inside.is_empty() || !drains_inside.is_empty() {
                let at_start = changes
                    .iter()
                    .rev()
                    .find(|change| change.at_ns <= since)
                    .map_or(limits.memory_ceiling, |change| change.new.memory_ceiling);
                let highest = inside
                    .iter()
                    .map(|change| change.new.memory_ceiling)
                    .fold(at_start, u64::max);
                if highest > peak_ceiling_bytes {
                    let mut across: Vec<String> = inside
                        .iter()
                        .map(|change| {
                            format!(
                                "the ceiling's move from {} to {} bytes at {} ms",
                                change.old.memory_ceiling,
                                change.new.memory_ceiling,
                                ms(change.at_ns)
                            )
                        })
                        .collect();
                    across.extend(drains_inside.iter().map(|drain| {
                        format!("a drain of {} bytes at {} ms", drain.bytes, drain.at_ms)
                    }));
                    window_note = Some(format!(
                        "{PEAK_WINDOW_NOTE} {since_ms} and {at_ms} ms, across {}; it is judged \
                         against the {highest} byte ceiling in force within that window, not the \
                         {peak_ceiling_bytes} byte ceiling in force at {at_ms} ms",
                        across.join(" and ")
                    ));
                    peak_ceiling_bytes = highest;
                }
            }
        }
        let peak_fraction_of_ceiling = if peak_ceiling_bytes > 0 {
            peak_anon as f64 / peak_ceiling_bytes as f64
        } else {
            0.0
        };
        let limits_timeline = changes
            .iter()
            .map(|change| {
                (
                    change.at_ns.saturating_sub(meta.start_ns) / 1_000_000,
                    LimitsSummary::of(&change.new),
                )
            })
            .collect();
        let mut notes = meta.notes.clone();
        // First, so that it is never among the notes a rendering leaves out.
        if let Some(note) = window_note {
            notes.insert(0, note);
        }
        if meta.process_peak.bytes > 0 {
            notes.push(if meta.memory_basis == moruna_kernel::MemoryBasis::NativeAccountCharge {
                "the peak is observed native Account charge pressure, not process RSS or a kernel interval mark".into()
            } else if meta.process_peak.exact {
                "the peak is the operating system's own high-water mark of the process".into()
            } else {
                "the peak is the highest of the samples the run took of the process".into()
            });
        }
        notes.extend(meta.controller_notes.iter().cloned());

        RunReport {
            run_id: run_id_hex(meta.run_id),
            exit: meta.exit.clone(),
            resumed: meta.resumed,
            manifest: meta.manifest.as_ref().map(|p| p.display().to_string()),
            wall_s,
            limits_initial: LimitsSummary::of(limits),
            limits_timeline,
            drains: meta.drains.clone(),
            io_paths: meta.io_paths.clone(),
            peak_anon_bytes: peak_anon,
            memory_basis: (meta.memory_basis != moruna_kernel::MemoryBasis::ProcessAnon)
                .then(|| meta.memory_basis.as_str().to_string()),
            cpu_ns: meta.process_usage.cpu_ns,
            mem_byte_seconds: meta.process_usage.mem_byte_seconds,
            usage_measured: meta.process_usage.measured,
            peak_fraction_of_ceiling,
            peak_ceiling_bytes,
            worker_busy_fraction,
            cpu_throttled_fraction,
            source_bytes_per_s,
            source_bandwidth,
            staging_bandwidth,
            staging_bytes_written: staging_written,
            staging_engaged: staging_written > 0,
            gil: meta.gil.clone(),
            gil_serialised: meta
                .gil
                .iter()
                .any(|(_, g)| matches!(g, GilState::Serialised)),
            sizer_used: meta.sizer.to_string(),
            sizer_fallback_at: meta.sizer_fallback_at,
            bottleneck_timeline: meta.bottleneck_timeline.clone(),
            stages: stage_reports,
            notes,
            overflow_failed: trace.overflow_failed(),
            late_records: trace.late_records(),
            write: meta.write.clone(),
            read: meta.read.clone(),
            buffers: meta.buffers.clone(),
            policy: meta.policy.clone(),
        }
    }

    /// The report as JSON, one object with the fields of `RunReport` (d.1).
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|e| {
            format!("{{\"error\":\"the run report could not be rendered as JSON: {e}\"}}")
        })
    }
}

/// The time-weighted mean of `knob_active_workers` over the run (f.2, "mean N is
/// time-weighted from the records"). The signal is piecewise constant: it takes the value
/// of the latest record started at or before each instant, over `[min t_start, max t_end]`.
fn mean_active_workers(samples: &mut [(u64, u64, u16, u16)], t_end_max: u64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    samples.sort_unstable();
    let mut weighted = 0f64;
    let mut total = 0f64;
    for i in 0..samples.len() {
        let start = samples[i].0;
        let end = if i + 1 < samples.len() {
            samples[i + 1].0
        } else {
            t_end_max.max(start)
        };
        let w = (end.saturating_sub(start)) as f64;
        weighted += w * samples[i].3 as f64;
        total += w;
    }
    if total > 0.0 {
        weighted / total
    } else {
        // Every record started at the same instant: the mean is the mean of their values.
        let sum: f64 = samples.iter().map(|s| s.3 as f64).sum();
        sum / samples.len() as f64
    }
}
