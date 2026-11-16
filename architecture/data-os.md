# The data OS: Asterinas and Moruna under the microkernel rule

Status: approved by Brackly on 2026-11-16, "as recommended", through the PM session. His
direction for building it: "What I would make is one step. Please look at that engine,
parallelize this work so that we can finish it." The parallel streams, their ownership and
their frozen interfaces are in `architecture/data-os-streams.md`. Nothing in this design is
built yet.

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
  - a worker thread pinned to a CPU core (a place to run, never a function to run);
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

**Step 1, on `moruna-bench` (D8ds_v6, KVM), 2026-11-16:**

- **The microkernel builds and boots under KVM.** CI's boot passed all 162 of its checks on 1, 2
  and 4 CPUs.
- **The engine runs real jobs on it as today's Linux program (EN1).** All 15 production-node job
  cases passed (five cases on 1, 2 and 4 CPUs).
- **Each fixture case takes 2.3 s to 3.1 s end to end under KVM.** That covers the host
  driver's setup, the VM's boot, the job and the shutdown. The medians are 2.58 s, 2.74 s and
  2.79 s on 1, 2 and 4 CPUs, against 5.91 s, 5.65 s and 5.66 s under QEMU's emulator (TCG). All
  30 runs passed, and this is the upper bound on boot plus a small job.

**Codecs (`arrow-core-0.7.md` §5; decision 5 here),** single-threaded, best of three, on the
gate's own pages. `ruzstd` 0.8.3 implements only its Fastest level; its Default and Better
levels panic "not implemented". C zstd and `ruzstd` decoded every page to the same bytes.

| Dataset | Decode: C zstd / `ruzstd` | Encode, level 1: C zstd / `ruzstd` Fastest | `ruzstd`'s bytes |
| --- | --- | --- | --- |
| Overture (2 files, zstd pages, 1.9 GB raw) | 939 / 201 MB/s (4.7x the time) | 380 / 56 MB/s (6.7x) | 1.17x |
| NYC 2024, as the benchmark reads it (3 months, 249 MB raw) | 683 / 161 MB/s (4.3x) | 302 / 52 MB/s (5.8x) | 1.11x |
| FineWeb (1 file, 3.4 GB raw) | 1,082 / 189 MB/s (5.7x) | 289 / 50 MB/s (5.8x) | 1.16x |

C zstd stays (decision 5): a pure-Rust zstd today would slow every zstd job's decode four- to
six-fold and every write about six-fold.

The forwarding and boundary-copy figures on the gate's own jobs need stream E's harness and are
added when it runs.

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

## 1a. The contracts: what the engine commits to, and what the OS drops for it

> my vision is that we could get significant speedups by commiting to certain analytical
> contracts and adapting os components to it. (Brackly, 2026-11-16)

> if we discover these, we should feel free to move it to moruna or retrofit it or any version
> of a solution that works. (Brackly, 2026-11-16)

**Each OS component of the data OS is built to a contract the engine keeps,** not for every
program. The contracts govern the engine's data path only. User code keeps full POSIX behaviour
through Asterinas's layer (§4). No contract may break the no-regression gate.

**A speedup lands wherever it measures best.** That may be the engine on today's Linux host, the
data OS, or both. If an OS component is better replaced by an engine-side solution, or the
reverse, the measurement chooses. The usual rules still hold:

- the gate decides;
- tests ship with the code;
- a change to a core decision gets its own reason and measurement.

**Each contract below gives:**

- what the engine commits to;
- how a violation is refused;
- what OS work that lets us drop;
- the expected gain, and how it is measured;
- whether Moruna already enforces it ("kept") or it is a new commitment for Brackly's review
  ("new");
- the streams that build to it.

### C1. Data is immutable Arrow records in pages, written once and read many times

- **Commitment.** A batch, once made, is never written again. Between processes it is one
  page-aligned Arrow record (contracts e.7), checked before use.
- **Enforced by:**
  - DP-I4 (a batch is mapped read-only into every consumer, and a write by a task faults);
  - the record check (DP1), which refuses a record that fails;
  - Arrow's own immutability inside the engine.
- **The OS drops:** copy-on-write and coherence work for data pages. A batch is shared by
  mapping its frames, freed once after its last holder (DP-I6), and the grouped revoke stays
  cheap.
