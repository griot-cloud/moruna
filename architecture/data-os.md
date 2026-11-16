# The data OS: Asterinas and Moruna under the microkernel rule

Status: design for Brackly's review, rewritten 2026-11-16 on his correction. Nothing in it is
built. Until he approves it, the only work is step 1's measurements
(`architecture/arrow-core-0.7.md`).

> We should first identify performance critical processes and let them run in the kernel. And
> then anything else runs on top. [...] The goal is to be able to have a data operating system
> that is literally [Asterinas] and [Moruna] built together and fused.

> Be very careful because there is no difference. There is a version of this that doesn't give a
> difference between using Linux and using the whole of Asterinas, right? Because the whole goal
> of this was to be able to reduce the attack surface ... And the rule is to have a micro kernel,
> not a kernel.

## In one page

- **The rule: a microkernel.**
  - **Kernel mode holds OSTD** (Asterinas's hardware layer, all the unsafe code) **and Moruna's
    core.** Nothing else runs in kernel mode.
  - **The design criterion is the attack surface:** the lines and the calls reachable in kernel
    mode (§0).
- **"Performance-critical in the kernel" means mechanisms, not logic.** The kernel provides:
  - memory objects and huge pages;
  - mapping one memory object into several processes;
  - device DMA into a process's own pages;
  - a thread pinned to a CPU;
  - futex notification;
  - a clock read without a call.

  These are set up once, so that **the engine's hot path never enters the kernel** (§3).
- **Everything else runs on top, as user processes:**
  - **Moruna's engine,** with its memory and its data disk granted to it at start. Its decisions
    are unchanged.
  - **Asterinas's Linux layer,** as the personality, in user space (`kernel-is-the-machine.md`
    §4.2).
  - **User code** in any language, a Python interpreter per process.
  - **The drivers and servers** Moruna already runs as processes.
- **"Fused"** means Asterinas's parts (OSTD below, its Linux layer above) and Moruna's core and
  engine built into one data OS. It does not mean one big kernel.
- **The earlier draft is out.** Its option (A), Asterinas's whole kernel with its Linux layer in
  kernel mode, has the attack surface Brackly is avoiding: the same class as Linux.

## 0. The criterion: what is reachable in kernel mode

| System | Kernel-mode code | Calls a program can make into kernel mode |
| --- | --- | --- |
| Linux | Tens of millions of lines, with every driver | Over 350 system calls, plus every driver's `ioctl` |
| Asterinas, whole | OSTD (about 49,000 lines) plus its Linux layer (about 137,000) plus its components (about 42,000) | 245 Linux calls |
| The data OS | OSTD (about 49,000) plus Moruna's core (6,952 today) plus the mechanisms of §2 (to be measured) | Moruna's 55 native calls, plus §2's few |

**Rules this design keeps:**

- **A Linux call never runs Linux code in kernel mode.** It enters the personality, which is user
  code in the caller's own process (§4).
- **Every mechanism added to kernel mode is named in §2, with its reason and its size.** Each one
  is counted against the table above.

## 1. What is performance-critical: the evidence

Every figure below comes from the 0.6.6 runs on the benchmark VM (Linux, 8 CPUs, 24 GiB,
2026-11-13 and 2026-11-14) and from `CHANGELOG.md`'s 0.6.6 section.

