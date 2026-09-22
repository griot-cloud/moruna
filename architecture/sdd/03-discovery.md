# Amoru SDD 03: Resource discovery and host profile (`amoru-discovery`)

**Document type:** software design document, component 3 of 12
**Status:** DRAFT · 2026-09-15 (becomes HANDOFF-READY when section m is empty and the preamble's E1 and E2 assumptions are accepted; the human flips it)
**Parent:** `architecture/amoru-runtime-design.md` sections 5.7, 6; criteria S5; global invariant G-I7
**Preamble:** `00-preamble.md`; **Contracts:** `01-contracts.md` d.12 (`Limits`, `Device`, `LimitSource`, `Guarantee`, `HostProfile`, `Sampler`, `Sample`), d.2 (`TierKind`)
**Component location:** `crates/amoru-discovery`, Rust
**Consumes:** contracts (1). Feature `cuda` adds `cudarc`. **Consumed by:** arena (2, via config), reactor (6), placement (9), controller (11), python surface (12)

**Decisions worth your eye:** (1) the host profile is one environment variable in `key=value` form, not a file, so a pod spec carries it; (2) under a declared `Present` guarantee the probe is still run once at start and a failure is a `Config` error naming the platform, never a fallback, while a probed result is recorded as `Probed(bool)` so every consumer can tell the two apart; (3) the budget against which the controller works is anonymous memory plus unevictable, read from `memory.stat`, not `memory.current`, because file-backed pages are reclaimable; (4) on a Databricks driver discovery refuses to run without an explicit budget, because the JVM's share of the machine is invisible to it.

---

## a. Purpose and boundary

Discovery answers three questions and answers them from the host, not from assumptions: how much memory may this process use, how many CPUs may it use, and what accelerators and fast paths does it have. It produces `Limits` once at start, `HostProfile` once at start (declared by the platform, probed where undeclared), the run's host tier (which the arena pins or does not), and `Sample`s continuously for the controller and the scheduler through the contract's `Sampler` trait. It is the only component that reads `/sys/fs/cgroup`, `/proc`, or the CUDA device list.

It owns: cgroup v2 (and v1 fallback) parsing; OS fallback; explicit override precedence; the sampler; host-profile parsing and probing; the page size; device enumeration; the host-tier decision.

It refuses to know: what the budget is used for (controller); how a fast path is used (reactor, arena); anything after the run starts except sampling.

## b. Vocabulary

**Ceiling.** The memory limit the process must stay under: `memory.high` if set below `memory.max`, else 0.9 × `memory.max`, else 0.9 × total RAM, else the explicit value; the lowest applicable wins.

**Kill line.** `memory.max` when in a cgroup; the value at which the OOM killer acts.

**Quota.** CPU allowance as a fraction of cores: `cpu.max` quota divided by period; `max` means the host's logical core count.

**Guarantee.** The declaration for each fast path: `Unknown`, `Present` or `Absent` as the platform states it, and `Probed(bool)` as discovery resolves an `Unknown` (contracts d.12).

**Probe.** A cheap, side-effect-free test that a fast path works on this host, run for `Unknown` guarantees (result recorded as `Probed(bool)`) and for `Present` guarantees (a failure is an error, DS-I5).

**Host tier.** The one host tier of the run (contracts e.1): `PinnedHost` when at least one device is present and `memlock` is available, else `Host`.

## c. Invariants

**DS-I1. Explicit beats discovered, and discovered beats default.** For each of memory, CPU and staging dir: an explicit value (constructor argument or `AMORU_*` environment variable) is used as given, clamped to the cgroup kill line if one exists and the explicit value exceeds it, with a warning naming both numbers; else the cgroup value; else the OS value.

**DS-I2. The ceiling is below the kill line.** `Limits.memory_ceiling < Limits.memory_kill` whenever the latter exists, by at least 5% of the kill line. Upholds G-I8.

**DS-I3. Samples are cheap and monotonic in time.** One `Sample` costs at most four file reads and no allocation beyond a fixed buffer; `at_ns` strictly increases across samples; `peak_anon_bytes` is the maximum of `anon_bytes` seen by this sampler since start, or since the last `reset_peak`, when the kernel does not provide `memory.peak`; when it does, `reset_peak` writes `memory.peak` (kernels that allow it) and otherwise falls back to the tracked peak. `sample` and `reset_peak` take `&self` and are safe to call from the controller thread and any worker at once (contracts d.12 `Sampler`).

**DS-I4. Budget is anon plus unevictable.** `Sample.anon_bytes` is `anon + unevictable` from `memory.stat`, or RSS anon from `/proc/self/statm` outside a cgroup; never `memory.current`. Rationale: architecture section 8, page cache is charged but reclaimable.

**DS-I5. A `Present` guarantee is verified once and never worked around.** Each `Present` path is probed once at start; a failing probe is `AmoruError::Config { name: "host_profile", msg }` and the run does not start. Upholds G-I7.

**DS-I6. An `Unknown` guarantee resolves to a fact before the run starts.** After `discover()` returns, every `HostProfile` field is `Present`, `Absent` or `Probed(bool)`; no component ever sees `Unknown`. A declared value is never rewritten as `Probed`, so the field itself says whether a consumer may fall back (contracts d.12; the reactor's RE-I3 and the arena's f.1 read it that way).

**DS-I7. Discovery is idempotent and re-runnable.** Calling `discover()` twice in one process returns equal `Limits` (modulo `devices[].free_bytes`) and performs no persistent side effect (the probe files it creates are deleted).

**DS-I8. A Databricks driver needs an explicit budget.** When `DATABRICKS_RUNTIME_VERSION` is set in the environment and neither a constructor budget nor `AMORU_BUDGET` is given, `discover` returns `Config { name: "budget", msg }` naming the variable and the two ways to set the budget; it does not fall back to the OS value. Rationale: the driver's cgroup limit, when there is one, is the machine's, and the JVM already holds most of it; an OS-derived ceiling is a number the process cannot use (preamble E7).

## d. Interfaces

### d.1 Exposed

```rust
pub struct DiscoveryInput {
    pub explicit_budget: Option<u64>,
    pub explicit_cpu: Option<f64>,
    pub explicit_staging_dir: Option<std::path::PathBuf>,
    pub profile_override: Option<HostProfile>,   // from the surface; env var still parsed underneath
}

pub struct Discovered {
    pub limits: Limits,
    pub profile: HostProfile,          // all fields Present, Absent or Probed(_) (DS-I6)
    /// The run's one host tier (contracts e.1): `TierKind::PinnedHost` when
    /// `limits.devices` is non-empty and `profile.memlock.is_available()`, else
    /// `TierKind::Host`. The facade passes it to the arena (`ArenaConfig::host_tier`,
    /// 02 d.1), which is the owner of `arena.pin`; nothing else decides pinning.
    pub host_tier: TierKind,
    pub cgroup_path: Option<std::path::PathBuf>,
    pub notes: Vec<String>,            // human-readable facts for the run report ("memory.high absent; using 0.9 × memory.max")
}

/// One-shot discovery. Runs probes for Unknown guarantees. Idempotent (DS-I7).
pub fn discover(input: &DiscoveryInput) -> Result<Discovered>;

/// The contract's `Sampler` (d.12), implemented over the cgroup files with interior
/// mutability (a `Mutex` around the open file handles, the fixed read buffer and the
/// running peak). One instance per run, shared as `Arc<dyn amoru_kernel::Sampler>`
/// by the controller and the scheduler.
pub struct Sampler { /* private: Mutex<{ file handles, read buffer, last sample, running peak }>, read_errors: AtomicU64 */ }
impl Sampler {
    pub fn new(d: &Discovered) -> Result<Sampler>;
    pub fn read_errors(&self) -> u64;
}
impl amoru_kernel::Sampler for Sampler {
    /// DS-I3. Never fails after `new`; a transient read error repeats the last sample and increments `read_errors`.
    fn sample(&self) -> Sample;
    /// DS-I3. Writes `memory.peak` when the kernel allows, else resets the running peak to the current `anon_bytes`.
    fn reset_peak(&self);
}

/// Parse `AMORU_HOST_PROFILE`; exposed for tests and for the surface's validation.
pub fn parse_profile(s: &str) -> Result<HostProfile>;
```

### d.2 Consumed

`amoru_kernel::{Limits, Device, DeviceId, LimitSource, Guarantee, HostProfile, Sampler as SamplerTrait, Sample, TierKind, AmoruError}`; `std::fs`; `libc::sysconf` for page size and core count; with `cuda`, `cudarc::driver::CudaDevice::{count, new}` and `mem_get_info`.

## e. Data model, formats and state machines

### e.1 Host profile environment variable

`AMORU_HOST_PROFILE` is a comma-separated list of `key=value` pairs; unknown keys are an error (`Config`), missing keys are `Unknown`. Keys and values:

| Key | Values | Field |
|---|---|---|
| `huge_pages` | `present`, `absent` | `huge_pages` |
| `memlock` | `present`, `absent` | `memlock` |
| `io_uring` | `present`, `absent` | `io_uring` |
| `direct_io` | `present`, `absent` | `direct_io_staging` |
| `gds` | `present`, `absent` | `gds` |
| `rdma` | `present`, `absent` | `rdma` |
| `staging_dir` | absolute path | `staging_dir` |
| `durable_staging` | `present`, `absent` | `durable_staging` (declares that `staging_dir` survives the node: a persistent volume or detachable disk; enables cross-node resume, placement f.13) |

Example for a Griot Cloud pod: `AMORU_HOST_PROFILE=huge_pages=present,memlock=present,io_uring=present,direct_io=present,gds=absent,staging_dir=/scratch,durable_staging=present` (with `/scratch` a persistent volume claim).

Other environment variables read here: `AMORU_BUDGET` (bytes, or a string with `GiB`/`MiB` suffix), `AMORU_CPU` (float), `AMORU_SPILL_DIR`, `AMORU_SPILL_LIMIT`. Constructor arguments take precedence over environment variables; both are "explicit" for DS-I1.

### e.2 Cgroup v2 file map

Resolved from `/proc/self/cgroup` (the line `0::<path>`) under `/sys/fs/cgroup<path>`:

| File | Used for |
|---|---|
| `memory.max` | kill line (`max` → none) |
| `memory.high` | ceiling if set |
| `memory.stat` | `anon`, `file`, `unevictable` for samples |
| `memory.current` | reported in notes only |
| `memory.peak` | `peak_anon_bytes` when present (kernel ≥ 5.19; verify per architecture 2.2) |
| `cpu.max` | `quota period` or `max` |
| `cpu.stat` | `throttled_usec` |

Cgroup v1 (detected when `/sys/fs/cgroup/memory/memory.limit_in_bytes` exists and v2 does not): `memory.limit_in_bytes` (kill line; the unlimited sentinel 9223372036854771712 → none), `memory.stat` (`rss` as anon, `cache` as file), `cpu.cfs_quota_us` / `cpu.cfs_period_us`, `cpu.stat` `throttled_time` (ns). v1 is a fallback with a note; not a supported target.

### e.3 Limits derivation

```
kill      = cgroup memory.max (if not "max") else None
ceiling   = explicit budget if given
            else memory.high if set and < kill
            else 0.9 * kill if kill
            else 0.9 * total RAM (/proc/meminfo MemTotal)
if kill: ceiling = min(ceiling, 0.95 * kill)                 // DS-I2
cpu_quota = explicit if given else cpu.max quota/period else logical cores
page_bytes = sysconf(_SC_PAGESIZE); if transparent huge pages "always" or "madvise" → still 4096 for IO alignment (huge pages affect the arena, not IO alignment)
devices   = cuda enumeration when feature cuda, else empty
source    = Explicit | Cgroup | Os by which branch supplied the ceiling
```

### e.4 Probes (for `Unknown` fields)

| Field | Probe | Present if |
|---|---|---|
| `huge_pages` | read `/sys/kernel/mm/transparent_hugepage/enabled` | value contains `[always]` or `[madvise]`; also `Present` if `/proc/meminfo` `HugePages_Total` > 0 |
| `memlock` | `mlock` one page of a temporary anonymous mapping, then `munlock` | syscall succeeds |
| `io_uring` | `io_uring_setup(8, ...)` then close | syscall succeeds (blocked by seccomp → `EPERM` → Absent) |
| `direct_io_staging` | open a temp file in the staging dir with `O_DIRECT`, write one page from an aligned buffer, read it back, delete | all succeed |
| `gds` | `cuFileDriverOpen` (feature `gds`) else Absent | returns success |
| `rdma` | `ibv_get_device_list` non-empty (feature `rdma`) else Absent | at least one device |
| `staging_dir` | if None: first writable of `$AMORU_SPILL_DIR`, `/local_disk0`, `/scratch`, `$TMPDIR`, `/tmp` | writable and ≥ 1 GiB free |
| `durable_staging` | never probed: `Unknown` resolves to `Probed(false)` (contracts d.12 treats it as `Absent`); a `Present` declaration on a `tmpfs` or `overlay` filesystem (`statfs` magic) is refused with `Config { name: "durable_staging" }` | n/a |

Probes run in this order; each is bounded to 100 ms. For an `Unknown` field the result is recorded as `Probed(true)` or `Probed(false)`; a probe that times out resolves to `Probed(false)` with a note. For a `Present` field the same probe runs and its failure is `Config` (DS-I5); the field stays `Present`. `staging_dir` is not a `Guarantee`: a probe that finds no writable directory leaves it `None`.

### e.5 Sample

`Sample { anon_bytes, file_bytes, peak_anon_bytes, throttled_us, device_used[8], at_ns }` per contracts d.12; `device_used` from `mem_get_info` per device (total minus free) when `cuda`, else zeros.

## f. Algorithms and policies

**f.1 `discover`.** Parse environment; merge with `DiscoveryInput` (explicit wins); if `DATABRICKS_RUNTIME_VERSION` is set and no explicit budget was given, return `Config { name: "budget" }` (DS-I8) before reading anything else; locate cgroup; read e.2; compute e.3; parse profile (e.1); for each `Unknown`, run its probe (e.4) and record `Probed(result)`; for each `Present`, run the same probe and raise `Config` on failure (DS-I5); for each `Absent`, skip; enumerate devices; decide `host_tier` (`PinnedHost` when `devices` is non-empty and `memlock.is_available()`, else `Host`, with a note when a device is present but memlock is not); assemble `Discovered` with notes for every fallback and clamp.

**f.2 `sample`.** Reads `memory.stat` (parse `anon`, `file`, `unevictable` lines only), `memory.peak` if present, `cpu.stat` `throttled_usec`; outside a cgroup, `/proc/self/statm` (RSS anon = `resident - shared` pages × page size) and `throttled_us = 0`. Keep file handles open and `pread` at offset 0 each time to avoid path lookups. Maintain the running peak when `memory.peak` is absent. All of this happens under the sampler's one mutex, held for the duration of the reads only; `reset_peak` takes the same mutex, writes `"reset"` to `memory.peak` when the file is writable (the write fails on older kernels, in which case the sampler notes it once and uses the running peak), and sets the running peak to the current `anon_bytes`.

**f.3 Size string parsing.** `AMORU_BUDGET` accepts an integer (bytes) or `<number><unit>` with units `KiB`, `MiB`, `GiB`, `TiB`, `KB`, `MB`, `GB`, `TB` (decimal); anything else is `Config`.

## g. Concurrency within the component

`discover` is called from the main thread once. `Sampler` is one instance per run, shared as `Arc<dyn amoru_kernel::Sampler>` by the controller thread (its tick) and the scheduler's workers (before and after every `apply`, SC f.2); it is `Send + Sync` through interior mutability (one mutex, never held across anything but the file reads, so a worker's sample waits at most for one other sample to finish). The mutex is outside the preamble's lock order because nothing is called while it is held.

