# Moruna 0.7.0: a Rust runtime on its own VM

Status: plan, rewritten 2026-11-16 on Brackly's direction:

> Stop designing for Python. [...] It should be a Rust-based runtime, and it has its own
> microkernel, it has its own engine. So start from designing from there [...] let's have 0.7.0,
> a runtime in pure Rust that uses the virtual machine to run.

This replaces the earlier revision of this file, which designed 0.7 around a Python host (it is
in git history at `65acdc8f`). The decisions in "What stays" carry over from it.

## What 0.7.0 is

- **One VM per run** on a Linux x86_64 node with KVM. Inside it: Moruna's kernel, the engine as its
  first process, and the job's tasks as processes. Batches move between them as memory pages.
- **Everything of Moruna's inside the VM is Rust.** That covers the kernel, its servers and the
  engine. The engine links no C library and calls no `malloc`.
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
- **Tasks stay Linux programs under the personality.** That is what lets user code be in any
  language.
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

### 5. No C in the engine

- **zstd:** a pure-Rust zstd (`ruzstd`) for Arrow IPC and Parquet, with Parquet's codec patched to
  use it. The cost to measure is decode speed against C zstd on Overture.
- **blake3:** its pure-Rust implementation.
- **`ring` and the HTTP stack:** gone with §4.
- **musl:** gone with §1.
- **wasm tasks leave 0.7.0.** wasmtime is an engine, with C parts and a memory model of its own,
  and image tasks already cover every language.
- **The check.** The engine target's dependency tree has no `cc` build and no `-sys` crate, and
  the build enforces it.

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

### 8. Python, afterwards

Python comes back in three parts, designed once the runtime runs at full speed:

- a client that submits a job and reads its events, logs and report;
- a shim in a Python image layer, which hands the function a `pyarrow.RecordBatch` read in place
  from `batch_get`;
- the builder that turns a decorated function into image layers (optimized-runtime 7).

## 9. The no-regression gate

0.7.0 is not versioned until every 0.6.6 benchmark holds.

**What runs.** The 0.6.6 jobs, with their kernels unchanged, run as image tasks inside the VM,
with CPython in the image. They are compared with 0.6.6 running them in process. Kept as image
tasks, the Python kernels are just programs in images, so the comparison needs nothing
Python-specific in Moruna.

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

The harness is `bench/vm/`. It gains a mode that runs the job through `moruna run` on the VM.

**What the image path pays today,** and the step that removes each cost:

| Cost | Removed in |
| --- | --- |
| Two boundary copies per batch | Step 2 |
| The personality forwarding CPython's syscalls | Hot calls answered in the kernel, if step 1's measurements show it matters |
| The engine's own forwarded syscalls | Step 4 |

## Order of work

1. **Boot what exists (KD2) on KVM**, on the benchmark VM.
   - Run a real job end to end with the engine as today's Linux program (KD2-T9).
   - Run the NYC jobs as image tasks. Measure the forwarded syscalls and the boundary copies.
   - This is the baseline for every step after it.
2. **Batches as memory objects end to end** (§3): `slot_push` of ranges; `offer` and `land` stop
   copying.
3. **Data devices** (§4): inputs read from block devices into the arena; outputs on the data disk;
   `object_store` out of the engine.
4. **The engine native** (§1, §2):
   - the `std` port;
   - the arena as the global allocator;
   - kernel adapters for the reactor and discovery;
   - EN1 retired.
5. **No C** (§5): `ruzstd`, blake3 in pure Rust, wasm out, and the build check.
6. **The host side** (§6): `moruna run` and logs on the job updates.
7. **KD3 conformance on the kernel, then the gate** (§9).
8. **Python** (§8).

The order puts measuring first, then the copies, because they cost on every batch. The native
engine is the largest piece; it starts once step 1 shows what the forwarding costs.

## Decisions for Brackly

1. **Python in 0.7.0.** The runtime has none. Python functions run as image tasks, and the `moruna`
   package returns afterwards as a client (§8). Recommended.
2. **The gate.** The 0.6.6 jobs, with the same kernels, run as image tasks in the VM against 0.6.6
   (§9). Recommended.
3. **How the engine reaches its kernel.** Recommended: natively, through a Rust `std` port (§1).
   The alternative keeps it a Linux program on a pure-Rust C library (c-ward). That removes the C,
   but keeps the personality and its IPC between the engine and its kernel.
4. **wasm tasks leave 0.7.0** (§5). Recommended.
5. **Outputs on the data disk as layers** (§4, SDD 30 m.3). Recommended.
6. **QEMU stays the launcher's hypervisor** in 0.7.0 (§6). Recommended.
7. **Where it runs while CI is paused.** Step 1 onwards needs an x86_64 machine with KVM. This Mac
   can only emulate one. The benchmark VM on griot-stg is the candidate.