- **Gain:** today's two copies per image-stage boundary go to zero. Measured by the plane's
  `COPIED` counter and the engine's boundary-copy counter on the gate's jobs.
- **Status:** kept. DP-I4, DP1 and contracts e.7 are already the law. Using them to share frames
  is new mechanism, not a new commitment.
- **Streams:** P (rings, regions, the grouped revoke), A (`windows_protect`).

### C2. Every input read is a split planned before the first read

This corrects "scanned in order, once". The scheduler reads splits in the order it decides, and
resume and retries read a split again.

- **Commitment.** A source plans every split (a row group's byte ranges, from the footer) before
  its first read (SO-I1). The engine reads only planned ranges, and the scheduler knows which
  splits come next.
- **Enforced by:** SO-I1. A read of a split not in the plan is refused (`Source` error,
  `crates/moruna-sources/tests/sources.rs`, SO-T1).
- **The OS drops:**
  - the page cache, its LRU and generic readahead heuristics, for the engine's inputs;
  - the copy from the page cache into the arena that a Linux read makes today.

  Readahead becomes the scheduler's: its `read_ahead` knob (`crates/moruna-scheduler/src/knobs.rs`)
  names the next splits, and their ranges are read by large DMA requests straight into arena
  pages.
- **Gain:** one copy of every input byte, and the page cache's memory beside the arena. Measured
  by input bytes copied (zero), CPU per input byte, and the read path's share of Enrich's
  40-of-46 CPU-seconds outside the function.
- **Status:** kept (SO-I1). The new part is the mechanism: readahead from the plan, DMA into the
  arena.
- **Streams:** C (the engine's virtio-blk, scheduler-driven readahead).
- **On a Linux host as well:** the same plan-driven readahead into the arena with `O_DIRECT`.
  It may land there first if it measures better.

### C3. Outputs are append-only and written once; the checkpoint ledger is the durability contract

- **Commitment.** A sink writes a file once and never rewrites it. A file is durable when the
  ledger records it as committed. Resume trusts the ledger and nothing else (SDD 08 f.5, f.7).
- **Enforced by:**
  - the sinks' commit: write under `.tmp`, then `fsync` and `rename` once per file
    (`crates/moruna-sinks/src/commit.rs:135`);
  - the ledger's refusal of a resume whose state disagrees (`Ledger::restore`).
- **The OS drops:**
  - a writable filesystem's metadata journaling;
  - per-write syncs.

  The output disk holds one layer file written at the run's end (`devices.rs`), with one flush.
- **Gain:** small per file. Measured by sink wall time and syscalls per committed file.
- **Status:** kept for files and the ledger. New: the output is a layer on a raw disk, not a
  filesystem (decision 5 of `arrow-core-0.7.md`).
- **Streams:** C (writing the output layer), D (reading it on the host).

### C4. Memory is one fixed budget per run, carved at start, and the engine never reads a byte it has not written

This corrects "sized by the probe". The probe sizes batches; the arena is carved from the run's
budget.

- **Commitment.**
  - The run's memory budget is fixed when the run starts. Production is one fixed node.
  - The engine's memory is the arena, carved from that budget.
  - Inside the engine, no code reads arena bytes it did not write. Safe Rust guarantees it, and
    Arrow's buffers keep it with their initialisation invariants.
- **Enforced by:** the budget's refusal (a run that would pass its ceiling is stopped with a
  diagnostic, F8.9, `crates/moruna-runtime/tests/whole_process.rs`), and Rust's safety for the
  initialisation rule.
- **The OS drops:**
  - demand paging and overcommit for the engine (the arena is populated at start, with huge
    pages);
  - zeroing pages again when the arena reuses them, because only the engine reads them.

  Pages are zeroed once, when the kernel first gives them. Pages handed to user code are still
  zeroed before a task first sees them, because that is a trust boundary.
- **Gain:** the faults and zeroing that cost Occupancy 21 of 53 CPU-seconds and Enrich 14 of 45.
  Measured by page faults per batch (zero in steady state), kernel CPU time, and TLB misses on
  the arena.
- **Status:** kept for the fixed budget and the arena. New: "no zeroing on reuse" as a stated
  commitment.
- **Streams:** A (`map::HUGE`, `map::POPULATE`), C (the arena and allocator on them).
- **On a Linux host as well:** the arena already stays mapped (0.6.6, "charged as used"). Skipping
  the zeroing of recycled arena memory, and huge pages via `madvise`, can land in the engine
  there too.

### C5. Work is batches of a known plan, run to completion on pinned cores

- **Commitment.**
  - A worker runs one batch of one function to completion and blocks only between batches. I/O
    is the reactor's, never a worker's.
  - Workers are pinned to cores. Any worker still runs any function's batch, as the scheduler
    decides today; pinning is a place, never a function.
- **Enforced by:** the scheduler's design (workers run kernels, the reactor does I/O). A
  function's own blocking is the function's, inside a task.
- **The OS drops,** on worker cores:
  - timer ticks while a worker runs;
  - device interrupts, which go to the engine's I/O core (polled while busy);
  - migration.
- **Gain:** cache locality and no preemption jitter. Part of Audit's last 1% to 3%. Measured by
  per-batch CPU time and its variance, migrations (zero), and interrupts on worker cores.
- **Status:** kept for run-to-completion and any-worker-any-function. New: tickless worker cores
  and interrupt steering, as mechanisms. Their kernel-mode cost is counted under §0.
- **Streams:** A (pinning, and tick and interrupt steering if measured worth it), C (pinning the
  pool).

### C6. User functions are Arrow in, Arrow out, and stateless unless declared

- **Commitment.**
  - A function takes and returns Arrow (contracts e.7 between processes).
  - It keeps no state between batches unless it declares state (`stateful`, `ResumePolicy`).
  - A stateless function can be retried, split in halves (RB5) or restarted on another process.
- **Enforced by:**
  - the declaration's refusal of a stateful kernel without `setup` and `__call__`, or of
    `resume='checkpoint'` without `checkpoint` and `restore`;
  - the record check on what a function returns.
- **The OS drops:** a general process model on the engine's side. The engine never forks; a task
  that fails is a process restarted from its image, not a fork of the engine.
- **Gain:** simplicity and a cheap restart. Measured by restart time against today's task start.
- **Status:** kept.
- **Streams:** P (restart on the plane), B2 (the task's process model, inside user code).

### C7. One job per VM, one tenant

- **Commitment.** A VM runs one job for one tenant and is deleted after it.
- **Enforced by:** the launcher: one job per VM, nothing kept between runs.
- **The OS drops:**
  - multi-user checks between the engine's own parts on the hot path (the engine drives its
    own disk, decision 2);
  - users, groups and permissions in the engine's world.

  The user-code boundary stays (§5).
- **Gain:** no message or check per I/O between the engine and a driver process. Measured by
  calls or messages per I/O (zero).
- **Status:** new as a stated commitment of the data OS. Today's host runs more than one job per
  process.
- **Streams:** D (the launcher), C (the engine's own driver).

### C8. Time is monotonic and read from the clock page

- **Commitment.** The engine's timing (the trace, the controller, the cost model) uses monotonic
  time only, read from the clock page with no call.
- **Enforced by:** the engine's clock port. Wall-clock time is read once, at start, for the
  report.
- **The OS drops:** a call per time read.
- **Gain:** small per read, but the engine reads time for every batch and every trace record.
  Measured by calls per batch.
- **Status:** new.
- **Streams:** A (the clock page), C (the engine's clock on it).

### Not made contracts

- **"Inputs are read once, in order."** Resume, retries and adaptive order read splits again and
  out of order, so C2 commits only to the plan.
- **"The arena is sized by the probe."** The probe sizes batches, not the arena (C4).
- **Anything about user programs.** They keep POSIX through the personality, and the contracts
  stop at the user-code boundary.

## 2. Kernel mode: the mechanisms, and what each costs

| Mechanism | Today | What it removes | Kernel-mode cost |
| --- | --- | --- | --- |
| Memory objects, charged to an account | Exists (`MEMORY_CREATE`, accounts, ABI 2.3 to 2.5) | Memory from the OS on every batch | None new |
| Map a memory object into several processes, read-only to some | Exists (`MEMORY_MAP`, `PROCESS_MAP`, slots) | Copies between processes | None new |
| Map pre-faulted, with huge pages | New. The kernel maps 4 KiB pages today (SDD 29 m.1). | Faults, zeroing and TLB misses on the arena | A mapping flag, and large-page support in OSTD's page tables. To be measured. |
| Device DMA into a process's own pages | Exists for driver processes (`MEMORY_PIN`, `DEVICE_MEMORY_CREATE`, `INTERRUPT_CREATE_MSI`, `DMA_RELEASE`) | A copy from a driver's buffer into the arena | None new. The engine is granted the data disk as a driver process is (§3). |
| A thread pinned to a CPU core | New | Losing cache locality between decode and the function | One call. Small. It places a thread on a core and nothing else: it never binds a worker to a function or a stage. |
| Futex wait and wake | Exists (`FUTEX_WAIT`, `FUTEX_WAKE`) | A call per batch: waits happen only when a ring is empty | None new |
| No timer ticks or device interrupts on worker cores (C5) | New | Preemption jitter and cache disturbance on a pinned worker | Changes to OSTD's timer and interrupt routing. Built only if C5's measurement shows a gain, and counted under §0. |
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
| Placing work | Each worker thread is pinned once, to a core. The engine's scheduler hands any worker a batch of any function through its own queues, as it does today, with no call. A worker is never bound to a function or a stage. |
| Timing a batch | The clock page. |
| Logs and events | A task's text goes to the log ring (SDD 29 f.7). Events go out on the vsock, which is not on the hot path. |

What still enters the kernel is set-up, teardown and idle waits. Step 1 measures today's numbers,
and the gate measures the result.

**The engine's own standard library.** Because the hot path makes no calls, the engine's `std`
can come from either of two places at no hot-path cost:

- a native `std` port to Moruna's core (step 4 of `arrow-core-0.7.md`);
- running as a Linux program on the user-space personality (§4).

The second removes the `std` port. It is Brackly's call (§8).

## 3a. Security posture: Talos's (Brackly, 2026-11-16)

> some security design decisions still stand which would or would not affect the ported over
> code, borrowing from talos, there should not be a shell, and everything about what we need
> from asterinas should coincide with talos

Talos Linux has no shell, no SSH and no console login. It is managed only through its API, and
its root is immutable, so its workloads run without the OS ever needing a shell. The data OS
takes the same posture.

- **No shell anywhere Moruna ships or runs.**
  - None in the kernel's boot image, the engine's layer, the node image or any layer Moruna
    builds.
  - No interactive console, no login and no SSH.
  - No booted test uses one.
- **Tasks start by exec of their command, never through `sh -c`.** A user's image may contain a
  shell binary, as a container image on Talos may, but Moruna never invokes it, and nothing in
  the personality depends on one.
- **Managed only through the API.** The VM is driven only by the job protocol over the vsock,
  through the engine. The host side has no other way in.
- **An immutable root.** The root filesystem is the verified, read-only layers (SDD 27). The only
  writable places are a task's scratch and the engine's output disk.
- **The carried Linux layer is cut to match.**
  - It keeps what a job's programs need to run (processes, threads, files, pipes, signals,
    epoll, `fork` and `execve`).
  - It refuses administrative calls: `mount` and `umount`, `pivot_root` and `chroot`, `unshare`
    and `setns`, `ptrace`, module loading, `kexec`, `reboot`, `swapon` and `swapoff`, and
    changes of time.
  - It provides no terminals, `devpts` or login records.
  - A refused call answers `EPERM` or `ENOSYS` and is counted, as `kernel/linux` counts them
    today.
  - **The list** (stream B2, `kernel/linuxlayer/core/src/syscall/refused.rs`). `EPERM` is checked
    before any handler runs, which is what Linux answers an unprivileged process:
    - mounts: `mount`, `umount2`, and the new mount API (`open_tree`, `move_mount`, `fsopen`,
      `fsconfig`, `fsmount`, `fspick`, `mount_setattr`);
    - `pivot_root` and `chroot`;
    - namespaces: `unshare`, `setns`, and `clone` and `clone3` with a `CLONE_NEW*` flag;
    - `ptrace`, and the calls that need its permission (`process_vm_readv`,
      `process_vm_writev`, `pidfd_getfd`);
    - modules: `init_module`, `finit_module`, `delete_module`;
    - `kexec_load`, `kexec_file_load`, `reboot`, `swapon`, `swapoff`;
    - time: `settimeofday`, `clock_settime`, `adjtimex`, `clock_adjtime`;
    - names: `sethostname`, `setdomainname`.

    Every number the table lacks answers `ENOSYS`. Sockets keep their answers from
    `no_socket.rs`. Each refusal is counted per process and printed when the process ends.

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
- **`fork` (integrator, 2026-11-16, on stream B2's question).** One instance per task means
  every Linux process of the task must see the personality's handles and memory the same way.
  - **The core shares a handle table on request:** `process_clone` with `SHARE_HANDLES` gives
    the clone the cloned process's own table, as Linux's `CLONE_FILES` does.
    - Only a caller that has a personality, and holds that same table, may ask. The engine has
      no personality, so its table is never shared.
    - Ending a task frees the table and every process in it, even when processes hold each
      other's handles.
    - This grants nothing new: the task's processes already share the personality's memory,
      which holds every value in the table.
  - **The personality's region needs no core change:** its allocator maps each object, up to
    256 MiB, at an address it picks in every process of the task, under one lock, before
    handing out any memory from it. A clone inherits those mappings. Shared page tables for
    the region were declined: they are new code in OSTD's page tables, and these maps are
    rare. A measurement that shows they cost reopens it.
  - The engine never forks (C6), so none of this is on the engine's path.
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
- **A native task** reads the ring through `moruna_sys::plane` and sees each batch in place. The
  gate's functions, ported to Rust, are native tasks.
- **Python, afterwards** (`arrow-core-0.7.md` §8): a shim in an image's layer turns a batch into a
  `pyarrow.RecordBatch` in place. Stream P built it, but it is not on 0.7.0's gate.
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
- **Any worker runs any function's batch, as today.** Pinning places worker threads on cores. It
  never dedicates a worker to a function or a stage (Brackly, 2026-11-16), so the scheduler keeps
  its freedom to hand any morsel to any worker.
- **Changes to decisions are separate.** "Decode on the worker that consumes the batch" chooses
  the core a batch's decode runs on; it does not dedicate workers to functions. It is still a
  scheduler change, so it gets its own reason and measurement. KD3's conformance (same rows,
  refusals and decisions on a Linux host and on the data OS) proves the rest.

## 7. The gate, and what this does to the 0.7 steps

**The gate** (`arrow-core-0.7.md` §9; Brackly, 2026-11-16: data in, the function, data out):

- the seven jobs' functions, ported to Rust, run as native tasks on 0.6.6's input;
- 0.6.6's output rows, by count and content hash;
- timed against 0.6.6 on the same data: wall time and peak memory;
- VM boot reported separately and included in the wall time;
- both machine types, alternating order.

"Much better" becomes per-job targets after step 1.

**The steps:**

| Step | What | Against today's plan |
| --- | --- | --- |
| 1 | Today's measurements on Moruna's microkernel under KVM: forwarding, copies, boot, how the gate's Python image runs on today's personality, and `ruzstd` against C zstd | Unchanged |
| 2 | The arena on granted memory objects, pre-faulted, with huge pages; CPU pinning; the clock page | Step 2, plus three mechanisms |
| 3 | The engine drives the data disk's queues itself, with DMA into arena pages | Step 3 (data devices) |
| 4 | Input regions, output windows, rings, the grouped revoke; native tasks on the plane | Step 2's zero copies, with no call per batch |
| 5 | The four mechanisms, then Asterinas's Linux layer in user space; `kernel/linux` retired | §7a, unchanged |
| 6 | The engine's `std`: the native port, or a Linux program on the new personality | Step 4, now Brackly's choice (§3) |
| 7 | Dependencies: wasm out, the HTTP stack out, C as Brackly decides | Step 5 |
| 8 | The host side: `moruna run`, logs on the job updates | Step 6 |
| 9 | KD3 on the data OS, then the gate (the jobs' functions in Rust) on the benchmark VM | Step 7 |
| 10 | The Python client | Step 8 |

**Attack-surface check at each step.** Kernel-mode lines and the call list are counted at each
step against §0's table, and they grow only by the mechanisms named in §2.

## 8. Decisions (Brackly, 2026-11-16, as recommended)

1. **The kernel-mode set.** OSTD and Moruna's core, plus the mechanisms of §2 and nothing else.
2. **The engine drives the data disk itself,** with DMA into its arena, not through a separate
   driver process.
3. **The record check's one remaining crossing:** write access taken back in one call per group of
   batches, kept for DP1's safety (§5).
4. **The engine's `std`:** the engine stays a Linux program on the user-space personality. There is
   no native `std` port, because the hot path makes no calls (§3).
5. **Codecs:** C zstd stays until a pure-Rust zstd matches it on the gate.