## h. Behaviour

**Normal path (pod).** cgroup v2 found; `memory.max` = 8 GiB, `memory.high` absent → ceiling 7.2 GiB; `cpu.max` `400000 100000` → quota 4.0; profile from env with six `present`/`absent` values; probes for `Present` all succeed; no devices, so `host_tier = Host`; `source = Cgroup`.

**Normal path (laptop).** No cgroup; ceiling 0.9 × MemTotal; quota = cores; profile all `Unknown` → probed; typical result: huge pages `Probed(true)`, memlock `Probed(true)` (small), io_uring `Probed(true)`, direct IO `Probed(true)`, gds `Probed(false)`, rdma `Probed(false)`, staging `/tmp`, `host_tier = Host` (no device).

**Normal path (GPU pod).** One device enumerated and `memlock = Present`: `host_tier = PinnedHost`; the arena pins the whole host region (AR-I6) and every host payload of the run is `PinnedHost`.

**Databricks driver.** `DATABRICKS_RUNTIME_VERSION` is set; with an explicit budget (constructor or `AMORU_BUDGET`) discovery proceeds as on a laptop with `source = Explicit`, clamped to the kill line if one exists; without one it returns `Config { name: "budget" }` (DS-I8) and the run does not start. Elsewhere, a JVM detected through `JAVA_HOME` or a `java` process in `/proc/*/comm` (best effort) adds the note "explicit budget recommended on this host: JVM present" and the OS fallback stands.