| Cost | Measured | Kind |
| --- | --- | --- |
| Page faults and zeroing from memory returned to the OS | Occupancy spent 21 of its 53 CPU-seconds in the kernel, against 8 for the plain loop; Enrich 14 of 45, against 2. Keeping freed heap pages took Occupancy from 7.4 s to 5.4 s. | Crossing: fault |
| The landing copy of arena columns on a function's output | Fixed in `19aaefc6`. | Crossing: copy |
| Two copies per batch at an image stage's boundary | `offer` and `land` in `crates/moruna-micro/src/task.rs`. | Crossing: copy |
| Linux calls forwarded to the personality as messages | Every Linux call of the engine and of every task (`kernel/core/src/process.rs:976-979`). Step 1 measures it under KVM. | Crossing: call |
| A Python thread state made and freed per batch | Fixed in `30c359ca`. | Crossing: user boundary |
| The allocation guard on every Python object | 1.9 of Audit's 106 CPU-seconds. | Crossing: user boundary |
| Decode on one core, the function on another | Audit's last 1% to 3% against the loop, which decodes and computes on one core. | Placement |
| Free-threaded GC stopping every thread | On a Mac, GC was 34% of one thread's time and doubled it at eight threads. Ray's per-process GC avoids it. | User runtime |
| One process's C heap shared by eight workers | Overture shapes (shapely, GEOS) ran 38.3 s on 8 files against the loop's 31.4, contending on four C heaps; with glibc's default number of heaps it took 26.4 s. | User runtime |
| Parquet decode and encode | Enrich spent about 40 of 46 CPU-seconds reading and writing outside the function, against about 27 for pyarrow. Rust's `snap` decodes about 1.5x slower than C++. | Compute |
| Parallel encode and write lanes | Overture shapes: Moruna 723 s, the loop 870 s, Ray Data 941 s. | Compute and I/O |

Step 1's KVM figures (forwarding, boundary copies, VM boot) are added here when they exist.

**What the evidence says:**

- **Crossings** are the largest measured costs left: faults, copies, calls and the user boundary.
  A crossing is avoided by setting up mapped memory, granted devices and shared rings once, not
  by moving code into the kernel.
- **Placement** is the engine's decision. The kernel only has to let it pin a worker to a core.
- **Compute** runs at the same speed in user mode as in kernel mode. Better codecs, better encoding
  and better placement make it faster.
- **User runtime** costs come from how user code is hosted. A process per interpreter removes GC
  stops and heap contention.

**So nothing in the evidence needs logic in kernel mode.** It needs mechanisms that let user
processes run their hot paths without calling the kernel.

## 2. Kernel mode: the mechanisms, and what each costs

| Mechanism | Today | What it removes | Kernel-mode cost |
| --- | --- | --- | --- |
| Memory objects, charged to an account | Exists (`MEMORY_CREATE`, accounts, ABI 2.3 to 2.5) | Memory from the OS on every batch | None new |
| Map a memory object into several processes, read-only to some | Exists (`MEMORY_MAP`, `PROCESS_MAP`, slots) | Copies between processes | None new |
| Map pre-faulted, with huge pages | New. The kernel maps 4 KiB pages today (SDD 29 m.1). | Faults, zeroing and TLB misses on the arena | A mapping flag, and large-page support in OSTD's page tables. To be measured. |
| Device DMA into a process's own pages | Exists for driver processes (`MEMORY_PIN`, `DEVICE_MEMORY_CREATE`, `INTERRUPT_CREATE_MSI`, `DMA_RELEASE`) | A copy from a driver's buffer into the arena | None new. The engine is granted the data disk as a driver process is (§3). |
| A thread pinned to a CPU | New | Losing cache locality between decode and the function | One call. Small. |
| Futex wait and wake | Exists (`FUTEX_WAIT`, `FUTEX_WAKE`) | A call per batch: waits happen only when a ring is empty | None new |
| A clock readable without a call | New: a read-only page the kernel updates | `clock_gettime` calls on every timed batch | One page and its update. Small. |
| Revoke a task's write access to a set of windows | Partly exists (`PROCESS_PROTECT`, `batch_put`'s unmap) | The record check's need for bytes no task can write (DP1), paid once per group of batches, not per batch (§5) | A call that takes a list. Small. |
| The personality entry, copy-on-write clones, a user pager, thread state and interruption | New (`kernel-is-the-machine.md` §4.1) | Running Linux programs with no Linux code in kernel mode (§4) | The largest addition: four mechanisms, each tested in the core's own suite. Size to be measured against the table in §0. |

**Nothing of the engine's logic goes into the kernel.** That covers decode, the scheduler, the
controller, placement decisions and the sink lanes.

**The rule for any future addition:** if a hot path ever seems to need more than a mechanism in
kernel mode, the design names it, measures why, and states its attack-surface cost before it goes
in.

## 3. The engine's hot path never enters the kernel

The engine is one user process. It is granted, at start, everything its hot path touches.

