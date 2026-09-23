//! Moruna component 3, resource discovery and the host profile (`Limits`, `HostProfile`, `Sampler`).
//!
//! Design: `architecture/sdd/03-discovery.md`. Discovery answers three questions from the host
//! rather than from assumptions: how much memory this process may use, how many CPUs it may use,
//! and what accelerators and fast paths it has. It produces `Limits` and `HostProfile` once at
//! start, the run's one host tier, and `Sample`s for the controller and the scheduler through the
//! contract's `Sampler` trait. It is the only component that reads `/sys/fs/cgroup`, `/proc` or
//! the device list, and it caches nothing across `discover` calls (preamble 1.3).

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
// Every fallible function here returns the contract's `Result<T>`, whose error is `MorunaError`.
// CT-I10 makes that error carry the morsel and its features, so it is large by design; boxing it
// would be a change to `01-contracts.md` (E10), not something this component decides.
#![allow(clippy::result_large_err)]

mod cgroup;
mod devices;
mod env;
mod limits;
mod os;
mod probes;
mod sampler;

use std::path::{Path, PathBuf};

use moruna_kernel::{Guarantee, HostProfile, Limits, Result, TierKind};

use crate::cgroup::Cgroup;
use crate::env::{EnvSource, ProcessEnv, config};
use crate::probes::ProbeReport;

pub use crate::env::parse_profile;
pub use crate::sampler::Sampler;

/// What the caller already knows, and what it wants to override (d.1).
#[derive(Clone, Debug, Default)]
pub struct DiscoveryInput {
    /// A host memory ceiling in bytes, which beats everything discovered (DS-I1).
    pub explicit_budget: Option<u64>,
    /// A CPU quota in cores, which beats everything discovered (DS-I1).
    pub explicit_cpu: Option<f64>,
    /// Where staging segments go, which beats everything discovered (DS-I1).
    pub explicit_staging_dir: Option<PathBuf>,
    /// `budget.disk` in bytes, which beats `MORUNA_SPILL_LIMIT` and the 20% default (f.6).
    pub explicit_spill_limit: Option<u64>,
    /// A host profile from the surface; `MORUNA_HOST_PROFILE` is still parsed underneath and a
    /// field this profile leaves `Unknown` is taken from the variable.
    pub profile_override: Option<HostProfile>,
}

/// What discovery found (d.1).
#[derive(Clone, Debug)]
pub struct Discovered {
    /// The host's limits.
    pub limits: Limits,
    /// The host profile; every field is `Present`, `Absent` or `Probed(_)` (DS-I6).
    pub profile: HostProfile,
    /// The run's one host tier (contracts e.1): `TierKind::PinnedHost` when `limits.devices` is
    /// non-empty and `profile.memlock.is_available()`, else `TierKind::Host`. The facade passes
    /// it to the arena (`ArenaConfig::host_tier`, 02 d.1), which is the owner of `arena.pin`;
    /// nothing else decides pinning.
    pub host_tier: TierKind,
    /// The cgroup directory the figures came from, when there was one.
    pub cgroup_path: Option<PathBuf>,
    /// `budget.disk` (f.6): the explicit limit, else `MORUNA_SPILL_LIMIT`, else 20% of the free
    /// space in `profile.staging_dir`, else 0. The facade hands it to the placement engine; 0
    /// disables the disk tier. Computed here because discovery is the only component that
    /// reads the filesystem's free space.
    pub disk_budget: u64,
    /// Human readable facts for the run report, printed verbatim (j).
    pub notes: Vec<String>,
}

/// Where the host's directories are. Parameters rather than constants so every path in this
/// crate is tested against a fixture directory (section l).
struct Roots {
    /// `/sys/fs/cgroup`.
    cgroup: PathBuf,
    /// `/proc`.
    proc_dir: PathBuf,
    /// `/sys`.
    sys: PathBuf,
}

impl Roots {
    /// The real host.
    fn real() -> Roots {
        Roots {
            cgroup: PathBuf::from("/sys/fs/cgroup"),
            proc_dir: PathBuf::from("/proc"),
            sys: PathBuf::from("/sys"),
        }
    }
}

/// One-shot discovery (d.1, f.1). Runs the probes of e.4 for every `Unknown` guarantee and for
/// every `Present` one, and is idempotent: it caches nothing and leaves no file behind (DS-I7).
pub fn discover(input: &DiscoveryInput) -> Result<Discovered> {
    discover_with(&ProcessEnv, &Roots::real(), input)
}

