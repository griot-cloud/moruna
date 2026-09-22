//! The run report: a pure function of the trace, the discovered limits and the run meta
//! (d.1, f.2, TR-I3, G-I4). Every report field is computed here and nowhere else.

use std::collections::BTreeMap;
use std::path::PathBuf;

use amoru_kernel::{GilState, IoPaths, Limits, Outcome, RunId, Seq, StageId, TraceRecord};
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
}

/// One accelerator, as the report names it.
#[derive(Clone, Debug, Serialize)]
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
#[derive(Clone, Debug, Serialize)]
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
    /// The discovered limits.
    pub limits: LimitsSummary,
    /// The direct paths taken (G-I7).
    #[serde(serialize_with = "io_paths_json")]
    pub io_paths: IoPaths,
    /// The largest `mem_anon_peak` any record saw.
    pub peak_anon_bytes: u64,
    /// That over `limits.memory_ceiling` (S1).
    pub peak_fraction_of_ceiling: f64,
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
                peak_anon = peak_anon.max(r.mem_anon_peak);
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
        let peak_fraction_of_ceiling = if limits.memory_ceiling > 0 {
            peak_anon as f64 / limits.memory_ceiling as f64
        } else {
            0.0
        };
        let mut notes = meta.notes.clone();
        notes.extend(meta.controller_notes.iter().cloned());

        RunReport {
            run_id: run_id_hex(meta.run_id),
            exit: meta.exit.clone(),
            resumed: meta.resumed,
            manifest: meta.manifest.as_ref().map(|p| p.display().to_string()),
            wall_s,
            limits: LimitsSummary::of(limits),
            io_paths: meta.io_paths.clone(),
            peak_anon_bytes: peak_anon,
            peak_fraction_of_ceiling,
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
