//! The profile store (e.3, f.9).
//!
//! One JSON file per kernel fingerprint and input schema hash, advisory only: it seeds the
//! amplification and the margin, and the probe overrides it whenever the two disagree by more
//! than half (f.2). It is the controller's memory across runs, and on a resumed run it is its
//! memory of the run that was interrupted (f.14), which is why it is written periodically and
//! not only at the end.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use moruna_kernel::Fingerprint;
use serde::{Deserialize, Serialize};

use crate::{Inner, PROFILE_WRITE_AFTER};

/// The format version this build writes and reads (e.3).
pub(crate) const VERSION: u32 = 1;
/// The weight a new run's figures get when they are merged into the stored ones (e.3).
const NEW_RUN_WEIGHT: f64 = 0.3;
/// The z score of the 95th percentile of a normal distribution, which is how p95 is derived
/// from the running mean and variance (see `from_stage`).
const Z95: f64 = 1.645;

/// Distinguishes the temporary files of concurrent writers in one process.
static TEMPS: AtomicU64 = AtomicU64::new(0);

/// One kernel's stored behaviour (e.3).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Profile {
    /// The format version; an unknown one is ignored with a note.
    pub version: u32,
    /// The kernel's fingerprint, as 64 lowercase hex characters.
    pub fingerprint: String,
    /// The input schema's hash, as 64 lowercase hex characters.
    pub schema_hash: String,
    /// When the file was last written, RFC 3339 in UTC.
    pub updated: String,
    /// The median of `peak_delta / bytes_in`.
    pub a_k_p50: f64,
    /// Its 95th percentile, which is what seeds the amplification.
    pub a_k_p95: f64,
    /// The device equivalent; zero for a kernel that allocates no device memory.
    pub a_k_dev_p95: f64,
    /// Trace records the statistics were computed from, summed across runs.
    pub a_k_samples: u64,
    /// The running variance of `peak_delta / bytes_in`, merged across runs.
    pub a_k_var: f64,
    /// The largest `state_bytes` any instance reported.
    pub state_bytes_max: u64,
    /// The morsel target the run ended at.
    pub final_target: u64,
    /// The active worker count the run ended at.
    pub final_workers: u16,
    /// The safety multiplier the run ended at.
    pub final_safety: f32,
    /// How many runs have contributed.
    pub runs: u32,
    /// The 95th percentile of the active sizer's prediction error (f.8).
    pub prediction_error_p95: f64,
}

/// What the store had for one stage.
pub(crate) enum Loaded {
    /// A usable profile.
    Found(Profile),
    /// No file.
    Missing,
    /// A file that could not be read, parsed or understood; the string says which.
    Unreadable(String),
}

/// The file one kernel's profile lives in (e.3).
pub(crate) fn path(dir: &Path, fingerprint: &Fingerprint, schema_hash: &[u8; 32]) -> PathBuf {
    dir.join(format!(
        "{}-{}.json",
        fingerprint.to_hex(),
        hex(schema_hash)
    ))
}

fn hex(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Read one profile. A missing file is not an error; a corrupt one is ignored with a reason
/// (h, failures), because a stale opinion is never worth failing a run over.
pub(crate) fn load(dir: &Path, fingerprint: &Fingerprint, schema_hash: &[u8; 32]) -> Loaded {
    let file = path(dir, fingerprint, schema_hash);
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Loaded::Missing,
        Err(err) => return Loaded::Unreadable(format!("{err}")),
    };
    let profile: Profile = match serde_json::from_str(&text) {
        Ok(profile) => profile,
        Err(err) => return Loaded::Unreadable(format!("corrupt: {err}")),
    };
    if profile.version != VERSION {
        return Loaded::Unreadable(format!("unknown version {}", profile.version));
    }
    Loaded::Found(profile)
}

/// Merge a new run's figures into the stored ones with the exponential weight of e.3, so a
/// long history moves slowly and a first run is taken as it stands.
pub(crate) fn merge(stored: &Profile, fresh: &Profile) -> Profile {
    let blend = |old: f64, new: f64| old * (1.0 - NEW_RUN_WEIGHT) + new * NEW_RUN_WEIGHT;
    Profile {
        version: VERSION,
        fingerprint: fresh.fingerprint.clone(),
        schema_hash: fresh.schema_hash.clone(),
        updated: fresh.updated.clone(),
        a_k_p50: blend(stored.a_k_p50, fresh.a_k_p50),
        a_k_p95: blend(stored.a_k_p95, fresh.a_k_p95),
        a_k_dev_p95: blend(stored.a_k_dev_p95, fresh.a_k_dev_p95),
        // Evidence adds up; it is not averaged. The sample count is what buys the margin down
        // in f.2, and a second run of the same kernel is more evidence, not fresher evidence.
        a_k_samples: stored.a_k_samples.saturating_add(fresh.a_k_samples),
        a_k_var: blend(stored.a_k_var, fresh.a_k_var),
        state_bytes_max: stored.state_bytes_max.max(fresh.state_bytes_max),
        final_target: fresh.final_target,
        final_workers: fresh.final_workers,
        final_safety: fresh.final_safety,
        runs: stored.runs.saturating_add(1),
        prediction_error_p95: blend(stored.prediction_error_p95, fresh.prediction_error_p95),
    }
}