/// [`discover`] with the environment and the host's directories supplied, which is how the tests
/// drive every branch of f.1 on a host that has no cgroups.
fn discover_with(env: &dyn EnvSource, roots: &Roots, input: &DiscoveryInput) -> Result<Discovered> {
    let mut notes: Vec<String> = Vec::new();

    // f.1: the Databricks refusal comes before anything is read (DS-I8).
    let budget_from_env = match env.get("MORUNA_BUDGET") {
        Some(text) => Some(crate::env::parse_size("budget", &text)?),
        None => None,
    };
    let explicit_budget = input.explicit_budget.or(budget_from_env);
    if env.get("DATABRICKS_RUNTIME_VERSION").is_some() {
        if explicit_budget.is_none() {
            return Err(config(
                "budget",
                "DATABRICKS_RUNTIME_VERSION is set and no budget was given: the driver's cgroup limit is the machine's and the JVM already holds most of it, so set the budget explicitly, either as DiscoveryInput::explicit_budget or as the MORUNA_BUDGET environment variable",
            ));
        }
        notes.push(
            "DATABRICKS_RUNTIME_VERSION is set; the explicit budget stands and nothing is derived from the machine"
                .to_string(),
        );
    } else if jvm_present(env, &roots.proc_dir) {
        notes.push("explicit budget recommended on this host: JVM present".to_string());
    }

    let explicit_cpu = match (input.explicit_cpu, env.get("MORUNA_CPU")) {
        (Some(cpu), _) => Some(cpu),
        (None, Some(text)) => Some(crate::env::parse_cpu(&text)?),
        (None, None) => None,
    };

    // f.6: the staging cap. The value is computed once the staging directory is known, below;
    // the environment variable is parsed here so a malformed one fails before any probe runs.
    let spill_limit_from_env = match env.get("MORUNA_SPILL_LIMIT") {
        Some(text) => Some(crate::env::parse_size("budget.disk", &text)?),
        None => None,
    };
    let explicit_spill_limit = input.explicit_spill_limit.or(spill_limit_from_env);

    // e.2, e.3: the cgroup, then the limits.
    let located = cgroup::locate(&roots.cgroup, &roots.proc_dir);
    let cgroup_path = match &located {
        Cgroup::V2(dir) => Some(dir.clone()),
        Cgroup::V1 { memory, .. } => {
            notes.push(
                "cgroup v1 in use; it is a fallback and not a supported target (e.2)".to_string(),
            );
            Some(memory.clone())
        }
        Cgroup::None => {
            notes.push("no cgroup found; the operating system's figures stand".to_string());
            None
        }
    };
    // DS-I4: where the platform answers instead of a cgroup, the note says which quantity the
    // memory figures in the run report are, so a figure measured on a developer's host is not
    // read as a cgroup's `anon + unevictable` when it is mach's `phys_footprint`.
    notes.extend(probes::platform_memory_note());
    let (device_list, device_notes) = devices::enumerate();
    notes.extend(device_notes);
    let limits = limits::derive(
        limits::LimitsInput {
            explicit_budget,
            explicit_cpu,
            cgroup: &located,
            proc_root: &roots.proc_dir,
            devices: device_list,
        },
        &mut notes,
    );

    // e.1: the declared profile, the environment variable underneath the override.
    let mut profile = match env.get("MORUNA_HOST_PROFILE") {
        Some(text) => parse_profile(&text).inspect_err(|err| {
            tracing::error!(target: "discovery.profile_violation", error = %err, "MORUNA_HOST_PROFILE is malformed");
        })?,
        None => HostProfile::default(),
    };
    if let Some(override_profile) = &input.profile_override {
        merge_override(&mut profile, override_profile, &mut notes);
    }

    // e.4: the staging directory, which the direct IO probe needs.
    profile.staging_dir = resolve_staging_dir(env, input, profile.staging_dir.take(), &mut notes);

    // e.4: every Unknown is probed and recorded as Probed(_); every Present is probed and a
    // failure is a Config error naming the platform (DS-I5); every Absent is left alone.
    let sys = roots.sys.clone();
    let proc_dir = roots.proc_dir.clone();
    profile.huge_pages = resolve(
        env,
        "huge_pages",
        profile.huge_pages,
        || probes::probe_huge_pages(&sys, &proc_dir),
        &mut notes,
    )?;
    profile.memlock = resolve(
        env,
        "memlock",
        profile.memlock,
        probes::probe_memlock,
        &mut notes,
    )?;
    profile.io_uring = resolve(
        env,
        "io_uring",
        profile.io_uring,
        probes::probe_io_uring,
        &mut notes,
    )?;
    let staging_for_probe = profile.staging_dir.clone();
    profile.direct_io_staging = resolve(
        env,
        "direct_io",
        profile.direct_io_staging,
        || probes::probe_direct_io(staging_for_probe),
        &mut notes,
    )?;
    profile.gds = resolve(env, "gds", profile.gds, probes::probe_gds, &mut notes)?;
    profile.rdma = resolve(env, "rdma", profile.rdma, probes::probe_rdma, &mut notes)?;
    profile.durable_staging = resolve_durable(&profile, &mut notes)?;

    let host_tier = limits::host_tier(&limits.devices, profile.memlock, &mut notes);
    let disk_budget = disk_budget(
        explicit_spill_limit,
        profile.staging_dir.as_deref(),
        &mut notes,
    );

    Ok(Discovered {
        limits,
        profile,
        host_tier,
        cgroup_path,
        disk_budget,
        notes,
    })
}

/// f.6. `budget.disk`: the explicit limit, else `MORUNA_SPILL_LIMIT`, else 20% of the free space
/// `statvfs` reports for the staging directory, else 0. An explicit cap above the free space is
/// clamped to it, because a cap above the disk is not a cap. A note records which it was.
fn disk_budget(explicit: Option<u64>, staging_dir: Option<&Path>, notes: &mut Vec<String>) -> u64 {
    let free = staging_dir.and_then(probes::free_bytes);
    match (explicit, staging_dir, free) {
        (Some(limit), _, Some(free)) if limit > free => {
            notes.push(format!(
                "budget.disk was set to {limit} bytes and clamped to the {free} bytes free in \
                 the staging directory"
            ));
            free
        }
        (Some(limit), _, _) => {
            notes.push(format!("budget.disk is the {limit} bytes that were given"));
            limit
        }
        (None, Some(dir), Some(free)) => {
            // The note names the rule and the directory, not the figure: free space moves
            // between two calls and DS-I7 asks for the same notes from both.
            notes.push(format!(
                "budget.disk defaults to 20% of the free space at {}",
                dir.display()
            ));
            free / 5
        }
        (None, Some(dir), None) => {
            notes.push(format!(
                "budget.disk is 0: the free space at {} could not be read, so the disk tier is \
                 off",
                dir.display()
            ));
            0
        }
        (None, None, _) => {
            notes.push(
                "budget.disk is 0: there is no staging directory, so the disk tier is off"
                    .to_string(),
            );
            0
        }
    }
}

