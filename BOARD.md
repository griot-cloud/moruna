# Moruna build board

The PM agent's working board. It is the one place that says what is done, what is next and who is waiting on whom; the documents in `architecture/` remain the specification and this board never restates them. When every box on this board is ticked, `moruna` is a production-ready package: every wave gate of preamble 6.6 is green, every sufficiency criterion S1 to S17 is closed on the reference host or listed as skipped by id with Brackly's acceptance, every decision Q1 to Q10 is decided or its assumption is explicitly accepted, and wheels for the four interpreters are published from a tagged release.

## November 11: Griot image registry and Azure validation

- Portable OCI callable descriptors and the registry publisher are prepared for 0.6.5. Twelve real amd64 SOC function images were built and pushed to the user's private Docker Hub repository with `Job.build(registry=...)`; their MJS holds immutable OCI references, no local paths. ARM demo artifacts were preserved.
- Full local Rust tests exposed a late cancellation acknowledgement after terminal completion; the lifecycle now ignores cancellation after the attempt has ended, with a regression test. The 1,653 default tests and feature bridges pass on macOS, but the per-crate coverage gate refuses the push there; the clean Linux/KVM gate in Germany passed default tests and feature bridges, then stopped at the H6 governed write/view test with a twice-failed staging promotion. The exact H6 test passes when rerun alone; the timing-sensitive cause is still under investigation.
- All 13 SOC tasks now run successfully on Germany x86 compute through an owner-scoped Griot CLI preview, peak ten active tasks, using the private Docker Hub images. This does not establish the production node launcher or authoring transport.
- Staging moves now retain the actual failed completion cause (including short reads) through retries instead of reporting only “failed twice”; the existing three move-failure regressions pass. Linux stress verification remains.
- Linux validation found `lx_t17` assuming a 022 umask when checking an imported file's mode. The fixture now explicitly sets the mode it expects the image to preserve. Production permission preservation is unchanged.
- Open integration gap: Griot's cloud host still asks for a Linux guest, external run disks, adjustable resource limits and host-owned sockets that the released native descriptor launcher does not accept. Metadata/publication work does not close that launcher contract or the production authoring transport, leases and credential renewal. Do not claim a deployed cloud flow until those are implemented and exercised.
- Release remains pending the full Linux gate, PR checks and the normal release workflow. No tag has been created by hand.
- Brackly resumed the original deployment assignment after the SOC demo changes. The same Germany West Central x86/KVM validation VM was restarted; the full normal quality gate is running again at `26bc99d`, with the installed-wheel Python gate included. Its previous run was interrupted during default tests by the requested deployment pause; the separate wheel build was likewise terminated, not counted as passing. The old preview warehouse credential stays revoked. An automatic shutdown is configured as a cost backstop; explicit deallocation after verification is still required.

- November 11 resumed gate: default Linux tests stopped at two jail tests with
  `deny setgroups: Permission denied`. A fresh Griot browser x86 task fails at
  the same identity step. Kernel audit proves Ubuntu AppArmor's
  `unprivileged_userns` profile denies namespace capabilities after restart.
  An explicit validation-runner profile and a path-specific preview-engine
  profile are being installed; global restrictions stay enabled. This is not a
  runtime code fix or a completed release gate. Full validation must run again.

- The scoped namespace setup passes five jail tests; ordinary unprofiled namespace
  creation remains denied, and Ubuntu's global restriction remains enabled.
  The Griot browser read/review probe now passes on the private Docker Hub amd64
  functions: 12 processing tasks and one preview capture, peak ten active task
  lifecycles, exit 0. This uses Moruna 0.6.4 and the preview transport, not the
  production VM launcher. Warehouse writes were disabled and canonical findings
  remained unchanged.
- Record for the next runtime investigation: one target-only Review attempt
  refused its budget in ThreatFox stage 7. The report measured 2,310,443,008 peak
  bytes against a 3 GiB ceiling, but the controller estimated a 5,441,201,627-byte
  minimum against 2,245,500,928 bytes of headroom. An isolated trace run passes at
  1,297,575,936 peak bytes and a browser retry at 1,491,501,056. No memory guard
  was weakened and a passing retry is not evidence of a root-cause fix. Comparison
  reports and the Arrow trace are retained outside sample data. Full Linux
  coverage and fresh-wheel acceptance are still pending.

- The resumed Linux gate passed default tests (1,770), append-column, Polars /
  DataFusion, the full governed suite including H6, and Iceberg. Separate pinned
  cargo-deny 0.20.2 bans/sources/licence checks pass. It then stopped because
  an embedded Python integration-test executable could not locate
  `libpython3.14t.so.1.0` from the uv-managed interpreter. The quality runner now
  derives and exports that interpreter's Linux shared-library directory before
  adapter, Python-end, coverage and fresh-wheel checks. Affected integration tests
  are being rerun; no completed full gate or release is claimed.

- Final release review found the missing 0.6.5 changelog section. The normal
  pre-push gate on `bf270fa` was stopped before completion; it is not recorded as
  passing. The release entry is now included and the final clean commit will run
  the complete normal hook again, including Python, coverage and a fresh wheel.
- Griot's read-only x86 Review passes after a real VM restart with cold task
  results: all 12 processing tasks plus preview capture, ten active lifecycle
  tasks at peak, exit 0. Its 4 GiB allocation reports 2,639,396,864 peak bytes.
  The earlier 3 GiB refusal remains open. Griot also found a separate local
  authenticated API stall (health still answered), captured a thread sample and
  recovered its saved draft by restarting only the candidate backend. These
  Griot UI/backend issues do not establish a Moruna launcher or controller fix.

- Final normal pre-push on `84ffae4` passed 1,770 default tests (54 ignored),
  append-column, Polars/DataFusion, governed peQL/Iceberg, and the Python adapter
  and Source/Sink checks. It then failed during instrumented coverage at
  `vsock_backend_seccomp`: the filtered backend dies on
  `prctl(PR_GET_PDEATHSIG)` during process exit. The same uninstrumented test
  passes. The failed hook receipt and syscall trace are retained; coverage and
  fresh-wheel acceptance are incomplete. No push, PR merge, tag or release was
  accepted, and the production syscall filter was not relaxed.
- A separate Griot engine compiled against these actual 0.6.5 crates passes the
  read-only SOC project window: all 12 processing tasks plus preview capture,
  exit 0 in 24.60 s, peak 2,537,074,688 bytes under 4 GiB / two CPUs. Its own
  learned-profile follow-up also passes: 16.80 s, peak 1,592,258,560 bytes.
  The protected demo still uses 0.6.4; warehouse writes were disabled and every
  temporary access credential was revoked and removed.
- A retained older learning-profile set reproduces a zero-byte placement budget
  and ten spilled queues before task execution. Those profiles sum to
  1,338,421,248 state bytes against a 1,283,719,168-byte arena; the successful
  isolated-profile set sums to 1,258,938,368 bytes after two runs. This narrows
  the investigation to learned-state funding, but neither deleting profiles nor
  a successful retry establishes a controller fix. Keep this open for release.

## How the board works

- **Epic** = one wave of preamble 6.6 (E0 to E5), the release-readiness epic (E6) and the documentation epic (E7). Waves are the gate order, not the start order: an executor starts when its crate's dependencies are merged and its fakes exist (preamble 6.6, rule set by Brackly 2026-09-22), so most of E1 to E4 runs concurrently after wave 0; the "Start ladder" below is the actual schedule. Executors run in separate git worktrees. The PM launches every ready feature at once, bounded only by the build host (about six concurrent cargo builds on the 10-core laptop; more on a bigger host).
- **Feature** = one unit of work for one agent in one session: the code plus the tests that prove it, at or above 90% line coverage (preamble 6.7, `tools/quality/check.sh`). A feature is sized so an Opus executor can carry it from brief to a green gate without stopping for the PM, and so that no feature owns more than about a third of a component's invariants.
- **Component branch and pull request.** One pull request per component (CONTRIBUTING.md). A component split into several features lands them as consecutive sessions on the same `component/NN-<slug>` branch; the pull request opens after the first feature and is reviewed against the SDD when the last feature is done (decision D-B1 below).
- **Tasks** under a feature are the checklist the executor and the PM tick. Every feature carries the same closing tasks: the pull request template filled, the PM checklist in `architecture/agents/pm.md` section 4 green, section m of the SDD still empty.
- **Status** per feature: `todo`, `briefed`, `in progress`, `in review`, `returned`, `merged`, `blocked (why)`. The PM updates this file in the same commit as the review or the merge.
- **Test ids** are the SDD's section k ids. A tag in parentheses is the SDD's: `(E1)` skipped and listed by id (device or GDS) or provisional with the host name (timing); `(integration, wave N)` closes in that wave.

Executors are briefed from `architecture/agents/executor.md`; filled briefs live in `architecture/agents/briefs/`.

## Critical path

The one question this section answers: what stands between today and a process that runs a full pass over a dataset larger than its budget. Everything else on this board is either on this path, or is support that the documents require, or is work that can wait. When the two are in tension the critical path wins, and a feature that is not on it does not start before one that is (Brackly, 2026-09-22, after finding the bench suite had been built before anything could use it).

| Step | Crate | State |
|---|---|---|
| 1. The types every component speaks | `moruna-kernel` | merged |
| 2. Memory, limits, the trace | `moruna-arena`, `moruna-discovery`, `moruna-trace` | merged |
| 3. IO that never blocks a worker | `moruna-reactor` | merged |
| 4. Bytes in and out | `moruna-sources`, `moruna-sinks` | merged |
| 5. The queue that makes disk a tier | `moruna-placement` | merged (90.8%) |
| 6. The thing that runs the work | `moruna-scheduler` | merged (91.0%) |
| 7. The thing that decides the sizes | `moruna-controller` | merged |
| 8. The facade that wires 2 to 7 into `Runtime::run` | `moruna-runtime` | merged (94.6%). **Moruna runs**: `rt_t1` takes a real Parquet file through real components to a real Parquet file inside a real budget and asserts 40,000 rows in the report and again by reading the output back; `rt_t6` kills a run and resumes it from its own manifest |
| 9. The surface a user touches | `moruna-py`, `python/moruna` | merged (97.8%). **A user's first program works from a wheel and honours its budget**: 200,000 rows, a Python kernel appending a column, `budget="512MiB"`, peak 0.669 of the ceiling, verified by the PM against a built wheel on 2026-09-23 |

Moruna ran for the first time on 2026-09-22. What is left is step 9 and the defects that first run exposed, which are tracked as F4.9 below: it runs at test sizes and not yet at a realistic budget, and saying otherwise would be the kind of claim this board exists to prevent. The surprises did land where this paragraph predicted, in the wiring rather than in the components.

Support the documents require, which is not on the path and is judged separately: the testkit (contracts d.15, and every component's tests name its fakes, so parallel building depends on it) and the bench suite (preamble 6.5, needed by criterion S3 in wave 5, and built earlier than it should have been).

## Start ladder

What can start when, from the solid edges of preamble 1.3 and the fakes of contracts d.15. A rung starts the moment the rung above it is merged; features on one rung run concurrently.

| Rung | Starts after | Features that start together |
|---|---|---|
| 0 | now | F0.2 contracts (running); F1.6 bench generator (no internal dependency); F7.1 docs scaffold; F6.5 supply chain and DCO checks |
| 1 | F0.2 merged | F0.3 testkit (implements the traits) |
| 2a | F0.2 merged (the contracts crate; the testkit need not wait) | F1.1 arena, F1.2 discovery, F1.3 trace: their software design documents name no fake, so the crate alone is enough (verified by grep, 2026-09-22) |
| 2b | F0.3 merged (the fakes) | F1.4 adapters and F2.1 reactor (`FakeAllocator`), F3.1 sources, F3.3 sinks, F3.5 placement, F4.4 controller (`FakeKnobs`, `FakePlacement`, `FakeSampler`, `FakeTrace`); their integration-tagged tests close in their wave |
| 2, same branch | the previous session of the same component | F1.5 bridges, F2.2 reactor direct paths, F3.2 tensor and iterator sources, F3.4 tensor, IPC and reorder sinks, F3.6 and F3.7 placement staging and lineage, F4.5 controller classification |
| 3 | F3.3 and F3.4 merged (`SinkHandle`, `ReorderBuffer` are a crate dependency) | F4.1, F4.2, F4.3 scheduler |
| 4 | every crate 2 to 11 merged | F4.6 facade, F4.7 integration closure; F5.3 bench runner and tuned baselines; F5.4 engine baseline (needs the facade for the comparison only) |
| 5 | F4.6 merged | F5.1 Python package, F5.2 wheels and clamping walk; F7.2 to F7.7 documentation (can draft against merged crates and finish after F5.1) |
| 6 | wave 5 gate | F5.5 reference-host campaign (D-B3); F6.1 to F6.4, F6.6 release; F7.8 doc gate |

Gates still close in wave order (E0 to E5), each in its wave report, because the later gates cite the earlier ones.


## Releases now (0.6.4, 0.6.5)

Brackly, 2026-11-06: 0.6.4 is what moving the first project (`wazuh_analytics`) onto 0.6.3 found; the big items move to 0.6.5. Nothing is released until the project's whole flow works on a local build of this branch: its tests, a real window against Griot, the job's spec written, its images built, and the built job run. This branch (`release/0.6.4`) carries this section, updated as each item moves. Tests stay Moruna's own: each is a minimal job of Moruna's tasks, nothing of the project.

**Release handoff (2026-11-08): 0.6.4 is not released.** The implementation is in `release/0.6.4`, worktree `/Users/brackly/Desktop/Projects/moruna-guest-fix`; [PR #88](https://github.com/griot-cloud/moruna/pull/88) is open. The latest [CI run](https://github.com/griot-cloud/moruna/actions/runs/42353411137), at `177c1f47a7dc94c81ba93b8194a2eb3296b4a412`, failed in two jobs, detailed below. The previous local flow evidence remains useful, but does not establish a passing release gate. The preserved `f74b4c7` commit only aligns SDD 10 f.4 with the scheduler implementation and is preserved.

**Brackly's clarified direction, 2026-11-08:** finish releasing 0.6.4 and clear its CI failures. No subagents. Do not hunt for unrelated bugs; record any discovered during release work for the next release. The release CI failures below are in scope. Do not bypass the required checks or create a release tag by hand.

### Release CI failures (in scope)

| ID | Evidence and effect | Release change and verification |
|---|---|---|
| REL-1 | `python/tests/test_branches.py::test_function_subclasses_on_separate_branches_run_at_the_same_time`: six 0.5 s branches took 1.599 s on CI, failing the `< 1.5 s` assertion. This assertion includes startup overhead and does not independently establish serialization. | Test overlapping callbacks with bounded synchronization, rather than a wall-clock throughput cutoff. No scheduler change. The fresh installed macOS 3.14t wheel passes the overlap test. Linux CI pending. |
| REL-2 | `python/tests/test_host.py::test_ho_t14_file_run_equals_library_run`: `RunReport.from_bytes(report.json)` refused CLI-written bytes as noncanonical. Confirmed the canonical writer's decimal-tie mismatch: `f64::from_bits(0x3e60000000000000)` was written as `2.9802322387695313e-08`, while Python writes `2.9802322387695312e-08`. The failed report was not retained, so this specific value is not claimed to be that run's offending field. | Use serde_json's shortest-decimal digits, positioned as Python does, preserving strict canonical-byte validation. Deterministic positive/negative halfway cases and the complete `moruna-job` test suite pass locally. The fresh installed wheel passes both host tests (including the real CLI report round-trip) and all six report tests. `moruna-job` line coverage is 91.2% (4,843/5,310). Linux CI pending. |
| REL-3 | `microkernel builds and boots (KB1)`, production-node `cpu4/case3`: the original kernel fault witness was missing. The artifact shows the malformed record refused by the engine, but `lx-badrecord` exited with code 5 first. CPU1/CPU2 recorded the expected kernel fault. The fixture assumed synchronous validation; SDD 29 f.3 makes it asynchronous. | The fixture waits for its next output window after the asynchronous put, keeping it alive until the engine's refusal ends it. Neither the kernel nor the checker is changed. x86_64 C syntax/warnings checks pass locally. CI run 42489712712 passes all 15 production-node cases, 1/2/4-CPU boots, strict deadlines and the complete kernel job. |

| REL-4 | CI run [42489712712](https://github.com/griot-cloud/moruna/actions/runs/42489712712) reaches the Rust tests and fails only `vsock_backend_seccomp::backend_serves_a_whole_session_inside_its_filter`: line 123 connects after checking socket-path existence and receives `ConnectionRefused`. A bound socket path can exist before `listen`. | Both connection points in this test now wait for an actual connection within the existing 10-second bound, retrying only missing/not-yet-listening sockets and refusing a dead backend. The seccomp and complete-session assertions are unchanged. The shared readiness helper compiles locally; full Linux cross-checking is blocked by the absent x86_64-linux-gnu C compiler, so the actual Linux integration run remains CI-owned. |

The original run passed the workspace Rust tests, image-kind tests, peQL/Iceberg tests, CT-T12 and Python-kernel checks before these failures. Run 42489712712 has a green complete kernel job. Its Rust-test failure REL-4 is the next release blocker; the later Python wheel checks have not run yet. The supply-chain/container job is scheduled or manually requested, so its skipped PR status is expected. An unchanged failed-job retry was cancelled when Brackly clarified that these CI fixes are in scope; there is no reason to rerun stale source. Release remains gated by the actual required PR checks, followed by the existing merge-to-main wheel/PyPI/tag workflow in `CONTRIBUTING.md`.

| # | 0.6.4 item | Found by | Status |
|---|---|---|---|
| 1 | Stateful tasks run at once across branches. Six branches of a task sleeping 0.5 s: 3.7 s as `moruna.Function` subclasses, 3.5 s as stateful objects with one instance, 1.9 s with two, 1.0 s as stateless `@moruna.function`. A `Function` subclass is always stateful, so every task a spec can hold runs one stage at a time | the project's branch-concurrency test | fixed (`moruna-scheduler` `instances::home`): each stage's instances were counted from 0, so every single-instance stage's home was worker 0; the run's instances are now dealt to the workers in turn across stages, at init, restore and re-init. `python/tests/test_branches.py` (six branches of 0.5 s in under 1.5 s), two unit tests of the deal |
| 2 | A run's `timestamp` argument takes a Python `datetime`: it is written as `str(datetime)` (`2026-09-05 10:58:00+00:00`) and refused as not RFC 3339 | the window passed as run arguments | fixed (`_args_json`): a `datetime` is written as RFC 3339, a `date` as `YYYY-MM-DD`, any other object JSON cannot write refused by name; every task reads an argument as its JSON value, in process as inside an image. `test_a_timestamp_argument_may_be_a_datetime_and_every_task_reads_its_text` |
| 3 | A schema declaration (`takes`, `gives`) expresses `list<struct<...>>`: today `unknown type list<item: struct<...>> in a schema declaration` | context columns and the case sink | fixed (`moruna-types` `TypeName::Struct`, `moruna-kernel` `declare`): `struct<name: type, ...>`, nested in lists and structs; `test_a_declaration_names_a_list_of_structs`, the grammar's round trip and refusals |
| 4 | A run whose source plans no rows completes with zero rows: today `PlanError: the source plan is exhausted; there is nothing to probe with` | an empty window | fixed (`moruna-controller` `probe`): a fresh run whose plan has nothing to read is seeded from hints with a note, as a resumed one was; SDD 11 f.14 and RC-T20 amended to agree with SDD 10 f.5 MA1; and (`moruna-scheduler` `probe`, found by the project's empty window) a stage no source feeds, with nothing waiting and every source read through, answers the same, where it answered "stage k had no morsel to probe with" and ended the run. `test_a_source_with_nothing_to_read_is_a_run_with_no_rows` (no splits, one and three empty splits, a stage after a join), RC-T20 |
| 5 | A source's first split is processed while its later rows are still being read | the project's streaming test, still failing with splits of one row | fixed (`moruna-sources`: the Python, iterator, stream, Parquet and Vortex sources): a source's `read` did its whole read when the drive issued it and returned a finished future; the drive issues up to its read-ahead before it polls (SDD 10 f.5), so every split waited for the slowest and nothing downstream ran until the source was read through (five 0.3 s reads, the first batch processed at 2.1 s). Each now reads when polled. `test_a_split_is_processed_while_later_splits_are_still_being_read` |
| 6 | The project's whole flow on a local build of this branch: tests, a window against Griot, spec, build, the built job run | | done (2026-11-07): its 45 tests pass on the build; a real and an empty window run against local Griot; its eleven images build from the Linux wheels; its built job, every task an image, runs to its end in a Linux container (Apple's `container`, arm64, `/proc` unmasked, a delegated cgroup) through `moruna run` and through `moruna.run` |
| 7 | Release: the full local checks once, one push, one PR, one release | | in progress: release-CI corrections pass focused local tests; the complete kernel job passes remotely. The newly observed socket-readiness race is fixed in the test, pending Linux CI; publication is still pending. Earlier Linux arm64 evidence includes workspace clippy, 1673 tests, peQL/Iceberg, CT-T12, Python and image tests. The full Mac coverage build previously exhausted disk (`target/llvm-cov/stderr.log`); the changed crate passes 91.2% coverage, without repeating that unbounded build. No required remote check is waived |
| 9 | Moruna can be installed in its own images. A task image installs its lock for `manylinux_2_28` (`_build.PLATFORMS`), but the release publishes Linux wheels only for `manylinux_2_39` (built on ubuntu-24.04) and only for x86_64, so `job.build()` of any task that imports Moruna is refused: "uv could not install the lock for linux/x86_64: platforms: manylinux_2_39_x86_64, macosx_11_0_arm64" (and for linux/aarch64 there is no Linux wheel at all; the vmm job builds aarch64 wheels for the guest and does not publish them). The release builds its Linux wheels for `manylinux_2_28`, x86_64 and aarch64 | the project's `main.py --build` | fixed (`tools/release/linux-wheels.sh`, one recipe for the release runner, with docker, and a Mac, with Apple's container): the wheels are built in the `quay.io/pypa/manylinux_2_28` image as the caller, with libclang from its wheel for bindgen (the runner had clang, the image does not); the release's wheels job builds Linux on `ubuntu-24.04` and `ubuntu-24.04-arm` through it. Proven here: both aarch64 wheels built in a container |
| 10 | An image task of a job whose sink is a task of its own can run. With no `cache` setting an image unpacked under the run's output, a file sink's path, so a job whose sink is its own task was refused at every image: "tasks[0].cache: is needed: the run's output `` is not a directory on this machine to keep unpacked images beside" | running the project's built job | fixed (`moruna-runtime` `kinds::image::unpack_dir`): beside a local output as before, else Moruna's cache (`$XDG_CACHE_HOME/moruna/unpacked`, else `~/.cache/moruna/unpacked`), refused only with neither; a unit test of the rule |
| 11 | `moruna.run` can run a built job's images. The sandbox an image runs in came only from a loaded package or `moruna run --sandbox-cgroup`, so from Python a built job could not run at all | running the project's built job | fixed (`moruna.run(sandbox_python=, sandbox_cgroup=)`, `_core.run`): the run's own sandbox host, refused when it differs from its loaded packages'; `test_a_built_job_runs_its_images_from_python_given_the_sandbox` |
| 12 | A Python image Moruna builds can start in the sandbox. Without a home, CPython's start looks the user up, glibc asks nscd over a Unix socket, and the image filter kills the process for the socket (SIGSYS) before it writes a byte: every Python image `moruna build` makes died at its start, on every architecture. CI never ran one (`test_build_image.py` needs `MORUNA_BUILD_REAL`; the image kind's CI tests use a static program) | running the project's built job, then Moruna's own `test_build_image.py` in a container | fixed (`_build.IMAGE_ENV`: `HOME=/tmp`, the sandbox's one writable place, and `PYTHONNOUSERSITE=1`; the build key names it with the shim, so no image built before is reused) |
| 13 | A source or sink image the filter kills is named. With a fault in its stream, a source's or sink's process was killed before Moruna looked at how it had ended, so a SIGSYS was reported only as "its output is not an Arrow IPC stream ... it wrote nothing to its standard error" | the same | fixed (`moruna-sandbox` `ProcessEnd::wait`): the process has a second to end by itself, and how it ended leads the message, the stream's fault kept as the cause; IM-T30 |
| 14 | A source image can ask its ports while it plans. The run bound its stages' ports after it had built its sources and sinks, and a source image's process starts, and plans, as it is built: its first call was refused, "the kernel called a port outside a run" | the project's built job (its alerts source counts the window through the warehouse port) | fixed (`moruna-runtime` `run::drive`): the run's ports (its cancel and its waits) are bound before any source or sink is built. `test_build_image.py` builds its job from a module of its own: inside an image the task's module is imported, and `moruna` there holds only the task classes, so a job built at that import is refused (`docs/images.md` says so) |
| 15 | A sink image reads what the stage before it gave. An image kernel plans its input's schema (SDD 19 f.6, "the sink adopts the first reply's"), but the stream an image sink reads opened with the planned schema and then wrote batches of another, so the sink's process read a malformed stream (`buffer_index out of range`) and the run said only "Broken pipe" | the project's built job (its agent takes alerts and gives cases) | fixed (`moruna-sinks` `ArrowStreamSink`): the stream adopts the first batch's schema, as the Parquet and Vortex sinks do; a later batch with other columns is refused naming both; a run that wrote nothing ends a stream of the planned schema; a write, start or end the reader refuses is reported with how the reader ended. `the_stream_adopts_the_first_batchs_schema_and_keeps_it`, `a_write_the_reader_refuses_says_how_the_reader_ended` |
| 16 | An image reads every batch it is given. The engine lays a batch as a record whose length runs to the last buffer's page; a buffer of no bytes (a string column of nulls) sits at the next page, so the record runs past the body its message declares. The bridge wrote the whole record to the program, and a stream reader takes those zeros for the end of the stream: the program stopped after its first batch and exited 0 before its input ended ("its input could not be ended: Broken pipe", or its output's window refused) | the project's built job (its agent's input ends with a gap column of nulls) | fixed (`moruna-plane-linux` `stream::stream_bytes`): a stream is given each record only to the end of its declared body. `a_stream_of_records_holds_every_batch_when_a_last_buffer_is_empty`; `test_a_function_image_gives_another_schema_at_its_end_to_a_sink_image` (an image function giving another schema at its end, read by an image sink, its input ending in a column of nulls) |
| 8 | A run lets go of its arena when it ends. A `moruna.Function` subclass's kernel kept the arena of the first run it was in (bound once, "a second call is ignored"), and a reference cycle through the extension (the task holds its kernel, the kernel held the task) kept both for good: every run's arena, a share of the budget (3 to 4.6 GB on this Mac), stayed mapped, until a run was refused because the process held more than its budget (13.9 GB) before it started | the project's test suite, in one process | fixed (`moruna-adapters` `PyKernel::release_run`, `KernelMemory::release`; `moruna-runtime` `ReleaseRun`; `_Instance` holds its task weakly): the arena and gate are bound per run and released when it ends, however it ends, and a task is collected with its job. `python/tests/test_run_releases.py` |

