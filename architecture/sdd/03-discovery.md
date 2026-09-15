# Amoru SDD 03: Resource discovery and host profile (`amoru-discovery`)

**Document type:** software design document, component 3 of 12
**Status:** DRAFT · 2026-09-15
**Parent:** `architecture/amoru-runtime-design.md` sections 5.7, 6; criteria S5; global invariant G-I7
**Preamble:** `00-preamble.md`; **Contracts:** `01-contracts.md` d.12 (`Limits`, `Device`, `LimitSource`, `Guarantee`, `HostProfile`, `Sample`)
**Component location:** `crates/amoru-discovery`, Rust
**Consumes:** contracts (1). Feature `cuda` adds `cudarc`. **Consumed by:** arena (2, via config), reactor (6), placement (9), controller (11), python surface (12)

**Decisions worth your eye:** (1) the host profile is one environment variable in `key=value` form, not a file, so a pod spec carries it; (2) under a declared `Present` guarantee the probe is still run once at start and a failure is a `Config` error naming the platform, never a fallback; (3) the budget against which the controller works is anonymous memory plus unevictable, read from `memory.stat`, not `memory.current`, because file-backed pages are reclaimable.

---

## a. Purpose and boundary

Discovery answers three questions and answers them from the host, not from assumptions: how much memory may this process use, how many CPUs may it use, and what accelerators and fast paths does it have. It produces `Limits` once at start, `HostProfile` once at start (declared by the platform, probed where undeclared), and `Sample`s continuously for the controller. It is the only component that reads `/sys/fs/cgroup`, `/proc`, or the CUDA device list.

It owns: cgroup v2 (and v1 fallback) parsing; OS fallback; explicit override precedence; the sampler; host-profile parsing and probing; the page size; device enumeration.

It refuses to know: what the budget is used for (controller); how a fast path is used (reactor, arena); anything after the run starts except sampling.

## b. Vocabulary

**Ceiling.** The memory limit the process must stay under: `memory.high` if set below `memory.max`, else 0.9 × `memory.max`, else 0.9 × total RAM, else the explicit value; the lowest applicable wins.

**Kill line.** `memory.max` when in a cgroup; the value at which the OOM killer acts.

**Quota.** CPU allowance as a fraction of cores: `cpu.max` quota divided by period; `max` means the host's logical core count.

**Guarantee.** The three-state declaration (`Unknown`, `Present`, `Absent`) for each fast path.

**Probe.** A cheap, side-effect-free test that a fast path works on this host, run only for `Unknown` guarantees.

## c. Invariants

**DS-I1. Explicit beats discovered, and discovered beats default.** For each of memory, CPU and staging dir: an explicit value (constructor argument or `AMORU_*` environment variable) is used as given, clamped to the cgroup kill line if one exists and the explicit value exceeds it, with a warning naming both numbers; else the cgroup value; else the OS value.

**DS-I2. The ceiling is below the kill line.** `Limits.memory_ceiling < Limits.memory_kill` whenever the latter exists, by at least 5% of the kill line. Upholds G-I8.

**DS-I3. Samples are cheap and monotonic in time.** One `Sample` costs at most four file reads and no allocation beyond a fixed buffer; `at_ns` strictly increases across samples; `peak_anon_bytes` is the maximum of `anon_bytes` seen by this sampler since start when the kernel does not provide `memory.peak`.

**DS-I4. Budget is anon plus unevictable.** `Sample.anon_bytes` is `anon + unevictable` from `memory.stat`, or RSS anon from `/proc/self/statm` outside a cgroup; never `memory.current`. Rationale: architecture section 8, page cache is charged but reclaimable.

**DS-I5. A `Present` guarantee is verified once and never worked around.** Each `Present` path is probed once at start; a failing probe is `AmoruError::Config { name: "host_profile", msg }` and the run does not start. Upholds G-I7.

**DS-I6. An `Unknown` guarantee resolves to a fact before the run starts.** After `discover()` returns, every `HostProfile` field is `Present` or `Absent`; no component ever sees `Unknown`.

