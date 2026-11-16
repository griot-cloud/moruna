# The data OS, built as one parallel step

Status: the split of `architecture/data-os.md` into parallel streams, 2026-11-16. The
integrator is the `arrow-core-0.7` session on `feature/arrow-core-0.7`.

**Authority.** Brackly approved `architecture/data-os.md` "as recommended", and said: "What I
would make is one step. Please look at that engine, parallelize this work so that we can finish
it." Afterwards the PM session takes the complete build to the benchmarks, the NYC taxi set
first, and iterates until Moruna is significantly better than the other engines.

## Rules for every stream

- **Branch and merge.**
  - Work in your own worktree, on a branch `data-os/<stream>`, cut from the commit that added
    this file.
  - Commit there. Push nothing: the CI pause holds, so queue no runs.
  - The integrator merges each stream into `feature/arrow-core-0.7` locally.
- **Write only the files your stream owns** (below). Each shared file has a marked block per
  stream. Write only inside yours, and only in the functions your stream owns.
- **The frozen files change only through the integrator.** Ask in the shared notes folder or by
  message:
  - `kernel/abi/src/lib.rs` (ABI 2.6);
  - `kernel/core/src/syscall.rs` (the dispatch);
  - `kernel/core/src/lib.rs` and `kernel/sys/src/lib.rs` (the module lists);
  - `kernel/sys/src/raw.rs`;
  - `crates/moruna-micro/src/lib.rs` and `devices.rs`;
  - `crates/moruna-reactor/src/lib.rs`;
  - `architecture/*`, `BOARD.md`, `CHANGELOG.md`.
- **Dependencies.**
  - Add a dependency in your own crate's `Cargo.toml` only.
  - `Cargo.lock` and the root `Cargo.toml`'s `[workspace.dependencies]` conflicts are the
    integrator's to settle at merge. Name each new dependency in your report.
- **Build to your contracts.** Each stream builds its components to the contracts named for it
  below (`data-os.md` §1a). A contract governs the engine's data path, never user programs.
  No contract may break the gate.
