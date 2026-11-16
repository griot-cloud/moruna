# The data OS: Asterinas and Moruna, fused

Status: design for Brackly's review, 2026-11-16. Nothing in it is built. Until he approves it,
the only work is step 1's measurements (`architecture/arrow-core-0.7.md`).

> We should first identify performance critical processes and let them run in the kernel. And
> then anything else runs on top. [...] The goal is to be able to have a data operating system
> that is literally [Asterinas] and [Moruna] built together and fused.

## In one page

- **One kernel inside the VM.** It is Asterinas's kernel, with Moruna's data path built into it
  as kernel components. The Linux node outside stays (Brackly, 2026-11-16), and the VM runs on it
  under KVM, one job per VM.
- **The kernel holds every cost that 0.6.6's benchmarks measured as a crossing.**
  - **Memory:** the arena's pages stay mapped, use huge pages, and are never faulted in or zeroed
    again on reuse.
  - **Copies:** batches move between the engine and user code as pages, never as bytes.
  - **I/O:** block reads land in arena pages.
  - **Linux calls** are answered in the kernel, with no message to a server.
  - **Placement:** a batch's work runs where its data is.
- **Everything else runs on top, as processes on Asterinas's Linux interface.**
  - Moruna's engine, unchanged in what it decides.
  - User code in any language, a Python interpreter per process.
- **Recommended base: (A), Asterinas's own kernel with Moruna fused in.** It is what "literally
  fused" says. It is also smaller than carrying Asterinas's Linux layer into user space on
  Moruna's microkernel (B). The VM, one per job, is the isolation wall, and OSTD still holds all
  the unsafe code (§4).
- **What it reverses:**
  - DECISIONS MK1's "Asterinas's Linux kernel is not used";
  - BOARD's "everything else is a process" for this kernel;
  - the rejection of "Asterinas's whole kernel" in `kernel-is-the-machine.md` §4.8.

## 1. What is performance-critical: the evidence

Every figure below comes from the 0.6.6 runs on the benchmark VM (Linux, 8 CPUs, 24 GiB,
2026-11-13 and 2026-11-14) and from `CHANGELOG.md`'s 0.6.6 section. "Kind" says what removes the
cost.

| Cost | Measured | Kind |
| --- | --- | --- |
| Page faults and zeroing from memory returned to the OS | Occupancy spent 21 of its 53 CPU-seconds in the kernel, against 8 for the plain loop; Enrich 14 of 45, against 2. Keeping freed heap pages took Occupancy from 7.4 s to 5.4 s. | Crossing: fault |
| The landing copy of arena columns on a function's output | Fixed in `19aaefc6`. | Crossing: copy |
| Two copies per batch at an image stage's boundary | `offer` and `land` in `crates/moruna-micro/src/task.rs`. | Crossing: copy |
| Linux calls forwarded to the personality as messages | Every Linux call of the engine and of every task (`kernel/core/src/process.rs:976-979`). Step 1 measures it under KVM. | Crossing: call |
| A Python thread state made and freed per batch | Fixed in `30c359ca`. Every batch started with cold allocator heaps. | Crossing: user boundary |
| The allocation guard on every Python object | 1.9 of Audit's 106 CPU-seconds. | Crossing: user boundary |
| Decode on one core, the function on another | Audit's last 1% to 3% against the loop, which decodes and computes on one core. | Placement |
| Free-threaded GC stopping every thread | On a Mac, GC was 34% of one thread's time and doubled it at eight threads. Ray's per-process GC avoids it. | User runtime |
| One process's C heap shared by eight workers | Overture shapes (shapely, a billion GEOS geometries) ran 38.3 s on 8 files against the loop's 31.4, contending on four C heaps; with glibc's default number of heaps it took 26.4 s. | User runtime |
| Parquet decode and encode | Enrich spent about 40 of 46 CPU-seconds reading and writing outside the function, against about 27 for pyarrow. Rust's `snap` decodes about 1.5x slower than C++. Linux Parquet encode costs more than pyarrow's (statistics, RLE, zstd). | Compute |
| Parallel encode and write lanes | Overture shapes: Moruna 723 s, the loop 870 s, Ray Data 941 s, from a Parquet sink writing several files at once and decoding and encoding on the worker pool. | Compute and I/O |