**DS-I7. Discovery is idempotent and re-runnable.** Calling `discover()` twice in one process returns equal `Limits` (modulo `devices[].free_bytes`) and performs no persistent side effect (the probe files it creates are deleted).

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
    pub profile: HostProfile,          // all fields Present or Absent (DS-I6)
    pub cgroup_path: Option<std::path::PathBuf>,
    pub notes: Vec<String>,            // human-readable facts for the run report ("memory.high absent; using 0.9 × memory.max")
}

/// One-shot discovery. Runs probes for Unknown guarantees. Idempotent (DS-I7).
pub fn discover(input: &DiscoveryInput) -> Result<Discovered>;

pub struct Sampler { /* private: opened file handles, last sample */ }
impl Sampler {
    pub fn new(d: &Discovered) -> Result<Sampler>;
    /// DS-I3. Never fails after `new`; a transient read error repeats the last sample and increments `read_errors`.
    pub fn sample(&mut self) -> Sample;
    pub fn read_errors(&self) -> u64;
}

/// Parse `AMORU_HOST_PROFILE`; exposed for tests and for the surface's validation.
pub fn parse_profile(s: &str) -> Result<HostProfile>;
```

### d.2 Consumed

`amoru_kernel::{Limits, Device, DeviceId, LimitSource, Guarantee, HostProfile, Sample, AmoruError}`; `std::fs`; `libc::sysconf` for page size and core count; with `cuda`, `cudarc::driver::CudaDevice::{count, new}` and `mem_get_info`.

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

Example for a Griot Cloud pod: `AMORU_HOST_PROFILE=huge_pages=present,memlock=present,io_uring=present,direct_io=present,gds=absent,staging_dir=/scratch`.

Other environment variables read here: `AMORU_BUDGET` (bytes, or a string with `GiB`/`MiB` suffix), `AMORU_CPU` (float), `AMORU_SPILL_DIR`, `AMORU_SPILL_LIMIT`. Constructor arguments take precedence over environment variables; both are "explicit" for DS-I1.

### e.2 Cgroup v2 file map

Resolved from `/proc/self/cgroup` (the line `0::<path>`) under `/sys/fs/cgroup<path>`:

| File | Used for |
|---|---|
| `memory.max` | kill line (`max` → none) |
| `memory.high` | ceiling if set |
| `memory.stat` | `anon`, `file`, `unevictable` for samples |
| `memory.current` | reported in notes only |
| `memory.peak` | `peak_anon_bytes` when present (kernel ≥ 5.19; verify per preamble 2.2) |
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

Probes run in this order; each is bounded to 100 ms; a probe that times out resolves to `Absent` with a note.

### e.5 Sample

`Sample { anon_bytes, file_bytes, peak_anon_bytes, throttled_us, device_used[8], at_ns }` per contracts d.12; `device_used` from `mem_get_info` per device (total minus free) when `cuda`, else zeros.

## f. Algorithms and policies

**f.1 `discover`.** Parse environment; merge with `DiscoveryInput` (explicit wins); locate cgroup; read e.2; compute e.3; parse profile (e.1); for each `Unknown`, run its probe (e.4); for each `Present`, run the same probe and raise `Config` on failure (DS-I5); for each `Absent`, skip; enumerate devices; assemble `Discovered` with notes for every fallback and clamp.

**f.2 `sample`.** Reads `memory.stat` (parse `anon`, `file`, `unevictable` lines only), `memory.peak` if present, `cpu.stat` `throttled_usec`; outside a cgroup, `/proc/self/statm` (RSS anon = `resident - shared` pages × page size) and `throttled_us = 0`. Keep file handles open and `pread` at offset 0 each time to avoid path lookups. Maintain the running peak when `memory.peak` is absent.

**f.3 Size string parsing.** `AMORU_BUDGET` accepts an integer (bytes) or `<number><unit>` with units `KiB`, `MiB`, `GiB`, `TiB`, `KB`, `MB`, `GB`, `TB` (decimal); anything else is `Config`.

## g. Concurrency within the component

`discover` is called from the main thread once. `Sampler` is owned by the controller thread; not `Sync`; one instance per run.

## h. Behaviour

**Normal path (pod).** cgroup v2 found; `memory.max` = 8 GiB, `memory.high` absent → ceiling 7.2 GiB; `cpu.max` `400000 100000` → quota 4.0; profile from env with six `present`/`absent` values; probes for `Present` all succeed; two devices absent; `source = Cgroup`.

**Normal path (laptop).** No cgroup; ceiling 0.9 × MemTotal; quota = cores; profile all `Unknown` → probed; typical result: huge pages present, memlock present (small), io_uring present, direct IO present, gds absent, rdma absent, staging `/tmp`.

**Databricks driver.** No usable cgroup limit (or one equal to the machine); explicit budget required; if absent, `discover` still succeeds with the OS value but adds a note "explicit budget recommended on this host: JVM present" when `JAVA_HOME` or a `java` process is detected via `/proc/*/comm` (best effort).

**Edge cases.** `memory.high` > `memory.max` (misconfigured): ignore high, note it. `cpu.max` quota below one core (e.g. `50000 100000`): quota 0.5; the scheduler will run one worker. Explicit budget above the kill line: clamped to 0.95 × kill with a warning (DS-I1). Staging dir on a read-only filesystem: `direct_io_staging` Absent and `staging_dir` None; placement then runs without a disk tier and the report says so.

**Failures.** Unreadable cgroup files (permissions): fall back to OS with a note. `AMORU_HOST_PROFILE` malformed: `Config` (a platform error, fail fast). Probe timeout: Absent with a note.

## i. Configuration

`budget.host`, `budget.device`, `workers.max` (as `cpu_quota`), `page.bytes`, `staging.dir`, `budget.disk` (parsed here, applied by placement), `arena.huge_pages`, `arena.pin`, `host_profile`.

## j. Observability

`Discovered.notes` is printed in the run report verbatim. `tracing`: `discovery.limits` (info, all fields), `discovery.probe` (debug, per field, result and duration), `discovery.clamp` (warn), `discovery.profile_violation` (error, before the `Config` is returned).

## k. Tests

**DS-T1 precedence.** Matrix of explicit / env / cgroup / OS combinations for budget, CPU and staging dir; the chosen value and `LimitSource` follow DS-I1. Uses a fake cgroup directory (the parser takes a root path).

**DS-T2 ceiling_below_kill.** For kill lines from 256 MiB to 1 TiB and every `memory.high` variant, `ceiling ≤ 0.95 × kill`. DS-I2.

**DS-T3 sample_cost.** 100,000 samples in under 2 s on the reference host; `at_ns` strictly increasing; peak tracking correct against a synthetic `memory.stat` that changes between reads. DS-I3.

**DS-T4 anon_not_current.** Synthetic `memory.stat` with large `file`; `anon_bytes` excludes it. DS-I4.

**DS-T5 present_guarantee_enforced.** Profile declares `io_uring=present` in a test environment where the probe fails (seccomp shim); `discover` returns `Config`. DS-I5.

**DS-T6 no_unknown_after_discover.** All-`Unknown` profile; every field resolves. DS-I6.

**DS-T7 idempotent.** Two calls, equal `Limits` except device free bytes; temp probe files absent afterwards. DS-I7.

**DS-T8 profile_parse.** Valid strings, unknown key, bad value, duplicate key; errors name the key. e.1.

**DS-T9 container_gate.** (CI job, container with `--memory=2g --cpus=1.5`) `ceiling = 0.9 × 2 GiB`, `cpu_quota = 1.5`, `source = Cgroup`. Parent S5.

**DS-T10 size_strings.** `8GiB`, `8GB`, `8589934592`, `8 GiB` (space: error), `8gib` (case: accept). f.3.

## l. Implementation notes for the agent

Files: `src/lib.rs`, `src/env.rs` (e.1, f.3), `src/cgroup.rs` (e.2, v1 fallback, root path parameter for tests), `src/os.rs` (`/proc/meminfo`, `sysconf`), `src/probes.rs` (e.4, each probe a function returning `Guarantee` plus a note, bounded by a timeout thread), `src/devices.rs` (feature `cuda`), `src/sampler.rs` (f.2), `src/limits.rs` (e.3). `unsafe` only in `probes.rs` for `mlock`/`io_uring_setup` via `libc`, with `// SAFETY:` comments.

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