- **A speedup lands wherever it measures best** (Brackly: "feel free to move it to moruna or
  retrofit it or any version of a solution that works").
  - A contract-driven gain found here may also land in the engine on today's Linux host.
  - An OS component may be replaced by an engine-side solution, or the reverse.
  - Say which you chose and show the measurement.
- **Wiring keeps Moruna.**
  - The arena, scheduler, controller, placement, reactor, resume, the report and the events keep
    their decisions. Change only what is under their ports.
  - A change to a decision gets its own reason and measurement, and goes to the integrator
    first.
- **Tests ship with the code,** at the 80% per-crate line floor:
  - `cargo llvm-cov` for root crates;
  - the host tests CI's kernel job runs for kernel crates;
  - booted tests in the kernel's boot image for mechanisms only a running kernel can show.
- **The Mac's disk.**
  - Set `CARGO_INCREMENTAL=0` and a `CARGO_TARGET_DIR` of your own.
  - Delete it when your stream is done.
  - Never run a workspace-wide build or test: test the crates you touched.
- **KVM is one machine.**
  - `moruna-bench` (griot-stg) is the only x86-64 KVM machine, and this Mac is arm64.
  - Every heavy stage takes `~/.vm-lock` (`mkdir`, write `owner`), respects `~/.vm-lock-next`,
    runs one VM at a time at no more than 6 GiB, never steals the lock, and releases only its
    own.
  - The machine shuts down at 02:00 UTC, and that wipes `/data`. Keep results on
    `/home/bench`.
- **Style.** No em-dashes in text. Plain, short comments, as the files already have.
- **Report** to the integrator when done: your branch and its commits, test results with
  per-crate coverage, measurements, and anything left open.

## The frozen interfaces (at the commit that added this file)

| Interface | Where | Producer | Consumers |
| --- | --- | --- | --- |
| ABI 2.6 numbers: `THREAD_SET_CPU`, `CLOCK_PAGE`, `WINDOWS_PROTECT` | `kernel/abi` `nr` | A | C, P |
| ABI 2.6 numbers: `PERSONALITY_SET` to `THREAD_INTERRUPT` | `kernel/abi` `nr` | B1 | B2 |
| `map::HUGE`, `map::POPULATE` (refused until A adds them to `map::ALL`) | `kernel/abi` `map` | A | C |
| The clock page's layout (a seqlock) | `kernel/abi` `clock_page` | A | C, P |
| `windows_protect`'s ranges | `kernel/abi` `windows` | A | P |
| The personality's entry frame and its reasons | `kernel/abi` `personality` | B1 | B2 |
| The dispatch arms, routing to `datapath::*` and `personality::*` | `kernel/core/src/syscall.rs` | integrator | A, B1 |
| `moruna_sys::datapath`: `thread_set_cpu`, `clock_page`, `ClockReading`, `windows_protect` | `kernel/sys/src/datapath.rs` | integrator (signatures), A (kernel side) | C, P |
| `moruna_sys::personality`: `thread_state_read` and `thread_state_write` now; the rest B1 adds in its first commit | `kernel/sys/src/personality.rs` | B1 | B2 |
| The data devices: `disk://NAME` inputs as `disk.in.NAME`, the output disk `disk.out` holding one layer file | `crates/moruna-micro/src/devices.rs` | integrator | C, D, E |
| The engine's ports: `Allocator`, `Reactor`, `Sampler` | `moruna-kernel` (unchanged) | C | the engine |
| The host protocol over vsock | `crates/moruna-runtime/src/host/protocol.rs` (unchanged, except D's log records) | D | E |

Until a producer lands, each reserved call answers `BadSyscall`. A consumer keeps today's path
and switches to the new one when the call answers.

## The streams

### A. Kernel data-path mechanisms

- **Owns:**
  - `kernel/core/src/datapath.rs` and `kernel/sys/src/datapath.rs` (the kernel side; the
    signatures are frozen);
  - `kernel/core/src/{sched,clock}.rs`;
  - in `vm.rs`: `Space::map`, `populate`, `protect` and the stream A block;
  - the stream A blocks in `memory.rs` and `process.rs`;
  - if large user pages need OSTD changes, a vendored `ostd` under `kernel/vendor/` and its
    `[patch]` line in `kernel/Cargo.toml`.
- **Contracts:** C1 (`windows_protect`), C4 (`map::HUGE`, `map::POPULATE`), C5 (pinning, and
  no ticks or device interrupts on worker cores if measured worth it), C8 (the clock page).
- **Builds:**
  - `thread_set_cpu`: a thread on one core, never bound to work;
  - `clock_page` and its updates;
  - `windows_protect`: one flush for a list of ranges;
  - `map::HUGE` and `map::POPULATE` in `memory_map` and `process_map`.
- **Tests:**
  - host tests of any pure logic;
  - booted tests in the boot image that pin a thread and check where it runs, read the clock
    page against `clock_get`, revoke a group of windows and fault on a write, and map a huge,
    populated range and count no faults on touch;
  - `cargo test -p moruna-sys` on x86-64 Linux (the clock reader).
- **Done when:** the four mechanisms answer, CI's boot passes under KVM on 1, 2 and 4 CPUs with
  the new tests, and the kernel-mode lines and calls added are listed for `data-os.md` §0.

### B1. The personality's mechanisms in the core

- **Owns:**
  - `kernel/core/src/personality.rs` and `kernel/sys/src/personality.rs`;
  - `kernel/core/src/{foreign,usercopy}.rs`;
  - in `vm.rs`: `Space::fault` and the stream B1 block;
  - the stream B1 blocks in `memory.rs` and `process.rs`;
  - the user-mode loop's syscall branch in `process.rs` (lines around 976).
- **Contracts:** none of the data path's. Its measure is the kernel-mode cost in `data-os.md` §0,
  and C6's restart of a task as a fresh process.
- **Builds:** `kernel-is-the-machine.md` §4.1:
  - a personality entry on the calling thread, an upper region the program cannot reach (in the
    shape of Zircon's restricted mode);
  - copy-on-write snapshots and address-space clones;
  - a user pager;
  - thread state and interruption.

  Its first commit fixes the wrappers' arguments in `moruna_sys::personality`, so B2 can build
  against them.
- **Tests:** the core's suite for each mechanism with no personality present (KK-style, booted),
  plus host tests.
- **Done when:**
  - a test program enters, handles a call and returns through the entry;
  - a forked address space copies on write;
  - a pager serves a fault and refuses one;
  - a sleeping thread is interrupted;
  - CI's boot passes under KVM.

### B2. Asterinas's Linux layer in user space

- **Owns:**
  - a new tree `kernel/linuxlayer/`: Asterinas's `kernel/core/src` Linux layer, pinned to
    Asterinas `a0de2258` (version 0.19.0, the OSTD Moruna uses), its files kept MPL-2.0 with
    their source;
  - the personality process that hosts it.
- **Contracts:** C6 (a task's process model is the personality's, inside user code). The contracts
  stop at the user-code boundary, so user programs keep full POSIX behaviour.
- **Builds:** `kernel-is-the-machine.md` §4.2, cut to 0.7.0:
  - processes, files, pipes, signals, epoll and the synthetic filesystems;
  - the mechanical swaps (OSTD's locks and wait queues on the core's futex, user memory through
    the fault-tolerant copy);
  - the rewrite of what touches the entry, clones, the pager and thread state, against B1's
    wrappers.

  Sockets and the cloud platform stay out.
- **Tests:**
  - host tests of the carried logic;
  - then booted: static musl, dynamic glibc, `sh`, CPython 3.14 importing NumPy and pyarrow, and
    the gVisor system-call tests that apply.
- **Done when:**
  - the gate's Python image runs as an image task on it under KVM;
  - the engine runs on it;
  - `kernel/linux` can be retired. The integrator retires it at merge.
- **Size:** the largest stream. About 137,000 lines, about 82% mechanical. If it needs to split
  by subsystem, it says so to the integrator, who starts the extra streams.

### C. The engine's memory and I/O

- **Owns:**
  - `crates/moruna-micro/src/{arena,blk,accounts}.rs` and `blk/`;
  - `crates/moruna-reactor/src/kernel_blk.rs`;
  - `crates/moruna-arena` (a backing on granted memory objects);
  - `crates/moruna-runtime/src/bin/moruna-engine.rs` and the engine's adapter wiring in
    `crates/moruna-runtime/src/run.rs`;
  - `kernel/engine` (the supervisor's grants of memory and devices to the engine).
- **Contracts:**
  - C2: planned reads, readahead from the scheduler's next splits, DMA into the arena, no page
    cache;
  - C3: the output layer;
  - C4: the arena on one fixed budget, populated, huge pages, no zeroing on reuse;
  - C5: the pool pinned to cores, any worker any function;
  - C7: the engine drives its own disk;
  - C8: the engine's clock on the clock page.

  C2's readahead and C4's "no zeroing on reuse" may land in the engine on a Linux host too, if
  they measure better there.
- **Builds:**
  - the arena on memory objects granted at start, populated, with huge pages (falling back while
    A's flags answer `BadSyscall`);
  - the engine's allocator on the arena;
  - workers pinned with `thread_set_cpu` (a place, not a function);
  - the clock page for timing;
  - the engine driving each `disk.in.*` and `disk.out` device's virtio-blk queues itself, with
    DMA into arena pages, polled while busy and woken through a futex when idle (reusing
    `kernel/drivers`' virtio-blk logic);
  - the reactor on those queues;
  - the output layer file written to `disk.out` at the run's end;
  - the `Sampler` on the run's and tasks' accounts.
- **Tests:** unit tests on Linux of each adapter, and booted runs that read a `disk://` input
  into the arena and count no faults and no copies.
- **Done when:** a real job reads a `disk://` Parquet input under KVM with DMA into arena pages
  and no page faults on the arena, and writes its output layer, with KD3's rows unchanged.

### P. Batches between the engine and tasks

- **Owns:**
  - `crates/moruna-micro/src/{rings,task,layout}.rs` and `rings/`;
  - `kernel/plane/core` (the engine's `KernelPlane`);
  - `kernel/sys/src/plane.rs` (the task's API);
  - a new `images/python/` holding the Python shim, which is image-side code, not part of
    Moruna's runtime.
- **Contracts:**
  - C1: immutable records shared as frames, the grouped revoke;
  - C4's trust boundary: pages are zeroed before a task first sees them;
  - C6: a failed stateless task is restarted as a fresh process.
- **Builds:** `data-os.md` §5:
  - per-task input regions mapped read-only once;
  - the engine decoding a batch straight into the region of the task that consumes it;
  - output windows mapped writable;
  - single-producer, single-consumer rings with futex waits only when empty;
  - the grouped revoke before the record check (DP1), on `windows_protect`, with today's unmap
    path until A lands;
  - fan-out;
  - the Python shim (`pyarrow.foreign_buffer` over the mapping, one interpreter per process,
    N processes per stage);
  - descriptors 0 and 1 kept as the copying fallback for an unmodified image.

  `offer` and `land`'s copies go.
- **Tests:**
  - DP-I1 to DP-I9 hold, by the plane's conformance suite;
  - the `COPIED` counter and an engine boundary-copy counter stay at zero through N image
    stages;
  - no call per batch, counted.
- **Done when:** a Python image task runs a batch function under KVM with no boundary copy and
  no call per batch in steady state, and its rows match.

### D. The host side

- **Owns:**
  - `crates/moruna-runtime/src/host/` (`cli.rs`'s `run_vm`, `native.rs`, `protocol.rs`,
    `logs.rs`);
  - `crates/moruna-vmm`;
  - `kernel/node/` (the image the launcher boots).
- **Contracts:** C3 (reading the output layer on the host), C7 (one job per VM, nothing kept
  between runs).
- **Builds:** `moruna run JOB` on a Linux x86-64 node with KVM, which:
  - stages remote inputs on the host;
  - attaches each input as a read-only virtio-blk device and an output disk (`devices.rs`), the
    launcher's devices of KD4;
  - boots the VM;
  - sends the job over the vsock;
  - streams events, logs (as records on the job updates, with producer, level and line,
    rate-limited) and the report back;
  - reads the output layer into the job's output directory;
  - reports VM boot time on its own.

  QEMU stays the hypervisor.
- **Tests:** unit tests of the launcher's devices and arguments, and a booted end-to-end run of a
  small job under KVM.
- **Done when:** `moruna run` runs a Parquet-to-Parquet job end to end under KVM on
  `moruna-bench`, with the logs on the update stream and boot time reported.

### E. Proof

- **Owns:**
  - `bench/vm/`;
  - KD3's fixtures and peer (`crates/moruna-runtime/examples/kd3*`,
    `crates/moruna-py/tests/fixtures/kd3-matrix.json`);
  - `kernel/node/tools/proof.py`;
  - `kernel/tools/node-fixtures.sh`;
  - a new `images/gate/` for the gate's images.
- **Contracts:** measures every contract's gain as `data-os.md` §1a names it (copies, faults,
  calls per batch, interrupts on worker cores, bytes through a page cache), against 0.6.6 and
  against today's microkernel figures from step 1.
- **Builds:**
  - a KVM mode for the node fixtures (`proof.py` forces TCG today, line 356);
  - KD3's conformance on the data OS (same rows, refusals and decisions as on a Linux host);
  - the gate's Python image (CPython 3.14 with NumPy and pyarrow, 0.6.6's kernels unchanged,
    P's shim);
  - the harness's data OS mode: 0.6.6's jobs through `moruna run` on `moruna-bench`, against
    0.6.6, on the PM's terms (`arrow-core-0.7.md` §9).
- **Tests:** the harness's own unit tests, and the fixtures under KVM.
- **Done when:** the gate runs end to end on `moruna-bench` for the NYC set, with identical rows,
  boot reported, and both machine types ready, and hands its numbers to the PM.

## Order and dependencies

- **Every stream starts at once.** Each builds against the frozen interfaces and keeps today's
  path until its producer lands.
- **Hard dependencies:**
  - B2's entry loop needs B1's wrappers (B1's first commit);
  - the gate needs C, P and D;
  - the gate's Python image runs on B2's personality, or on today's personality until B2 lands.
- **Merge order** into `feature/arrow-core-0.7`: A and B1 first (the kernel), then C and P, then
  D, then B2 (with `kernel/linux` retired), and E throughout.
- **The integrator,** at each merge:
  - settles `Cargo.lock`;
  - runs the touched crates' tests and CI's boot under KVM;
  - updates `data-os.md` §0's attack-surface figures.
