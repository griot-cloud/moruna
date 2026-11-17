# Moruna 0.7.0: a Rust runtime on its own VM

Status: plan, rewritten 2026-11-16 on Brackly's direction:

> Stop designing for Python. [...] It should be a Rust-based runtime, and it has its own
> microkernel, it has its own engine. So start from designing from there [...] let's have 0.7.0,
> a runtime in pure Rust that uses the virtual machine to run.

This replaces the earlier revision of this file, which designed 0.7 around a Python host (it is
in git history at `65acdc8f`). The decisions in "What stays" carry over from it.

**Under review:** `architecture/data-os.md` (2026-11-16) proposes a data OS under the microkernel
rule: OSTD and Moruna's core in kernel mode, with mechanisms that keep the engine's hot path out
of the kernel, and Asterinas's Linux layer in user space. It would reshape steps 2 to 6 below. Until Brackly approves
it, this plan stands, and only step 1's measurements proceed.

Brackly approved this plan on 2026-11-16, and the PM session agreed it the same day. The PM's
decisions are folded in below.

- **0.7.0 is this plan, plus finishing Goal A.** That means KD2 booted, KD3 and KD4.
- **Where the kernel runs (Brackly, 2026-11-16, through the PM).**
  - His words: "We have a Linux node, and then we run a VM on top of it, which is [Moruna]."
  - **The host is a Linux node** (Griot's D4ls_v5), and the Moruna VM runs on top of it under KVM.
  - **0.7.0 is the one big release.** In his words: "We don't need 0.7 and then 0.8."
  - **The kernel as the node itself** (Goal B, `architecture/kernel-is-the-machine.md` on
    `design/kernel-is-the-machine`) is parked and unscheduled. He is not asking for it.
  - **Whatever of Goal B 0.7.0 needs comes into 0.7.0.**
  - **Asterinas's Linux layer is a firm 0.7.0 step** (§7a). In his words: "Where is Asterinas?"
    and "Once Asterinas becomes involved, Linux is gone." It replaces Moruna's own Linux
    personality (`kernel/linux`). Every Linux program in an image runs on it, the gate's CPython,
    NumPy and pyarrow included. With the engine native (§1), no Linux code is left inside the VM.
    The only Linux is the node that hosts it.
- **Every step is this branch's.**
  - It builds on the work still on `feat/kd2-engine-process`, `feat/kd1-data-plane`,
    `feat/ka2-plane-linux` (its conformance suite is KD3's base) and `feat/kb5-memory-quota` (the
    account counting step 2 needs).
  - `feat/kd7c-policies` stays out: it changes scheduler decisions, which is core work, not wiring.

## What 0.7.0 is

- **One VM per run** on a Linux x86_64 node with KVM. Inside it: Moruna's kernel, the engine as its
  first process, and the job's tasks as processes. Batches move between them as memory pages.
- **Everything of Moruna's inside the VM is Rust.** That covers the kernel, its servers and the
  engine.
  - The engine links no library it does not need, and its memory is the arena, not `malloc`.
  - C code stays only where a measurement shows that removing it costs speed (§5, decision 8).
- **User code runs as tasks, in images, in any language.** A Python function is one of them: its
  CPython is in the image, not in Moruna.
- **The host side is small.** It launches the VM, hands it the job and its data, and streams back
  events, logs and the report.
- **Python as an interface comes afterwards** (§8): the `moruna` package as a client of this
  runtime, and a shim in a Python image. It is not part of 0.7.0's design.

```
+--------------------------------------------------------------+
| tasks      processes on the kernel, from images, any language|
| engine     moruna: sources, scheduler, sinks, arena, controller|
| plane      Arrow records in pages: batch_get, batch_put        |
| kernel     Moruna's kernel (OSTD, Rust), its drivers, servers  |
| launcher   moruna-vmm: the VM, its devices, the vsock          |
| node       Linux x86_64 with KVM (Azure Standard_D4ls_v5)      |
+--------------------------------------------------------------+
```

`architecture/optimized-runtime.md` describes this target. This plan says what 0.7.0 builds of
it, in what order, and what leaves.

## What Moruna is, and stays (Brackly, 2026-11-16)

> make sure we maintain what moruna is, there is a risk of changing things just because were
> wiring in

Moruna is memory-aware, adaptive execution:

- the arena;
- the scheduler and its sizing;
- the controller and its models;
- placement and spill;
- the reactor;
- checkpoint and resume;
- the run report and the events.

The VM work wires these onto the kernel through their ports, as new adapters beneath them. It
does not change what they decide or how.

- **No change to core logic as a side effect of wiring.** A change to how the scheduler, the
  controller or placement decides needs its own reason. It is measured on its own, and it is
  never folded into a wiring step.
- **Same rows, same refusals, same decisions.** KD3's conformance runs every kind on a Linux host
  and on the kernel, and the two must agree.
- **Same speed or better.** The gate (§9) reruns on the benchmark VM when the work is complete.

## What stays (settled 2026-11-16)

- **No engine or library code in Moruna.** Data formats may be exceptions, engines may not. Kept
  formats: Parquet, Arrow IPC, safetensors, npy.
- **Already removed on this branch:**
  - peQL and DataFusion (`171b2d53`);
  - Polars and Vortex (`1b085bb9`);
  - MRB1 as a tensor file format (`65acdc8f`).
- **Arrow is the contract.**
  - Between processes, a batch is one Arrow IPC record in pages (contracts e.7).
  - Tensors are DLPack inside a task and cross the plane as Arrow (`fixed_shape_tensor`), with
    no copy. Moruna defines no tensor format of its own.
- **Analytical workloads.**
- **No regressions** (§9).
- **One platform: Linux x86_64.** Staging and production run on the D4ls_v5. The Dlsv5 series
  supports nested virtualization, so the VM runs there under KVM.

## Where it stands (read from the code, 2026-11-16)

| Piece | State |
| --- | --- |
| Launcher, `moruna-vmm` | Runs QEMU with KVM, jailed, on Linux x86_64 only (`microkernel.rs:726-768`). Its own KVM monitor exists only under `cfg(test)`. |
| Kernel, `kernel/core` | Real: about 7,000 lines on OSTD. The drivers, image filesystem and Linux personality are real too. |
| Data plane (KD1) | The kernel moves pages: a 64 MiB batch with 0 bytes copied, under TCG. In review. |
| Engine on the kernel (KD2) | The root workspace's runtime, built as a static musl Linux program and run by the personality. Code complete, never booted (SDD 30 m.6). |
| How the engine reaches its kernel | Every Linux syscall it makes is an IPC round trip to the personality process (`kernel/core/src/process.rs:976-979`). |
| Stage boundary | Two copies per batch: `offer` lays it out in the plane, `land` copies the task's output into the run's arena (`crates/moruna-micro/src/task.rs:8-12`, `690-747`). |
| Data in and out | Inputs come from image layers. Object URLs are refused without a storage backend. Outputs stay in scratch and leave with the VM (SDD 30 m.3). |
| C in the engine | `zstd-sys` (through `arrow-ipc` and `parquet`); `ring` (through `object_store`, `reqwest` and `rustls`); `blake3`'s C path; `wasmtime`; musl. Snappy, LZ4, brotli and deflate are already pure Rust. |
| Conformance (KD3), launcher devices (KD4) | To do. |

## The design

### 1. The engine is a native process of its kernel

**Today** the engine is a Linux program:

- musl sits under it;
- the personality sits between it and the kernel, so every file read, `mmap` and futex is an IPC
  round trip;
- its memory comes from `malloc`.

That shape adapts the engine to an interface that is not its own.

**In 0.7** the engine is the kernel's own process, reversing DECISIONS EN1. It calls the kernel
directly through `moruna-sys`: memory objects, slots, channels, threads, futexes and the clock.

Step 1 measures today's forwarding and copies, as the "before" figure. If those numbers show the
port gains little, they go to Brackly before anything is dropped.

- **How.** Rust's standard library is ported to the kernel as a target (`x86_64-unknown-moruna`),
  as `x86_64-unknown-hermit` is in Rust today. Arrow and Parquet need `std`, so this keeps them
  unmodified. The port covers what the engine uses:
  - allocation (§2);
  - threads, and synchronization on futexes;
  - time;
  - files on the image filesystem and in scratch, through their protocols;
  - standard output and error to the log ring.

  Networking and process creation are `Unsupported`, because the engine has neither in the VM.
- **The engine's ports get kernel adapters wherever they assumed Linux:**
  - the reactor waits on the kernel's channels instead of epoll;
  - discovery reads the start page and the run's account instead of `/proc`, `/sys` and cgroups;
  - I/O goes to the block driver (§4).

  tokio and mio leave the engine.
- **Tasks stay Linux programs,** on Asterinas's Linux layer (§7a). That is what lets user code be
  in any language.
- **Until the port runs,** the engine keeps running as today's Linux program, so every other step
  can proceed and be measured.

### 2. Memory: one allocator, the arena, counted by the kernel

- **The engine's global allocator is the arena.** The arena is built from memory objects charged
  to the run's account. Every byte the engine holds is in it, and the kernel's account is the
  measurement. Regions above 2 MiB use huge pages.
- **What goes:**
  - `malloc` and allocator tuning (`allocator.rs`, which only acts under glibc);
  - the counting allocator.
- **A task's memory is its own process's,** charged to its own account. Which C library or
  allocator it uses is its business, bounded by that account. Moruna hooks nothing in it.
- **The controller is unchanged.** Its models stay, the out-of-arena model included: a task's
  memory per batch is still what that model predicts. Only where its measurements come from
  changes. Its `Sampler` port gets a kernel adapter that reads the run's account and each task's,
  exact figures in place of sampled RSS.

### 3. Batches are memory objects, end to end

The arena is the plane's (SDD 30 EN-O1). A batch lives in arena memory objects from the moment it
is decoded until a sink writes it.

- **Engine to task.**
  - The batch's memory objects go into the task's slot and are mapped read only. The `offer` copy
    goes.
  - This needs two things:
    - each batch's buffers in memory objects of their own, so its pages hold no other batch's
      bytes;
    - `slot_push` of a list of ranges, a change to SDD 20's call 44.
- **Task to engine.** After the check (DP1), the window the task wrote is an arena batch as it
  stands. The `land` copy goes.
- **Fan-out** is the same pages mapped read only into each consumer, freed after the last one
  (DP-I6).
- **What is still copied:** only format changes, meaning decoding Parquet into Arrow and encoding
  for a sink.
- **Proof.** The plane's `COPIED` counter stays at zero, and so does an engine counter of boundary
  copies, for a batch that passes through any number of image stages.

### 4. Data in and out: devices, not a network

The VM has no network.

- **Inputs.**
  - Each input is a read-only block device the launcher attaches: a layer (content-addressed and
    checked by the image filesystem) or a plain file.
  - The engine reads it through the block driver straight into arena pages, with no page cache in
    between.
  - Uncompressed Arrow IPC is mapped, not read.
- **Remote objects** (S3, GCS, Azure) are fetched on the host before the VM boots, or streamed
  through a host port. `object_store`, `reqwest`, `rustls` and `ring` leave the engine.
- **Outputs.** The engine writes them to a data disk the launcher attaches, in a layout the host
  reads back (SDD 30 m.3). The recommendation is each sink's files as one output layer, which the
  host reads with `moruna-layer`.
- **Spill** goes to the same data disk.

### 5. No library dependencies in the engine, and no C where it costs nothing

The no-regression rule outranks "no C" (the PM's decision, 2026-11-16). Every 0.6.6 benchmark
writes zstd Parquet, and Rust's `snap` already loses about 1.5x to the C++ codec.

- **zstd.** C zstd (`zstd-sys`) stays unless a pure-Rust zstd (`ruzstd`) matches it on the gate's
  jobs. `ruzstd` decodes slower and compresses far worse today. Brackly's call is open: see
  "Decisions".
- **blake3:** its pure-Rust implementation, if it measures the same.
- **`ring` and the HTTP stack:** gone with §4. They are library dependencies the VM cannot use.
- **musl:** goes with the native engine (§1).
- **wasm tasks leave 0.7.0.** wasmtime is an engine, with C parts and a memory model of its own,
  and image tasks already cover every language.
- **The check.** Each `cc` build and `-sys` crate left in the engine target's dependency tree is
  listed with the measurement that keeps it, and the build refuses any other.

### 6. The host side

- **`moruna run`** on a Linux x86_64 node with KVM does five things:
  - launches the VM;
  - attaches the input devices;
  - sends the job over the vsock;
  - streams events, logs and the report back;
  - reads the outputs.
- **QEMU stays** the launcher's hypervisor in 0.7.0. `moruna-vmm`'s own KVM monitor can replace it
  later; that does not change the runtime's speed.
- **Logs ride the job updates** (kept from the earlier plan). A task's text lands in the plane's
  log ring (SDD 29 f.7). It goes out on the event stream with its producer, its level and its line,
  rate-limited per producer. A host such as Griot forwards it; Moruna carries no Kora code.
- **Development.**
  - This Mac (arm64) boots the x86_64 kernel only under emulation (TCG), so it checks correctness.
  - Speed is measured on an x86_64 Linux machine with KVM.

### 7. What leaves, or waits for Python

**Leaves the VM runtime:**

- running jobs in the user's own interpreter (`moruna-py`'s `_core`);
- `moruna-adapters`' Python path: the guard's hooks, the NumPy hook and the boundary copy;
- `moruna-sandbox`'s host confinement;
- `allocator.rs`;
- the counting allocator;
- the engine's host Linux paths (io_uring, `/proc`, cgroups).

**Removal goes in step order.** Some of these still serve the KD3 fixture and the `.pex` path,
which embeds Python through pyo3 (`kd3-fixture.rs:769`, `native_pex.rs`). Each piece goes in the
step that replaces it.

**Set aside, not landing:** the local branch `wip/engine-heap-reuse`, which holds glibc tuning
and a partial removal of the NumPy hook.

### 7a. Linux programs on Asterinas's Linux layer

This follows `architecture/kernel-is-the-machine.md` §4.1 and §4.2, on `design/kernel-is-the-machine`,
cut to what a VM with no network needs. That design's steps 2 and 3 are in 0.7.0; its sockets,
cloud platform and boot-time job (steps 4 and 5) are not.

- **The core's four mechanisms.** None of them knows Linux, and each is tested in the core's own
  suite with no personality present:
  - a personality entry on the calling thread, in the shape of Zircon's restricted mode, in place
    of today's message per call (`foreign.rs`);
  - copy-on-write clones of a memory object and of an address space, which `fork` needs;
  - a user pager, which carries file-backed `mmap` and the page cache;
  - thread state and interruption, which signals and `EINTR` need.
- **The port.** Asterinas's Linux layer runs in user space, one instance per image task.
  - About 137,000 lines, of which about 82% carry with mechanical swaps (OSTD's locks and wait
    queues onto the core's futex).
  - About 25,000 lines are rewritten against the four mechanisms.
  - What 0.7.0 needs: processes, files, pipes, signals, epoll and the synthetic filesystems.
  - It is pinned to one Asterinas commit, and the carried files stay MPL-2.0, as DECISIONS MK1
    accepted for OSTD.
- **The data plane carries over.** A task's `batch_get`, `batch_put` and `port_call`, and its
  descriptors 0 and 1, work on the new personality as they do on today's (SDD 29).
- **Done when:**
  - static musl and dynamic glibc programs run on it, with no shell;
  - `kernel/linux` is retired.

  CPython on it waits for §8. The gate's tasks are native and do not wait on this step.
- **The figure to beat.** Step 1's measurement of today's personality.

### 8. Python, afterwards

Python comes back in three parts, designed once the runtime runs at full speed:

- a client that submits a job and reads its events, logs and report;
- a shim in a Python image layer, which hands the function a `pyarrow.RecordBatch` read in place
  from `batch_get`;
- the builder that turns a decorated function into image layers (optimized-runtime 7).

## 9. The no-regression gate

0.7.0 is not versioned until every 0.6.6 benchmark holds.

**What runs (Brackly, 2026-11-16).** "We have data, then a function, then producing some other
data; we're supposed to measure that, simple."

- Each of the seven jobs keeps its input data and its output. Its function is ported to Rust and
  runs as a native Moruna task in the VM, through the plane (`moruna_sys::plane`, stream P).
  There is no Python, no libc and no Linux layer on the gate's path.
- The port must produce 0.6.6's output rows from 0.6.6's input: the same row count and the same
  content hash, both taken on a canonical form (sorted rows; a per-batch aggregate re-aggregated
  by its group keys first), since batch boundaries may differ between the builds. A float column that cannot match bit for bit is named, with its tolerance, before
  it is measured.
- It is timed against 0.6.6 running the same job on the same data: wall time and peak memory, on
  the method below.

| Benchmark (8 CPUs, 24 GiB) | 0.6.6 | Loop | Ray Data |
| --- | --- | --- | --- |
| NYC Score (NumPy) | 25.2 s | 27.0 s | |
| NYC Occupancy (pyarrow) | 7.6 s | 8.0 s | |
| NYC Enrich (vectorised, filter) | 7.0 s | 7.5 s | |
| NYC Audit (pure Python) | 14.8 s (14.2 to 14.8) | 14.5 s | |
| Overture shapes | 723 s | 870 s | 941 s |
| Overture attributes | 1,118 s | 1,142 s | 1,111 s |
| FineWeb MinHash | 2,144 s | 2,184 s | |

**Method** (unchanged from the earlier plan):

- **Two machine types,** the D8ds_v6 and the D4ls_v5, each with both builds side by side in one
  session.
- **Alternate the order.** NYC runs in pairs, and the large jobs change which build goes first.
- **Measure as 0.6.6 did:**
  - NYC: one learning run, then five measured repeats;
  - the large jobs once per build.
- **Measure the noise.** Two NYC pairs of 0.6.6 against itself give the same-session noise.
- **Pass.** On every benchmark, 0.7's median `wall_s` and median peak memory are no worse than
  0.6.6's beyond that noise, with no OOM kills.
- **The PM's controls for ported functions** (2026-11-16):
  - 0.6.6 is also timed with the same Rust function where it can run one, and the report says
    so where it cannot;
  - a plain Rust thread pool (arrow-rs, parquet, rayon) runs the same function as the "other
    options" baseline, beside Ray and Spark running the Python functions, labelled;
  - every run uses the same zstd level, row group size, files and splits;
  - CPU-seconds and peak memory are reported beside wall time;
  - each job's time is split into read and decode, the function, encode and write, and waits;
  - each port records the Python function it replaces and its matched rows.
- **The PM's NYC matrix** (2026-11-16), each row labelled by its function's language:
  - **Machines:** D8ds_v6 first, then D4ls_v5. griot-stg's 10-vCPU quota holds one at a time.
  - **Rows:**
    1. 0.7 on the data OS, with the Rust function;
    2. the plain Rust thread pool, with the Rust function;
    3. 0.6.6 with the Python function, and with the Rust function where it can run one;
    4. the loop, Polars, DuckDB, Ray, Spark and Dask with the Python function, as `runner.py`
       runs them;
    5. native expressions where a job needs no UDF: Enrich as DuckDB SQL and as Polars
       expressions, and Occupancy as DuckDB SQL. Score and Audit have none.
  - **Columns:** median wall, CPU-seconds, peak memory, OOM kills, and the time split. 0.7's
    speedup is shown against the best of row 2 and the best of row 5, not only against the
    Python rows.
  - **"Significantly better":** at least 1.5x the median wall of the best like-for-like row on
    every NYC job, with CPU-seconds no worse. The native-expression rows are reported beside it,
    and any gap is named.
  - **Raised by Brackly (2026-11-17): win every benchmark.** "You have a whole OS to optimize for
    data workloads. I expect nothing less than dominance in every benchmark." 0.7 passes a job
    only when its median wall is at least 1.5x faster than the best of **every** row, the
    native-expression rows included, and its CPU-seconds are no more than any row's. A job that
    misses gets another round on the stage its time split names; it is not done.
  - **Then prune what the OS does not use** (Brackly, same day). The timed runs carry no
    counters. They keep what already exists: the Linux calls the server refused, the processes
    the boot started, and the report's `vm` object. A separate, untimed usage build (stream U:
    OSTD's `coverage` feature and minicov, off by default, the production build unchanged)
    runs the same jobs. It records the native calls and their counts, the Linux calls answered
    and refused, the drivers and servers started, and the coverage of kernel mode, OSTD and each
    service. Everything with zero use across all jobs becomes a removal candidate: kernel calls,
    OSTD parts, personality subsystems, servers and drivers. Each one removed also shrinks the
    attack surface. Removal waits for FineWeb and Overture too, so nothing the large jobs need
    is cut.

The harness is `bench/vm/`. It gains a mode that runs the job through `moruna run` on the VM.

**The PM's requirements (2026-11-16):**

- **Same inputs and the same output rows,** checked by row count and a content hash.
- **VM boot reported separately,** and included in the wall time.
- **Superseded by Brackly (2026-11-16):** the PM's requirements for parallel Python, and its
  rule that Rust ports are never the gate. The gate measures data in, the function, data out,
  with the function in Rust.

**"Much better" is written as per-job targets after step 1's KVM measurement, not before.** The
items to commit to then:

- boundary copies from two to zero;
- huge pages for the arena;
- block reads straight into arena pages;
- decode on the worker that consumes the batch, which is the cache-locality loss behind Audit's
  last 1 to 3%.

**What the image path pays today,** and the step that removes each cost:

| Cost | Removed in |
| --- | --- |
| Two boundary copies per batch | Step 2 |
| The engine's own forwarded syscalls | Step 4 |

## Order of work

1. **Boot what exists (KD2) on KVM**, on the benchmark VM.
   - First, take what the KD1, KD2, KA2 and KB5 branches have that this branch lacks.
   - Run a real job end to end with the engine as today's Linux program (KD2-T9).
   - Run the NYC jobs as image tasks. Measure the forwarded syscalls, the boundary copies and the
     VM's boot.
   - Measure how today's personality runs the gate's CPython, NumPy and pyarrow images. That is
     the "before" figure for §7a, not a test that decides it.
   - This is the baseline for every step after it, and for the per-job targets.
2. **Batches as memory objects end to end** (§3).
   - `slot_push` of ranges; `offer` and `land` stop copying.
   - KB5's account counting.
   - The arena on memory objects, with huge pages.
3. **Data devices** (§4): KD4's launcher devices; inputs read from block devices into the arena;
   outputs on the data disk; `object_store` out of the engine.
4. **The native engine** (§1, §2):
   - the `std` port;
   - the arena as the global allocator;
   - kernel adapters for the reactor and discovery;
   - EN1 retired.

   It starts once step 1 shows what the forwarding costs. Steps 2 and 3 make their kernel calls
   through `moruna-sys`, which works in either form of the engine. If step 1 shows the port gains
   little, the numbers go to Brackly before the step is dropped.
5. **Asterinas's Linux layer** (§7a):
   - the core's four mechanisms, first;
   - then the port of processes, files, pipes, signals, epoll and the synthetic filesystems;
   - then the gate's images on it, and `kernel/linux` retired.

   It starts after step 1, beside steps 2 to 4. It changes the kernel and the personality while
   they change the engine, so the two lines of work do not collide.
6. **Dependencies** (§5): wasm out, the HTTP stack out, C kept only where measurement says so,
   and the build check.
7. **The host side** (§6): `moruna run` and logs on the job updates.
8. **KD3 conformance on the kernel, then the gate** (§9), rerun on the benchmark VM.
9. **Python** (§8).

The order puts measuring first, then the copies, because they cost on every batch.

## Decisions

Settled on 2026-11-16, by Brackly's approval of this plan and by the PM:

1. **Python in 0.7.0.** The runtime has none. Python functions run as image tasks, and the `moruna`
   package returns afterwards as a client (§8).
2. **The gate.** The seven jobs' functions, ported to Rust, run as native tasks in the VM on 0.6.6's
   input and must produce 0.6.6's output rows. They are timed against 0.6.6 on the PM's method
   (§9). Brackly, 2026-11-16.
3. **How the engine reaches its kernel.** As the kernel's own process, through a `std` port (§1,
   step 4), as approved. If step 1's numbers show the port gains little, they go to Brackly.
4. **wasm tasks leave 0.7.0** (§5).
5. **Outputs on the data disk as layers** (§4, SDD 30 m.3).
6. **QEMU stays the launcher's hypervisor** in 0.7.0 (§6).
7. **Where it runs.** On `moruna-bench` (D8ds_v6, `/dev/kvm`, griot-stg), pre-approved for this
   work, under `~/.vm-lock`, and before its 02:00 UTC shutdown wipes `/data`.

Open, Brackly's:

8. **"No C" or "no regression" when they conflict.** Brackly asked for a runtime "in pure Rust"
   and said C libraries are not needed in Rust. The PM ranks the no-regression rule higher: C zstd
   stays unless `ruzstd` matches it (§5), and musl stays unless the native engine pays (§1).
