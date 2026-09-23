//! The limits derivation (e.3), the Databricks refusal (DS-I8) and the host tier decision (f.1).

use std::path::Path;

use moruna_kernel::{Device, Guarantee, LimitSource, Limits, TierKind};

use crate::cgroup::{self, Cgroup};
use crate::os;

/// The low end of `budget.host`'s range (preamble section 5).
pub(crate) const BUDGET_MIN: u64 = 256 * 1024 * 1024;

/// The fraction of the kill line the ceiling must stay at or below, so that the ceiling is below
/// the kill line by at least 5% of it (DS-I2).
const KILL_FRACTION: f64 = 0.95;

/// The fraction of a limit taken as the ceiling when the platform declares no `memory.high`.
const CEILING_FRACTION: f64 = 0.9;

/// What the derivation needs beyond the cgroup.
pub(crate) struct LimitsInput<'a> {
    /// A budget from the constructor or `MORUNA_BUDGET` (DS-I1).
    pub(crate) explicit_budget: Option<u64>,
    /// A CPU quota from the constructor or `MORUNA_CPU` (DS-I1).
    pub(crate) explicit_cpu: Option<f64>,
    /// The cgroup this process is in, if any.
    pub(crate) cgroup: &'a Cgroup,
    /// Where `/proc` is (a fixture directory in tests).
    pub(crate) proc_root: &'a Path,
    /// Every accelerator discovery enumerated.
    pub(crate) devices: Vec<Device>,
}

/// Derive `Limits` (e.3), appending a note for every fallback and every clamp.
pub(crate) fn derive(input: LimitsInput<'_>, notes: &mut Vec<String>) -> Limits {
    let page_bytes = os::page_bytes();
    let total_ram = os::total_ram_bytes(input.proc_root);
    let cgroup_limits = cgroup::limits(input.cgroup);
    let kill = cgroup_limits.memory_max;

    // `memory.high` above `memory.max` is a misconfiguration: ignore it and say so (h).
    let high = match (cgroup_limits.memory_high, kill) {
        (Some(high), Some(kill)) if high > kill => {
            notes.push(format!(
                "memory.high ({high}) is above memory.max ({kill}); ignoring memory.high"
            ));
            None
        }
        (high, _) => high,
    };

    let (mut ceiling, source) = if let Some(budget) = input.explicit_budget {
        (budget, LimitSource::Explicit)
    } else if let Some(high) = high {
        (high, LimitSource::Cgroup)
    } else if let Some(kill) = kill {
        notes.push("memory.high absent; using 0.9 x memory.max".to_string());
        (fraction(kill, CEILING_FRACTION), LimitSource::Cgroup)
    } else if let Some(total) = total_ram {
        notes.push("no cgroup memory limit; using 0.9 x total RAM".to_string());
        (fraction(total, CEILING_FRACTION), LimitSource::Os)
    } else {
        notes.push(format!(
            "neither a cgroup limit nor total RAM could be read; using the minimum budget of {BUDGET_MIN} bytes"
        ));
        (BUDGET_MIN, LimitSource::Os)
    };

    // The range of `budget.host` (preamble section 5), clamped here because discovery owns it.
    let range_top = kill.or(total_ram);
    if ceiling < BUDGET_MIN {
        clamp_note(
            notes,
            format!("budget.host {ceiling} is below the range minimum {BUDGET_MIN}; clamped up"),
        );
        ceiling = BUDGET_MIN;
    }
    if let Some(top) = range_top
        && ceiling > top
    {
        clamp_note(
            notes,
            format!("budget.host {ceiling} is above the host's {top} bytes; clamped down"),
        );
        ceiling = top;
    }

    // DS-I2, applied last so it holds whatever the range clamp did.
    if let Some(kill) = kill {
        let bound = fraction(kill, KILL_FRACTION);
        if ceiling > bound {
            clamp_note(
                notes,
                format!(
                    "budget.host {ceiling} is not below the kill line {kill}; clamped to {bound} (0.95 x memory.max)"
                ),
            );
            ceiling = bound;
        }
    }

    let cpu_quota = if let Some(cpu) = input.explicit_cpu {
        cpu
    } else if let Some(quota) = cgroup::cpu_quota(input.cgroup) {
        quota
    } else {
        notes.push("no cgroup CPU quota; using the host's logical core count".to_string());
        os::logical_cores()
    };

    if let Some(current) = cgroup::memory_current(input.cgroup) {
        notes.push(format!("memory.current at discovery: {current}"));
    }

    let limits = Limits {
        memory_ceiling: ceiling,
        memory_kill: kill,
        cpu_quota,
        page_bytes,
        devices: input.devices,
        source,
    };
    tracing::info!(
        target: "discovery.limits",
        memory_ceiling = limits.memory_ceiling,
        memory_kill = limits.memory_kill,
        cpu_quota = limits.cpu_quota,
        page_bytes = limits.page_bytes,
        devices = limits.devices.len(),
        source = ?limits.source,
        "limits discovered"
    );
    limits
}