| Hot-path operation | How it avoids the kernel |
| --- | --- |
| Allocating batch memory | The arena is memory objects granted and mapped once, pre-faulted, with huge pages. The engine's allocator hands out arena pages, so a batch never faults and is never zeroed. |
| Reading the data disk | The engine is granted the data disk's virtio-blk queues and drives them itself, as Moruna's driver processes do today. Requests go on the device's ring, and data lands by DMA in arena pages. Completion is polled while busy, with an interrupt woken through a futex only when idle. |
| Writing outputs and spill | The same device ring, from arena pages. |
| Handing a batch to user code | Pre-shared regions and rings (§5). |
| Placing work | Workers are pinned once. The engine's scheduler hands batches to pinned workers through its own queues, as it does today, with no call. |
| Timing a batch | The clock page. |
| Logs and events | A task's text goes to the log ring (SDD 29 f.7). Events go out on the vsock, which is not on the hot path. |

What still enters the kernel is set-up, teardown and idle waits. Step 1 measures today's numbers,
and the gate measures the result.

**The engine's own standard library.** Because the hot path makes no calls, the engine's `std`
can come from either of two places at no hot-path cost:

- a native `std` port to Moruna's core (step 4 of `arrow-core-0.7.md`);
- running as a Linux program on the user-space personality (§4).

The second removes the `std` port. It is Brackly's call (§8).

## 4. User space: Asterinas's Linux layer as the personality

This is `kernel-is-the-machine.md` §4.1 and §4.2, cut to a VM with no network.

- **Asterinas's Linux layer** (about 137,000 lines) runs in user space.
  - It is one instance per image task.
  - Its code and state sit in an upper region of the task's own process. The program cannot reach
    that region, and a call or a fault switches the same thread into it.