/// Write one profile atomically: a temporary file named for this process, then a rename, so a
/// reader never sees half a file and two runs on one machine never share a path.
pub(crate) fn write(dir: &Path, profile: &Profile, file: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let counter = TEMPS.fetch_add(1, Ordering::Relaxed);
    let temp = dir.join(format!(
        "{}.tmp.{}.{counter}",
        file.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("profile.json"),
        std::process::id()
    ));
    let text = serde_json::to_string_pretty(profile)
        .map_err(|err| std::io::Error::other(format!("{err}")))?;
    std::fs::write(&temp, text)?;
    std::fs::rename(&temp, file)
}

/// The profile one stage's measurements add up to.
///
/// The percentiles come from the running mean and variance rather than from a kept sample:
/// p50 is the mean and p95 is the mean plus 1.645 standard deviations, so the stored figures
/// are a function of every record the run saw rather than of the last thirty-two, at the price
/// of assuming the ratio is roughly normal. The price is the right one to pay because what the
/// number is used for is a margin, and a margin that is slightly wrong in the safe direction
/// costs throughput, not correctness.
pub(crate) fn from_stage(state: &crate::ControllerState, at: usize) -> Option<Profile> {
    let ctl = state.stages.get(at)?;
    let kernel = state.kernels.iter().find(|k| k.stage == ctl.stage)?;
    let mean = if ctl.var_count > 0 {
        ctl.var_mean
    } else {
        ctl.a_k
    };
    let variance = if ctl.var_count > 1 {
        ctl.var_m2 / (ctl.var_count - 1) as f64
    } else {
        0.0
    };
    let p95 = (mean + Z95 * variance.max(0.0).sqrt()).max(mean);
    Some(Profile {
        version: VERSION,
        fingerprint: kernel.fingerprint.to_hex(),
        schema_hash: hex(&kernel.schema_hash),
        updated: rfc3339(now_secs()),
        a_k_p50: mean,
        a_k_p95: p95,
        a_k_dev_p95: ctl.a_k_dev,
        a_k_samples: ctl.var_count,
        a_k_var: variance,
        state_bytes_max: ctl.state_by_instance.values().copied().max().unwrap_or(0),
        final_target: ctl.target,
        final_workers: state.active_workers,
        final_safety: ctl.safety,
        runs: 1,
        prediction_error_p95: percentile(&ctl.sizer_errors, 0.95),
    })
}

/// The p-th percentile of a window, nearest rank; zero for an empty window.
pub(crate) fn percentile(window: &std::collections::VecDeque<f64>, p: f64) -> f64 {
    if window.is_empty() {
        return 0.0;
    }
    let mut sorted: Vec<f64> = window.iter().copied().collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let rank = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

/// Write every stage's profile, merging with what the store already has (f.9). Notes, not
/// errors: a store that cannot be written costs the next run its head start and nothing else.
pub(crate) fn write_all(ctl: &Inner) {
    let plan = {
        let state = ctl.held();
        let Some(dir) = state.cfg.profiles_dir.clone() else {
            return;
        };
        let mut plan = Vec::new();
        for at in 0..state.stages.len() {
            if state.stages[at].records < PROFILE_WRITE_AFTER && state.stages[at].var_count == 0 {
                continue;
            }
            let Some(fresh) = from_stage(&state, at) else {
                continue;
            };
            let Some(kernel) = state
                .kernels
                .iter()
                .find(|k| k.stage == state.stages[at].stage)
            else {
                continue;
            };
            plan.push((
                path(&dir, &kernel.fingerprint, &kernel.schema_hash),
                state.stages[at].profile.clone(),
                fresh,
            ));
        }
        (plan, dir)
    };
    let (plan, dir) = plan;
    let mut failures = Vec::new();
    for (file, stored, fresh) in plan {
        let merged = match &stored {
            Some(stored) => merge(stored, &fresh),
            None => fresh,
        };
        if let Err(err) = write(&dir, &merged, &file) {
            failures.push(format!("{err}"));
        }
    }
    if let Some(first) = failures.first() {
        let mut state = ctl.held();
        state.note(format!("profile store not written: {first}"));
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// Seconds since the epoch as an RFC 3339 timestamp in UTC. Written out here rather than
/// taken from a crate because the dependency table has no date library and one field of one
/// advisory file does not justify an addition to it (preamble 6.2, E2).
pub(crate) fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let time = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time / 3600,
        (time % 3600) / 60,
        time % 60
    )
}

/// Howard Hinnant's days-to-civil algorithm, for days since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}