**Edge cases.** `memory.high` > `memory.max` (misconfigured): ignore high, note it. `cpu.max` quota below one core (e.g. `50000 100000`): quota 0.5; the scheduler will run one worker. Explicit budget above the kill line: clamped to 0.95 × kill with a warning (DS-I1). Staging dir on a read-only filesystem: `direct_io_staging` `Probed(false)` (or `Config` if declared `Present`) and `staging_dir` None; placement then runs without a disk tier, no manifest is written, and the report says so. A device present with `memlock = Absent` or `Probed(false)`: `host_tier = Host`, a note says device copies will bounce (06 e.2), and the run proceeds.

**Failures.** Unreadable cgroup files (permissions): fall back to OS with a note. `AMORU_HOST_PROFILE` malformed: `Config` (a platform error, fail fast). Probe timeout: `Probed(false)` with a note. Databricks without a budget: `Config { name: "budget" }` (DS-I8).

## i. Configuration

`budget.host`, `budget.device`, `workers.max` (as `cpu_quota`), `page.bytes`, `staging.dir`, `budget.disk` (parsed here, applied by placement), `arena.huge_pages`, `arena.pin` (decided here as `Discovered.host_tier`, applied by the arena), `host_profile`. Range clamping of `budget.host` is owned here (preamble section 5): a value outside the range is clamped and noted.