/// A field the surface declares replaces the one the environment variable carried (d.1).
fn merge_override(profile: &mut HostProfile, over: &HostProfile, notes: &mut Vec<String>) {
    let fields: [(&str, Guarantee, &mut Guarantee); 7] = [
        ("huge_pages", over.huge_pages, &mut profile.huge_pages),
        ("memlock", over.memlock, &mut profile.memlock),
        ("io_uring", over.io_uring, &mut profile.io_uring),
        (
            "direct_io",
            over.direct_io_staging,
            &mut profile.direct_io_staging,
        ),
        ("gds", over.gds, &mut profile.gds),
        ("rdma", over.rdma, &mut profile.rdma),
        (
            "durable_staging",
            over.durable_staging,
            &mut profile.durable_staging,
        ),
    ];
    for (name, declared, target) in fields {
        if declared != Guarantee::Unknown && *target != declared {
            notes.push(format!(
                "host profile `{name}` overridden by the caller as {declared:?}"
            ));
            *target = declared;
        }
    }
    if let Some(dir) = &over.staging_dir {
        profile.staging_dir = Some(dir.clone());
    }
}

/// e.4: resolve one guarantee by running its probe.
fn resolve(
    env: &dyn EnvSource,
    field: &'static str,
    declared: Guarantee,
    probe: impl FnOnce() -> ProbeReport,
    notes: &mut Vec<String>,
) -> Result<Guarantee> {
    match declared {
        Guarantee::Absent => Ok(Guarantee::Absent),
        Guarantee::Probed(result) => Ok(Guarantee::Probed(result)),
        Guarantee::Unknown => {
            let report = run_probe(env, field, probe);
            if let Some(note) = report.note {
                notes.push(format!("{field}: {note}"));
            }
            Ok(Guarantee::Probed(report.available))
        }
        Guarantee::Present => {
            let report = run_probe(env, field, probe);
            if report.available {
                Ok(Guarantee::Present)
            } else {
                let reason = report
                    .note
                    .unwrap_or_else(|| "the probe failed".to_string());
                let msg = format!(
                    "the platform declares `{field}=present` and the probe disagrees: {reason}"
                );
                tracing::error!(target: "discovery.profile_violation", field, %msg, "declared guarantee does not hold");
                Err(config("host_profile", msg))
            }
        }
    }
}

/// Run one probe, honouring `MORUNA_TEST_FAIL_PROBE` in the crate's own tests only (section l).
#[cfg(test)]
fn run_probe(
    env: &dyn EnvSource,
    field: &'static str,
    probe: impl FnOnce() -> ProbeReport,
) -> ProbeReport {
    let forced = env
        .get("MORUNA_TEST_FAIL_PROBE")
        .is_some_and(|value| value.split(',').any(|name| name.trim() == field));
    if forced {
        return ProbeReport {
            available: false,
            note: Some(format!("MORUNA_TEST_FAIL_PROBE forced `{field}` to fail")),
        };
    }
    probe()
}

/// Run one probe. Outside the crate's own tests nothing can force a probe to fail.
#[cfg(not(test))]
fn run_probe(
    _env: &dyn EnvSource,
    _field: &'static str,
    probe: impl FnOnce() -> ProbeReport,
) -> ProbeReport {
    probe()
}

/// e.4: `durable_staging` is never probed. `Unknown` becomes `Probed(false)`, and a `Present`
/// declaration over a filesystem that cannot outlive the node is refused.
fn resolve_durable(profile: &HostProfile, notes: &mut Vec<String>) -> Result<Guarantee> {
    match profile.durable_staging {
        Guarantee::Unknown => {
            notes.push(
                "durable_staging was not declared; cross-node resume is off (it cannot be probed)"
                    .to_string(),
            );
            Ok(Guarantee::Probed(false))
        }
        Guarantee::Present => {
            let Some(dir) = &profile.staging_dir else {
                let msg =
                    "declared `durable_staging=present` with no staging directory to be durable"
                        .to_string();
                tracing::error!(target: "discovery.profile_violation", %msg, "declared guarantee does not hold");
                return Err(config("durable_staging", msg));
            };
            if probes::filesystem_is_ephemeral(dir) == Some(true) {
                let msg = format!(
                    "declared `durable_staging=present` for {}, which is on a tmpfs or overlay filesystem and cannot outlive the node",
                    dir.display()
                );
                tracing::error!(target: "discovery.profile_violation", %msg, "declared guarantee does not hold");
                return Err(config("durable_staging", msg));
            }
            Ok(Guarantee::Present)
        }
        other => Ok(other),
    }
}

/// e.4 `staging_dir`: explicit, then the environment, then the declared profile, then the first
/// writable candidate with at least 1 GiB free. A directory that is not usable is dropped with a
/// note, and the run then has no disk tier (h).
fn resolve_staging_dir(
    env: &dyn EnvSource,
    input: &DiscoveryInput,
    declared: Option<PathBuf>,
    notes: &mut Vec<String>,
) -> Option<PathBuf> {
    let mut candidates: Vec<(String, PathBuf)> = Vec::new();
    if let Some(dir) = &input.explicit_staging_dir {
        candidates.push(("explicit".to_string(), dir.clone()));
    }
    if let Some(dir) = env.get("MORUNA_SPILL_DIR") {
        candidates.push(("MORUNA_SPILL_DIR".to_string(), PathBuf::from(dir)));
    }
    if let Some(dir) = declared {
        candidates.push(("host profile".to_string(), dir));
    }
    let chosen = candidates.first().cloned();
    if let Some((source, dir)) = chosen {
        if probes::staging_dir_is_usable(&dir) {
            notes.push(format!("staging directory {} ({source})", dir.display()));
            return Some(dir);
        }
        notes.push(format!(
            "staging directory {} ({source}) is not writable with 1 GiB free; the run has no disk tier",
            dir.display()
        ));
        return None;
    }
    let mut fallbacks: Vec<PathBuf> =
        vec![PathBuf::from("/local_disk0"), PathBuf::from("/scratch")];
    if let Some(tmp) = env.get("TMPDIR") {
        fallbacks.push(PathBuf::from(tmp));
    }
    fallbacks.push(PathBuf::from("/tmp"));
    for dir in fallbacks {
        if probes::staging_dir_is_usable(&dir) {
            notes.push(format!("staging directory {} (discovered)", dir.display()));
            return Some(dir);
        }
    }
    notes.push(
        "no writable staging directory with 1 GiB free; the run has no disk tier".to_string(),
    );
    None
}