Step 1's KVM figures (forwarding, boundary copies, VM boot) are added here when they exist.

**What the evidence says:**

- **Crossings** (faults, copies, calls, the user boundary) are what a kernel can remove. They are
  the largest measured costs that remain.
- **Placement** is a scheduler decision. The kernel can make it cheap to carry out: a worker on a
  given core, woken there.
- **Compute** runs at the same speed in kernel mode and in user mode, because it is the same
  instructions. Kernel mode does not make a codec faster. Better codecs, better encoding and
  better placement do.
- **User runtime** costs come from how user code is hosted. A process per interpreter removes
  GC stops and heap contention between workers.

So "performance-critical, in the kernel" means: every crossing and the mechanism of placement.
Compute runs on top, in the processes whose code it is.

## 2. What runs in the kernel and what runs on top

**The Arrow line.** Below it are pages, blocks, rings and counters, and they go in the kernel.
Above it are Arrow and Parquet, and they run on top.

| In the kernel (safe Rust components on OSTD) | On top (processes on the Linux interface) |
| --- | --- |
| **The arena:** kernel memory objects for the run, huge pages, mapped into the engine pre-faulted, never zeroed on reuse, charged to the run's account | **Moruna's engine:** sources, the scheduler, placement, the controller, sinks, resume and the report, a Rust `std` program, its decisions unchanged |
| **Block I/O into arena pages:** reads and writes submitted on a shared ring and completed into arena frames by the virtio-blk driver's DMA, with no page cache in between | **Parquet and Arrow IPC decode and encode,** on the engine's worker threads |
| **The data plane:** batches handed to a task as read-only mappings of their arena pages, and output windows handed back, over shared rings (§3) | **User code:** any Linux program from an image; Python as one interpreter per process |
| **The record check** (`moruna-ipc-check`, already `no_std`), run on a window the task can no longer write (DP1) | **The host protocol** over vsock |
| **Accounts:** the run's and each task's memory, exact, which the controller reads | |
| **Placement mechanism:** a worker pinned to a core and woken there; which batch goes where stays the engine's decision | |
| **Asterinas's Linux layer,** in kernel mode, as Asterinas has it: processes, files, pipes, signals, epoll, procfs and the rest | |
| **The image filesystem:** Moruna's verified layers (SDD 27) as a kernel filesystem | |
| **Drivers:** virtio-blk and virtio-vsock, Asterinas's own | |

**Why the engine stays on top.**

- Arrow and Parquet need Rust's `std`, so in the kernel they would need a `no_std` fork of
  `arrow` and `parquet`, maintained by Moruna. That is a large, lasting cost.
- What it would buy is the user-to-kernel crossings left after this design. With rings and a
  pre-faulted arena, steady-state work makes almost no calls (§3).
- The engine on top is a `std` program on Asterinas's Linux interface. Its calls are answered in
  the kernel, so the step-4 `std` port for Moruna's microkernel is no longer needed.

**The engine in the kernel stays an option.** It becomes a firm option for a given hot path if
measurement on the fused kernel shows that path's crossings still cost.

## 3. The user-function boundary: no copy, no call per batch

- **Input.**
  - A task receives a batch as read-only mappings of the batch's arena pages, laid out as
    today's page-aligned Arrow record (contracts e.7).
  - A shared ring announces each batch. The task reads the ring and the batch in place, so steady
    state makes no call.
  - The task makes one futex wait only when the ring is empty.
- **Output.**
  - The task writes into a window the kernel mapped for it, and posts it on the completion ring.
  - The kernel unmaps the window from the task, and the engine checks the record, which no task
    can still write (DP1). The record then becomes an arena batch as it stands, with no copy.
- **Fan-out** maps the same pages read-only into each consumer, and frees them after the last one
  (DP-I6).
- **Python.**
  - A shim in the image's layer turns a mapped batch into a `pyarrow.RecordBatch` in place
    (`pyarrow.foreign_buffer` over the mapping) and calls the function.
  - One interpreter per process, N processes per stage. That removes free-threaded GC stops and
    per-process heap contention, and keeps the 0.6.6 kernels unchanged (§6).