## j. Observability

`Discovered.notes` is printed in the run report verbatim. `tracing`: `discovery.limits` (info, all fields), `discovery.probe` (debug, per field, result and duration), `discovery.clamp` (warn), `discovery.profile_violation` (error, before the `Config` is returned).

## k. Tests

**DS-T1 precedence.** Matrix of explicit / env / cgroup / OS combinations for budget, CPU and staging dir; the chosen value and `LimitSource` follow DS-I1. Uses a fake cgroup directory (the parser takes a root path).

**DS-T2 ceiling_below_kill.** For kill lines from 256 MiB to 1 TiB and every `memory.high` variant, `ceiling ≤ 0.95 × kill`. DS-I2.

**DS-T3 sample_cost.** (reference host, E1, for the timing; the rest runs anywhere) 100,000 samples in under 2 s on the reference host; `at_ns` strictly increasing; peak tracking correct against a synthetic `memory.stat` that changes between reads; `reset_peak` followed by `sample` reports `peak_anon_bytes == anon_bytes`; 8 threads calling `sample` and `reset_peak` through `Arc<dyn Sampler>` concurrently see no torn sample (every sample equals one of the synthetic states). DS-I3.

**DS-T4 anon_not_current.** Synthetic `memory.stat` with large `file`; `anon_bytes` excludes it. DS-I4.