/// Best effort: is a JVM on this host (h, the note outside Databricks)?
fn jvm_present(env: &dyn EnvSource, proc_root: &Path) -> bool {
    if env.get("JAVA_HOME").is_some() {
        return true;
    }
    let Ok(entries) = std::fs::read_dir(proc_root) else {
        return false;
    };
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .chars()
            .all(|c| c.is_ascii_digit())
        {
            continue;
        }
        if let Ok(comm) = std::fs::read_to_string(entry.path().join("comm"))
            && comm.trim() == "java"
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cgroup::fixtures::{TempDir, cgroup_v2, proc_root, write};
    use crate::env::MapEnv;
    use moruna_kernel::{LimitSource, Sampler as SamplerTrait};

    /// Roots that point at fixtures and at nothing real.
    fn roots_at(dir: &Path) -> Roots {
        Roots {
            cgroup: dir.join("cgroup"),
            proc_dir: dir.join("proc"),
            sys: dir.join("sys"),
        }
    }

    /// A `/proc` fixture with a MemTotal and a v2 cgroup line.
    fn proc_fixture(dir: &Path, cgroup_line: &str, mem_total_kib: u64) {
        let root = proc_root(dir, cgroup_line);
        write(&root, "meminfo", &format!("MemTotal: {mem_total_kib} kB\n"));
    }

    /// A usable staging directory inside the fixture, so the tests never touch `/tmp` policy.
    fn staging(dir: &Path) -> PathBuf {
        let path = dir.join("staging");
        std::fs::create_dir_all(&path).expect("staging");
        path
    }

    /// DS-T1 precedence: a matrix of explicit, environment, cgroup and operating system values
    /// for budget, CPU and staging directory; the chosen value and `LimitSource` follow DS-I1.
    #[test]
    fn ds_t1_precedence() {
        let tmp = TempDir::new("ds-t1");
        let gib = 1024u64 * 1024 * 1024;
        proc_fixture(tmp.path(), "0::/\n", 64 * 1024 * 1024);
        cgroup_v2(tmp.path(), &(8 * gib).to_string(), "max", "400000 100000");
        let roots = roots_at(tmp.path());
        let spill = staging(tmp.path());
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&other).expect("other");

        // Explicit beats the environment, which beats the cgroup, which beats the OS.
        let cases: [(Option<u64>, Option<&str>, u64, LimitSource); 4] = [
            (Some(3 * gib), Some("5GiB"), 3 * gib, LimitSource::Explicit),
            (None, Some("5GiB"), 5 * gib, LimitSource::Explicit),
            (None, None, 8 * gib * 9 / 10, LimitSource::Cgroup),
            (None, None, 8 * gib * 9 / 10, LimitSource::Cgroup),
        ];
        for (explicit, env_budget, expected, source) in cases {
            let mut pairs = vec![("MORUNA_SPILL_DIR", spill.to_str().expect("utf-8"))];
            if let Some(value) = env_budget {
                pairs.push(("MORUNA_BUDGET", value));
            }
            let env = MapEnv::with(&pairs);
            let found = discover_with(
                &env,
                &roots,
                &DiscoveryInput {
                    explicit_budget: explicit,
                    ..DiscoveryInput::default()
                },
            )
            .expect("discovery");
            assert_eq!(found.limits.memory_ceiling, expected);
            assert_eq!(found.limits.source, source);
        }

        // The operating system branch: no cgroup at all.
        let bare = TempDir::new("ds-t1-os");
        proc_fixture(bare.path(), "0::/\n", 16 * 1024 * 1024);
        let bare_roots = roots_at(bare.path());
        let found = discover_with(&MapEnv::default(), &bare_roots, &DiscoveryInput::default())
            .expect("discovery");
        assert_eq!(found.limits.source, LimitSource::Os);
        assert_eq!(found.limits.memory_ceiling, 16 * gib * 9 / 10);
        assert!(found.cgroup_path.is_none());

        // CPU: explicit beats MORUNA_CPU beats cpu.max beats the core count.
        let env = MapEnv::with(&[("MORUNA_CPU", "2.5")]);
        let found = discover_with(
            &env,
            &roots,
            &DiscoveryInput {
                explicit_cpu: Some(7.0),
                ..DiscoveryInput::default()
            },
        )
        .expect("discovery");
        assert!((found.limits.cpu_quota - 7.0).abs() < f64::EPSILON);
        let found = discover_with(&env, &roots, &DiscoveryInput::default()).expect("discovery");
        assert!((found.limits.cpu_quota - 2.5).abs() < f64::EPSILON);
        let found = discover_with(&MapEnv::default(), &roots, &DiscoveryInput::default())
            .expect("discovery");
        assert!((found.limits.cpu_quota - 4.0).abs() < f64::EPSILON);
        let found = discover_with(&MapEnv::default(), &bare_roots, &DiscoveryInput::default())
            .expect("discovery");
        assert!(found.limits.cpu_quota >= 1.0);

        // Staging directory: explicit beats MORUNA_SPILL_DIR beats the declared profile.
        let env = MapEnv::with(&[
            ("MORUNA_SPILL_DIR", spill.to_str().expect("utf-8")),
            (
                "MORUNA_HOST_PROFILE",
                &format!("staging_dir={}", other.display()),
            ),
        ]);
        let found = discover_with(
            &env,
            &roots,
            &DiscoveryInput {
                explicit_staging_dir: Some(other.clone()),
                ..DiscoveryInput::default()
            },
        )
        .expect("discovery");
        assert_eq!(found.profile.staging_dir, Some(other.clone()));
        let found = discover_with(&env, &roots, &DiscoveryInput::default()).expect("discovery");
        assert_eq!(found.profile.staging_dir, Some(spill.clone()));
        let env = MapEnv::with(&[(
            "MORUNA_HOST_PROFILE",
            &format!("staging_dir={}", other.display()),
        )]);
        let found = discover_with(&env, &roots, &DiscoveryInput::default()).expect("discovery");
        assert_eq!(found.profile.staging_dir, Some(other));
    }

    /// DS-T5 present_guarantee_enforced: the profile declares `io_uring=present` where the probe
    /// fails, and `discover` returns `Config`. DS-I5.
    #[test]
    fn ds_t5_present_guarantee_enforced() {
        let tmp = TempDir::new("ds-t5");
        proc_fixture(tmp.path(), "0::/\n", 16 * 1024 * 1024);
        let roots = roots_at(tmp.path());
        let spill = staging(tmp.path());
        let env = MapEnv::with(&[
            ("MORUNA_HOST_PROFILE", "io_uring=present"),
            ("MORUNA_TEST_FAIL_PROBE", "io_uring"),
            ("MORUNA_SPILL_DIR", spill.to_str().expect("utf-8")),
        ]);
        let err = discover_with(&env, &roots, &DiscoveryInput::default())
            .expect_err("a declared guarantee that does not hold is a Config error");
        match err {
            moruna_kernel::MorunaError::Config { name, msg } => {
                assert_eq!(name, "host_profile");
                assert!(msg.contains("io_uring"), "{msg}");
                assert!(msg.contains("present"), "{msg}");
            }
            other => panic!("expected Config, got {other:?}"),
        }

        // Without the forcing flag the same declaration is decided by the host: kept as
        // `Present` where the probe agrees, refused where it does not. Both are DS-I5.
        let env = MapEnv::with(&[
            ("MORUNA_HOST_PROFILE", "memlock=present"),
            ("MORUNA_SPILL_DIR", spill.to_str().expect("utf-8")),
        ]);
        match discover_with(&env, &roots, &DiscoveryInput::default()) {
            Ok(found) => assert_eq!(found.profile.memlock, Guarantee::Present),
            Err(err) => assert!(
                matches!(
                    err,
                    moruna_kernel::MorunaError::Config {
                        name: "host_profile",
                        ..
                    }
                ),
                "expected Config, got {err:?}"
            ),
        }
    }

    /// DS-T6 no_unknown_after_discover: an all-`Unknown` profile resolves to `Probed(_)`
    /// everywhere; a declared field stays declared and is never rewritten as `Probed`;
    /// `is_available` and `is_guaranteed` agree with the field values. DS-I6.
    #[test]
    fn ds_t6_no_unknown_after_discover() {
        let tmp = TempDir::new("ds-t6");
        proc_fixture(tmp.path(), "0::/\n", 16 * 1024 * 1024);
        let roots = roots_at(tmp.path());
        let spill = staging(tmp.path());

        // Nothing declared: every field comes back probed.
        let env = MapEnv::with(&[("MORUNA_SPILL_DIR", spill.to_str().expect("utf-8"))]);
        let found = discover_with(&env, &roots, &DiscoveryInput::default()).expect("discovery");
        for field in every_guarantee(&found.profile) {
            assert!(
                matches!(field, Guarantee::Probed(_)),
                "every undeclared field is probed, found {field:?}"
            );
        }

        // A declared field stays declared, and the rest are still probed. `gds=absent` is a
        // declaration every v1 build honours, because no v1 build has the gds feature (6.3).
        let env = MapEnv::with(&[
            ("MORUNA_HOST_PROFILE", "gds=absent"),
            ("MORUNA_SPILL_DIR", spill.to_str().expect("utf-8")),
        ]);
        let found = discover_with(&env, &roots, &DiscoveryInput::default()).expect("discovery");
        assert_eq!(found.profile.gds, Guarantee::Absent);
        assert!(matches!(found.profile.huge_pages, Guarantee::Probed(_)));
        for field in every_guarantee(&found.profile) {
            assert_ne!(field, Guarantee::Unknown, "DS-I6: no Unknown survives");
            assert_eq!(
                field.is_available(),
                matches!(field, Guarantee::Present | Guarantee::Probed(true))
            );
            assert_eq!(field.is_guaranteed(), field == Guarantee::Present);
        }

        // The four arms of e.4, with the probe's answer supplied rather than taken from the
        // host, so the rule is proved the same way on a laptop, in a container and on the
        // reference host: a declared `Present` that holds stays `Present` and is never
        // rewritten as `Probed(true)`; one that does not hold stops the run (DS-I5); an
        // `Unknown` becomes `Probed(_)`; and `Absent` is never probed at all.
        let env = MapEnv::default();
        let mut notes = Vec::new();
        let works = || ProbeReport {
            available: true,
            note: None,
        };
        let refuses = || ProbeReport {
            available: false,
            note: Some("the probe refused".to_string()),
        };
        assert_eq!(
            resolve(&env, "io_uring", Guarantee::Present, works, &mut notes).expect("it holds"),
            Guarantee::Present
        );
        assert!(resolve(&env, "io_uring", Guarantee::Present, refuses, &mut notes).is_err());
        assert_eq!(
            resolve(&env, "io_uring", Guarantee::Unknown, works, &mut notes).expect("probed"),
            Guarantee::Probed(true)
        );
        assert_eq!(
            resolve(&env, "io_uring", Guarantee::Unknown, refuses, &mut notes).expect("probed"),
            Guarantee::Probed(false)
        );
        assert_eq!(
            resolve(
                &env,
                "io_uring",
                Guarantee::Absent,
                || panic!("an Absent field is never probed"),
                &mut notes
            )
            .expect("absent"),
            Guarantee::Absent
        );
        assert_eq!(
            resolve(
                &env,
                "io_uring",
                Guarantee::Probed(true),
                || panic!("an already probed field is not probed again"),
                &mut notes
            )
            .expect("kept"),
            Guarantee::Probed(true)
        );
        assert!(notes.iter().any(|note| note.contains("io_uring")));
    }

    /// Every guarantee of a profile, in one array.
    fn every_guarantee(profile: &HostProfile) -> [Guarantee; 7] {
        [
            profile.huge_pages,
            profile.memlock,
            profile.io_uring,
            profile.direct_io_staging,
            profile.gds,
            profile.rdma,
            profile.durable_staging,
        ]
    }

    /// DS-T14 disk_budget: `budget.disk` is the explicit limit, else `MORUNA_SPILL_LIMIT`, else
    /// 20% of the free space in the staging directory, else 0, and an explicit cap above the
    /// free space is clamped to it. f.6.
    #[test]
    fn ds_t14_disk_budget() {
        let tmp = TempDir::new("ds-t14");
        let gib = 1024u64 * 1024 * 1024;
        proc_fixture(tmp.path(), "0::/\n", 16 * 1024 * 1024);
        cgroup_v2(tmp.path(), &(4 * gib).to_string(), "max", "200000 100000");
        let roots = roots_at(tmp.path());
        let spill = staging(tmp.path());
        let spill_str = spill.to_str().expect("utf-8");
        let free = probes::free_bytes(&spill).expect("the staging directory has a filesystem");

        // The environment variable, which the old code parsed and threw away.
        let env = MapEnv::with(&[
            ("MORUNA_SPILL_DIR", spill_str),
            ("MORUNA_SPILL_LIMIT", "2GiB"),
        ]);
        let found = discover_with(&env, &roots, &DiscoveryInput::default()).expect("discover");
        let want = (2 * gib).min(free);
        assert_eq!(
            found.disk_budget, want,
            "MORUNA_SPILL_LIMIT carries through"
        );
        assert!(
            found.notes.iter().any(|n| n.starts_with("budget.disk")),
            "a note says where the cap came from: {:?}",
            found.notes
        );

        // The documented default: 20% of the free space, measured with `statvfs`.
        let env = MapEnv::with(&[("MORUNA_SPILL_DIR", spill_str)]);
        let found = discover_with(&env, &roots, &DiscoveryInput::default()).expect("discover");
        let now = probes::free_bytes(&spill).expect("free space");
        let slack = now / 100;
        assert!(
            found.disk_budget.abs_diff(now / 5) <= slack,
            "f.6: {} is not within a percent of a fifth of {now}",
            found.disk_budget
        );
        assert!(found.disk_budget > 0, "the disk tier is on by default");

        // The explicit limit beats the variable, and a cap above the disk is clamped to it.
        let input = DiscoveryInput {
            explicit_spill_limit: Some(u64::MAX),
            ..DiscoveryInput::default()
        };
        let env = MapEnv::with(&[
            ("MORUNA_SPILL_DIR", spill_str),
            ("MORUNA_SPILL_LIMIT", "2GiB"),
        ]);
        let found = discover_with(&env, &roots, &input).expect("discover");
        assert!(
            found.disk_budget <= now + slack && found.disk_budget > 0,
            "an explicit cap above the disk is clamped to it: {}",
            found.disk_budget
        );

        // No staging directory: no disk tier.
        let nowhere = tmp.path().join("no-such-dir");
        let env = MapEnv::with(&[(
            "MORUNA_HOST_PROFILE",
            format!("staging_dir={}", nowhere.display()).as_str(),
        )]);
        let found = discover_with(&env, &roots, &DiscoveryInput::default()).expect("discover");
        if found.profile.staging_dir.is_none() {
            assert_eq!(found.disk_budget, 0, "no staging directory, no disk tier");
        }
    }

    /// DS-T7 idempotent: two calls give equal `Limits` (device free bytes aside) and leave no
    /// probe file behind. DS-I7.
    #[test]
    fn ds_t7_idempotent() {
        let tmp = TempDir::new("ds-t7");
        let gib = 1024u64 * 1024 * 1024;
        proc_fixture(tmp.path(), "0::/\n", 16 * 1024 * 1024);
        cgroup_v2(tmp.path(), &(4 * gib).to_string(), "max", "200000 100000");
        let roots = roots_at(tmp.path());
        let spill = staging(tmp.path());
        let env = MapEnv::with(&[("MORUNA_SPILL_DIR", spill.to_str().expect("utf-8"))]);

        let first = discover_with(&env, &roots, &DiscoveryInput::default()).expect("first");
        let second = discover_with(&env, &roots, &DiscoveryInput::default()).expect("second");
        assert_eq!(first.limits.memory_ceiling, second.limits.memory_ceiling);
        assert_eq!(first.limits.memory_kill, second.limits.memory_kill);
        assert_eq!(first.limits.page_bytes, second.limits.page_bytes);
        assert_eq!(first.limits.source, second.limits.source);
        assert_eq!(first.limits.devices.len(), second.limits.devices.len());
        assert!((first.limits.cpu_quota - second.limits.cpu_quota).abs() < f64::EPSILON);
        assert_eq!(first.host_tier, second.host_tier);
        assert_eq!(first.cgroup_path, second.cgroup_path);
        assert_eq!(first.notes, second.notes);

        // Every probe deletes its own file. A probe that ran into the e.4 timeout is still
        // finishing on its own thread when `discover` returns, so give it a moment before
        // concluding that a file was left behind: the invariant is that none survives, not that
        // none exists while a probe is still running.
        let leftovers = |dir: &Path| -> Vec<String> {
            std::fs::read_dir(dir)
                .expect("staging")
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().to_string())
                .collect()
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !leftovers(&spill).is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let remaining = leftovers(&spill);
        assert!(
            remaining.is_empty(),
            "probe files left behind: {remaining:?}"
        );
    }

    /// DS-T9 container_gate: under the CI container job's `--memory=2g --cpus=1.5` the ceiling is
    /// 0.9 x 2 GiB, the quota is 1.5 and the source is `Cgroup`. Parent S5.
    #[test]
    #[ignore = "integration, closes in wave 1: the CI container job with --memory=2g --cpus=1.5"]
    fn ds_t9_container_gate() {
        let gib = 1024u64 * 1024 * 1024;
        let roots = Roots::real();
        let found = discover_with(&MapEnv::default(), &roots, &DiscoveryInput::default())
            .expect("discovery");
        assert_eq!(found.limits.source, LimitSource::Cgroup);
        assert_eq!(found.limits.memory_kill, Some(2 * gib));
        assert_eq!(found.limits.memory_ceiling, 2 * gib * 9 / 10);
        assert!((found.limits.cpu_quota - 1.5).abs() < 1e-9);
    }

    /// DS-T11 databricks_budget_required: no budget is a `Config` naming the variable; a budget
    /// succeeds with `source = Explicit`; with the variable unset the OS fallback stands with a
    /// note. DS-I8.
    #[test]
    fn ds_t11_databricks_budget_required() {
        let tmp = TempDir::new("ds-t11");
        proc_fixture(tmp.path(), "0::/\n", 16 * 1024 * 1024);
        let roots = roots_at(tmp.path());
        let spill = staging(tmp.path());
        let spill_str = spill.to_str().expect("utf-8");

        let env = MapEnv::with(&[
            ("DATABRICKS_RUNTIME_VERSION", "14.3"),
            ("MORUNA_SPILL_DIR", spill_str),
        ]);
        let err = discover_with(&env, &roots, &DiscoveryInput::default())
            .expect_err("a Databricks driver needs an explicit budget");
        match err {
            moruna_kernel::MorunaError::Config { name, msg } => {
                assert_eq!(name, "budget");
                assert!(msg.contains("DATABRICKS_RUNTIME_VERSION"), "{msg}");
                assert!(msg.contains("MORUNA_BUDGET"), "{msg}");
                assert!(msg.contains("explicit_budget"), "{msg}");
            }
            other => panic!("expected Config, got {other:?}"),
        }

        let env = MapEnv::with(&[
            ("DATABRICKS_RUNTIME_VERSION", "14.3"),
            ("MORUNA_BUDGET", "4GiB"),
            ("MORUNA_SPILL_DIR", spill_str),
        ]);
        let found = discover_with(&env, &roots, &DiscoveryInput::default()).expect("with a budget");
        assert_eq!(found.limits.source, LimitSource::Explicit);
        assert_eq!(found.limits.memory_ceiling, 4 * 1024 * 1024 * 1024);

        // The constructor argument counts as explicit too.
        let env = MapEnv::with(&[
            ("DATABRICKS_RUNTIME_VERSION", "14.3"),
            ("MORUNA_SPILL_DIR", spill_str),
        ]);
        let found = discover_with(
            &env,
            &roots,
            &DiscoveryInput {
                explicit_budget: Some(2 * 1024 * 1024 * 1024),
                ..DiscoveryInput::default()
            },
        )
        .expect("with a constructor budget");
        assert_eq!(found.limits.source, LimitSource::Explicit);

        // Unset, the operating system fallback stands.
        let env = MapEnv::with(&[("MORUNA_SPILL_DIR", spill_str)]);
        let found = discover_with(&env, &roots, &DiscoveryInput::default()).expect("no databricks");
        assert_eq!(found.limits.source, LimitSource::Os);
        assert!(
            found
                .notes
                .iter()
                .any(|note| note.contains("no cgroup found"))
        );

        // A JVM on the host adds the note, and nothing else changes.
        let env = MapEnv::with(&[("JAVA_HOME", "/opt/java"), ("MORUNA_SPILL_DIR", spill_str)]);
        let found = discover_with(&env, &roots, &DiscoveryInput::default()).expect("jvm note");
        assert!(found.notes.iter().any(|note| note.contains("JVM present")));
    }

    /// e.4 `durable_staging`: never probed, refused on an ephemeral filesystem, kept when
    /// declared absent.
    #[test]
    fn durable_staging_is_declared_not_probed() {
        let tmp = TempDir::new("durable");
        proc_fixture(tmp.path(), "0::/\n", 16 * 1024 * 1024);
        let roots = roots_at(tmp.path());
        let spill = staging(tmp.path());
        let spill_str = spill.to_str().expect("utf-8");

        let env = MapEnv::with(&[("MORUNA_SPILL_DIR", spill_str)]);
        let found = discover_with(&env, &roots, &DiscoveryInput::default()).expect("discovery");
        assert_eq!(found.profile.durable_staging, Guarantee::Probed(false));

        let env = MapEnv::with(&[
            ("MORUNA_HOST_PROFILE", "durable_staging=absent"),
            ("MORUNA_SPILL_DIR", spill_str),
        ]);
        let found = discover_with(&env, &roots, &DiscoveryInput::default()).expect("discovery");
        assert_eq!(found.profile.durable_staging, Guarantee::Absent);

        // Declared present with no staging directory at all is refused.
        let env = MapEnv::with(&[
            ("MORUNA_HOST_PROFILE", "durable_staging=present"),
            ("MORUNA_SPILL_DIR", "/moruna/definitely/absent"),
        ]);
        let err = discover_with(&env, &roots, &DiscoveryInput::default())
            .expect_err("nothing to be durable");
        assert!(matches!(
            err,
            moruna_kernel::MorunaError::Config {
                name: "durable_staging",
                ..
            }
        ));

        // Declared present over a real directory is kept when the filesystem can outlive the node.
        let env = MapEnv::with(&[
            ("MORUNA_HOST_PROFILE", "durable_staging=present"),
            ("MORUNA_SPILL_DIR", spill_str),
        ]);
        match discover_with(&env, &roots, &DiscoveryInput::default()) {
            Ok(found) => assert_eq!(found.profile.durable_staging, Guarantee::Present),
            Err(err) => assert!(
                matches!(
                    err,
                    moruna_kernel::MorunaError::Config {
                        name: "durable_staging",
                        ..
                    }
                ),
                "an ephemeral temporary directory is the only other legal answer, got {err:?}"
            ),
        }
    }

    /// d.1: the caller's profile overrides the environment variable field by field, and the
    /// variable is still parsed underneath.
    #[test]
    fn the_caller_overrides_the_environment_profile() {
        let tmp = TempDir::new("override");
        proc_fixture(tmp.path(), "0::/\n", 16 * 1024 * 1024);
        let roots = roots_at(tmp.path());
        let spill = staging(tmp.path());
        let env = MapEnv::with(&[
            ("MORUNA_HOST_PROFILE", "gds=present,rdma=absent"),
            ("MORUNA_SPILL_DIR", spill.to_str().expect("utf-8")),
        ]);
        let over = HostProfile {
            gds: Guarantee::Absent,
            ..HostProfile::default()
        };
        let found = discover_with(
            &env,
            &roots,
            &DiscoveryInput {
                profile_override: Some(over),
                ..DiscoveryInput::default()
            },
        )
        .expect("the override turns the declaration off");
        assert_eq!(found.profile.gds, Guarantee::Absent);
        assert_eq!(found.profile.rdma, Guarantee::Absent);
        assert!(found.notes.iter().any(|note| note.contains("overridden")));

        // A malformed variable is a Config error even when an override is supplied.
        let env = MapEnv::with(&[("MORUNA_HOST_PROFILE", "gds=perhaps")]);
        assert!(discover_with(&env, &roots, &DiscoveryInput::default()).is_err());
    }

    /// h: a staging directory that cannot be used leaves the run without a disk tier, and the
    /// direct IO probe says so rather than failing.
    #[test]
    fn an_unusable_staging_directory_leaves_no_disk_tier() {
        let tmp = TempDir::new("no-staging");
        proc_fixture(tmp.path(), "0::/\n", 16 * 1024 * 1024);
        let roots = roots_at(tmp.path());
        let env = MapEnv::with(&[("MORUNA_SPILL_DIR", "/moruna/definitely/absent")]);
        let found = discover_with(&env, &roots, &DiscoveryInput::default()).expect("discovery");
        assert_eq!(found.profile.staging_dir, None);
        assert_eq!(found.profile.direct_io_staging, Guarantee::Probed(false));
        assert!(found.notes.iter().any(|note| note.contains("no disk tier")));
    }

    /// e.2: the v1 fallback is reported as such, and the sampler reads the v1 field names.
    #[test]
    fn cgroup_v1_is_a_noted_fallback() {
        let tmp = TempDir::new("v1-discover");
        proc_fixture(tmp.path(), "3:memory:/\n", 16 * 1024 * 1024);
        let root = tmp.path().join("cgroup");
        write(&root, "memory/memory.limit_in_bytes", "2147483648");
        write(&root, "memory/memory.stat", "rss 1024\ncache 2048\n");
        write(&root, "cpu/cpu.cfs_quota_us", "150000");
        write(&root, "cpu/cpu.cfs_period_us", "100000");
        let roots = roots_at(tmp.path());
        let spill = staging(tmp.path());
        let env = MapEnv::with(&[("MORUNA_SPILL_DIR", spill.to_str().expect("utf-8"))]);
        let found = discover_with(&env, &roots, &DiscoveryInput::default()).expect("discovery");
        assert_eq!(found.limits.source, LimitSource::Cgroup);
        assert_eq!(found.limits.memory_kill, Some(2 * 1024 * 1024 * 1024));
        assert!((found.limits.cpu_quota - 1.5).abs() < f64::EPSILON);
        assert!(found.notes.iter().any(|note| note.contains("cgroup v1")));
        assert_eq!(found.cgroup_path, Some(root.join("memory")));

        let sampler = Sampler::new(&found).expect("sampler over the v1 fixture");
        assert_eq!(sampler.sample().anon_bytes, 1024);
    }

    /// h, normal path (laptop): `discover` on the real host answers, leaves no `Unknown`, and
    /// produces a sampler. This is the path the whole team runs on macOS.
    #[test]
    fn discovers_the_real_host() {
        let found = discover(&DiscoveryInput {
            explicit_budget: Some(1024 * 1024 * 1024),
            ..DiscoveryInput::default()
        })
        .expect("discovery on the development host");
        assert_eq!(found.limits.memory_ceiling, 1024 * 1024 * 1024);
        assert_eq!(found.limits.source, LimitSource::Explicit);
        assert!(found.limits.page_bytes >= 4096);
        assert!(found.limits.cpu_quota >= 1.0);
        for field in every_guarantee(&found.profile) {
            assert_ne!(field, Guarantee::Unknown);
        }
        assert_eq!(found.host_tier, TierKind::Host, "no device on this host");
        let sampler = Sampler::new(&found).expect("sampler");
        assert!(sampler.sample().anon_bytes > 0);
        assert!(!found.notes.is_empty());
    }

    /// `MORUNA_SPILL_LIMIT` and malformed sizes are parsed here (section i, f.3).
    #[test]
    fn parses_the_remaining_environment_variables() {
        let tmp = TempDir::new("env-vars");
        proc_fixture(tmp.path(), "0::/\n", 16 * 1024 * 1024);
        let roots = roots_at(tmp.path());
        let spill = staging(tmp.path());
        let spill_str = spill.to_str().expect("utf-8");

        let env = MapEnv::with(&[
            ("MORUNA_SPILL_LIMIT", "2GiB"),
            ("MORUNA_SPILL_DIR", spill_str),
        ]);
        let found = discover_with(&env, &roots, &DiscoveryInput::default()).expect("discovery");
        assert!(
            found
                .notes
                .iter()
                .any(|note| note.contains("budget.disk") && note.contains("2147483648"))
        );

        for bad in [
            ("MORUNA_BUDGET", "lots"),
            ("MORUNA_CPU", "none"),
            ("MORUNA_SPILL_LIMIT", "8 GiB"),
        ] {
            let env = MapEnv::with(&[bad, ("MORUNA_SPILL_DIR", spill_str)]);
            assert!(
                discover_with(&env, &roots, &DiscoveryInput::default()).is_err(),
                "{bad:?} must be rejected"
            );
        }

        // TMPDIR is one of the discovered candidates when nothing else names a directory.
        let env = MapEnv::with(&[("TMPDIR", spill_str)]);
        let found = discover_with(&env, &roots, &DiscoveryInput::default()).expect("discovery");
        assert_eq!(found.profile.staging_dir, Some(spill));
    }
}