- **An unmodified image** that knows nothing of the rings still works through descriptors 0 and 1
  as an Arrow IPC stream. That path copies, and the report says so.
- **The user process is a real boundary.** It has its own address space and sees only the batches
  it is given. A batch is read-only once it is a batch (DP-I4).

## 4. The base: (A) against (B)

| | (A) Asterinas's kernel, Moruna fused in | (B) Moruna's microkernel, Asterinas's Linux layer in user space (today's §7a) |
| --- | --- | --- |
| **The Linux layer** | Asterinas's 137,000 lines, as they run in Asterinas today, in kernel mode | The same lines, carried into user space: about 112,000 mechanical, about 25,000 rewritten |
| **New kernel mechanisms** | None for Linux programs. Moruna's data-path components are added (§2). | Four: a personality entry, copy-on-write clones, a user pager, thread interruption (`kernel-is-the-machine.md` §4.1) |
| **A Linux call** | A trap into the kernel, answered there | A switch into the personality's upper region in the same process: cheaper than today's message, dearer than a trap |
| **The engine** | A `std` program on the Linux interface, with no port | A native process, needing the `std` port to Moruna's microkernel (step 4) |
| **Moruna's microkernel** (`kernel/core`, the `svc` SDK, drivers as processes, `kernel/linux`) | Retired. The ideas that carry (accounts, slots, the plane, verified layers) move into (A) as components. | Kept and extended |
| **Trusted code in kernel mode** | OSTD, Asterinas's Linux layer and Moruna's components | OSTD and Moruna's core, about 7,000 lines |
| **Fit with "literally fused"** | Literal: one kernel | Two systems side by side: Moruna's core, and Asterinas's layer as a process |

**Recommendation: (A).**

1. It is the fusion Brackly asked for.
2. It needs no new kernel mechanism to run Linux programs, and no `std` port for the engine. Both
   were the largest pieces of (B).
3. A Linux call is a trap, the cheapest crossing there is, and Python, NumPy and pyarrow make many
   of them.
4. Moruna already runs on Asterinas's OSTD 0.19.0 (MK1), so the components move onto the
   foundation they were written for.

**What (A) reverses, by name:**

- **DECISIONS MK1**, "Asterinas's Linux kernel is not used". OSTD stays the hardware layer, and
  the MPL-2.0 acceptance extends from OSTD to the Asterinas kernel files carried in. They are
  file-level copyleft, kept as MPL with their source available.
- **BOARD's "everything else is a process"** for this kernel. Drivers and filesystems become kernel
  components, as in Asterinas's framekernel.
- **`kernel-is-the-machine.md` §4.8**, which rejected "Asterinas's whole kernel as the machine's
  kernel" because "a logic bug in any of its 137,000 lines then reaches every key in the machine".

**The security argument.** That rejection was written for the kernel as a cloud machine, with a
network and keys. Here:

- **One job per VM, and the VM is the wall between jobs.** KVM, `moruna-vmm`'s jail and QEMU on
  the Linux node hold it, as they do today.
- **Nothing in the VM is another tenant's.** The engine and the user's code serve the same job,
  on that job's data.
- **No network inside the VM,** and no credentials. A port call leaves only through the engine, and
  the host's embedder applies its own policy to it (optimized-runtime 4), so a task that
  subverted the engine still reaches only what the host allows that job.
- **Memory safety is still bounded by OSTD.** Asterinas's kernel and Moruna's components are safe
  Rust above OSTD, which holds all the unsafe code (the framekernel).
- **User code is still a user process.** A task is kept from the engine by address spaces, and
  sees only its batches (§3).

What (A) gives up: the small kernel-mode code size of (B). A logic bug in the Linux layer reaches
the engine and the job's data, but not another job's.

**Risks of (A), and the first evidence for each:**

- **Can Asterinas run the gate's images?** Whether CPython 3.14 with NumPy and pyarrow runs on
  Asterinas, and how fast, is not yet shown. A stock Asterinas boot under KVM with the gate's
  image, before any fusion, is the cheapest first proof.
- **Asterinas's memory performance:** huge pages for user mappings, fault cost and `mmap`
  behaviour. Measure them in the same boot.