**DS-T5 present_guarantee_enforced.** Profile declares `io_uring=present` in a test environment where the probe fails (seccomp shim); `discover` returns `Config`. DS-I5.

**DS-T6 no_unknown_after_discover.** All-`Unknown` profile; every field resolves to `Probed(_)`; a profile with `io_uring=present` and the rest `Unknown` resolves `io_uring` to `Present` (not `Probed(true)`) and the rest to `Probed(_)`; `Guarantee::is_available` and `is_guaranteed` agree with the field values. DS-I6.

**DS-T7 idempotent.** Two calls, equal `Limits` except device free bytes; temp probe files absent afterwards. DS-I7.

**DS-T8 profile_parse.** Valid strings, unknown key, bad value, duplicate key; errors name the key. e.1.

**DS-T9 container_gate.** (integration, closes in wave 1: the CI container job with `--memory=2g --cpus=1.5`) `ceiling = 0.9 × 2 GiB`, `cpu_quota = 1.5`, `source = Cgroup`. Parent S5.

**DS-T10 size_strings.** `8GiB`, `8GB`, `8589934592`, `8 GiB` (space: error), `8gib` (case: accept). f.3.

**DS-T11 databricks_budget_required.** With `DATABRICKS_RUNTIME_VERSION=14.3` in the process environment and no budget, `discover` returns `Config { name: "budget" }` whose message names the variable; with `AMORU_BUDGET=4GiB` it succeeds with `source = Explicit`; with the variable unset and no budget the OS fallback stands with a note. DS-I8.