- **About 82% carries over with mechanical swaps** (OSTD's locks and wait queues become ones built
  on the core's futex). About 25,000 lines are rewritten against the four mechanisms of §2.
- **0.7.0 needs** processes, files, pipes, signals, epoll and the synthetic filesystems. Sockets
  and the cloud platform stay out: the VM has no network and runs on a Linux node.
- **Today's personality** (`kernel/linux`, 7,386 lines, a message per call) is retired when this
  runs the gate's images.
- **A Linux bug stays in user space.** It reaches the task it serves, never kernel mode and never
  another process.

## 5. The user-function boundary: no copy, no call per batch

- **Input.**
  - Each task has an input region, arena memory objects mapped read-only into it once.
  - The engine decodes a batch straight into the input region of the task that will consume it.
    The scheduler already knows which task that is; this places the allocation, not the decision.
  - A shared ring announces the batch. The task reads the ring and the batch in place, with no
    call, and waits on a futex only when the ring is empty.
- **Output.**
  - Each task has output windows mapped writable into it.
  - It writes a record as today's page-aligned Arrow record (contracts e.7) and posts it on its
    completion ring, with no call.
- **The record check stays safe (DP1).** The engine checks a record only on bytes the task can no
  longer write. It takes write access back from a group of posted windows in one call (§2), then
  checks them, so the cost is one call per group, not per batch. This is the one kernel crossing
  left at the boundary, and it is kept because the engine's safety depends on it.
- **A released region goes back** on the ring and is used again, and the task only ever sees its
  own batches.
- **Fan-out** maps the same pages read-only into each consumer, and frees them after the last one
  (DP-I6).
- **Python.**
  - A shim in the image's layer reads the ring and turns a batch into a `pyarrow.RecordBatch` in
    place (`pyarrow.foreign_buffer`).
  - One interpreter per process, N processes per stage. That removes GC stops and heap contention,
    and keeps 0.6.6's kernels unchanged.
- **An unmodified image** still works through descriptors 0 and 1 as an Arrow IPC stream. That path
  copies, and the report says so.

## 5a. Risks, and the first evidence for each

1. **The cost of carrying the Linux layer to user space.** This is the largest risk.
   - It needs the four core mechanisms and about 25,000 rewritten lines.
   - A Linux call becomes a crossing (a switch into the personality's region in the same process)
     instead of a trap answered in kernel mode.
   - The switch is cheaper than today's message per call to the personality process, and dearer
     than a trap.
   - **First evidence:** step 1 measures today's forwarding on the gate's Python image under KVM:
     calls per second, time per call, and its share of wall time. That is the cost the new entry
     has to beat, and the call rate says how much a dearer-than-trap crossing can cost at most.
2. **Huge pages for a process in OSTD's page tables.** These are not shown yet (SDD 29 m.1). The
   first evidence is a mapping test in the core's suite, and the fault and TLB counts on the
   arena.
3. **The engine driving the data disk itself.** Moruna's driver processes do this today, but the
   engine has not. The first evidence is a virtio-blk read into arena pages under KVM, timed
   against today's path.
4. **The grouped revoke.** Taking write access back costs a TLB flush on every CPU the task runs
   on. The first evidence is its cost per group, measured against the batches a group carries.

## Rejected: Asterinas's whole kernel

Asterinas's kernel, with its Linux layer in kernel mode and Moruna fused into it, was the first
draft's recommendation (`992a5662`). Brackly rejected it: "there is no difference ... between
using Linux and using the whole of Asterinas ... the whole goal of this was to be able to reduce
the attack surface ... the rule is to have a micro kernel, not a kernel." It would put about
228,000 lines and 245 Linux calls in kernel mode (§0).

## 6. What stays Moruna

Wiring keeps Moruna (`arrow-core-0.7.md`).

- **The arena, the scheduler, the controller, placement, the reactor, resume, the report and the
  events keep their decisions.**
- **What changes is underneath their ports:**
  - the arena's memory becomes granted memory objects;
  - the reactor drives the data disk's ring;
  - the controller's `Sampler` reads the accounts;
  - the transport is the vsock.
- **Changes to decisions are separate.** "Decode on the worker that consumes the batch" changes a
  scheduler decision, so it gets its own reason and measurement. KD3's conformance (same rows,
  refusals and decisions on a Linux host and on the data OS) proves the rest.

## 7. The gate, and what this does to the 0.7 steps

**The gate is unchanged** (`arrow-core-0.7.md` §9, the PM's terms):

- 0.6.6's jobs, with their kernels unchanged, run as image tasks against 0.6.6;
- identical rows by count and content hash;
- VM boot reported separately and included in the wall time;
- both machine types, alternating order;
- one interpreter per process for Audit and Overture attributes.

"Much better" becomes per-job targets after step 1.

**The steps:**

| Step | What | Against today's plan |
| --- | --- | --- |
| 1 | Today's measurements on Moruna's microkernel under KVM: forwarding, copies, boot, how the gate's Python image runs on today's personality, and `ruzstd` against C zstd | Unchanged |
| 2 | The arena on granted memory objects, pre-faulted, with huge pages; CPU pinning; the clock page | Step 2, plus three mechanisms |
| 3 | The engine drives the data disk's queues itself, with DMA into arena pages | Step 3 (data devices) |
| 4 | Input regions, output windows, rings, the grouped revoke; the Python shim; a process per interpreter | Step 2's zero copies, with no call per batch |
| 5 | The four mechanisms, then Asterinas's Linux layer in user space; `kernel/linux` retired | §7a, unchanged |
| 6 | The engine's `std`: the native port, or a Linux program on the new personality | Step 4, now Brackly's choice (§3) |
| 7 | Dependencies: wasm out, the HTTP stack out, C as Brackly decides | Step 5 |
| 8 | The host side: `moruna run`, logs on the job updates | Step 6 |
| 9 | KD3 on the data OS, then the gate on the benchmark VM | Step 7 |
| 10 | The Python client | Step 8 |

**Attack-surface check at each step.** Kernel-mode lines and the call list are counted at each
step against §0's table, and they grow only by the mechanisms named in §2.

## 8. Decisions for Brackly

1. **The kernel-mode set.** OSTD and Moruna's core, plus the mechanisms of §2 and nothing else.
2. **The engine drives the data disk itself,** with DMA into its arena (recommended), rather than
   through a separate driver process, which would need a call or a message per I/O.
3. **The record check's one remaining crossing:** write access taken back in one call per group of
   batches, kept for DP1's safety (§5).
4. **The engine's `std`:** a native port to Moruna's core, or a Linux program on the user-space
   personality. The hot path makes no calls either way (§3).
5. **Still open from before:** "no C" against "no regression" for the codecs (`arrow-core-0.7.md`,
   decision 8).