- **Upstream.** The kernel is pinned to one Asterinas commit, the one whose OSTD Moruna uses, and
  later changes are carried by hand.

## 5. What stays Moruna

Wiring keeps Moruna (BOARD, `architecture/arrow-core-0.7.md`).

- **The arena, the scheduler, the controller, placement, the reactor, resume, the report and the
  events keep their decisions.**
- **What changes is underneath their ports:**
  - the arena's memory becomes kernel memory objects;
  - the reactor's I/O goes on the block ring;
  - the controller's `Sampler` reads the accounts;
  - the transport is the vsock.
- **Changes to decisions are separate.** "Decode on the worker that consumes the batch" changes a
  scheduler decision, so it gets its own reason and its own measurement and is never folded into
  a wiring step. KD3's conformance (same rows, refusals and decisions on a Linux host and on the
  fused kernel) proves the rest.

## 6. The gate, and what this does to the 0.7 steps

**The gate is unchanged** (`arrow-core-0.7.md` §9, the PM's terms):

- 0.6.6's jobs, with their kernels unchanged, run as image tasks in the VM against 0.6.6;
- identical rows by count and content hash;
- VM boot reported separately and included in the wall time;
- both machine types, alternating order, same-session noise;
- one interpreter per process for Audit and Overture attributes.

"Much better" becomes per-job targets after the first fused-kernel measurement.

**The steps under (A):**

| Step | What | Replaces |
| --- | --- | --- |
| 1 | Today's measurements on Moruna's microkernel (running now), plus a stock Asterinas boot under KVM with the gate's Python image: does it run, and how fast | Step 1, extended |
| 2 | The fused kernel boots under `moruna-vmm`: Asterinas's kernel with virtio-blk, vsock, Moruna's verified layers and accounts as components, the engine as its first process, and the host protocol unchanged | The kernel half of steps 1 and 3; §7a; step 4's `std` port |
| 3 | The arena on kernel memory objects with huge pages, and block I/O into arena pages on a ring | Steps 2 and 3 (memory, data devices) |
| 4 | The data plane as a kernel component with shared rings; the Python shim; a process per interpreter | Step 2 (no boundary copies) |
| 5 | Placement: workers pinned and woken on cores. Decoding on the consuming worker is a separate, measured scheduler change. | New |
| 6 | Dependencies: wasm out, the HTTP stack out, C as Brackly decides | Step 5 |
| 7 | The host side: `moruna run`, logs on the job updates | Step 6 |
| 8 | KD3 on the fused kernel, then the gate on the benchmark VM | Step 7 |
| 9 | The Python client | Step 8 |

**Size, roughly:**

- **(A) adds** Moruna's components to Asterinas (the plane, accounts, verified layers, the arena's
  memory objects, the block ring), and the engine's adapters to them.
- **(A) retires** Moruna's microkernel, its Linux personality, its driver processes and its `svc`
  SDK. The network, storage and node work is already parked with Goal B.
- **(A) removes from the plan** the four core mechanisms, the 25,000-line rewrite and the `std`
  port.
- **The new work is mostly porting what Moruna has written onto a kernel that already runs Linux
  programs.**
- **The largest unknown is step 1's second half:** how Asterinas runs the gate's Python images.

## 7. Decisions for Brackly

1. **The base: (A), Asterinas's kernel with Moruna fused in** (recommended), or (B), Moruna's
   microkernel with Asterinas's Linux layer in user space.
2. **The kernel line: the Arrow line** (recommended). Pages, I/O, the plane, accounts, placement
   and the Linux layer go in the kernel. The engine's Arrow and Parquet code stays on top unless a
   measured hot path says otherwise. The alternative is the engine itself in the kernel, which
   needs `no_std` forks of `arrow` and `parquet`.
3. **Reversing MK1** (Asterinas's Linux kernel used, its files under MPL-2.0) and "everything else
   is a process" for this kernel.
4. **Moruna's microkernel** (`kernel/core`, `kernel/linux`, the `svc` SDK, driver processes)
   retired under (A), its ideas carried in as components.
5. **Still open from before:** "no C" against "no regression" for the codecs (`arrow-core-0.7.md`,
   decision 8). Under (A) it also covers the C library under the engine's `std`: musl, or a
   pure-Rust libc.