**DS-T12 host_tier.** With a fake device list of one device and `memlock = Present` or `Probed(true)`, `host_tier == PinnedHost`; with no device, or with `memlock = Absent` or `Probed(false)`, `host_tier == Host` and, when a device is present, a note names the bounce path. f.1.

## l. Implementation notes for the agent

Files: `src/lib.rs`, `src/env.rs` (e.1, f.3), `src/cgroup.rs` (e.2, v1 fallback, root path parameter for tests), `src/os.rs` (`/proc/meminfo`, `sysconf`), `src/probes.rs` (e.4, each probe a function returning `Guarantee` plus a note, bounded by a timeout thread), `src/devices.rs` (feature `cuda`), `src/sampler.rs` (f.2, the `amoru_kernel::Sampler` impl), `src/limits.rs` (e.3, DS-I8, `host_tier`). `unsafe` only in `probes.rs` for `mlock`/`io_uring_setup` via `libc`, with `// SAFETY:` comments.

Never cache a cgroup value across `discover` calls (preamble 1.3 row 3). Do not depend on `cgroups-rs` or `procfs` crates; parse the six files directly so the fake-directory tests are exact.

Verify before starting: `memory.peak` presence on the reference host (`uname -r`); whether the CI container runtime blocks io_uring (`DS-T5` needs a way to force failure: use a profile flag `AMORU_TEST_FAIL_PROBE=io_uring` honoured only under `cfg(test)`).

## m. Open items

None.

## n. Traceability

| Parent id | Invariant | Test |
|---|---|---|
| S5 | DS-I1, e.3 | DS-T1, DS-T9 |
| G-I8 | DS-I2 | DS-T2 |
| G-I7 | DS-I5, DS-I6 | DS-T5, DS-T6 |
| section 8 (page cache) | DS-I4 | DS-T4 |
| G-I10 | DS-I1 | DS-T1 |
| E7 (Databricks) | DS-I8 | DS-T11 |
| contracts e.1 (one host tier), d.12 `Sampler` | f.1 `host_tier`, DS-I3 | DS-T12, DS-T3 |

## o. Deferred (post-v1)

None.