Found, not in 0.6.4 (2026-11-06): with the default budget (0.9 of the host's memory) a run's arena is a third of the allowance, 4.6 GB on this Mac, and every page of it is touched before the run starts (SDD 02 f.1), about 2 s for a run of one alert; a window job pays it every window. Whether the arena should be sized by what the plan needs rather than by the budget is a design question for 0.6.5.

0.6.5 (moved from 0.6.4 on 2026-11-06): the run VM's network (the row below), the per-run jailed process for the vsock services, runtime enforcement of an image task's declared `gives`, the kernel node supervisor's shutdown race, Griot's six launcher items (KF4: boot Griot's Linux guest, attach the run's disk, resize a running VM, a host-made run directory with group access, the vsock back end and QEMU shipped, one job digest), the guest image's kernel and packages cached by content, and the kill-and-resume test that fails under load.

### Wazuh Canvas follow-up : 2026-11-09

- **Live Rust/MJS integration verified (November 10).** Griot's configured
  owner/tenant-bound Apple Container compute now imports/exports lossless MJS,
  persists semantic commands and restores a persistent Rust session after VM
  restart. Existing `default/soc/soc_analytics_demo` was replaced in place by
  version 5 (13 original native tasks; earlier versions retained). Firing
  `40f9a590-da91-4e9d-8dc4-fa8e14b68ece` succeeded; 124 native lifecycle events
  arrived through authenticated cursor SSE. Saved outputs match the independent
  Python graph: 4 review cases, 17 evidence rows, 1 window; evidence IDs verified.
  Browser QA verified concurrent Running cards, node Arrow output, unfinished
  settings recovery after reload, VM-compiled Save, and creating/connecting/
  running a new native flow. Hidden Canvas streams now pause and resume from
  their durable cursor, avoiding HTTP/1 connection starvation; switching away
  during Run and returning replayed all 13 successes. UI dirty comparison
  ignores JSON object key order. Latest app regression: 73 tests passed.

  This is a Griot Rust adapter using released Moruna 0.6.4 crates, without an
  upstream engine patch. General platform gaps remain as listed below. Native
  publication currently supports caller-owned on-demand execution. The final static
  UI is deployed on localhost:8498. Browser firing
  `1f8ffc6f-36b0-4b09-b7ee-4a4a76f57cc0` completed all 13 tasks, with
  124 native events and three simultaneous Running cards captured. The ordered
  alerts/events/native-analytics rehearsal also passed after deployment; output
  parity was verified again. Current checks: 73 Wazuh, 13 Linux session,
  3 journal and 49 focused UI tests passed, plus TypeScript and release builds.
  Temporary test-job heads were removed with version history retained.
  Consolidated receipt: Wazuh `runs/demo/critical-path/native-authoring-acceptance.json`.


- **Rust compute adapter implementation and direct execution proof (November 9).**
  Griot now has separate Rust protocol, durable journal, compute-session and
  Linux-engine crates. The control plane relays semantic commands; Moruna's
  released 0.6.4 Rust APIs read/check/compile MJS on compute. Direct Linux
  execution of the original 13-task Wazuh graph succeeded against the real
  warehouse: 4 cases, 17 evidence rows and 1 window, matching the independent
  reference. Native lifecycle facts show overlapping tasks. Node execution
  materialized 210 alert rows and reused the cached result on its second run.
  Thirteen compute-session tests, three durable-journal tests, 27 focused UI
  tests and the existing 73 Wazuh tests pass. Browser deployment verification
  is still pending the Griot build; this entry does not claim a live UI pass.

  Remaining general-platform integration: immutable remote image staging /
  registry resolution; compute registration and renewable actor credentials;
  worker leases/fencing across control replicas; external source-version cache
  bindings; standardized run-ledger/custody/output-manifest integration; and
  receipt-based idempotent warehouse writes. This demo uses an explicitly
  owner/workspace-bound Linux VM and replaces its three output tables.
  Uncertain executions are never blindly replayed. Existing legacy flows
  remain on their compatibility path until explicitly imported as native MJS.
  The persistent authoring service is a Griot Rust adapter around Moruna 0.6.4,
  not a claim that upstream 0.6.4 already exposes that authoring protocol.


- **Authoring ownership refined by Brackly: event-driven compute sessions.**
  UI/CLI/MCP/SDK send semantic edit commands to a persistent compute-side Moruna
  authoring adapter, not compiler logic in Griot UI/backend. Durable auto-saved
  draft snapshots/journal and deduplicated commands live outside the VM; Save
  compiles on compute and publishes immutable `.mjs`. RunTarget and RunGraph use
  Moruna plans, with revision/input-bound output cache and native lifecycle
  events over existing Kora/projections/SSE. Replay restores edits/status, never
  blindly repeats side effects. A compute disconnect must not lose accepted
  edits or permit stale workers to submit duplicate runs.

  Full proposed protocol, ownership, caching, reconnect/fencing and acceptance:
  Griot `docs/design/moruna-authoring-sessions.md`. It supersedes the earlier
  idea that Griot should construct `.mjs` during save for this native route.
  A compute-side authoring/target-capture capability audit is required; the
  persistent authoring service is not claimed to exist in Moruna 0.6.4. Initial
  Griot IR/transport prototype was preserved under Wazuh ignored
  `runs/demo/critical-path/mjs-prototype/` and removed from the active checkout
  pending this design; prior deployed compatibility fixes/live demo retained.
  No authoring-session implementation was deployed and no prototype test pass
  is claimed (build stopped after the scope changed).


- **Architecture requirement clarified by Brackly: bidirectional native job execution.**
  Canonical Moruna `.mjs` IR must survive Python build → Griot import/edit/export
  → local execution, with complete graph/ports/arguments/needs semantics. Whole
  Canvas runs, including drafts, submit the complete DAG to a version-pinned
  Moruna compute runtime; Moruna owns internal scheduling. Griot resolves
  governed deployment bindings and records authored/execution identity, not a
  second competing scheduler. The current successful segmented SQL/WASM demo
  verifies analytics behavior only, not completion of this contract.

  Griot work: retain complete canonical IR instead of lossy SavedTask projection;
  native DAG submission (existing JobDoc lowering is a one-source/kernel-chain/
  one-sink path); registry-pinned image sources/functions/sinks and artifact
  resolution; governed Warehouse port adapter; integrate Linux image compute;
  reuse existing lifecycle→Kora→projection→execution/watch SSE in Canvas; same
  whole-job path for saved/draft, with separate explicit node preview. Exported
  Griot-specific kinds require the same local runtime plugins and authenticated
  bindings, not stock Moruna alone. Runtime/task/version IDs must remain mapped
  through events and UI. Local source paths should be non-portable inspection or
  explicit remapping, not silently runnable remote references.

  Moruna core DAG/image/port/event capabilities are already exercised by the
  standalone Wazuh native graph. No engine rewrite or version bump is justified
  solely by this integration. Validate supported IR/ABI/event versions on both
  sides. Independently reproduced empty SQL schema loss remains a Moruna fix;
  image finalization output-window race is still an inferred engine defect with
  an application workaround, needing a minimal reproduction. Full acceptance:
  both-direction round trip, concurrent roots, dependency barriers, live events,
  reconnect replay, cancellation/failure, empty input and retry-safe sinks.
  These are required changes/tests, not newly implemented functionality.


- **Open: Canvas draft scheduling and status delivery : Griot ownership.**
  User observed near-sequential cards and stalled/burst updates. Verified saved
  firing `ebd6d2b2-432c-4835-8258-de3cb3fb21ab`: ten source runs overlap, start
  spread 2.026 seconds. Current UI is a modified draft and uses interactive task
  execution instead: whole Play silently changes route when draft differs.
  `pipeline/canvas.rs::advance_task_execution` claims/dispatches one ready task
  then returns, staggering ready siblings across completions/ticks. Fix bounded
  ready-wave concurrency with atomic claims, cancellation and no duplicate sinks;
  label execution modes explicitly. It is not proof of a Moruna DAG bug.

  Interactive cards poll task-run snapshots (200 ms after each response), not
  engine phases. Backend snapshot reads run records sequentially; separate
  per-card launches/staging add latency. Saved-job `useFiringExecution` polls
  every 1.5 s after a complete cycle and waits on all segment projections with
  `Promise.all`; one slow response blocks all updates, one failure clears them,
  and no per-request deadline exists. Existing execution SSE watch is unused by
  these Canvas hooks. Fix independent updates/failure isolation, bounded reads,
  freshness/phase visibility and SSE with cursor recovery. No transport outage
  has been established. No behavioral patch or restart made during user's run.
  Wazuh details: `docs/CANVAS_EXECUTION.md`; timing and screenshot receipts:
  `runs/demo/critical-path/status-investigation/`.


- **Open: empty DataFusion SQL → Arrow IPC loses the schema (0.6.4).**
  Reproduced both in Griot's Canvas task execution and independently with the
  published `moruna[package]==0.6.4` CLI on macOS/Python 3.14. A typed zero-row
  result exits **1**, `sink: the sink has no schema`. Expected: successful empty
  Arrow IPC output retaining column `n: int64`. This blocks empty-window flows;
  it is not specific to Wazuh, the Griot SDK, or a warehouse connection.
  Candidate for 0.6.5 triage; no engine fix was made in this work.

  Minimal legacy job (replace the two absolute directories with writable paths):

  ```json
  {"moruna_spec":1,"run_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","source":{"kind":"datafusion","root":"/absolute/empty-root","sql":"SELECT CAST(1 AS BIGINT) AS n WHERE FALSE","caller":{"id":"empty-repro","tenant":"empty-repro","purpose":"analytics","other":{}}},"sink":{"kind":"arrow_ipc","url":"file:///absolute/empty-output/"}}
  ```

  Run `moruna run empty-sql.json`. Exact reproduction, stdout and report are in
  `/Users/brackly/Desktop/Projects/wazuh_analytics/runs/demo/flow-gaps/`:
  `empty-sql.json`, `empty-sql.log`,
  `moruna-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.report.json`.
  Griot reproduction: `runs/demo/domain-migration/clear-old-samples.log`,
  execution `facc26dd-834a-4381-b51c-ed03fc3a1937`; source task failed and sink
  task remained blocked. No synthetic case was written.

- **Fixed locally: Griot Canvas rejected the 0.6.4 kind catalogue (integration/UI).**
  Chrome at `http://localhost:8498/data/canvas?job=default%2Fsoc%2Fnormalize_wazuh_alerts`
  shows “The engine returned an invalid kind listing: unsupported format version.”
  The canvas stays empty and Validate/Play are disabled. The released CLI
  `moruna kinds` returns `moruna_kinds: 2`, `moruna: "0.6.4"` and the groups
  `sources`, `functions`, `sinks`. Griot's vendored builder still requires
  `moruna_kinds === 1` and reads `kernels` in
  `workspace/vendor/job-builder-example/src/kind-schema.ts:21`.
  Owner: Griot Canvas compatibility with the released Moruna catalogue; do not
  classify this as an engine execution failure. Align the frontend decoder,
  declaration types and forms with v2, then verify graph hydration and Run from
  Canvas. The saved jobs already execute successfully through the SDK.
  Evidence in the Wazuh project: `runs/demo/flow-gaps/runtime-kinds.json` and
  `runs/demo/domain-migration/canvas-kind-listing-gap.png`. Now fixed in the
  Griot integration checkout: v1/v2 decoder plus actual 0.6.4 fixture, 24 focused
  tests passed. Rebuilt served assets match dist. Canvas full analytics execution
  visibly succeeded; `runs/demo/critical-path/canvas-analytics-success.png`.

- **Open: importing the app's native `.mjob` is refused (Griot support gap).**
  Tried `client.job.import_spec("default", "soc", "soc_window", body=Path(...),
  run_as=owner)` with the existing built `soc_window.mjob`, SHA-256
  `8176ece0826345527dcb5587317d4b15bed7e7a067d96d166c8f9994f06f23bc`.
  HTTP 400: `task alerts is a Moruna image source, which Griot does not run yet:
  Griot reads through its contracts, buckets and files`.
  Owner: Griot spec importer/runtime integration, not a corrupt Moruna spec.
  Decide whether to support image sources or explicitly map the app's source
  onto a Griot contract before importing; then evaluate image functions/sink,
  image availability, runtime needs and window parameters. Do not silently
  rewrite or run the case-generating graph. No job/version was created by this
  refused import and no case rows were written.
  Exact response: Wazuh `runs/demo/domain-migration/soc-window-import.json`.

- **Full app import/execution scope : source review, 2026-11-09.**
  Removing the first source refusal is insufficient. Also cover the image sink,
  translate native Moruna image settings into Griot's registry-backed staging,
  and bind the app's `warehouse` need with run-as read/write authorization.
  `crates/runs/src/image.rs` explicitly describes current Container tasks as
  having no ports; the Wazuh app supplies `WarehouseServer` through Moruna ports.
  Preserve job argument declarations (`window_start`/`window_end`), inline task
  definitions and named input/output edges: current `from_spec` retains a task
  subset and dependencies, while `to_spec` rebuilds default ports/edges and a
  fresh `Job`. These are compatibility requirements found by code inspection,
  not additional runtime failures already demonstrated. Verify full import /
  export fidelity and one actual Linux image-graph run, including empty input,
  denied warehouse access and retry-safe case/evidence writes.

  **Proposed import policy (Brackly raised registry/local handling):** portable
  runnable imports require explicit registry/repository plus immutable digest;
  reuse Griot's workspace registry credentials and verified OCI staging rather
  than inventing a second credential system. Reject submitter filesystem paths,
  `file://` layouts and local-daemon image names with an actionable publish
  instruction. A private/internal registry remains valid if configured and
  reachable by compute; it is not the same as a local filesystem image.
  Import must not execute tasks, install credentials or grant requested needs.
  Missing images, unsupported platforms or unbound needs must prevent execution.
  Optionally support a clearly non-runnable inspect-only import for local/draft
  specs; this is a product proposal, not implemented policy. Existing Moruna
  standalone local-layout use remains valid.

- **Portable analytics admission gaps : 2026-11-09.**
  Moruna check cannot synthesize struct values inside an input list; Griot
  refuses that package as not checkable. Moving to column-wise arrays passes
  schema checks, but costing then fails on `list<item: list<item: string>>`:
  verdict `agreed`, exit 2, warm-up error names `rule_groups`. Independently
  reproduced with the released CLI (`moruna check <package> --json --budget 3`).
  Evidence: Wazuh `runs/demo/critical-path/analytics-build.log` and
  `pack-warm-check.json`. Candidate checker/costing coverage work; no engine
  bypass or patch made. Demo integration instead uses ordinary source records
  serialized into a flat staging representation, with explicit marked empty
  control rows to retain the SQL source schema.

  The original image spec was also probed directly on macOS: config `sandbox`
  refuses because image commands require Linux. This is an expected platform
  limitation, not another engine bug. `image-probe.log` records the exact result.
  A capture-only run of the existing Python graph against SOC succeeded for
  2026-09-05 10:38–10:39 UTC: 210 alerts, 428 related events, 4 offline review
  cases and 17 evidence rows; warehouse writes zero. Portable flow must match
  that reference before it is called ready (`reference-run.json`).

- **Costing large WASM copy batches : observed during analytics admission.**
  A stateless record serializer passed schema agreement but costing failed at
  `1000000 rows (Skewed keys)` with `fuel allocated for hostcalls has been
  exhausted`. The same package's bounded `moruna check --budget 3` passed;
  the Griot admission check runs the default budget and reached that larger
  batch. Evidence: Wazuh `runs/demo/critical-path/recent-registration-check.json`
  and `recent-check.json`. Triage realistic morsel sizing / host-copy fuel during
  costing; do not bypass registration. The demo serializer was made more compact
  by encoding values in declared column order instead of repeating field names
  per record, then resubmitted for normal admission.

- **Original image graph on Apple Container : 2026-11-09.**
  Apple Container supports the Linux image runtime: user namespaces and a
  writable cgroup v2 hierarchy are available. In a dedicated VM, delegate CPU /
  memory controllers after moving supervisor processes to a leaf. Nested proc
  mounting needs `--cap-add SYS_ADMIN --masked-path NONE --read-only-path NONE`;
  these settings are for this private disposable VM, not a shared host. Task
  images remain confined by Moruna and access the warehouse only through ports.
  `apple-container-image-nested.log` proves the real source reached its port.

  The full original graph then read real Griot data and reached `agent`, but
  failed with `its output's window of 891043840 bytes: Unknown("output")`.
  Evidence: Wazuh `runs/demo/critical-path/image-spec-real-data-2.log`.
  Source review: image finish grants another output window after consuming its
  final extra batch; the plane can already have removed the ended task's
  outputs. This is an inference pending a minimal race reproduction, not an
  engine fix. The app now uses a declared alert-count boundary so the agent
  emits on its last data batch, retaining whole-window grouping and evidence
  checks. Native image execution succeeded with real Griot reads; 4 cases,
  17 evidence rows and 1 window match Python (capture only, zero writes).
  The final 73-test application suite, including the 1,001-alert split regression,
  passed. Empty image windows remain unverified.
  No Moruna source was changed.

- **Griot flow follow-up fixes / limits : 2026-11-09.**
  Connected-input SQL previews cast nulls using Arrow Debug type strings, which
  include Field structs for lists and are not parseable. The Wazuh graph check
  refused its final three projections with `unknown token: Field`. Changed to
  Arrow Display (the parseable grammar); rebuilt and full graph checks pass.
  A reused Cargo build-script executable also embedded another checkout's web
  assets: use runtime CARGO_MANIFEST_DIR in `crates/griot/web_embed.rs`. Served
  JS digest did not match current dist; rebuilt deployment now matches dist.
  Constant-only pipeline SQL was refused a disk key because it names no storage
  contract. Empty VT/OpenVAS warehouse snapshots were initialized with
  zero rows so these source branches can read their actual governed contracts;
  no fabricated observations are inserted. The constant-SQL/storage planning
  case remains a downstream Griot limitation.

- **Complete Wazuh demo flow verified : November 9, 19:01 EAT.**
  The 29-task SQL/WASM `default/soc/soc_analytics_demo` v2 ran from Canvas and
  succeeded through all 14 execution stages. Persisted 4 cases / 17 evidence /
  1 window match the independent Python graph except run timestamps; cited IDs
  exist in actual Wazuh rows. A separate repeat preserves output counts and IDs.
  Then both raw normalizers and analytics were rehearsed in order, all succeeded:
  744 alerts / 1,578 events → 4/17/1. Final firing
  `ebd6d2b2-432c-4835-8258-de3cb3fb21ab`; Wazuh receipts under
  `runs/demo/critical-path/{rehearsal,persisted-output-verification}.json`.
  Native `.mjs` image parity is separately verified in `image-capture/parity.json`;
  direct native-spec Griot import/registry/warehouse binding remains open.

  Griot runtime configuration fixes: the first large empty-key source hash join
  exceeded its default plan allowance; the fixed demo window now skips scans
  when its indicator set is empty and uses named 1 CPU / 1 GiB compute. A later
  fanout hit `too many open files` with launchd's 256 limit. Raising the local
  service soft/hard limit to 8,192 and restarting resolved it; three subsequent
  complete analytics runs succeeded. These are Griot configuration/query-planning
  findings, not Moruna engine fixes. Output replacement is demo-scoped, not a
  multi-table transaction or production retention/exactly-once guarantee.


- **Verified nonempty saved-job backend path:** raw Wazuh alerts/events → SQL deduplication
  → registered WASM normalizer → contract sink in overwrite mode. Both saved
  Griot jobs succeeded on Moruna 0.6.4. All 744 alert and 1,578 event output rows
  match the Python reference by complete row hashes. All original 1,446 alert
  and 3,061 event rows remain in raw tables, including replayed and malformed
  input. Case generation is a separate flow still to build; prior demo case
  output was cleared at Brackly's request.

- **Downstream ownership (Griot SDK / ADBC):** SDK `table.write` exposes append
  only although the backend supports replacement. An empty reader sends no
  Flight frame (`DoPut with no frame`); explicitly supplying one zero-row batch
  lets ADBC `mode="replace"` clear a table. This workaround cleared the prior
  sample outputs. Track it as SDK ergonomics, not a Moruna engine defect.

## Now

| | |
|---|---|
| Where the code is | ten crates merged to `main`: contracts, testkit, arena, discovery, trace, adapters, the two engine bridges, reactor, sources, sinks, controller, and the bench suite. 470 tests pass, every crate above 90% line coverage |
| Delegation | Brackly delegated decision making to the PM on 2026-09-22 ("with great power comes great responsibility"); the PM now decides every item the preamble's table routes to the human, records each in this board's decisions table with the date, and reports it in the wave report; a decision that changes an SDD's behaviour still goes through the design-change template first |
| In flight | F5.1, the Python surface, and F4.9, the six defects the first real run exposed |
| Next to start, when work resumes | F4.6, the facade `moruna-runtime`. Every crate it wires is now merged, so it is unblocked: it turns twelve parts into `Runtime::run` and is the first time anything runs end to end. Its executor reads every SDD (preamble 9) |
| After that | F4.7 integration closure (RC-T12, SC-T16, PL-T17 with real components, which is what closes the wave 4 gate), then wave 5 (the Python package, the tuned and engine baselines) and E7 documentation |
| Paperwork owed | the wave 1, 2, 3 and 4 gate reports. Each needs one run of the weekly `slow` job from the Actions tab, which is what executes DS-T9, PL-T6 and the other container-gated tests; the crates are merged and green, the reports are what record it |
| Then | F4.6 the facade and F4.7 integration, once placement and the scheduler are in; after those, wave 5 (the Python package, the baselines) and E7 documentation |
| Gates still to close | wave 1, 2 and 3 reports, each needing one run of the weekly `slow` job from the Actions tab, which is what executes DS-T9 and the other container-gated tests. The crates are merged; the gate reports are the paperwork that says so |
| Blocking Brackly items | none; D-B3 (reference host access) is needed at wave 5 |

---

## E0. Wave 0: contracts, testkit, workspace, CI, quality gate

Gate (preamble 6.6, wave 0): the workspace compiles with every member crate as a stub; `moruna-kernel` has no runtime dependency (CT-T12); every fake in contracts d.15 compiles and exercises every knob (CT-T13); CT tests pass; the trace schema hash is pinned (CT-T9); `tools/lint/no_tier_wildcard.sh` runs in CI (CT-T14); `tools/quality/check.sh` is green; the four CI jobs are green on the stubs.

| Id | Feature | Branch | Scope (SDD sections, tests by id) | Status |
|---|---|---|---|---|
| F0.0 | PM preparation: quality gate, hook, board, brief for component 1 | `infra/pm-wave0-prep` | preamble 6.1, 6.6, 6.7 edits; `tools/quality`, `tools/hooks`; this board | merged |
| F0.1 | Workspace skeleton, CI with four jobs, `tools/lint`, pinned versions | `infra/workspace` | preamble 6.1, 6.2, 6.3, 6.6; CT-T12, CT-T14 (the lint half) | merged |
| F0.2 | Contracts crate `moruna-kernel` | `component/01-contracts` | 01 d.1 to d.14, e.1 to e.7, f.1 to f.6; CT-T1 to CT-T12, CT-T14 to CT-T19 | merged (121 tests, 93.6%) |
| F0.3 | Testkit `moruna-testkit` | `component/01-contracts` (its own PR, per the amended 6.7) | 01 d.15; CT-T13 | merged (91.6%) |
| F0.4 | Wave 0 gate and wave report (PM) | `main` | pm.md sections 2, 4, 8 | todo |

### F0.0 tasks
- [x] `tools/quality/check.sh`: em dashes, fmt, clippy, lint, tests, per-crate coverage at 90% (`coverage_gate.py`)
- [x] `tools/hooks/pre-commit` and `tools/hooks/install.sh` (`core.hooksPath`); installed in this clone
- [x] Preamble 6.1, 6.6, 6.7, CONTRIBUTING, PR template, pm.md, executor.md carry the gate and the 90% floor
- [x] `DECISIONS.md` Q8 date column filled
- [x] `BOARD.md` (this file) with every epic, feature and task
- [x] Filled brief for component 1 in `architecture/agents/briefs/01-contracts.md`
- [x] D-B1 and D-B2 decided under delegation, D-B3 deferred to wave 5; branch merged to `main` and pushed to origin

### F0.1 tasks
- [x] Environment facts verified in the PR (rustc, cargo, edition 2024; docker and MinIO reachable in CI)
- [x] `rust-toolchain.toml` pins stable; `Cargo.toml` workspace with every member of preamble 6.1; `[workspace.dependencies]` pinned; the versions recorded in the preamble 6.2 table (replaces `_pending_`)
- [x] Every crate of 6.1 as a compiling stub with its `Cargo.toml`, feature flags per 6.3, and a one-line `lib.rs` doc comment naming its SDD
- [x] `python/moruna/` and `bench/` placeholders that the later waves fill
- [x] `tools/lint/no_tier_wildcard.sh` (CT-T14 lint) with a self-test fixture
- [x] `.github/workflows/ci.yml`: job 1 `tools/quality/check.sh` on Linux; job 2 container gate with `--memory` and `--cpus`; job 3 MinIO service for the object-store paths; job 4 Python matrix 3.13, 3.13t, 3.14, 3.14t (proves the matrix runs)
- [x] `cargo tree -p moruna-kernel` is free of tokio, cudarc, pyo3, parquet, object_store (CT-T12 as a CI step until F0.2 makes it a test)
- [x] PR template complete; PM checklist green; merged

### F0.2 tasks
- [x] Environment facts from 01 section l verified and recorded (cargo version, `blake3`, `dlpark` versioned-struct support)
- [x] `src/` files exactly as 01 section l lists them; `unsafe` only in `tensor.rs`, `payload.rs`, `view.rs`, `buffer.rs` with `// SAFETY:` on every block
- [x] d.1 to d.3: ids, `Tier`, `TierKind`, `StagingCodec`, `Buffer`, `BufferView`, `Allocator`, `AllocStats`
- [x] d.4 to d.5: `DType`, `ManagedTensor` (dlpark wrapper, `from_buffer`), `Payload`, `PayloadSpec::check`, `SourceSchema::hash`, `Morsel`, `MorselFeatures` (f.2 O(columns))
- [x] d.6 to d.8: `Source`, `Kernel`, `KernelState`, `Sink` with the resume defaults that refuse
- [x] d.9: `Completion` over std, `Reactor`, `ObjectMetadata`, copy endpoints; d.10 `Placement` and the resume types; d.11 `Knobs`, `StatsSource`, `Prober`, `CancelToken`, `RecordHook`; d.12 `Limits`, `HostProfile`, `Sampler`; d.13 `TraceRecord`, `SCHEMA_HASH`, `TraceSink`, `TraceTail`; d.14 errors
- [x] e.4 `MRB1` reader and writer over slices; e.7 page-aligned IPC `encode_framing` and `decode`; e.6 `Fingerprint::compute`
- [x] Tests CT-T1 to CT-T12 and CT-T14 to CT-T19 under their SDD names; CT-T9 pins the hash value in the test; CT-T11 timing reported with the host name (see finding N-2)
- [x] Every CT invariant CT-I1 to CT-I12 cited by at least one test in the PR
- [x] Coverage at or above 90% for `moruna-kernel`; PR template complete

### F0.3 tasks
- [x] Every fake of d.15 (`FakeAllocator`, `FakeReactor`, `FakePlacement`, `FakeSource`, `FakeSink`, `FakeKernel`, `FakeSampler`, `FakeTrace`, `FakeKnobs`) with exactly the knobs and observables the table lists, and a `shutdown_calls` counter wherever the trait has `shutdown`
- [x] `FakeAllocator` buffers are real heap allocations tagged with the tier so tier inference, `into_arrow_buffer` and `BufferView::of_arrow` work
- [x] `FakePlacement::with_manifest_store` round-trips `checkpoint`/`restore` across engine instances
- [x] CT-T13: every method and every knob exercised once
- [x] `moruna-testkit` depends on `moruna-kernel` only; coverage at or above 90%; same PR as F0.2

### F0.4 tasks
- [x] PM review of F0.1 and F0.2+F0.3 against pm.md section 4, lines 1 to 11
- [ ] Wave 0 report: tests run, tests skipped by id, provisional figures with host names, escalations opened
- [ ] E1 environment items for wave 1 recorded (Python interpreters with NumPy and pyarrow, cgroup v2 host)
- [ ] SDD 01 listed for Brackly to flip to HANDOFF-READY once E1 and E2 assumptions are accepted

---

## E1. Wave 1: arena, discovery, trace, adapters, bench generator and kernels

Gate (6.6, wave 1): AR, DS, TR, AD tests pass against fakes; G-I2 for the Python adapter (zero payload copies); S13 partial; the generator writes every dataset shape the suite names to local disk and MinIO. Environment: wave 0 plus the four interpreters with NumPy and pyarrow, and a cgroup v2 host (a container is enough) for component 3.

| Id | Feature | Branch | Scope | Status |
|---|---|---|---|---|
| F1.1 | Memory arena `moruna-arena` | `component/02-arena` | 02 in full; AR-T1 to AR-T12; `cuda` arms present, device tests listed | merged (93.4%) |
| F1.2 | Resource discovery `moruna-discovery` | `component/03-discovery` | 03 in full; DS-T1 to DS-T12; DS-T3 timing provisional (E1); DS-T9 (integration, wave 1: the CI container job) | merged (96.3%) |
| F1.3 | Trace writer and run report `moruna-trace` | `component/04-trace` | 04 in full; TR-T1 to TR-T11 | merged (92.2%) |
| F1.4 | Python kernel adapter `moruna-adapters` | `component/05-adapters` | 05 in full except the bridges; AD-T1, T2, T4 to T8, T12; AD-T3, AD-T11 (E1, skipped and listed) | merged (95.0%) |
| F1.5 | Polars and DataFusion bridges `moruna-polars`, `moruna-datafusion` | `component/05-adapters` (same PR) | 05 bridge sections; AD-T9, AD-T10 (integration, wave 1; need F1.7's `normalise` kernel) | merged (polars 94.5%, datafusion 98.8%) |
| F1.6 | Bench data generator | `infra/bench` | preamble 6.5: Parquet with controllable rows, column mix, null ratio, row-group size, to local disk and MinIO; safetensors and aligned binary tensors | merged (96.9%) |
| F1.7 | Bench kernels | `infra/bench-kernels` (its own PR; bench is not a component, so the one-pull-request-per-component rule does not bind it) | preamble 6.5: identity, normalise, tokenise-explode, adversarial, wide-intermediate (Python, releases the GIL), embed-score; torch-score deferred (no GPU host, E1) | merged |
| F1.8 | Wave 1 gate and report (PM) | `main` | | todo |

### F1.1 tasks
- [x] Environment facts from 02 section l verified; `unsafe` only in the modules 02 section l names
- [x] `Allocator` implemented per 1.3's edge contract: 64-byte alignment, page alignment above a page, tier honoured or the call fails; `is_pinned` all or nothing
- [x] Size classes, free lists and the lock-free versus mutex decision recorded with the benchmark that justified it (preamble 4.2)
- [x] `contains`, `tier_of`, `AllocStats` exact; small-budget mode; large coalescing
- [x] AR-T1 to AR-T12 under SDD names; every AR-I cited; coverage at or above 90%; PR template complete

### F1.2 tasks
- [x] Environment facts from 03 section l verified (cgroup v2 paths in a container, `memory.peak` presence)
- [x] `discover`: precedence explicit, cgroup, OS; ceiling below kill; `LimitSource`; size strings; Databricks refusal (E7 assumption)
- [x] `HostProfile` parsing from `MORUNA_HOST_PROFILE`; every `Unknown` becomes `Probed`; `Present` on tmpfs or overlay staging refused
- [x] `Sampler` with `reset_peak`, anon not current, idempotent, sample cost measured and labelled with the host
- [x] DS-T1 to DS-T12; DS-T9 wired into the CI container job; every DS-I cited; coverage; PR template

### F1.3 tasks
- [x] Environment facts from 04 section l verified
- [x] Bounded channel, writer thread, in-memory chunks with `trace.memory_limit` overflow to disk, flush on `finish` and on abort, `TraceTail`
- [x] Run report as a pure function of the trace and the limits (formulas of 04), staging versus source bandwidth, disk-full handling
- [x] TR-T1 to TR-T11; every TR-I cited; coverage; PR template

### F1.4 tasks
- [x] Environment facts from 05 section l verified (the four interpreters, NumPy, pyarrow, pyo3 at or above 0.28)
- [x] Python callable as `Kernel`: zero-copy crossing both ways, one boundary copy of non-arena output (AD-I2, `boundary_copies_total`), deleter attaches to the interpreter, exception context, fingerprint from qualified name plus source hash, class-kernel shape
- [x] GIL detection and flip reported per stage (`GilState`)
- [x] AD-T1, T2, T4 to T8, T12 pass; AD-T3 and AD-T11 exist, ignored with the E1 reason, listed; every AD-I cited; coverage; PR template

### F1.5 tasks
- [x] `moruna-polars`: a kernel as a Polars expression plugin, no kernel logic in the wrapper; `moruna-datafusion`: a kernel as a `ScalarUDF`
- [x] AD-T9, AD-T10 against the bench `normalise` kernel (after F1.7 merges); S7 closed for the two hosts
- [x] Both crates depend on `moruna-kernel` plus the host engine only; feature flags per 6.3; coverage; same PR as F1.4 or a follow-up commit on the branch

### F1.6 tasks
- [x] `bench/` generator: 12 suite datasets and four ad hoc commands, deterministic by seed, local and S3
- [x] Output names the machine and the generator version; `bench/README.md` documents every dataset, scale and variable
- [x] 88 tests, 96.2% line coverage
- [x] The MinIO half runs in CI: the `minio` job now exports `MORUNA_S3_BUCKET=moruna-ci` (fixed by the PM after PR #3 merged), so the S3 test no longer skips; the wave 1 gate clause "and MinIO" closes when the next run on `main` is green

### F1.7 tasks
- [x] Six kernels with their declared amplification classes; `wide-intermediate` in Python releasing the GIL
- [x] Each kernel usable by AD-T9/T10 and by the wave 4 end-to-end tests
- [x] Tests and coverage; PR with F1.6

### F1.8 tasks
- [ ] PM reviews five PRs against pm.md section 4; merges; wave 1 report; E1 items for wave 2 (an `O_DIRECT` filesystem, io_uring where allowed)
- [ ] SDDs 02 to 05 listed for Brackly to flip

---

## E2. Wave 2: IO reactor

Gate (6.6, wave 2): RE tests pass; direct IO and the fallback both exercised on the developer host; G-I7. Environment: wave 1 plus an `O_DIRECT` filesystem (ext4 or xfs on a local disk), io_uring where the host allows, MinIO; GDS and CUDA tests tagged for the reference host (E1).

| Id | Feature | Branch | Scope | Status |
|---|---|---|---|---|
| F2.1 | Reactor core: completions, file and object IO, concurrency bounds, shutdown | `component/06-reactor` | 06 d, f (except f.5), g, h; RE-T1, T2, T3, T6, T7, T8, T9, T13, T14, T15 | merged (93.3%) |
| F2.2 | Reactor direct paths: direct IO, io_uring, sticky fallback, copy table with `cuda`/`gds`/`rdma` arms | `component/06-reactor` (same PR) | 06 f.5, section l FFI notes; RE-T4, T10, T12; RE-T5 (E1, skipped and listed); RE-T11 (E1, provisional with host name) | merged (same branch) |
| F2.3 | Wave 2 gate and report (PM) | `main` | | todo |

### F2.1 tasks
- [x] Environment facts from 06 section l verified (filesystem accepts `O_DIRECT`, io_uring probe result, MinIO reachable)
- [x] tokio reactor with `reactor.threads`; every operation completes exactly once into the buffer it was given, on a reactor thread; submission never blocks; `then` runs on the reactor thread
- [x] `read_file`, `read_file_opt`, `write_file`, `read_object`, `write_object`, multipart, `ObjectMetadata`, segment registry, `paths()`, `shutdown` within the longest operation
- [x] RE-T1, T2, T3, T6, T7, T8, T9, T13, T14, T15; every RE-I cited; coverage; PR template

### F2.2 tasks
- [x] Direct IO when aligned, buffered otherwise; io_uring path behind `uring` with byte-equal fallback; sticky fallback after a failure; `Probed` versus `Present` behaviour (G-I7)
- [x] Copy table f.5 with every arm named: `cuda` copy engine rows, `gds` rows over the minimal `libcufile` FFI (section l), `rdma` rows returning `Unsupported("rdma")`
- [x] RE-T4, T10, T12 pass; RE-T5 skipped and listed; RE-T11 measured and labelled provisional with the host name
- [x] Coverage for the non-device paths; PR template; same PR as F2.1

### F2.3 tasks
- [ ] PM review, merge, wave 2 report; E1 items for wave 3 (10 GiB free local disk, MinIO)
- [ ] SDD 06 listed for Brackly to flip

---

## E3. Wave 3: sources, sinks, placement

Gate (6.6, wave 3): SO, SI, PL tests pass; S10 and S15 with `FakeSink` throttle; G-I3; manifest round trip and sink commit tracking (PL-T16, SI-T12, SI-T13). Environment: wave 2 plus 10 GiB free for the staging tests and MinIO. Three executors in parallel.

| Id | Feature | Branch | Scope | Status |
|---|---|---|---|---|
| F3.1 | `ParquetSource` over local paths and object stores | `component/07-sources` | 07 a to h for Parquet; SO-T1 to T8, T11, T13 to T16; SO-T12 (E1, provisional) | merged (91.7%) |
| F3.2 | `TensorSource` (safetensors, MRB1) and `PyIteratorSource` | `component/07-sources` (same PR) | 07 tensor and iterator sections; SO-T9; SO-T10 (integration, wave 3; `python`) | merged (same branch) |
| F3.3 | `ParquetSink`, `SinkHandle`, commit tracking and resume | `component/08-sinks` | 08 a to h for Parquet, e.5, resume; SI-T1 to T4, T7, T11 to T16; SI-T8 (integration, wave 3) | merged (93.8%) |
| F3.4 | `TensorSink`, `ArrowIpcSink`, `ReorderBuffer` | `component/08-sinks` (same PR) | 08 e.3, reorder; SI-T5, T9; SI-T10 (integration, wave 3); SI-T6 (E1, skipped and listed) | merged (same branch) |
| F3.5 | Placement core: queues, accounting, admission, head hot, promotion window, move table | `component/09-placement` | 09 c, d, e.1, e.4, f (promotion and demotion), g; PL-T1 to T4, T8, T9, T11, T19 to T21, T24 | merged (90.8%) |
| F3.6 | Placement staging: segments, record format, disk bound, Q0 eviction and replace, move failures | `component/09-placement` (same PR) | 09 e.2, e.3, f.5 to f.10, h; PL-T5 to T7, T10, T12, T14, T15; PL-T13 (E1, provisional) | merged (same branch) |
| F3.7 | Placement lineage and manifest: checkpoint, restore, lineage index, lock order, shutdown | `component/09-placement` (same PR) | 09 e.5, f.11, f.12; PL-T16, T18, T22, T23; PL-T17 (integration, wave 4) | merged (same branch) |
| F3.8 | Wave 3 gate and report (PM) | `main` | | todo |

### F3.1 tasks
- [x] Environment facts from 07 section l verified; bench generator files available in test setup (F1.6)
- [x] `plan` complete before the first `read`, exact metadata from the footer, row-group sub-splitting with `RowSelection`, pruning, one decode copy into the arena in the requested tier, zero-row and oversized-row handling, schema mismatch, read failure paths, local paths through `read_file` only
- [x] SO-T1 to T8, T11, T13 to T16 pass; SO-T12 provisional with host name; every SO-I cited; coverage; PR template

### F3.2 tasks
- [x] `TensorSource`: safetensors header parse, mapped bytes, aligned copy only when the offset is unaligned (architecture 2.2 row), MRB1 read; `PyIteratorSource` with `repeatable() == false`
- [x] SO-T9 passes; SO-T10 under the `python` feature; coverage; same PR as F3.1

### F3.3 tasks
- [x] Environment facts from 08 section l verified
- [x] `ParquetSink`: ownership once, encode-only copy into the arena, row-group and file roll targets, `finish` exactly once, no partial final file, `SinkSummary` exact, `sink.concurrency`, slow store behaviour
- [x] `committed_seq` never overstates (SI-I8), `skip`, `checkpoint` returns `Some` from open, `resume` discards uncommitted output; the non-resumable path says so (SI-T14)
- [x] `SinkHandle` for the scheduler (SI-T15)
- [x] SI-T1 to T4, T7, T11 to T16 pass; SI-T8 closes when `ParquetSource` merges; every SI-I cited; coverage; PR template

### F3.4 tasks
- [x] `TensorSink` writing MRB1 and safetensors; `ArrowIpcSink` writing the page-aligned IPC of contracts e.7; `ReorderBuffer` bounded by `ordering.buffer_bytes`
- [x] SI-T5, T9 pass; SI-T10 closes with `TensorSource`; SI-T6 skipped and listed; coverage; same PR as F3.3

### F3.5 tasks
- [ ] Environment facts from 09 section l verified; `unsafe` only where 09 section l permits
- [ ] Entry state machine e.1 with the disk-copy dimension; per-queue, per-tier atomic byte counters exact; FIFO; `is_full` against high water; head stays hot; promotion window ahead of the head; consumer tier from `set_consumer`
- [ ] Move table e.4 with every arm named; `Tier::Remote` and `Locality::Local` return `Unsupported("rdma")` (PL-T19); completions observed through `then`, never a placement thread
- [ ] PL-T1 to T4, T8, T9, T11, T19 to T21, T24 pass; the PL-I they cite listed; coverage

### F3.6 tasks
- [ ] Staging segments: `staging.segment_bytes` files, page-aligned records (e.3) with the `StagingCodec` byte, direct IO through the reactor, `register_segment`/`unregister_segment`, segment deleted when empty, `budget.disk` bound with the S10 diagnostic
- [ ] Q0 eviction (D5) with `evicted`/`replace`; Q0 staging when the source is not repeatable; move failure paths; concurrency test
- [ ] PL-T5 to T7, T10, T12, T14, T15 pass; PL-T13 (S15 throughput) provisional with host name; coverage

### F3.7 tasks
- [ ] Manifest e.5 written atomically (write, fsync, rename) on the calling thread; `restore` validates plan digest and fingerprints, rebuilds `OnDisk` entries, lists `to_recompute`; lineage index bounded by `set_committed` (f.11); lock order of preamble 4.2 (PL-T23); `shutdown` idempotent
- [ ] PL-T16, T18, T22, T23 pass; PL-T17 tagged for wave 4; every PL-I1 to PL-I15 cited across F3.5 to F3.7; PR template complete; PR reviewed as one

### F3.8 tasks
- [ ] PM reviews three PRs; S10 and S15 evidence with `FakeSink` throttle recorded; G-I3 evidence (PL-T1, PL-T8)
- [ ] Wave 3 report; E1 items for wave 4 (a container runtime honouring `--memory` and `--cpus`)
- [ ] SDDs 07, 08, 09 listed for Brackly to flip

---

## E4. Wave 4: scheduler, controller, Rust facade

Gate (6.6, wave 4): end-to-end with fakes and with real components: S1, S2, S4, S5, S6, S11; G-I1, G-I4, G-I5, G-I8; kill-and-resume equivalence (SC-T16, PL-T17); the facade's lifecycle (preamble 4.4) driven by RC-T12. Environment: wave 3 plus a container runtime that honours `--memory` and `--cpus`. Three executors; the facade executor reads every SDD.

| Id | Feature | Branch | Scope | Status |
|---|---|---|---|---|
| F4.1 | Scheduler core: chain validation, worker pool, pick and admission, instance pools, trace emission, knobs, heartbeat | `component/10-scheduler` | 10 c, d, e, f.1 to f.4, g; SC-T1 to T6, T8, T17, T18, T19 | merged (91.0%) |
| F4.2 | Scheduler drives, probe protocol, error policies, cancellation, evicted replay | `component/10-scheduler` (same PR) | 10 f.5 to f.10, h; SC-T7, T9, T10, T11, T13; SC-T12 (E1, provisional) | merged (same branch) |
| F4.3 | Scheduler checkpoint thread, watermark, `apply_resume_point`, `run_resumed` | `component/10-scheduler` (same PR) | 10 f.11 onward, resume; SC-T14, T15; SC-T16 (integration, wave 4) | merged (same branch) |
| F4.4 | Controller sizing: prepare, probe, envelope, rule sizer, AIMD, damping, oscillation freeze, breach, state growth, tick bound | `component/11-controller` | 11 c, d, f.1 to f.5; RC-T1 to T8, T11, T15 to T18 | merged (92.6%) |
| F4.5 | Controller classification, profile store, resume seeding, learned-sizer stub with shadow error | `component/11-controller` (same PR) | 11 f.6 onward, e.3; RC-T9, T10, T14; RC-T13 (E1); RC-T12 (integration, wave 4) | merged (same branch) |
| F4.6 | Rust facade `moruna-runtime`: lifecycle 4.4, `RunSpec`, report, cancel, mimalloc | `component/12-runtime` | 12 sections for the facade (f.1, f.2, f.7, section l Rust files); the lifecycle of preamble 4.4 fresh and resumed | merged (94.6%); Moruna runs end to end |
| F4.9 | Defects the first real run exposed: the budget arithmetic (the arena was subtracted from the controller's allowance twice, and the facade papered over it with `available / 2`), the arena refusing allocations a region should serve, `ParquetSink` reserving a whole file in arena memory, `budget.disk` never computed, no manifest on a completed run, resume unable to probe | `infra/first-run-defects` | 02 f.1 and e.2, 08 f.1 and f.2, 11 f.1, 12 f.1 and f.7, 03 e.4 | in progress |
| F4.7 | Wave 4 integration closure with real components | `component/12-runtime` (same PR) or `infra/wave4-integration` | RC-T12 in a container, SC-T16, PL-T17; S1, S2, S4, S5, S6, S11 evidence | SC-T16 and PL-T17 done in F8.4 (`crates/moruna-runtime/tests/kill_resume.rs`); RC-T12 and the S evidence still todo |
| F4.8 | Wave 4 gate and report (PM) | `main` | | todo |

### F4.1 tasks
- [ ] Environment facts from 10 section l verified
- [ ] Chain validation (`PayloadSpec::check` at plan time), N workers spawned parked, atomic stage cursor pick, admission rule (least output buffered, source last), `peek_resident` before pop, instance pools with affinity, eager `init_instances`, failure of a stateful init
- [ ] One `TraceRecord` per morsel per stage (G-I4), sampler before and after `apply`, `state_bytes` from `footprint`, `RecordHook`
- [ ] `Knobs` with clamping counted in `knob_clamps`, forwarding to placement setters; `StatsSource`; worker heartbeat; single row larger than max handled
- [ ] SC-T1 to T6, T8, T17 to T19 pass; coverage

### F4.2 tasks
- [ ] Source and sink drives (preamble 4.1), `Completion::wait` only there, `read_ahead` and `is_full` obeyed, `SinkHandle` writes, `set_committed`
- [ ] `Prober`: stage 1 by one read at the cursor, later stages by popping the previous head, one worker with the rest parked
- [ ] Error policies `terminate`, `skip`, `budget(n)` with `Sink::skip`; draining order; cancel token; evicted replay through `evicted`/`replace`
- [ ] SC-T7, T9, T10, T11, T13 pass; SC-T12 provisional with host name

### F4.3 tasks
- [ ] Checkpoint thread per preamble 4.1 (`KernelState::checkpoint`, `Sink::checkpoint`, `Placement::checkpoint`), final manifest on termination and cancel from the scheduler's thread; watermark and skips
- [ ] `apply_resume_point`: refusal when checkpoint disabled naming sink or staging dir, `Sink::resume`, cursor and sequence, `Checkpoint` restore and `Reinit` re-init; `run_resumed` re-reads `to_recompute`
- [ ] SC-T14, T15 pass; SC-T16 written and tagged; every SC-I1 to SC-I11 cited; PR template complete

### F4.4 tasks
- [x] Environment facts from 11 section l verified
- [x] `prepare` (baseline after every init), `probe_all` and `probe_missing`, envelope from budget and probe, rule sizer with AIMD, `damping_completions`, oscillation freeze (S11), breach handling with the G-I8 diagnostic through `Knobs::terminate`, workers bounded by memory, one action per tick, tiny dataset, tick lock bound (RC-T17), state growth, device OOM retry
- [x] Every knob written only by the controller (G-I5); `set_budgets` the only placement setter it calls
- [x] RC-T1 to T8, T11, T15 to T18 pass; coverage

### F4.5 tasks
- [x] Bottleneck classification table (f.6) from `SchedulerStats`, placement stats and samples; profile store keyed by fingerprint and schema hash (e.3) in `profiles.dir`; resume seeds the profile; learned sizer stub with shadow error tracking and `sizer.fallback_error_ratio` fallback
- [x] RC-T9, T10, T14 pass; RC-T13 tagged E1; RC-T12 tagged integration; every RC-I1 to RC-I10 cited; PR template

### F4.6 tasks
- [ ] Facade executor briefed with "reads every SDD" (preamble 9); environment facts verified
- [ ] `Runtime::run` builds and starts components in the order of preamble 4.4, fresh and resumed; `RunReport::compute`; `Terminated` mapped to an error with the partial report; `shutdown` and `Drop`; trace `finish` exactly once; clamping of every non-scheduler, non-discovery row reported in `notes`; `#[global_allocator]` mimalloc
- [ ] Unit tests with fakes for the ordering (PY-T1 startup_order lives here on the Rust side); coverage

### F4.7 tasks
- [ ] RC-T12 in the CI container job with `--memory`: all five benchmark kernels complete within B (S1, S2, S5, S6)
- [x] SC-T16 and PL-T17: SIGKILL at a random point, resume from the manifest, output equal (S17 on the CI host; F8.4)
- [ ] S4 and S11 evidence from the trace; G-I1, G-I4, G-I5, G-I8 named with their tests in the report
- [ ] Wave 4 timing figures labelled provisional with the CI host name

### F4.8 tasks
- [ ] PM reviews three PRs; merges; wave 4 report; E1 items for wave 5 (maturin, four interpreters, Polars and DuckDB, reference host access)
- [ ] SDDs 10, 11 and the facade half of 12 listed for Brackly to flip

---

## E5. Wave 5: Python package, wheels, bench baselines, reference host

Gate (6.6, wave 5): S3, S7, S8, S9, S12, S17 on the reference hardware; G-I9, G-I10, G-I12 (PY-T12); the whole configuration table clamped (PY-T13). Environment: wave 4 plus maturin and the four interpreters for the wheel matrix, Polars and DuckDB for the engine baseline, and the reference host for the timing gates.

| Id | Feature | Branch | Scope | Status |
|---|---|---|---|---|
| F5.1 | `moruna-py` module and `python/moruna` package | `component/12-python` | 12 a to h; PY-T1 (Python side), T2, T4, T5, T6, T8, T9, T14 | todo |
| F5.2 | Wheel matrix, configuration clamping walk, end-to-end in a container | `component/12-python` (same PR) | 12 section l packaging; PY-T7, PY-T13; PY-T3 and PY-T12 (integration, wave 5) | todo |
| F5.3 | Bench runner and hand-tuned baselines | `infra/bench` | preamble 6.5 wave 5 part: grid-searched plain loop per kernel, rows per second, host named; `MORUNA_BENCH_MORSEL_BYTES` for S12 | todo |
| F5.4 | Engine baseline (Polars streaming, DuckDB UDF) | `infra/bench` (same PR) | preamble 6.5: same kernel, same files, same container, defaults and then the documented memory limit; reported beside the tuned baseline, never a gate | todo |
| F5.5 | Reference-host campaign | `infra/reference-host` | every test tagged `(reference host, E1)` run on the Griot server in Nairobi: DS-T3, AD-T11, RE-T11, SO-T12, PL-T13, SC-T12, RC-T13, PY-T10, PY-T11; the device and GDS tests (AD-T3, RE-T5, SI-T6) listed as skipped by id | blocked (D-B3: agent access to the reference host) |
| F5.6 | Wave 5 gate and report (PM) | `main` | | todo |

### F5.1 tasks
- [ ] Environment facts from 12 section l verified (maturin, the four interpreters, pyo3-arrow)
- [ ] `moruna-py` `Cargo.toml` enables `python` and, on Linux, `uring` by default (preamble 6.3); the wave 0 stub left it at `default = []`
- [ ] `moruna.run(source, kernels, sink, ...)` and the source, kernel and sink wrappers; exception mapping from `MorunaError`; the report object; `KeyboardInterrupt` forwarded as cancel; GIL refusal unless `python.allow_gil`; no pandas dependency; re-entrancy
- [ ] PY-T1, T2, T4, T5, T6, T8, T9, T14 pass; every PY-I cited; Python tests at or above 90% coverage (`ruff` and `pytest --cov` in the gate); PR template

### F5.2 tasks
- [ ] CI wheel matrix 3.13, 3.13t, 3.14, 3.14t (PY-T7); version-specific wheels, no abi3; `moruna-py` enables `python` and, on Linux, `uring`
- [ ] PY-T13 walks the whole preamble section 5 table with one out-of-range value per row, checks the clamp owner and the report note
- [ ] PY-T3 in the container job; PY-T12 three-stage resume end to end with real components (G-I12)

### F5.3 tasks
- [ ] Bench runner: runs every benchmark in a container with `--memory` and `--cpus` and on the bare host; output names the machine
- [ ] Hand-tuned baseline per kernel by grid search over batch size and worker count; S3 ratio computed; S12 with the identity kernel and 256 MB morsels against the plain loop, defaults and the pinned figure both reported
- [ ] Tests and coverage for the runner

### F5.4 tasks
- [ ] Polars and DuckDB baselines per kernel, defaults then the memory limit set to the budget; reported beside the tuned baseline
- [ ] Documented as external evidence, not a gate

### F5.5 tasks
- [ ] Access path for an agent (or Brackly by hand) to run the suite on the reference host agreed (D-B3)
- [ ] Every E1-tagged timing test run there; figures recorded with the host name in the wave 5 report and the bench output
- [ ] Device and GDS tests listed as skipped by id with the reason "no GPU host (E1, decided 2026-09-22)"

### F5.6 tasks
- [ ] PM reviews, merges, wave 5 report: S3, S7, S8, S9, S12, S17 closed with host names; G-I9, G-I10, G-I12 evidence
- [ ] SDD 12 listed for Brackly to flip

---

## E6. Release readiness: a production-ready `moruna`

No preamble gate; this epic is what "done" means beyond the waves. Nothing here changes a design document without going through the escalation path.

| Id | Feature | Branch | Scope | Status |
|---|---|---|---|---|
| F6.1 | Decisions and document status closed | `main` | `DECISIONS.md` Q2 to Q7, Q9, Q10 decided or their assumptions explicitly accepted by Brackly; every SDD HANDOFF-READY; preamble section 8 traceability table with no gap row | todo |
| F6.2 | Sufficiency sign-off | `main` | S1 to S17 each cited to the test and the host that closed it, or listed as skipped by id with Brackly's acceptance (device and GDS parts of S14) | todo |
| F4.10 | A user's first program hung for ever. **The PM diagnosed this as "the Python boundary is a thousand times too slow" and that was wrong**: the boundary is fine, at one crossing per morsel and about 90 ns per row of genuine interpreter work. The cause was two defects with nothing to do with Python: `ParquetSink` passed a schemeless path through, which `moruna-sinks` reads as a local directory and the reactor reads as a bare S3 key, so every write failed; and `sink_drive::finish` took the exit claim and then handed its error to `policy::terminate`, which claims again and published nothing, so any sink failing at finish hung the run silently in any language. 200,000 rows now runs in 0.11 s of `moruna.run` | `infra/python-kernel-speed` | 12 f.3, 10 f.6; PY-T16 | merged 2026-09-23 |
| F4.11 | **A user's first program exceeds its own budget by 13 percent.** Ceiling 536,870,912 bytes, peak anonymous 606,797,824, `peak_fraction_of_ceiling` 1.13, measured from a wheel with `budget="512MiB"` and a Python kernel. S1 says the budget is never exceeded and G-I1 says it at every sample, so this is the first criterion of the whole design failing on the first program anyone would write. The runtime reports it honestly rather than hiding it, which is how it was found. Architecture section 8 already concedes that a kernel's own allocations are observed rather than governed, with the reserve absorbing them and the cgroup containing them, so the question is whether 10 percent of reserve is simply too little for a Python kernel, whether the controller should have reacted within a morsel and did not, or whether S1 needs restating to match what section 8 admits. It is the last blocker before a wheel is worth publishing | not started | S1, G-I1, G-I8, 11 f.1 and f.7, architecture section 8, E13 | `infra/budget-overshoot` | merged 2026-09-23: the PM's framing was wrong twice. The controller was not blind and it did react, shedding a worker on every record with the morsel target already at its floor, but the inequality it fed compared the kernel's measured anonymous growth against the arena's capacity, which the arena already occupies in full, so the bound was about 1.2 times the ceiling by construction. f.3 is now two inequalities and the second bounds the quantity S1 measures. The same job: refused at 512 MiB and 1 GiB with a diagnostic naming morsel, stage, footprint and budget; 0.55 of the ceiling at 2 GiB; 0.40 at 4 GiB; the Rust kernel unchanged at 0.92 |
| F4.12 | Two defects the budget work uncovered, both outside its remit. **`Sample::anon_bytes` on Apple hosts returns resident memory including file-backed pages, against its own comment**, so every S1 figure measured on a developer host is inflated by the mmapped source and the loaded dylibs, and the run-to-run spread we treated as noise is partly that. And **the facade gives the arena the whole allowance**, leaving the ten percent reserve as the only governed headroom for everything outside it, about 20 MiB of which the runtime's own reader and writer buffers already take at a 512 MiB ceiling: that, and not the job's real cost, is why an ordinary job is refused below about 2 GiB | `infra/arena-share-and-anon` | 03 DS-I4 and probes, 12 f.1; S1 | `infra/arena-share-and-anon` | merged 2026-09-23. The sampler correction moved the project's figures **mostly up**, not down: a mapped file was inflating them but compressed anonymous pages were missing from them, and the second was larger, so the 2 GiB figures the PM had been quoting were understated by 22 and 34 points. `anon_bytes` is now `phys_footprint` and `file_bytes` is `external`, proven by measuring all three routes on one process at one moment, and DS-T13 fails by 268 MB against the old code. The arena now takes `2/(2 + a_anon)` of the allowance rather than all of it |
| F4.13 | The sink asked the arena for 129 MiB and was charged a 256 MiB class, which was the whole reason an ordinary job would not fit in 512 MiB; and the controller divided **absolute** retention by bytes in flight, so a Python job's fixed 170 MB of interpreter overhead was charged per morsel byte and the refusal threshold **rose as the morsel shrank**, which is the opposite of a control loop | `infra/sink-class-and-fit` | 08 f.1, 11 f.3 and f.7, 12 f.1 | merged 2026-09-23. The sink takes one whole size class with the footer inside it; the arena floor is 176 MiB, measured rather than guessed, after the byte-exact 160 MiB failed. The fit is two terms, with the constant read from `mem_anon_before` and the slope from the growth, chosen over least squares on the whole reading because within a run the bytes in flight do not vary and the two terms are not identifiable. Held-out mean relative error 0.42 against the one-term form's 1.76, and it bounds the next morsel's peak 117 times in 144 against 45 |
| F4.14 | The five intermittent test failures three agents had written off as known flakes. **Two were real code defects and one of them was silent data loss**: `advance_closes` read a placement snapshot before taking the stage table lock and closed the whole cascade against it, so a morsel pushed in between was left in a closed queue and the run reported `Completed` without it, against SC-I9; and the sink drive and a worker doing `Sink::skip` could both publish `set_committed`, so the watermark went backwards. A third test had installed a process-wide no-op panic hook for the length of its run, which swallowed the assertion messages of everything failing beside it, and is why these read as flakes with no message at all | `infra/flake-hunt` | 10 f.7 and f.11, 09 f.11 and PL-T23, preamble 4.2 | merged 2026-09-23. Rates: `sc_t8` 4 in 500 to 0 in 20,000 under stress; `sc_f5` 3 in 500 to 50 of 50; `pl_t23` 4 in 30 to 50 of 50; the whole suite 20 consecutive passes, and the scheduler suite under 20 spinning hogs 0 of 250 against 3 of 250 before. Nothing was loosened, ignored or retried |
| F6.3 | Packaging and publishing | `infra/release` | Q1 remainder: PyPI, crates.io and GitHub namespace checks, trademark and domain; version `0.1.0`; `CHANGELOG.md`; trusted publishing workflow for wheels and sdist; PY-O1 (GPU wheel) decided or the separate-wheel assumption accepted | todo |
| F6.5 | Supply chain and security | `infra/supply-chain` | `cargo audit` and `cargo deny` (licences, advisories, bans, sources) in CI; `SECURITY.md` improved; every action pinned by SHA; DCO sign-off checked in CI; `Cargo.lock` freshness | merged |
| F6.6 | Release `v0.1.0` | `main` | tag, GitHub release with the wave 5 report and the bench figures (host named), wheels on PyPI for the four interpreters, `README.md` status changed from "Design"; E7 complete first | todo |

### F6.1 tasks
- [ ] Each of Q2, Q3, Q4, Q5, Q6, Q7, Q9, Q10 has a `decided <date>` row or an `assumption accepted <date>` row in `DECISIONS.md`, applied to the document that carries it in the same PR
- [ ] Every SDD status line reads HANDOFF-READY (flipped by Brackly)
- [ ] Preamble section 8 traceability: every parent id has invariants and tests; no "(deferred)" row except D11 (E8)

### F6.2 tasks
- [ ] A sufficiency table in the wave 5 report: S-id, test ids, host, figure, verdict
- [ ] S14 device and GDS halves and S13's device part recorded as skipped by id with acceptance
- [ ] Brackly signs the table

### F6.3 tasks
- [ ] Namespace checks run and recorded (Q1 remainder); names reserved where needed
- [ ] `CHANGELOG.md`; `Cargo.toml` and `pyproject.toml` versions; release workflow builds wheels for the matrix and publishes on a tag with trusted publishing
- [ ] PY-O1 decided; if the assumption stands, the `moruna-cuda` wheel is documented as not shipped in `0.1.0`

### F6.5 tasks
- [x] `cargo audit` and `cargo deny check advisories licenses bans sources` green; allow list is Apache-2.0, MIT, BSD-2, BSD-3, ISC, Zlib, Unicode-3.0 and Apache-2.0 WITH LLVM-exception, with two documented exceptions (BSL-1.0 via polars, bzip2-1.0.6 via datafusion); `[graph] targets` names the four supported platforms, which removes the CC0 crate honestly rather than by exception
- [x] DCO check with `tools/quality/check_dco.sh` and a ten-fixture self-test; 23 action references across three workflows pinned by 40-character SHA; `Cargo.lock` freshness checked
- [ ] Before the wave 2 gate: remove or re-decide the RUSTSEC-2026-0194 and 0195 ignores in `deny.toml` and the matching `cargo-audit --ignore` flags, per DECISIONS.md S1 and issue #9; cargo-deny's `advisory-not-detected` warning is the tripwire, and there is no machine-enforced expiry because cargo-deny 0.20.2 has no `expiration` key

### F6.6 tasks
- [ ] E7 documentation epic complete and published
- [ ] E8 hosted-engine epic complete: H1 to H13 closed on the reference host, `moruna-vmm` released
- [ ] `v0.1.0` tagged from a green `main`; release notes; wheels visible on PyPI; `pip install moruna` works on a clean 3.13 and 3.14 interpreter
- [ ] `README.md` status updated; this board's "Now" section says released

---

## E7. Documentation: intensive, end-of-build

Written after wave 5 so it describes what shipped, from the design documents and the run report, never reverse-engineered from code. Gate: every page below exists, builds in CI, cites its SDD or preamble section for every number and default, and a reader who has never seen the repository can install a wheel and complete each tutorial. Every executor here reads the SDDs its pages cover and the merged code as evidence, and reports a mismatch as a bug (`.github/ISSUE_TEMPLATE/bug.md`), never by documenting the code's behaviour over the document's.

| Id | Feature | Branch | Scope | Status |
|---|---|---|---|---|
| F7.1 | Docs site scaffold and CI | `docs/site` | `docs/` as an mdBook (Rust-native, no Node); a CI job that builds it, checks links and runs the em-dash and no-tabs conventions; published from `main` to GitHub Pages | merged |
| F7.2 | User guide | `docs/user-guide` | install from a wheel per interpreter; `moruna.run` walkthrough; sources (Parquet local and object store, safetensors, NumPy, Python iterator) and sinks (Parquet, tensor, Arrow IPC); ordering and error policies; budgets and the two environment-variable families; reading the run report and the trace; cancellation and resume (`resume=`, `checkpoint.*`) | todo |
| F7.3 | API reference | `docs/api` | Python: every public name in `python/moruna` from docstrings (the module's docstrings are the source); Rust: `cargo doc` for `moruna-kernel` with every trait's contract sentence from preamble 1.3 on its doc comment, published beside the book; the configuration table of preamble section 5 rendered with owner and range per row | todo |
| F7.4 | Kernel author guide | `docs/kernels` | a Rust kernel against `moruna-kernel` alone; the same kernel as a Polars plugin and a DataFusion function (S7); a Python kernel, GIL and free-threading, what releases the GIL; stateful kernels, instances, `ResumePolicy`; hints and `footprint`; the zero-copy rules and what breaks them (G-I2, CT-I4) | todo |
| F7.5 | Operator and hosting guide | `docs/hosting` | one page per host class of architecture section 6: laptop, cgroup v2 container and Kubernetes pod (memory.high, cpu.max, io_uring seccomp), Databricks single node (explicit budget, E7), Griot Cloud pod profile (`MORUNA_HOST_PROFILE`, durable staging); staging disk sizing; profiles directory; diagnosing a `Budget` termination; what the direct paths need and how the report names the path taken (G-I7) | todo |
| F7.6 | Architecture and internals for contributors | `docs/internals` | the component map and waves from the preamble; how a morsel moves (tiers, the move table, staging segments, the manifest); how the controller decides (probe, envelope, AIMD, classification); how to read an SDD and where each crate's tests map to its ids; the escalation path and the agent prompts; benchmark methodology and how to reproduce a figure on any host | todo |
| F7.7 | Tutorials and examples | `docs/tutorials`, `examples/` | end-to-end runnable examples with generated data: score a Parquet table with a NumPy kernel; embed a text column to a tensor; kill and resume a run; run inside a container with a budget; each example is a CI smoke test | todo |
| F7.8 | Documentation gate and release notes (PM) | `main` | every page reviewed against its cited sections; bug issues filed for mismatches; `CHANGELOG.md` and release notes for `v0.1.0` drafted from the wave reports | todo |

### F7.1 tasks
- [x] mdBook scaffold, 54 pages, each naming its feature and the sections it must cite; `tools/docs/check_docs.py` (links, anchors, SUMMARY completeness, tabs, em dashes, missing citations) with a nine-fixture self-test, wired into the gate and into `.github/workflows/docs.yml`
- [ ] Published from `main`: the deploy job exists and is gated on the repository variable `DOCS_PAGES_ENABLED`; publishing is outward-facing, so it waits for Brackly to enable Pages (Settings, Pages, Source = GitHub Actions) and set that variable, at F7.8 or earlier if he wants a preview; `README.md` already links it

### F7.2 tasks
- [ ] Every default and range cites its preamble section 5 row; every environment variable named with its owner
- [ ] Run report and trace pages show a real report from the wave 5 run with the host named

### F7.3 tasks
- [ ] Python docstrings complete for every public name (checked by a docstring-coverage step in the gate); Rust `cargo doc --no-deps` warning-free with `#![deny(missing_docs)]` on `moruna-kernel`
- [ ] Configuration table rendered from one source (the preamble) so it cannot drift

### F7.4 tasks
- [ ] The three-host kernel example (runtime, Polars, DataFusion) compiles and runs in CI (reuses AD-T9, AD-T10)
- [ ] Python kernel page shows the GIL-serialised report line and the free-threaded speedup figure with host

### F7.5 tasks
- [ ] Each host class page verified by running the container example there where the wave 5 environment allows; otherwise marked "described, not verified on this class" with the reason
- [ ] Termination diagnostics page reproduces the S6 adversarial run's diagnostic verbatim

### F7.6 tasks
- [ ] Diagrams are the preamble's and SDDs' Mermaid blocks, included, not redrawn
- [ ] Bench methodology page reproduces one S3 figure from the wave 5 report step by step

### F7.7 tasks
- [ ] Four examples runnable from a clean interpreter with a published wheel; each a CI smoke test with a small budget
- [ ] Resume example kills with SIGKILL and shows byte-equal output (S17)

### F7.8 tasks
- [ ] PM review of every page against its cited sections; mismatches filed as bugs, none documented around
- [ ] `CHANGELOG.md` and `v0.1.0` release notes drafted; F6.6 unblocked

## E8. Hosted engine: the job document, elasticity, the checkable kernel, the monitor

Designed in `architecture/moruna-hosted-engine.md` (MH). What Moruna must become to be the programming model a platform biases every author toward, and the microVM that platform runs. Gate: MH H1 to H13 closed on the reference host, `moruna-vmm` boots the guest image with a disk attached and resizes it, and a Griot run (`griot-cloud/BOARD.md` F2.7) completes inside it. Rules as for every epic: one executor per feature, one pull request, 90% coverage, no more than five executors in progress across both repositories, Opus 5.5.

| Id | Feature | Branch | Scope | Status |
|---|---|---|---|---|
| F8.1 | The job document and the host protocol | `hosted/spec-protocol` | `RunSpec` serde and the JSON format of MH 4.1 with precedence and strict mode; `moruna run` with the exit codes of MH 4.2; `moruna serve` and the newline-JSON protocol of MH 4.3 (`hello`, `heartbeat`, `limits_changed`, `report`, `exit`; `spec`, `cancel`, `checkpoint`); the report file always written; SDD `13-host.md` byte-exact and `12-python.md` amended so `moruna.run(...)` builds the same struct. Tests: H1, H2, H8 over a Unix socket; peer-gone continuation | merged 2026-09-26 (`feat/f8.1-spec-protocol`; runtime 92.9%, py 98.7%, scheduler 91.0%). SDD 13 not written (design docs out of scope for executors); the spec digest is recorded in the manifest as `spec.digest` beside the split plan digest rather than replacing it |
| F8.2 | The budget follows the machine | `hosted/elastic` | `LimitsWatch` publishing `Limits` per tick with the guest sources (`/proc/meminfo`, `cpu/online`) and the cgroup sources; parked threads to `cpu_max` with `active` bounded by the current limit; arena `grow`/`shrink` by region with the draining state and the sizing arithmetic re-run on change; `limits_timeline` in the report and `peak_fraction_of_ceiling` against the ceiling in force; SDD 02, 03, 04, 10, 11 amended. Tests: H3, H4 under a cgroup whose limits are rewritten mid-run; H12 later under F8.7's guest | merged (H3, H4 from a scripted machine through the watcher's code path; the real cgroup gates RT-T12, RT-T13 tagged for the reference host; SDD amendments dropped by Brackly's brief change) |
| F8.3 | The kernel is checkable; Polars as syntax; standard kernels | `hosted/check` | `input_schema`/`output_schema` on the decorator and the `Kernel` trait (absolute, or `adds`/`drops`/`changes`); the fingerprint; the Polars-signature acceptance path through `moruna-polars`, zero-copy; `moruna check` (load, seeded synthetic batch, run, compare, profile row, fingerprint, JSON report, exit codes) and SDD `15-check.md`; the `moruna-kernels` crate of standard kernels by arguments (`cast`, `rename`, `select`, `drop`, `filter`, `fill_null`, `dedupe`, `hash`, `mask`, `explode`, `concat_str`, `date_trunc`) with declared schemas, profiles and adjacent fusion; `RunSpec.kernels[]` kind `std`; SDD 05 amended. Tests: H13; every standard kernel passes `moruna check` | merged 2026-09-26 (`feat/f8.3-check`; kernels 96.4%, runtime 93.3%, py 98.7%). SDD 15 not written (code and tests only); `moruna check` is `python -m moruna check` and `moruna check` through the console script, the harness `moruna_runtime::check::check`; the job document carries `{"kind": "std"}` entries |
| F8.4 | Resume on a surviving disk | `hosted/resume` | F4.7 closed (SC-T16, PL-T17 SIGKILL resume); issued splits in the manifest and re-issued on resume; `--resume auto`; `staging.durable` in the spec setting `durable_staging=present`; the `checkpoint` message honoured. Tests: H5 | done (SC-T16, PL-T17, PL-T25, PL-T26, SC-T21, RT-T9; H5 on this host) |
| F8.5 | A plan is a source, a contract write is a sink | `hosted/datafusion-peql` | `PlanSource` over a peQL `Engine::view` `LogicalPlan` in `moruna-datafusion` behind a `peql` feature, one split per output partition, the `repeatable()` rule; `PeqlSink` over `Engine::write` with the manifest refreshed on `finish`; SDD 07 amended. Depends on peQL exposing `Engine::view` (griot-cloud F2.6). Tests: H6 with a gated plan and a write both larger than the budget | merged 2026-09-26 (`feat/f8.5-peql-bridge`; peQL `93d9481`: shapes and the budget charge are in the planned plan, `Engine::plan`/`Engine::view` return it, writes arrive in parts; datafusion bridge 95.5% with `peql`, runtime 93.1%). Splits are the plan's partitions cut at 4 MiB, counted by one pass that keeps nothing, since a split's rows are known before a read (d.6); file-scan work stealing is off. H6 at 256 MiB over 400 MB, each run its own process; the unrepeatable view into a Parquet sink that failed to find an arena buffer there, and the sink rule comparing contract names, were closed by F8.8. SDD 07 not amended (design docs out of scope for executors) |
| F8.6 | No network | `hosted/socket-object-store` | `unix://` and `vsock://` endpoints in `ObjectStoreConfig` with a custom connector speaking HTTP/1.1 over the socket; no change to sources or sinks. Tests: H7 under `unshare -n` | done (H7 over a Unix socket; the `unshare -rn` variant tagged reference host, E1; vsock compiled on Linux only) |
| F8.7 | `moruna-vmm` | `hosted/vmm` | The monitor of MH 4.8 on the rust-vmm crates: the device-model trait; `virtio-blk`, `virtio-vsock`, serial over `virtio-mmio`; KVM setup, memory map, vCPU threads; boot with `linux-loader`; the guest image recipe (pinned kernel with `memhp_default_state=online_movable`, Moruna as init, CPython 3.14t, `pyarrow`, the wheel); `virtio-mem` and vCPU hot-add; the control socket and `resize`; `moruna run --vm`; exit codes; SDD `14-vmm.md`; release artefacts (two Linux binaries, guest image as OCI by digest, cosign, SBOM). No network device and no field for one. Tests: H10, H11, H12 on the reference host; a Raspberry Pi 5 boot or a written finding | merged (96.6%); the KVM boots (vm_t40 to vm_t44: H10, H11, H12, panic, stop) and the Pi 5 boot are tagged for the reference host and open |
| F8.8 | Hosted-engine gate | `feat/f8.8-gate` | `moruna run --vm` over `moruna_vmm::boot_and_run`; a governed plan's DataFusion memory inside the run's budget (a `BudgetPool` sized, charged to the controller and moved with the limits, spilling to staging; the plan planned for the partitions its share holds, peQL `Engine::plan_for`/`view_for`); the unrepeatable view into a Parquet sink at 256 MiB; the sink rule on resolved data locations (peQL `Engine::location`); tests waiting on their event; the preamble 6.2 rows; the H1 to H13 table below. Docs pages and `DECISIONS.md` rows dropped by Brackly's brief | merged 2026-09-27 (gate: 844 passed, 0 failed, 16 ignored; every crate above 90%, runtime 93.3%, vmm 96.6%); H1 to H9 and H13 pass here, the KVM halves of H10 to H12 and the cgroup and `unshare` variants tagged for the reference host (table below). Finding: with no kernel in the chain the report's `peak_anon_bytes` is 0, so its peak fraction says nothing for a copy, and the kernel's own lifetime `phys_footprint` on macOS reaches about 2x the ceiling at 256 MiB on the peQL write path (with or without a plan) and on DataFusion's scans, which no pool accounts for; the plan's operators stay inside their pool (the H6 test asserts it). Closed by F8.9 |
| F8.9 | The whole process stays under the ceiling | `feat/f8.9-whole-process` | The report and the sampler measure the whole process from the operating system for every shape of run (`Sampler::process_peak`: `/proc/self/status` or the cgroup, and on macOS the kernel's lifetime `phys_footprint` mark); what no pool covered bounded (the process allocator, a peQL write's buffers through peQL `Writing::with_memory`, a plan's scans and its operators' unaccounted memory, a copy's heap decode) or refused; the controller stopping a run with no kernel before the operating system's ceiling; a budget under a run's floor refused with the floor named (N-7). Tests: H6 and a copy with no kernel at 256 MiB and 1 GiB, each run its own process, whole-process peak under the ceiling | merged 2026-09-27 (gate: 856 passed, 0 failed, 17 ignored; every crate above 90%, runtime 93.7%, controller 93.4%, discovery 96.7%, datafusion bridge 96.1% with `peql`; clippy clean on Linux too; peQL `23450f1`). Peak over ceiling before, measured by macOS's lifetime `phys_footprint` (the report said 0 for all of them), at 256 MiB: write 2.09, view 1.74, copy 1.70, view into Parquet 1.21, aggregation 2.38, copy with no kernel 1.21; at 1 GiB: 1.19, 0.92, 0.85, 0.86, 1.34, 1.01. After, the report's peak on macOS (equal to the kernel's mark to the byte) at 256 MiB: 0.74, 0.64, 0.69, 0.85, 0.90, 0.81; at 1 GiB: 0.54, 0.43, 0.45, 0.66, 0.78, 0.72; under glibc in a Linux container, sampled from `/proc/self/status`: 0.58, 0.50, 0.52, 0.69, 0.72, 0.70 and 0.50, 0.39, 0.39, 0.62, 0.66, 0.67. What held it: mimalloc (3 and 2) kept freed memory and is gone, the platform allocator is used, and under glibc the mmap threshold is fixed at 256 KiB and the heaps at four; DataFusion's parallel Parquet serialiser kept about half of every part written for the life of the process (off in peQL); an aggregation re-read after an eviction ran the whole plan again, concurrently (stateful operators are no longer repeatable, which was also a correctness hole: a spilled aggregation's order is not stable); evicted Q0 entries were all re-read at once past the high water; a copy's arena took the whole allowance. Named reserves in the report's notes: an engine sink's share (0.2 of the allowance, divided among the parts in flight, each bounding the morsel); a plan's share split a tenth pool, four tenths held back for what DataFusion's operators hold beyond their reservations (measured 3 to 5 times the pool on a spilling aggregation), half batches in flight; a copy from a non-engine source charged one byte beside each byte in flight. Floor: the arena's 64 MiB, a plan's 40 MiB and a write's 16 MiB over what the process held and the reserve (about 190 MB for a governed plan into a contract, from a process holding 9 MB); refused before anything is built. The controller stops a run with no kernel stage within a quarter of the reserve of its ceiling (a sink keeping 1 MiB a morsel stopped at 0.975 to 0.985); a run with kernels keeps f.7. Design documents not amended (12 l still names mimalloc; the preamble 6.2 row is stale) |

E8 gate evidence (F8.8, 2026-09-27, this host: macOS arm64, the full gate with `peql`; tagged tests are `#[ignore]` with the reason and run on the reference host, E1):

| H | Test | Status |
|---|---|---|
| H1 | `host.rs` `ho_t5_file_run_equals_library_run`; `python/tests/test_host.py` `test_ho_t14_file_run_equals_library_run` | the Rust test passed here; the Python one runs where `MORUNA_PYTHON` names a 3.14t with pyarrow, which this host did not set |
| H2 | `host.rs` `ho_t6_heartbeat_cadence_and_report_delivery`, `ho_t7_peer_gone_the_run_continues` | passed here |
| H3 | `elastic.rs` `memory_follows_the_machine` (scripted machine through the watcher) | passed here; `memory_follows_a_cgroup` tagged: needs a writable cgroup v2 |
| H4 | `elastic.rs` `cpus_follow_the_machine` | passed here; `cpus_follow_a_cgroup` tagged: needs a writable cgroup v2 |
| H5 | `kill_resume.rs` `sc_t16_sigkill_resume_equivalence`, `pl_t17_kill_and_resume` | passed here |
| H6 | `governed.rs` `h6_a_governed_plan_is_a_source_and_a_contract_write_is_a_sink`, `h6_an_unrepeatable_plan_into_parquet_inside_the_budget`, `a_sink_over_its_own_source_is_refused`; `plan_source.rs` `a_plan_larger_than_its_pool_spills_or_is_refused`; `whole_process.rs` (F8.9) | passed here and in a Linux container (F8.9, 2026-09-27): every run at 256 MiB and 1 GiB keeps its whole process under the ceiling, by the report and on macOS by the kernel's lifetime mark; peaks in the F8.9 row. The cgroup `memory.peak` variant is for the reference host |
| H7 | `no_network.rs` `h7_parquet_through_a_unix_socket` | passed here; `h7_parquet_through_a_unix_socket_under_unshare` tagged: needs Linux `unshare -rn` |
| H8 | `host.rs` `ho_t11_strict_refusal` | passed here |
| H9 | `ct_t14_reserved_variants_matched`; `tools/lint/no_tier_wildcard.sh` | passed here |
| H10 | `config.rs` `vm_t2_no_network_field` | passed here; `kvm_boot.rs` `vm_t41_h10_guest_has_loopback_only` tagged: needs `/dev/kvm` and the guest image |
| H11 | `host/cli.rs` `run_vm_boots_at_the_documents_budget_and_relays_the_guest` (the seam, fake monitor) | passed here; `kvm_boot.rs` `vm_t40_h11_boot_to_hello_and_completion` tagged: needs `/dev/kvm`; the time to `hello` is not yet measured |
| H12 | `cpu_hotplug.rs` `vm_t17_hot_add_marks_inserting_until_the_guest_clears_it`, `layout.rs` `vm_t5_overflowing_hotplug_is_refused` | passed here; `kvm_boot.rs` `vm_t42_h12_resize_reaches_the_guest` tagged: needs `/dev/kvm`; the Pi 5 boot open |
| H13 | `check.rs` `ck_t1_h13_a_kernel_that_disagrees_is_refused_naming_the_column`, `ck_t2_h13_the_profile_row_a_check_writes_is_read_by_a_later_run`, `ck_t5_every_standard_kernel_passes_check` | passed here |

Start order: F8.1 and F8.3 now, in parallel; F8.2 after F8.1; F8.4, F8.5, F8.6 after F8.1 (F8.5 also after griot-cloud F2.6); F8.7 after F8.1; F8.8 last. Executors are briefed from `architecture/agents/executor.md` with MH as the parent document.

---

## EA. Wave A: blocking fixes, release 0.5.7

The brief is `architecture/agents/briefs/waves-a-b-c.md` section 3. One pull request per component; the release is its own pull request on top of both. Waves B and C are not started.

| Id | Item | Branch | Scope (SDD sections, tests by id) | Status |
|---|---|---|---|---|
| MA1 | A source with zero rows never finishes | `fix/ma1-zero-row-source` | 10 f.5 (splits with no rows), h; SC-T24 (with its end-to-end half in `moruna-runtime/tests/governed.rs`), SC-T25 | in review |
| MA2 | Every task gets exactly one terminal event | `fix/ma2-terminal-events` (runtime), and the scheduler's half on `fix/ma1-zero-row-source` | 12 c PY-I11, k, n; PY-T23, PY-T24, PY-T25, PY-T26; 10 h (a drive that panics), SC-T26 | in review |
| R0.5.7 | Release 0.5.7 | `chore/release-0.5.7` | version bump, CHANGELOG section; `DECISIONS.md` Q4, AP1, FB1, CM1 | in review |

## EB. Wave B

The brief is `architecture/agents/briefs/waves-a-b-c.md` section 4. MC1 (section 5) was pulled forward into wave B by Brackly on 2026-10-28, so that MB3's new kinds have a registry to plug into.

| Id | Item | Branch | Scope (SDD sections, tests by id) | Status |
|---|---|---|---|---|
| MC1 | Moruna declares what it can run: one registry of kinds, a declaration per kind, a conformance suite per port, every existing kind moved onto it (pulled into wave B, 2026-10-28) | `feat/mc1-kinds-registry` | SDD 18 (new: KD-I1 to KD-I10, f.1 to f.5); 12 f.9; 16 PK-I8, f.8, PK-T11, PK-T12; MH 4.1; preamble 1 (component 18), 6.2 (`schemars`); `DECISIONS.md` KD1; KD-T1 to KD-T11, with HO-T1 to HO-T4, HO-T9, PK-T12, PO-T6 run through the registry | in review |
| KB4 | moruna-record: the record layout and the multi-producer ring the node's logs, events and metrics travel in | `feat/kb4-record-ring` | SDD 21 (new: RC-I1 to RC-I21, a to o); preamble 1 (component 21); RC-T1 to RC-T22 | in review |
| KA4 | `moruna-job`: declarations, the job document, the catalogue and `check`, for embedders | `feat/ka4-moruna-job` | SDD 22 (new: JB-I1 to JB-I9, JB-T1 to JB-T28); 18 e.1 to e.3, KD1, KD-I1, KD-I4, KD-I8; 12 f.9; MH 4.1; preamble 1 (component 22) | in review |
| KA5 | The job is a DAG of tasks: job document version 2 in FlatBuffers (`moruna-job`), and the runtime runs it (fan-out sharing one batch, fan-in by input port, many sinks, a queue per edge, checkpoint and resume over every edge) | `feat/ka5-dag-spec` | SDD 22 (JB-I10 to JB-I14, d.4, e.4, e.5, JB-T29 to JB-T39); 01 (d.7 ports, d.16, CT-I15, CT-T28); 09 (PL-I16, PL-I17, f.17, manifest version 2, PL-T27, PL-T28); 10 (f.16, SC-T27 to SC-T32); MH 4.1 (the job document is now version 2; events version 5); `architecture/adaptive-runtime.md` (the DAG model KD7 schedules) | in review |
| KA9 | `arrow_ipc` source: Arrow IPC files (the file format) as a source, one split per record batch block from the footer, the check before every decode, decoded in place in the arena | `feat/ka9-ipc-source` | SDD 07 (new: e.9, e.10, SO-T34 to SO-T39; SO-I10 extended); 01 e.8 (`footer_schema`, `Decoder::with_schema`); 12 (`ArrowIpcSource`); 18 (the kind, f.1); `moruna-job` catalogue (JB-T25); SO-T34 to SO-T39 | in review |
| KA10 | compressed Arrow IPC (ZSTD and LZ4_FRAME) accepted safely and written: every declared size is checked against what the message implies and a ceiling before anything is allocated, the batch is inflated into one arena buffer, and the `arrow_ipc` sink takes `compression` (`none`, `zstd` with a level, `lz4`) | `feat/ka10-ipc-compression` | SDD 01 (new: CT-I14, e.7, e.8, CT-T23 to CT-T27); 07 (e.10, SO-T40 to SO-T43); 08 (e.3, f.2, SI-T28 to SI-T32); 09 e.3; 12 (`ArrowIpcSink`); 16 e.5; 17 SB-I14; preamble 6.2 (`lz4_flex`, `zstd`, `flatbuffers`); `moruna-job` catalogue (JB-T25) | in review |
| KA11 | the IPC sink's blocks are 8-aligned (pyarrow reads them): the footer's block lengths are multiples of 8 and the record message declares the body length the block does | `feat/ka9-ipc-source` | SDD 08 (e.3, f.2, SI-T27); 01 e.7; SI-T27 and `python/tests/test_ipc.py` | in review |
| KA7 | The plan of a job: `moruna-plan` (the plan document in FlatBuffers with stable step ids, the `Planner` port and `Planners` registry, the `levels` planner, `explain`, `check_plan`), and runs start from a plan: the stages in the plan's order, every task event naming its step, `step_started`/`step_ended`, the report's order, step use and deviations; `moruna explain`, `moruna run --plan`, `moruna.explain` | `feat/ka7-plan-document` | SDD 26 (new: PN-I1 to PN-I14, PN-T1 to PN-T27); `architecture/adaptive-runtime.md` section 11 and section 13 (the plan in `moruna-plan`, above `moruna-job` and `moruna-cost`); MH 4.3 (events version 6, `spec.plan`, `report.plan`); 22 e.5 (no fusion across a barrier); preamble 1 and 6.1 (component 26); `moruna-cost` not yet on `main` (PN-M1) | in review |
| KA8 | Run events are records: every event is a `moruna-record` record (`seq`, `ts`, a subject per kind, an idempotent `msg_id`, the headers `run`, `attempt`, `plan`, `step`, `task`) whose body is a FlatBuffer (`moruna-event`, `schema/event.fbs`); the host protocol carries one as a frame (events version 7); a run with no peer writes `_events.mrec` in its output store; an embedder drains a ring; the JSON is rendered from the records at the edge (`moruna run --events-json`, `moruna.read_events`) (local, not pushed) | `feat/ka8-events-as-records` | SDD 12 (PY-I13, PY-T30 to PY-T37; PY-I11, PY-I12, PY-T23 to PY-T29 read through the records); SDD 26 e.2, PN-O3; SDD 21 (RC-I2, the layout); MH 4.3 (events version 7, the frame); `tools/flatc/generate.sh` (the third schema); HO-T16 | in review |

---

## EK. The Moruna microkernel (Brackly, 2026-10-28)

### Ownership and ordered work, 2026-10-30

This Codex chat now owns the Moruna 1.0 programme following Brackly's handover through the Griot PM. The existing integration checkout is `moruna-v1`, branch `integration/v1`, starting at `38447a99447151aec77eb28272a8a32e682052f3`. The frontend implementation remains `51bca015e3e50d34beb298a56bb618405800ad72`, with its explicit incomplete latency handoff saved in frontend BOARD commit `dcc72c018`; the Griot PM owns that remaining comparison. No frontend latency parity is claimed.

The authoritative inventory is the supplied `griot-cloud/.claude/workspace/moruna-v1-handover-20261030.md`. Historical EK rows below predate that inventory and do not establish completion. The initial audit checked every registered worktree: feature source trees were clean; `moruna-flakes/.vmrun/`, integration `target-linux/`, and the main clone's existing `.worktrees/` were the only untracked paths. Preserve them. The main clone remains the separate 0.5 line.

Work proceeds in this order:

1. Audit the five WIP branches (KA2, KD2, KB5, KD7c, KE2) against their SDDs, preserving existing worktrees and local commits. Finish implementation and review defects before calling them proven.
2. Run bounded proofs in series under the shared atomic reservation: Linux and Python suites, consistent coverage, and kernel boots on 1, 2 and 4 CPUs. Compile-only, historical and unrun results remain distinguished. On takeover the Griot PM holds the reservation for identity/PostgreSQL checks, so Moruna is limited to code and review until it is released.
3. Complete KD3 cross-adapter conformance, wire warm-up and online scheduling together when integration is authorized, and choose KD7d from the measured full policy matrix.
4. Implement the decided KS1 bucket storage and local cache/scratch model, then run the real multi-gigabyte KR workload on this Mac. Build the independent parts of KE3 while retaining cloud proof as requiring actual access.
5. Prepare the complete local readiness report, including unresolved design choices and actual proof results. The KD5 local Python boot decision and unavailable cloud credentials require a human decision or access; they do not stop independent work.

### Actual takeover proof and fixes
Three existing Sol 6.1 agents now have separate implementation ownership: `static_client` owns KA2 and the narrow KD2 boot-cleanup/record-serialization review fixes; `native_parity` owns KB5; `identity_gateway` owns KE2. Root owns integration BOARD, scheduler/cost work, the SQL proof and serial proof coordination. No new worktree or agent was created. Agents code/review concurrently and receive one explicit bounded heavy stage at a time, with the shared reservation still authoritative. The KA2 unsafe-module SDD list may be amended only for the actual checked mapping and trusted image/stdio endpoint boundaries, with their precise ownership/aliasing/lifetime obligations; this is a narrow soundness correction, not a general unsafe allowance.


On `feat/kd7c-policies`, local commit `f09bc95` fixes overlapping observation flushes corrupting in-flight accounting. CM-T44 demonstrably failed on inherited `6b4460b` and passed with the atomic flush reservation; all six online tests and all 69 cost-model tests passed on macOS (CM-T26 remains ignored as a reference-host measurement). The full workspace fast gate passed, including all-target clippy, formatting, docs and supply-chain checks, and the normal pre-commit hook passed again. Linux, coverage, scheduler matrix and integration of this feature are still pending. CM-T43 now waits for a signalled store arrival instead of spinning. Its previously unlisted specification is recorded alongside CM-T44 in SDD 24.

Raw logs and source/diff manifests are in the private proof directory `/var/folders/1d/f7v5jtsj3hd43ltj5v94smnr0000gn/T/moruna-v1-codex-tq3aoqyt`. To retain disk headroom after the workspace check, only 199 incremental cache directories created by that exact stage (953,618,188 bytes) were removed; the enumerated creation-time manifest is retained there. Every binary, source, credential and proof log was retained. No VM was started.

The parent PM's isolated Linux prerequisite probe was inspected at `/private/tmp/codex-f14-linux-probe-mklo3fxl/summary.json`: root and non-root namespace probes pass; delegated CPU/memory cgroup setup fails because the guest hierarchy is not writable, and bubblewrap is absent. The summary explicitly has `probe_exit=1`, `capability_pass=false`, and makes no Moruna, Python, seccomp or function-execution claim. Its 2 CPU / 2 GiB VM was owned and deleted by the parent. Do not duplicate a cold 0.5.8 build or modify another VM to bypass that result. The bounded Mac stage has ended and this chat's reservation is released. Honor the parent's `~/.vm-lock-next=codex-f14` marker before any further heavy stage.

On integration `38447a9`, fresh Mac runs of `sc_t24_zero_row_splits` and `sc_t25_any_plan_finishes` passed, including 200 generated plans. The governed SQL regression now also checks `_SUCCESS`, exactly one real Parquet file, zero metadata rows and the non-nullable `answer: int64` schema. The first bounded feature-enabled attempt stopped during dependency compilation when free disk fell below 5,500 MiB. A subsequent dedicated private target with debug information and incremental compilation disabled retained dependency progress across two 120-second stages, then the exact pinned test passed in stage `integration-empty-sql-small-3`: one passed, no failures, seven filtered. Source remains integration `38447a9` plus the test/SDD change; Cargo.lock is unchanged, using Rust 1.98.1, peQL `ff692906` and parcel `0af4c091`. The test checks `_SUCCESS`, one schema-bearing file, zero metadata rows and non-nullable `answer: int64`. The macOS linker emitted its existing large-unwind-table warning; it was not hidden. Formatting, documentation and diff whitespace checks pass. These integration edits remain uncommitted pending the normal pre-commit gate.

The integration stages' 2,189 newly created object/metadata/library cache files (3,682,547,915 bytes) were then reclaimed from this checkout's `target/debug/deps` using an enumerated creation-time manifest. Every executable, source, credential, preexisting cache and proof log was retained; no shared/global cache was cleared. Available disk returned to 8.7 GiB. Both cleanup manifests and the explicit resource-stop result remain in the private proof directory. Do not repeat this cold build while the parent needs the next build slot; resume with a coordinated target/cache budget.

### Continuing programme checkpoints

Brackly's latest direct instruction on 2026-10-30 is to deploy agents to finish implementation and limit root to merging and testing. The three existing agents are active implementation owners: `static_client` owns storage/cache/recovery and storage artifact ports; `native_parity` owns shared runtime cost/scheduling, engine logical-row retention, CLI/SDK wiring and remaining quota/event source fixes; `identity_gateway` owns standalone node/network implementation and its protocol fixes. Root no longer implements production features: it coordinates dependency merges, runs proofs and returns failures to their owner. Root's KD6b true-peak edits and KD7c matrix edits have been transferred to `native_parity`. Local integration merges are now authorized by this new human instruction; publication/deployment restrictions remain. The node agent reuses the clean existing `moruna-ke1-drivers` checkout on new local branch `codex/ke3-node`, preserving the original KE1 branch. SDD32 is storage; SDD33 is the standalone node.

Apple Container is the primary Linux workload, sandbox, kernel and performance proof environment, following Brackly's direct steering through the parent PM. Host checks below are supplementary. No Docker daemon/socket, Podman machine or Testcontainers fallback may be used. The parent reclaimed its own compiler outputs and restored enough disk for proper proofs; this chat does not treat that as authority to touch foreign caches, VMs or builders. Current source work and staged proof preparation continue while the shared heavy reservation is occupied.

- KD7c local commit `45497ef` includes complete prediction inputs in cache identity, so changed worker counts and newly known source rows are visible before another unit settles. It also stops training per-unit PeakMemory from two shared-process endpoint samples. Both regressions fail on the prior implementation; six learning tests and the full scheduler suite pass (91 passed, one reference-host measurement ignored), including SC-T33. The normal pre-commit fast gate passed. True account peak is a lifetime high-water charge, and cannot be passed off as a per-unit memory measurement. Isolated warm-up/adapter integration and Linux performance remain owed.
- Additional KD7c benchmark edits are uncommitted/unproven: report the actual full run finish including store drain, expose drain separately, check equal completed sink row counts across policies/models, and allow exact workload/cell partitioning so the full matrix can run in bounded stages. No default policy has been chosen from incomplete measurements.
- KD6b source review found sampled process peaks and missing child marks being published as complete per-unit PeakMemory, plus overlapping warm-ups resetting/disarming each other's process-wide counters. The uncommitted correction leaves PeakMemory absent when required marks are unavailable and invalidates both overlapping intervals while retaining duration/output measurements. The short interval coordinator never holds a lock over kernel calls. Added CM-T45 regressions and existing freed-spike assertions await Linux execution; formatting/whitespace checks alone do not establish correctness.
- KB5 source corrections cover exact VM split charges, unstarted-space cleanup, actual carved OutsideMemory, scratch-shrink failure ordering, account reservation rollback and bounded physical reserve. An initial host quota/ABI/handles subset passed 23 tests before the later protocol changes. Linux and imagefs required request bodies changed incompatibly; enforcing SDD23 means protocol 2.0 with old-major refusals, not a minor bump or a legacy unbounded adapter. Those updated host/kernel/boot tests and the normal hook remain pending.
- KA2 source-only review fixes make mapping aliasing/backing obligations explicit, enforce sealed input bounds, reject writable access to readonly maps and wrong typed slot roles, and consume a put guard on failure. KA2 Linux tests/measurements and full image sandbox proof remain pending. KD2 source-only fixes propagate image-record serialization errors and clean only owned tracked helper/QEMU jobs on all boot exits/signals. The controlled shell fixture passed all three cases, with the final log `/tmp/moruna-kd2-cleanup-jobs.yXuwMM`; this is not a kernel boot pass.
- KA8's unchanged 90% gate passed with 50 event tests: crate 96.61%, generated 96.23%. Public wire/accessor tests were added instead of excluding generated code. Subsequent fast-gate test-only warnings were corrected without weakening assertions; their final gate/coverage rerun is queued. Source is not committed until the normal hook passes. No current job/plan coverage percentages were invented from absent data.
- KE2 source now implements server-owned TLS upgrade, bounded trust/time injection, no downgrade after failure, loopback DNS, link-down routing and correct Ethernet MTU accounting. A host compile/link passed with no warnings; the first runtime test attempt failed in a test fixture that incorrectly assumed two external cards for a loopback-only stack, now corrected. Full runtime/cross-link/boot proofs and the normal hook are pending. Cached ring's RNG support does not prove baremetal link support. A narrowly vendored platform-build patch is permitted only if actual cross-link confirms the need, preserving crypto sources and license/provenance. Upstream documents the unsupported-target asm mismatch ([ring issue 1832](https://github.com/briansmith/ring/issues/1832)); the alternative RustCrypto provider explicitly warns against production use ([upstream README](https://github.com/RustCrypto/rustls-rustcrypto)). No provider is claimed proven yet.
- KS1 is implemented separately in the reused, previously clean detached proof checkout `moruna-kd7a-proof`, now branch `codex/ks1-storage` at `ee2b1b1`. No new worktree was created. Storage/auth/key/artifact ports, bucket CAS/multipart, encrypted cache, durable format and direct-device source are under construction and uncompiled. The raw adapter must use an enforced strict direct/uring capability, not reporting flags that still allow fallback. Its required reactor repair retains original-operation ownership through CQEs and makes one thread the ring owner. Raw home format integration, real devices, checkpoint migration, fused-kind logical taps and cloud proof are not yet delivered.

The first actual Apple Linux inventory completed successfully in the nonce-owned 2 CPU / 2 GiB VM `moruna-proof-501-baaba9dc8816`: guest exit 0, nonce/completion markers verified, owned stop/delete confirmed and the shared lock released. Evidence is `linux/moruna-proof-501-baaba9dc8816/summary.json` and `009-start.log` under the private proof directory. The cached ARM image has Rust 1.95.0, 1.98.1, nightly-2026-07-21, cargo-llvm-cov 0.8.5, GCC 12.2.0 and QEMU 10.2.1. It lacks clang/libclang, x86 GNU/musl tools, Python 3.14/3.14t, uv/pip, QEMU's user/libslirp backend, `/dev/kvm` and `/dev/vhost-vsock`. This is prerequisite inventory, not a Moruna test, kernel boot or sandbox pass. The cached Apple kernel also has TLS unset; a private explicitly selected kernel recipe is prepared, with no default-service/kernel changes or actual build yet.

Private source preparation continues in `linux/`. A reusable dependency image, exact offline source/vendor inputs, owned persistent build caches and the missing tools still require concrete preparation/proof. Never take over the existing shared `buildkit` builder. Boot gates must propagate failure and require actual QEMU status 33, checker status 0 and every required assertion/log. The historical helpers that echo after a failed checker are not acceptance gates. After inventory, this chat released its exact `moruna` next-turn marker; the parent has admitted its bounded CAS Linux stage. Moruna source ownership continues while that heavy slot is occupied. The frontend latency status question did not explicitly cancel Brackly's full Moruna assignment; the temporary inference that it did has been corrected without discarding source or starting competing heavy work.

A focused KE2 Linux input is now independently validated without compiling or creating a VM: `linux/ke2-source-nv1e357m/ke2-inputs.tar.gz`, archive SHA `aa929e50e259585f234c926ac8bd9fa829b9250c190c7f12d70d97d68c0d3ded`. It contains the frozen kernel workspace plus the exact 128 locked registry crates, no home or Git credential/config mounts. Root rehashed all 6,169 regular input files against the portable census and bound the original kernel lock SHA `f623591264bb39c49ada81f8f1aa9979f8e15052aee0d27be5a094cb98fa0f47`; source HEAD is `2f43d4607b48d2ec53289b21b0881822cf7d49a1` plus the recorded dirty-source census. `linux/ke2-linux.py` is prepared for separate compile, protocol/TLS/DNS/suite and robust-test stages on the cached ARM image, with offline Cargo, one exact owned private target-cache attachment, 2 CPUs / 4 GiB and nonce-verified teardown. Its prepared-only validation passes; none of those Linux stages has executed. The baremetal link and QEMU network proof still require the missing cross tools/backend and are separate gates.

Subsequent actual Linux results: KE2 native ARM host test binaries compiled/linked successfully in `moruna-proof-501-9a86d95b8f55` (exit 0, cleanup verified), using the exact snapshot above and native GCC; runtime tests remain a separate phase. The first KS1 attempt stopped at the disk floor before compilation. Root reclaimed only 1,455 owned generated SQL object/library/metadata cache files, 3,398,950,908 logical bytes, retaining binaries, dynamic libraries, source and proof logs; the exact manifest is `owned-sql-cache-reclaim-20261030T115728Z.json`. The retry `moruna-proof-501-7cfb9195e10f` checked the storage/reactor libraries on Linux with explicit `moruna-reactor/uring`, then failed all-targets on one test's unstable slice method and emitted three warnings. The storage owner has corrected those findings in newer source; this is not a rerun/pass for the new raw-volume implementation. Both VMs were ownership-verified and deleted. Every proof cache is bound to the exact full input census as well as lock/toolchain/target/profile, so normalized archive timestamps cannot reuse stale artifacts from repaired source.

### Inherited engine acceptance, mapped 2026-10-30

These are Moruna 1.0 acceptance requirements; the Griot PM owns the application API/UI side and any separately needed 0.5.9 verification. Historical failed/cancelled runs are evidence of defects, never successful completions.

| Concern | Verified source boundary | Remaining acceptance |
|---|---|---|
| Empty-result completion | Integration already contains `f5666f2084ad5b6260199dfbacb176ce226f7ea9`: `source_drive::settle` advances the shared cursor under its lock from peek, take and exhaustion. SC-T24 and SC-T25 cover zero-row plans; `moruna-runtime/tests/governed.rs::sc_t24_a_query_with_no_rows_finishes` uses the exact SQL query. | Rerun the focused scheduler and governed SQL regressions on the final integration. Inspect the existing Parquet assertion and add schema preservation if absent. The original cancelled run is not a pass. An ancestry check also confirms `f5666f2` is present in the detached 0.5 main head `e06c53c`; that is source ancestry, not a fresh 0.5 runtime proof. No 0.5 branch has been changed. |
| Inspect each fused task's historical rows | SDD 22 JB-O1 explicitly says job v2 carries no per-edge retain field. DataPlane page handover, retained stdout/stderr and scheduler spill are not a durable output-retention contract. | Specify and implement a governed retention/artifact port through job/engine and KS1 storage, preserving fused execution while capturing the requested task boundaries. Define resource policy and native refusal/completion facts; expose the same engine facts to CLI, SDK and the embedder for API/MCP/UI access. Do not promise every row retained without that policy. |
| Consistent cancellation terminal facts | `moruna-runtime/tests/host.rs::ho_t8_checkpoint_then_cancel` asserts cancelled/skipped facts for source, kernel and sink before `cancel_ended`, plus exit 130 and the persisted cancelled report. KA8 changes event encoding and still needs its own proof. | Rerun native lifecycle/event regressions with the final event protocol and map every saved-run task projection to those terminal facts. Keep the original stale-task projection failure distinct from empty-source exhaustion; no synthetic timer-based success or cancellation. |

Current authorization is local implementation by agents, review, tests, local commits and local integration merges by root. No push, remote CI, release, tag, PyPI publication or deployment. No new worktrees, host binary installation, changes to Griot's Moruna pin, or interference with the Wazuh VM or other sessions' machines and data. Heavy stages require `~/.vm-lock`, respect `~/.vm-lock-next`, use at most 6 GiB per VM and release only this chat's own reservation and processes. Only verified expendable build artifacts owned by this work may be reclaimed, with source, credentials and evidence retained.


Moruna becomes a microkernel that runs tasks as processes, booted two ways from one codebase: inside a VM on a Linux node, or as the node itself. Tasks are OCI images, any language; wasm is not needed. Design: `architecture/optimized-runtime.md`.

**Extensible by default.** The kernel holds only processes, memory, threads, interrupts, timers, messages and capability handles. Everything else is a process speaking a published protocol: drivers, the network stack, filesystems, the Linux personality, the supervisor. Adding a driver or a server is adding a process image to the boot manifest and implementing a protocol with the `moruna-svc` SDK; nothing in the kernel is rebuilt and nothing is forked. Every protocol has a conformance suite.

**Order (Brackly, 2026-11-01): Goal A first, then Goal B.**

- **Goal A (now): the kernel runs a host's jobs inside a VM.** A job's tasks run on the Moruna kernel in a microVM that `moruna-vmm` boots on a Linux machine with KVM; the host stays on Linux. Items: KB1 to KB3 (the kernel boots, drivers), KC1 and KC2 (Linux programs, the image filesystem), KD1 to KD4 (the data plane on kernel channels, the engine as the first process, conformance across adapters, the launcher's devices), then release 0.6.0. On the host's side: KF2 (isolation tiers and placement) and KF4 (the launcher in the host's node agent). Done when a real job runs on the kernel in a VM with the same rows and the same refusals as on Linux (KD3).
- **Goal B (after A): the kernel is the node, and an unmodified Linux server program runs on it.** Items: KE1 to KE3 and KS1 (drivers, the network server, the node image, storage; written, proof pending), then KC3 to KC5 (sockets, durable files and io_uring for Linux programs), then release 0.7.0. Linux kernel TLS (in KC3) is not required: it is added only if a measurement shows it is faster on this kernel than the program doing its own TLS.

Waves are gates, not start order: an item starts when what it needs is merged. Two agents at a time.

| wave | item | what | status |
|---|---|---|---|
| A. Any language on Linux, today | KA1 | `image` kind: OCI unpack (layers, whiteouts, digest cache), run the image's command under `moruna-sandbox`, Arrow IPC over stdin and stdout (SDD 19; IM-T1 to IM-T27, SC-T27) | in review (`feat/ka1-image-kind`, `bf2de62`, draft PR against `v1`) |
| | KA2 | the data-plane port: `batch_get`, `batch_put`, `port_call` as one trait the engine uses; today's sandbox channel is its first adapter | todo |
| | KA3 | the Python builder: `@moruna.kernel` to a four-layer image (runtime, locked dependencies, shim, function) | in review (`feat/ka3-python-builder`, local; SDD 30; BD-T1 to BD-T22 and BD-T30 to BD-T32 green on the Mac with no engine and no network; BD-T23 to BD-T29 pending the proof: the built wheel, then Linux, the network and a sandbox host for the image through the `image` kind and the converter) |
| B. The kernel boots | KB1 | microkernel core on OSTD: boot, address spaces, processes, threads, IPC, capability handles, timers; boots under QEMU in CI on x86_64 | in review (draft PR #67; SDD 20) |
| | KB2 | `moruna-svc`: the SDK for servers and drivers, the boot manifest, the protocol schema and its conformance harness | in review (draft PR #74; SDD 23; ABI 1.1; boots on 1, 2 and 4 CPUs) |
| | KB3 | first drivers as processes: virtio console, virtio block, vsock | in review (draft PR #79; SDD 25; ABI 1.2; DR-T1 to DR-T17 green on 1, 2 and 4 CPUs, vsock data path included) |
| C. Linux programs run | KC1 | the Linux personality server: the 60 syscalls; static musl, then glibc, then CPython | in review (`feat/kc1-linux-personality`, local; SDD 28; ABI 2.0, SDD 20 rows 30 to 42; LX-T1 to LX-T17, KK-T13, KK-T14 green, booted on 1, 2 and 4 CPUs under TCG in the build container: static musl, its threads, dynamic glibc 2.41 and CPython 3.14 from python:3.14-slim's layers) |
| | KC2 | the image filesystem server: layers as block devices, one overlay, whiteouts, paging on demand | in review (`feat/kc2-image-filesystem`, local; SDD 27; block protocol 1.1; IF-T1 to IF-T19 green, booted on 1, 2 and 4 CPUs under TCG in the build container) |
| | KC3 | sockets for Linux programs: `socket`, `bind`, `listen`, `accept4`, `connect`, `send*`/`recv*`, `shutdown`, `getsockname`/`getpeername`, `setsockopt`/`getsockopt` answered by the personality as calls on the network server (`moruna.net` 3.0, SDD 31), within the task's boot-granted listener policy; socket descriptors readable through `epoll` and `poll`. Linux kernel TLS (`TCP_ULP "tls"`, `SOL_TLS`) is answered `ENOPROTOOPT`, not served, unless a measurement shows it faster on this kernel than the program's own TLS (Brackly, 2026-11-01: support a Linux feature only where it proves an advantage). Today the personality refuses `socket` and `connect` and does not list the rest, so an unmodified Linux server program (an HTTP server) cannot listen | todo (added 2026-11-01, Brackly: the code between "a node boots Moruna" and "a Linux server program runs on it") |
| | KC4 | durable files for Linux programs: a path the task's boot configuration names (for example `/var/lib/<service>`) mounted from a storage volume (KS1, `kernel/storage`), read-write and surviving restarts, with `fsync`, `fdatasync`, `rename` and `ftruncate` durable on the volume. Today the only writable path is the task's scratch directory (SDD 27 IF-I8), so a program's database files do not survive | todo (added 2026-11-01) |
| | KC5 | io_uring for Linux programs, served (not refused, and not replaced by an `epoll` path: the programs that need it stay unmodified): `io_uring_setup`, `io_uring_enter`, `io_uring_register`, rings mapped from the task, with at least the operations a replicated log uses: `READ`/`WRITE` (and the vector forms) on files, `FSYNC`, `SPLICE` from a file through a pipe (`pipe2`) into a socket (also a kernel-TLS socket, KC3), `SEND`, `RECV`, `ACCEPT`, `CONNECT`, `TIMEOUT`, `ASYNC_CANCEL`, `NOP`; submissions served by the imagefs, storage (KC4) and network (KC3) servers. Before building, the exact opcode, flag and setup-parameter list is measured from the target program under `strace` on Linux and recorded here. Today `io_uring_*` are absent from the table and refused `ENOSYS` | todo (added 2026-11-01; Brackly: such a program's io_uring use is not to be changed) |
| D. Moruna on its kernel | KD1 | the data-plane adapter on kernel channels: shared pages, ownership handoff, the record check in `batch_put` | in review (`feat/kd1-data-plane`, local; SDD 29 (new: DP-I1 to DP-I9, DP-T1 to DP-T17); ABI 2.1, SDD 20 rows 43 to 48 and the data plane's five calls; `DECISIONS.md` DP1: the check in the engine's process on pages the task no longer maps; `moruna-ipc-check`, the IPC check in `no_std`, shared with `moruna-kernel`; `moruna.linux` 1.1 `run_plane`; booted on 1, 2 and 4 CPUs under TCG: a 64 MiB batch in the same frames with 0 bytes copied, fan-out freed once, 43 records refused, 1,000 batches of 1 MiB at 701, 1,374 and 1,437 batches/s (about 100 us to map a 1 MiB batch, 110 us to put one), KA1's `arrow_echo` unmodified through descriptors 0 and 1) |
| | KD2 | the engine as the first process after the supervisor | code complete, proof pending (`feat/kd2-engine-process`, local; SDD 30 (new: EN-I1 to EN-I8, KD2-T1 to KD2-T14); `DECISIONS.md` EN1: the engine is the root workspace's, an unmodified static musl program the Linux personality runs with granted handles; EN2: the vsock a vhost-user device whose back end is moruna-vmm's vsock device in a host program (`moruna-vhost-vsock`), so the boot runs on a Mac's VM with no `/dev/vhost-vsock`; EN3: an image entry's digest checked on the kernel against the record the launcher writes and the seals of the layers mounted, its command and environment from the verified config; ABI 2.2 `process_grant`; `moruna.linux` 1.2 `run_granted`, eventfd, epoll, rename, a machine's memory, a program bound to its connection, 1.3 `layers`; `moruna.imagefs` 1.1 `rename`, 1.2 `layers`, a layer's id its seal, checked at mount (IF-O5 done); `moruna-micro`: the vsock transport and the `image` kind as a task of the data plane; the engine's supervisor from `engine.conf`; the boot's five runs judged by `kernel/tools/embedder.py`; not yet booted) |
| | KD3 | conformance across adapters: every kind on Linux and on the kernel, same rows, same refusals | todo |
| | KD4 | launcher devices in `moruna-vmm`: shared memory, one block device per layer | todo |
| | KD6a | `moruna-cost` (`feat/kd6a-moruna-cost`): the inputs, `Estimate`, `UnitCost` and `Outcome`; the `CostModel` port and the `CostModels` registry, chosen by a setting, with shadow mode; the default `loglinear` model (RLS on logarithms with forgetting, a fixed overhead, ratios, fallbacks); the observation store and its local adapter. SDD 24 (new: CM-I1 to CM-I11, CM-T1 to CM-T21); `architecture/adaptive-runtime.md` sections 3 to 5, 7 | in review |
| | KD6c | the simulator and the `simulated` planner (`feat/kd6c-simulator`, local): `moruna-plan` on `moruna-cost` (its `Estimate`, `UnitCost` and `CostModel`; `Estimator` gone; `extrapolated` appended to the plan's estimate table); `moruna-cost`'s discrete-event simulator (units, buffers, workers, memory as a hard limit with known-future eviction, disk and worker contention, scaling curves, start-up and checkpoints, the `Policy` port and `admission`, Monte Carlo over 200 draws), the sampling helper, `CostModel::marginal` and morsel sizing; the `simulated` planner, which `explain` takes when a cost model is configured. SDD 24 (new: CM-I12 to CM-I18, CM-T22 to CM-T30, f.6 to f.10); SDD 26 (new: PN-I15 to PN-I17, PN-T28 to PN-T33, f.6, f.7; PN-M1 closed); `architecture/adaptive-runtime.md` sections 8 and 11 | in review |
| | release | 0.6.0: the kernel runs real jobs inside a VM | todo |
| | release | 0.6.3: a job spec is language-neutral; one image per task (build keys, shared layers, provenance), image tasks of every role, pulls by digest, params, needs as ports, run arguments, a task-level run report | released 2026-11-06 (v0.6.3, 9d2e7d7; PRs 83, 85, 86, 87; release run 42254040679 reused what run 42209879389 built) |
| | release | 0.6.4: what the first project found moving onto 0.6.3 (see "Releases now") | in progress (`release/0.6.4`) |
| | 0.6.5 | **The run VM's network, Moruna's half** (agreed with Brackly, 2026-11-05). (1) virtio-net in `moruna-vmm`. (2) A per-run network enforcer inside the 0.6.2 jail, in the run's own network namespace: DNS answered only for allow-listed names, pinning their addresses; TCP only to pinned addresses on declared ports; for `tls` and `https`, the SNI equal to the declared name, refusing ECH, a mismatch, or a second TLS name on a kept-alive connection; UDP other than its own DNS, ICMP and raw sockets refused, IPv4 and IPv6 alike; cgroup limits; a flow log. (3) The allow-list document `{name, host, port, protocol: tcp|tls|https, private}` as a port. (4) The macOS process path: the enforcer's socket is the only network a sandboxed process gets. (5) `secret` needs delivered to the task only inside its run, never logged. (6) The same per-run jailed process serves the VM's host-side ports. (7) No inbound connections | todo |
| E. The kernel as a node | KE1 | NVMe and the cloud network card as driver processes | code complete, proof pending (`feat/ke1-nvme-net`, local; SDD 25 f.15, DR-T18 to DR-T27; SDD 29 (new): `moruna.driver.net`, virtio-net, gVNIC, NT-I1 to NT-I8, NT-T1 to NT-T15; ABI 2.1, `memory_flags::CONTIGUOUS`, SDD 20; SDD 23 SV-T16, the boot description's `machine` and `disk` lines; host tests and clippy green; the boot on 1, 2 and 4 CPUs not yet run; gVNIC tested against a model only, not yet proven on hardware: KE3) |
| | KE2 | the network server: TCP/IP and TLS (smoltcp, rustls) | todo |
| | KE3 | the node image: bootloader, kernel, servers, a standalone supervisor; boots on Google Cloud | todo |
| | release | 0.7.0: a node boots Moruna | todo |
| F. Griot orchestrates (Griot repository) | KF1 | the pool port: Cluster API for Linux pools, Google Cloud for Moruna pools | todo |
| | KF2 | isolation tiers (shared, node, hardware) and placement by a kind's declared needs | todo |
| | KF3 | the shared tier on the control plane, under a ceiling | todo |
| | KF4 | the launcher moves into Griot's node agent | todo |

## Escalations and findings

Open items the PM found while reading, routed per preamble section 7. Ids: `N-` a documentation finding from the PM's read, `E1-env-` an environment gap for a wave, `D-B` a decision about the board itself.

| Id | What | Where | Who decides | Status |
|---|---|---|---|---|
| N-1 | Preamble 6.2 listed `tracing` as used by "all", contradicting 01 section a and CT-T12 (`moruna-kernel` depends on `arrow`, `dlpark`, `thiserror`, `blake3` only) | preamble 6.2 | PM (documentation, no interface change) | fixed on `infra/pm-wave0-prep` |
| N-2 | CT-T11 asserts "under 1 ms" and is untagged; the gate rule labels a timing figure measured off the reference host provisional, and a hard bound in a unit test is flaky on CI | 01 k, CT-T11 | PM (under delegation, 2026-09-22) | fixed on `infra/pm-wave0-prep`: tagged, timing provisional off the reference host, structural half runs everywhere |
| N-3 | Contracts d.12 cites "(DS, `Config` error)" without a section id; an executor may only read cited sections | 01 d.12, 03 (the guarantee table near line 168) | PM (under delegation) | fixed: cites 03 e.4 |
| N-4 | CONTRIBUTING and the preamble say one pull request per component; a session-sized feature plan needs several sessions per large component (placement, scheduler, contracts) | this board | PM | decided, D-B1 |
| N-5 | AD-T9 and AD-T10 (integration, wave 1) need the bench `normalise` kernel from the same wave; the bench PR must merge before those two close | 05 k, preamble 6.5 | PM (sequencing) | F1.5 waits for F1.7; noted |
| N-6 | `origin/main` is at `a972737`; the seven commits carrying the SDDs, the preamble revisions and the agents' prompts are local only; executors branch from `main` on GitHub | repository | PM | pushed 2026-09-22 |
| E1-env-1 | No `docker` on this machine; the container gate and the MinIO job run in CI only; an executor cannot run them locally before pushing | wave 0 environment (6.6) | PM | accepted: CI is the gate host for the container and MinIO jobs; executors record the fact under "Environment facts verified" |
| E1-env-2 | `python3.13t` and `python3.14t` are absent locally (3.13.7 and 3.14.3 GIL builds present); needed to prove the matrix locally in wave 0 and for component 5 in wave 1 | wave 0 and 1 environment | PM | closed 2026-09-22: installed with `uv python install 3.13t 3.14t` |
| E1-env-3 | `maturin` absent locally (wave 5) | wave 5 environment | PM | `uv tool install maturin` when wave 5 starts |
| N-7 | **A budget counts pages a memory pool is only holding, and S1 has a floor underneath that.** Two facts, found together. First: pyarrow allocates through jemalloc on Linux and jemalloc does not return pages until asked, so a process that has built and freed a dataset arrives at a run resident in memory it is not using. A runner reached this suite's S1 case holding 429 MB of a 512 MiB ceiling, leaving 53 MB above the arena for a kernel needing 84 MB per morsel; the run was refused and the peak had already passed the ceiling. The fixture now calls `release_unused()` before the run, and `inspect_host()['anon_bytes']` exposes the figure, but every user meets this: the README says so and the runtime could say it too, in the refusal, where it would be read. Second, underneath it: a budget smaller than the cost of measuring one morsel cannot be honoured at all, since the probe runs the kernel once to learn what it costs. No arrangement of the knobs closes the second; what is open is whether S1 should state it | 11 f.2 and f.3, 12 f.1, S1 in the architecture document, README | Brackly (a stated bound on the headline criterion is not a PM decision) | closed 2026-09-27 by F8.9: the runtime states the floor, in the refusal, where it is read. A budget under a run's floor (the arena's least, each engine's least and the reserve, over what the process already held) is refused before anything is built, naming the floor and what it is made of, and saying that a process which built and freed a dataset can hand the pages back. The probe's own cost stays the controller's diagnostic (f.7); the assertion stays `peak <= ceiling`, now measured for the whole process by the operating system |
| E1-ref-1 | The reference host is the Griot server in Nairobi; no access path for an agent is recorded | E1, wave 5 | Brackly | deferred to wave 5 (D-B3); the only open Brackly item |

### Decisions the board needs from Brackly

| Id | Decision | PM recommendation | Status |
|---|---|---|---|
| D-B1 | Several session-sized features per large component, all on the one component branch; one pull request reviewed against the SDD when the last feature lands, except that a self-contained part may merge early when it unblocks other executors (amended 2026-09-22 after finding that arena, discovery and trace need the contracts crate but no fake) | the former | decided 2026-09-22 (PM, under delegation): features are sessions on one component branch; the PM reviews each feature's commits as they land; the SDD review is once, at the end |
| D-B2 | Coverage floor 90% line coverage per crate, test code excluded, judged per crate, stubs not measured (as written into preamble 6.7 on this branch) | accept | decided 2026-09-22 (PM, under delegation) |
| D-B3 | How an agent runs the E1-tagged suite on the reference host: SSH access for the executor session, or Brackly runs `bench/` by hand and pastes the output into the wave 5 report | SSH for a wave 5 executor, read-only elsewhere | open until wave 5; Brackly |

## Wave reports

Filed here as each gate closes (pm.md section 2); the detail stays in the pull requests.

- Wave 0: the contracts crate, the testkit, the workspace, CI and the lint are merged and green. CT-T1 to CT-T19 pass except CT-T11's timing half, which is tagged for the reference host. Gate report not yet written.
- Waves 1 to 4: every component crate is merged and green on the developer host: arena, discovery, trace, adapters and the two bridges, the reactor, sources, sinks, placement, the scheduler and the controller. 573 tests pass, 15 are ignored and every one of those carries its tag's reason, and every crate is above 90% line coverage. What the reports still need is a run of the container job, which is where the tagged integration tests close, and the reference host, which is where every timing figure belongs (E1, D-B3).

### Delegated decisions log

Every item the preamble routes to the human that the PM decided under the delegation of 2026-09-22, newest last. A row here is also mirrored to `DECISIONS.md` when it traces to a Q-item.

| Date | Item | Decision | Where applied |
|---|---|---|---|
| 2026-09-22 | N-2 CT-T11 timing test | tagged (reference host, E1) for the timing; structural half everywhere | 01 k |
| 2026-09-22 | N-3 contracts d.12 citation | cites 03 e.4 | 01 d.12 |
| 2026-09-22 | D-B1, D-B2 | as recommended | this board, preamble 6.7 |
| 2026-09-22 | E1-env-1 | CI is the gate host for docker-dependent jobs | this board |
| 2026-09-22 | E2, arrow major (raised by F0.1) | one arrow in the workspace: arrow and parquet pinned to the 59 line, the version datafusion 55.1.0 and pyo3-arrow 0.19.0 require, because S7 and S13 need one RecordBatch type across moruna-kernel, the bridges and the Python surface | preamble 6.2, root Cargo.toml (F0.1 PR) |
| 2026-09-22 | E2, object_store major (raised by F0.1) | pinned to the 0.13 line, the version parquet 59 and datafusion 55.1 use, for the same one-type reason | preamble 6.2, root Cargo.toml (F0.1 PR) |
| 2026-09-22 | one pull request per component, amended | a component may land in more than one pull request when an early merge unblocks other executors; the whole-SDD review and the component gate happen on the last one; the contracts crate merges before its testkit so arena, discovery and trace start a session earlier | preamble 6.7, CONTRIBUTING, this board's start ladder |
| 2026-09-22 | bench E2 route, dataset naming, kernels PR (raised by F1.6) | preamble 6.5 is the bench agent's d.2, so the PM may approve a bench-only crate; the hand-rolled parser and RNG stand rather than churn them; dataset names live in `bench/README.md` as the reference; F1.7 kernels get their own branch and pull request | preamble 6.2 and 6.5, this board |
| 2026-09-22 | CI cost and pull requests (Brackly) | one workflow, one job, on pushes to main: format, lints, clippy, tests, the kernel boundary, the Python kernels. Coverage leaves CI (the hook enforces it); cargo-deny, the book, MinIO and the container tests move to a weekly `slow` job that also runs on demand; cargo-audit dropped as duplicated; the Python matrix becomes one interpreter, CPython 3.14 free-threaded, with the GIL path under PYTHON_GIL=1. No pull requests: push a branch, report, the PM reviews the diff and merges | `.github/workflows/ci.yml`, preamble 6.6 and 6.7, CONTRIBUTING, pm.md, executor.md, `architecture/agents/report-template.md` |
| 2026-09-22 | seven arena findings (raised by F1.1) | region rounds DOWN, not up to 512 MiB, which would start the process above its own ceiling; alloc(0) owns no slot; the mmap counting shim is always compiled; `Arena::arena_stats`; cudarc carries a CUDA version feature; `Allocator::note_payload_copy` and `note_boundary_copy` give the copy counters a writer | 02-arena.md, 01-contracts.md d.3, preamble 6.2, root Cargo.toml |
| 2026-09-22 | twelve discovery and trace findings (raised by F1.2, F1.3) | `StageReport` gains state_bytes_max and state_growth; `RunReport` gains overflow_failed and late_records; `LimitsSummary` defined; TR-I3 says compute; every libc call in probes.rs behind safe wrappers; the macOS fallback named; memory.peak is a floor, not the peak, because it counts page cache; a Present guarantee without its feature and durable_staging without a directory are Config errors; mimalloc recorded as component 4's dev-dependency with its measurement | 03-discovery.md, 04-trace.md, preamble 6.2 |
| 2026-09-22 | F7.1 notes: introduction pages, host-class pages (raised by F7.1) | introduction pages stay with F7.2 (same audience); the hosting guide keeps a GPU page marked described-not-verified (E1) and omits RDMA (E11, not a v1 host); Pages stays off until Brackly enables it, because publishing outward is outside the delegation | this board, F7.5 and F7.8 tasks |
| 2026-09-22 | parallelism rule (Brackly) | executors start when their dependencies are merged, not when their wave opens; one agent at a time only where a dependency forces it; separate worktrees | preamble 6.6, pm.md section 2, this board's start ladder |
| 2026-09-22 | preamble 6.3 reading (raised by F0.1) | `moruna-py` enables `python` and `uring` as the shipped module's features, set by F5.1 when the crate has code; the wave 0 stub keeps `default = []` so pyo3 never builds in `cargo build --workspace` | this board, F5.1 tasks |


### Codex integration update : 2026-10-30, source assembly and compiler repair

- Brackly assigned production implementation to the three existing agents; root owns integration, review and testing. Feature work remains local. No push, deployment, publication or Promotion 2 merge was performed here.
- Actual integration branch HEAD remains `38447a9`. Combined sources are being checked in an owned immutable proof candidate before normal-hook commits/merges; this is not a merged or accepted kernel.
- KE2 net2.0 actual ARM Linux full protocol/stack/DNS/robust/suites passed. The new Memory non-consumption test initially sent hello to a listener, then was corrected to establish the actual SDK session. Focused SDK/protocol/TLS replay: **50 passed, 0 failed**, one existing documentation example ignored; owned VM cleanup verified. Evidence `moruna-proof-501-81ce92c08133` under the private Linux proof root.
- Current root graph resolves offline against the exact **775 unchanged external source/version/checksum pins**. Combined standard-runtime check reached production placement and found three real old-Entry versus KD7a Catalog errors. Runtime owner adapted exact catalog/consumer/kind/size checks and retained actual PayloadRef ownership, without source re-query or consumer take; compiler replay is pending.
- Source now includes canonical KA8 events7 and the actual native typed duplex reader (`Message::Control(Outbound)` or canonical `EventMsg`), real charged Account/Memory origin ABI2.5, actual KD2 private storage bind and one shared startup identity envelope. These are source checkpoints, not accepted boot capabilities.
- Code still being completed: native storage raw journal/cache/provider and distinct checkpoint-metadata assembly; node service/authority/image assembly; compiler repairs; source/SDK/runtime model-CAS and checkpoint-history fixes awaiting integrated tests. Normal hooks, full Linux/host/kernel/coverage/Python gates and actual policy/release workloads remain open.
- Deployment audit: the task microkernel ELF/initramfs differs from the Linux/systemd host kernel currently required by Griot's assembler. Actual Kora-on-Moruna additionally needs socket/TLS, asynchronous IO and genuine persistent file/namespace/flush/exclusion/atomic exchange semantics or explicit Griot-owned native adapters. Current readonly imagefs plus ephemeral scratch is not Kora durable storage. No host conformance suite is counted as a Kora/Griot kernel boot.
- Current actual APIs, architecture/resource limits, full required code inventory and evidence paths are recorded in `/var/folders/1d/f7v5jtsj3hd43ltj5v94smnr0000gn/T/moruna-v1-codex-tq3aoqyt/host-integration-contract-20261030.json`.


### Consolidated Moruna branch, 2026-11-01

Brackly explicitly requested that all Moruna work be brought together, committed
on one branch and pushed, with changes confined to the Moruna repository. The
branch is `codex/moruna-consolidated-20261101`, based on integration `38447a9`.
This authorizes the integration snapshot push, not a release or deployment. Griot
source and its dependency pin remain outside this change.

The assembled source, the reviewed failure-diagnostic correction, and the three
existing local BOARD/specification/regression edits are reconciled here. The
private proof orchestration is preserved under `tools/proof-lab/`; large binary
artifacts and raw execution captures remain local and are excluded from Git.

See `architecture/moruna-integration.md` for the single integration handoff and
`tools/proof-lab/validation.json` for source-bound results. The latest strict CPU1
success case passed with 100,000 exact rows and sink acknowledgement. Case 2
stopped on a verifier diagnostic-field mismatch; its correction is reviewed but
not yet executed. Remaining CPU cases, native network/storage recovery, KVM
staging and actual Griot-on-Moruna acceptance remain open.


### Griot Task Library follow-up, 2026-11-10

Griot-owned fixes on `integration/local-cloud`: normalize the v2 `functions`
catalogue before rendering its Task Library; restore Container in Data processing
and the existing registry/image/command/environment/output drawer; retain the
shared drawer for imported callable parameters and image pins. Matching explicit
`definition.provenance.build_key` and `settings.image.build_key` allow Function
presentation without changing the executable image kind. MJS remains a job IR,
not an automatic catalog registration. Draft-time required-field diagnostics no
longer flood cards before explicit validation; incomplete jobs still refuse
compilation. Hidden browser pages release live streams and resync on return.

Remaining Griot integration work: a governed registry-to-compute image binding
with credential scope, digest verification, declared Arrow ABI/entry, staging and
portable import/export. The current semantic registry picker cannot yet run an
arbitrary image through the native authoring session; its compiler refuses that
case explicitly. Reusable image-backed catalog function registration/versioning,
the Griot decorator, and alternate execution hosts such as Ray are not implemented
by these UI fixes. No Moruna engine release is implied by this Griot work. Demo
localhost:8498 is held unchanged while the candidate runs separately on 8599.


Griot publishing design follow-up (2026-11-10): reuse existing generic OCI push
and resolver-based registry credentials; keep catalog/domain registration in the
Griot CLI/SDK integration. Planned UI includes per-callable image/catalog binding
status plus job-level bulk dependency resolution. Catalog registration is distinct
from image availability. Named registry profiles are optional generic convenience;
Griot options are provider-owned and validated outside portable MJS. The proposed
bulk publication path is not implemented. See Griot `docs/design/moruna-authoring-sessions.md`,
“Import, dependency resolution and bulk publishing”. Jobs retain no build-server
affinity; the original MJS remains available when relocated bindings produce a
new compiled revision.


Portable callable delivery map (2026-11-10): Griot design now separates the Moruna
Function contract from hosting: a Docker/OCI-hosted Moruna callable remains a
Function, while a plain OCI workload is Container. Moruna owns the stable callable
descriptor/identification and compatibility rule; Griot owns optional catalog
name/version bindings, authorization and artifact resolution. Work order and
acceptance are recorded in Griot `docs/design/moruna-authoring-sessions.md`,
“Delivery plan: callable identity independent of hosting”. First portable path:
formalize descriptor → compute OCI resolution → dependency preflight → UI bindings
→ external-registry acceptance. Catalog image-backed functions and bulk CLI/SDK
publication follow their stated dependencies. This is a work map, not a shipped
registry/catalog implementation or a claim that a Moruna release is ready.


Storage scope clarification (2026-11-10): Griot plans additive image-backed
function catalog records, preserving existing WASM/PEX package functions and
standard-kernel registrations. First delivery references digest-pinned images in
existing OCI registries; Griot is not to implement an OCI registry for this work.
Optional OCI archive upload can reuse managed artifact storage with explicit OCI
validation and compute staging, as a separate backend. See Griot design “Storage
decision: extend functions; do not build an OCI registry”. No package-to-image
conversion or retirement of existing function formats has been authorized here.


Superseding user decision : image-only Griot functions (2026-11-10): discard the
additive WASM/PEX compatibility proposal above. Griot function catalog entries
will reference runnable OCI images plus their Moruna callable definition/entry;
source code is not stored as a function and package-function upload/execution is
to be replaced in the candidate. Native built-in select/filter/join operations
remain Transform tasks, not another registered function format. Existing catalog
records need an explicit rebuild-to-image or retirement mapping; no automatic
source build, implicit conversion or protected-demo cleanup. Reuse external OCI
registries, with Griot-owned catalog resolution. The revised delivery sequence is
in Griot design “Current decision: image-only functions”. This is the accepted
target; the catalog/runtime migration remains implementation work.


OCI callable metadata implementation (2026-11-10): Moruna's builder now emits
`org.moruna.callable.version=1` and a canonical config-label descriptor containing
the existing MJS callable definition, entry, platform and build key. Minimal
manifest marker; no task params, credentials or Griot-specific metadata. Cache
reuse checks descriptor equality so older unannotated cached images are rebuilt.
The full descriptor matches the MJS declaration for source/function/sink builds.
Targeted builder/SDK/metadata checks: 61 passed, one existing opt-in skipped,
using a temporary source overlay with the installed 0.6.4 native extension.
The runtime command/env/layers comparison passes. SDK release remains pending;
the Griot integration and actual registry-to-compute acceptance are ongoing.
Contract: docs/image-callable-metadata.md.

Griot image-function acceptance update (2026-11-10): the isolated Griot candidate
now consumes descriptor v1 through connected OCI registries, versions image-only
catalog functions, supports bulk reference registration/identity reuse, and
materializes verified images on the native 0.6.4 compute. Registry-only SOC MJS
import/compile/export and execution passed: 13 tasks, 124 lifecycle events, peak
10 active tasks; browser Save/Play and individual node output passed. Warehouse
reads verified 4 cases/17 evidence/1 window. Reconnect replay preserved the draft
and MJS. Missing images imported/compiled and failed before tasks started. Source
package registration is retired in the new Griot path. Existing OCI layers were
republished with metadata without changing command/env/layers. SDK metadata
emission remains unreleased.

Open runtime issue from acceptance: at a 2 GiB explicit budget this SOC fan-out
can stop after its source with `staging: no unit can make progress`, all ten input
queues staged, and `0 of 0 budget bytes resident`. No unit is admitted despite no
running work. Reproduction reports (private local candidate):
`/Users/brackly/griot-kt/wazuh/authoring/state/candidate/state/4e1ebf6c34bb005e0671dccedaf699e7d9e2061968c3ea313cef2f2392d931d2/b7f64d1d8abe4c1893cc5dffdd979125/report.json`
and `82124267b145409aae7bd3961eefb2f7/report.json` in the same directory.
Griot now isolates engine accounting per execution and configures this 4 GiB demo
host with a 3 GiB runtime ceiling; repeat whole and targeted runs pass there.
This is a tested deployment configuration, not a fix for the lower-budget
Moruna scheduling diagnostic. Preserve the 0.6.4 runtime pin; investigate an
admission refusal/recovery regression before claiming that smaller-budget case.