/// The run's one host tier (contracts e.1, f.1): pinned when there is a device to copy to and
/// `memlock` is available, ordinary host memory otherwise.
pub(crate) fn host_tier(
    devices: &[Device],
    memlock: Guarantee,
    notes: &mut Vec<String>,
) -> TierKind {
    if devices.is_empty() {
        return TierKind::Host;
    }
    if memlock.is_available() {
        TierKind::PinnedHost
    } else {
        notes.push(
            "a device is present but memlock is not available; the host tier is Host and device copies bounce (06 e.2)"
                .to_string(),
        );
        TierKind::Host
    }
}

/// `value x fraction`, saturating rather than wrapping.
fn fraction(value: u64, fraction: f64) -> u64 {
    let scaled = value as f64 * fraction;
    if scaled >= u64::MAX as f64 {
        u64::MAX
    } else {
        scaled as u64
    }
}

/// Record a clamp in the notes and on the `discovery.clamp` event (j).
fn clamp_note(notes: &mut Vec<String>, note: String) {
    tracing::warn!(target: "discovery.clamp", note = %note, "limit clamped");
    notes.push(note);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cgroup::fixtures::{TempDir, cgroup_v2, proc_root, write};
    use moruna_kernel::DeviceId;

    fn device() -> Device {
        Device {
            id: DeviceId(0),
            total_bytes: 8 * 1024 * 1024 * 1024,
            free_bytes: 7 * 1024 * 1024 * 1024,
            name: "fake device".to_string(),
        }
    }

    fn proc_with_ram(dir: &std::path::Path, kib: u64) -> std::path::PathBuf {
        let root = dir.join("proc-ram");
        write(&root, "meminfo", &format!("MemTotal: {kib} kB\n"));
        root
    }

    /// DS-T2 ceiling_below_kill: for kill lines from 256 MiB to 1 TiB and every `memory.high`
    /// variant, `ceiling <= 0.95 x kill`. DS-I2.
    #[test]
    fn ds_t2_ceiling_below_kill() {
        let tmp = TempDir::new("ds-t2");
        let proc_dir = proc_with_ram(tmp.path(), 64 * 1024 * 1024);
        let mib = 1024u64 * 1024;
        let kills = [
            256 * mib,
            512 * mib,
            1024 * mib,
            2 * 1024 * mib,
            8 * 1024 * mib,
            64 * 1024 * mib,
            1024 * 1024 * mib,
        ];
        for kill in kills {
            for high in [
                "max",
                "1",
                "268435456",
                &format!("{}", kill / 2),
                &format!("{}", kill * 2),
            ] {
                for explicit in [None, Some(kill), Some(kill * 4), Some(1024u64)] {
                    let root = cgroup_v2(tmp.path(), &kill.to_string(), high, "max 100000");
                    let proc_self = proc_root(tmp.path(), "0::/\n");
                    let found = crate::cgroup::locate(&root, &proc_self);
                    let mut notes = Vec::new();
                    let limits = derive(
                        LimitsInput {
                            explicit_budget: explicit,
                            explicit_cpu: None,
                            cgroup: &found,
                            proc_root: &proc_dir,
                            devices: Vec::new(),
                        },
                        &mut notes,
                    );
                    assert_eq!(limits.memory_kill, Some(kill));
                    let bound = fraction(kill, KILL_FRACTION);
                    assert!(
                        limits.memory_ceiling <= bound,
                        "kill {kill} high {high} explicit {explicit:?}: ceiling {} > {bound}",
                        limits.memory_ceiling
                    );
                    // DS-I2: strictly below the kill line by at least 5% of it.
                    assert!(kill - limits.memory_ceiling >= kill / 20);
                }
            }
        }
    }

    /// h, normal path (pod): 8 GiB kill, no `memory.high`, quota 4.0, source Cgroup.
    #[test]
    fn pod_normal_path() {
        let tmp = TempDir::new("pod");
        let gib = 1024u64 * 1024 * 1024;
        let root = cgroup_v2(tmp.path(), &(8 * gib).to_string(), "max", "400000 100000");
        let proc_self = proc_root(tmp.path(), "0::/\n");
        let found = crate::cgroup::locate(&root, &proc_self);
        let mut notes = Vec::new();
        let limits = derive(
            LimitsInput {
                explicit_budget: None,
                explicit_cpu: None,
                cgroup: &found,
                proc_root: &proc_with_ram(tmp.path(), 64 * 1024 * 1024),
                devices: Vec::new(),
            },
            &mut notes,
        );
        assert_eq!(limits.memory_ceiling, fraction(8 * gib, 0.9));
        assert_eq!(limits.memory_kill, Some(8 * gib));
        assert!((limits.cpu_quota - 4.0).abs() < f64::EPSILON);
        assert_eq!(limits.source, LimitSource::Cgroup);
        assert!(notes.iter().any(|note| note.contains("memory.high absent")));
    }

    /// h, edge cases: `memory.high` above `memory.max` is ignored with a note; a quota below one
    /// core stands; an explicit budget above the kill line is clamped with a warning.
    #[test]
    fn edge_cases_of_the_derivation() {
        let tmp = TempDir::new("edges");
        let gib = 1024u64 * 1024 * 1024;
        let root = cgroup_v2(
            tmp.path(),
            &(2 * gib).to_string(),
            &(4 * gib).to_string(),
            "50000 100000",
        );
        let proc_self = proc_root(tmp.path(), "0::/\n");
        let found = crate::cgroup::locate(&root, &proc_self);
        let mut notes = Vec::new();
        let limits = derive(
            LimitsInput {
                explicit_budget: Some(16 * gib),
                explicit_cpu: None,
                cgroup: &found,
                proc_root: &proc_with_ram(tmp.path(), 64 * 1024 * 1024),
                devices: Vec::new(),
            },
            &mut notes,
        );
        assert!(
            notes
                .iter()
                .any(|note| note.contains("ignoring memory.high"))
        );
        assert!((limits.cpu_quota - 0.5).abs() < f64::EPSILON);
        assert_eq!(limits.memory_ceiling, fraction(2 * gib, KILL_FRACTION));
        assert!(
            notes
                .iter()
                .any(|note| note.contains("kill line") && note.contains("clamped"))
        );
        assert_eq!(limits.source, LimitSource::Explicit);
    }

    /// e.3: with no cgroup the ceiling is 0.9 x total RAM and the source is Os; with neither,
    /// the minimum budget stands with a note.
    #[test]
    fn os_fallback_and_last_resort() {
        let tmp = TempDir::new("os");
        let mut notes = Vec::new();
        let limits = derive(
            LimitsInput {
                explicit_budget: None,
                explicit_cpu: None,
                cgroup: &Cgroup::None,
                proc_root: &proc_with_ram(tmp.path(), 16 * 1024 * 1024),
                devices: Vec::new(),
            },
            &mut notes,
        );
        assert_eq!(limits.source, LimitSource::Os);
        assert_eq!(
            limits.memory_ceiling,
            fraction(16 * 1024 * 1024 * 1024, 0.9)
        );
        assert_eq!(limits.memory_kill, None);
        assert!(limits.cpu_quota >= 1.0);

        // A tiny host clamps up to the range minimum.
        let mut notes = Vec::new();
        let limits = derive(
            LimitsInput {
                explicit_budget: Some(1024),
                explicit_cpu: Some(2.0),
                cgroup: &Cgroup::None,
                proc_root: &proc_with_ram(tmp.path(), 16 * 1024 * 1024),
                devices: Vec::new(),
            },
            &mut notes,
        );
        assert_eq!(limits.memory_ceiling, BUDGET_MIN);
        assert!(notes.iter().any(|note| note.contains("range minimum")));
        assert!((limits.cpu_quota - 2.0).abs() < f64::EPSILON);
    }

    /// An explicit budget larger than the host is clamped to the host (preamble section 5 range).
    #[test]
    fn explicit_budget_above_the_host_is_clamped() {
        let tmp = TempDir::new("above-host");
        let mut notes = Vec::new();
        let limits = derive(
            LimitsInput {
                explicit_budget: Some(1024 * 1024 * 1024 * 1024),
                explicit_cpu: None,
                cgroup: &Cgroup::None,
                proc_root: &proc_with_ram(tmp.path(), 16 * 1024 * 1024),
                devices: Vec::new(),
            },
            &mut notes,
        );
        assert_eq!(limits.memory_ceiling, 16 * 1024 * 1024 * 1024);
        assert!(notes.iter().any(|note| note.contains("above the host's")));
    }

    /// Neither a cgroup nor a readable total: the minimum budget with a note.
    #[test]
    fn no_limit_source_at_all() {
        let tmp = TempDir::new("nothing");
        let mut notes = Vec::new();
        let limits = derive(
            LimitsInput {
                explicit_budget: None,
                explicit_cpu: None,
                cgroup: &Cgroup::None,
                proc_root: &tmp.path().join("absent"),
                devices: Vec::new(),
            },
            &mut notes,
        );
        // On a host with a real /proc or a platform interface the fallback answers; the last
        // resort only fires when neither does.
        if notes.iter().any(|note| note.contains("minimum budget")) {
            assert_eq!(limits.memory_ceiling, BUDGET_MIN);
        } else {
            assert!(limits.memory_ceiling >= BUDGET_MIN);
        }
    }

    /// DS-T12 host_tier (f.1): one device with memlock available gives `PinnedHost`; no device,
    /// or memlock unavailable, gives `Host`, and the note names the bounce path.
    #[test]
    fn ds_t12_host_tier() {
        for available in [Guarantee::Present, Guarantee::Probed(true)] {
            let mut notes = Vec::new();
            assert_eq!(
                host_tier(&[device()], available, &mut notes),
                TierKind::PinnedHost
            );
            assert!(notes.is_empty());
        }
        for unavailable in [
            Guarantee::Absent,
            Guarantee::Probed(false),
            Guarantee::Unknown,
        ] {
            let mut notes = Vec::new();
            assert_eq!(
                host_tier(&[device()], unavailable, &mut notes),
                TierKind::Host
            );
            assert_eq!(notes.len(), 1);
            assert!(notes[0].contains("bounce"));
        }
        for any in [
            Guarantee::Present,
            Guarantee::Absent,
            Guarantee::Probed(true),
        ] {
            let mut notes = Vec::new();
            assert_eq!(host_tier(&[], any, &mut notes), TierKind::Host);
            assert!(notes.is_empty());
        }
    }

    /// `fraction` saturates rather than wrapping at the top of the range.
    #[test]
    fn fraction_saturates() {
        assert_eq!(fraction(100, 0.9), 90);
        assert_eq!(fraction(u64::MAX, 2.0), u64::MAX);
    }
}
