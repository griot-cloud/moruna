# The data OS, built as one parallel step

Status: the split of `architecture/data-os.md` into parallel streams, 2026-11-16. The
integrator is the `arrow-core-0.7` session on `feature/arrow-core-0.7`.

**Authority.** Brackly approved `architecture/data-os.md` "as recommended", and said: "What I
would make is one step. Please look at that engine, parallelize this work so that we can finish
it." Afterwards the PM session takes the complete build to the benchmarks, the NYC taxi set
first, and iterates until Moruna is significantly better than the other engines.

## Rules for every stream

- **Talos's security posture binds every stream** (`data-os.md` §3a; Brackly, 2026-11-16).
  - No shell in anything Moruna ships or runs, and none in booted tests: no `sh` in any done
    criterion.
  - Tasks start by exec, never `sh -c`.
  - No console login, no SSH, no terminals or `devpts`.
  - The root is read-only.
  - The carried Linux layer refuses administrative calls.

- **Branch and merge.**
  - Work in your own worktree, on a branch `data-os/<stream>`, cut from `feature/arrow-core-0.7`
    when the stream starts (at or after `fa0a02bf`, which added the contracts).
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
- **Code carried from Asterinas is judged by test suites, not line coverage** (Brackly,
  2026-11-16: "Yes, test suites").
  - Moruna's own code keeps the 80% per-crate floor.
  - The files carried from Asterinas's Linux layer are judged by Asterinas's own tests and the
    gVisor system-call suite: report which pass and which fail.
  - Code we write or rewrite around the carried code is Moruna's own, so it gets the 80% floor.
    That covers the user-space OSTD, the glue and the rewritten files.
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

## The frozen interfaces (frozen at `add53e75`)

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
  - `kernel/core/src/{foreign,usercopy,futex}.rs`. `futex.rs` was added on 2026-11-16: words in a
    memory object mapped into several processes are keyed by the object and offset, not by
    process. B2's personality locks need that, and so do P's engine-to-task rings;
  - in `vm.rs`: `Space::fault` and the stream B1 block;
  - the stream B1 blocks in `memory.rs` and `process.rs`;
  - the user-mode loop's syscall branch in `process.rs` (lines around 976);
  - granted on 2026-11-16, each marked as a B1 hook:
    - in `process.rs`: one field on `Thread` and one on `Process`; `run()`'s exception and
      kernel-event branches and its loop condition; `Thread::killed()`; `activate_current`'s
      activation line;
    - in `vm.rs`: one field on `Space`; `unmap_ptes`; `Backing` and its `impl`; `frame`;
      `touch`; `fill_ptes`'s object and zero arms; `each_page`; `discard`;
    - in `memory.rs`: `MemoryObject`'s storage, its constructors, `frames`, `read` and `write`;
    - a boot test program, `kernel/programs/src/bin/personality.rs`, with its block at the end
      of `kernel/initramfs/boot.image` under a `# stream B1` comment.

  **Where A and B1 meet:** `fill_ptes` is B1's. Stream A reaches huge pages through its own
  functions, called from `Space::map` and `populate`. If A needs a hook inside `fill_ptes`, it
  adds one call line marked `// stream A hook`, and the integrator settles any conflict at
  merge. Each stream adds its boot test block at the end of `boot.image` under its own comment.
- **Contracts:** none of the data path's. Its measure is the kernel-mode cost in `data-os.md` §0,
  and C6's restart of a task as a fresh process.
- **Builds:** `kernel-is-the-machine.md` §4.1:
  - a personality entry on the calling thread, an upper region the program cannot reach (in the
    shape of Zircon's restricted mode);
  - copy-on-write snapshots and address-space clones;
  - a user pager;
  - thread state and interruption.

  - shared futexes, keyed by memory object and offset for shared mappings;
  - a fault-tolerant copy: `personality_set` registers a copy routine's range and a recovery
    address, and a fault in that range resumes at the recovery address with the fault address in
    `rdx`. B2 asked for this on 2026-11-16.

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
  - then booted: static musl and dynamic glibc (no shell), and the gVisor system-call tests that
    apply. CPython is not a 0.7.0 target (Brackly, 2026-11-16).
- **Done when:**
  - static musl and dynamic glibc programs, and a forking one, run on it under KVM;
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
  - the device manager's `hand_over(name)` request in `kernel/devices` (2026-11-16): for a table
    line that starts no driver, it hands the function's resources (config, BARs, MSI, DMA) to the
    supervisor once;
  - a worker-start hook in `moruna-scheduler`'s worker spawn, for pinning. It is wiring, and
    changes nothing the scheduler decides.
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
- **Done when:** a native Rust task (`moruna_sys::plane`) runs a batch function under KVM with
  no boundary copy and no call per batch in steady state, and its rows match. The Python shim
  stays in `images/python/` for later (`arrow-core-0.7.md` §8) and is not part of the proof.

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
    launcher's devices of KD4. They are transitional virtio-blk (1af4:1001), with an input's
    serial giving its file's length, and device-table lines that start no driver;
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
  - a new `images/gate/` for the gate's tasks.
- **Contracts:** measures every contract's gain as `data-os.md` §1a names it (copies, faults,
  calls per batch, interrupts on worker cores, bytes through a page cache), against 0.6.6 and
  against today's microkernel figures from step 1.
- **Builds:**
  - a KVM mode for the node fixtures (`proof.py` forces TCG today, line 356);
  - KD3's conformance on the data OS (same rows, refusals and decisions as on a Linux host);
  - the seven jobs' functions ported to Rust as native tasks on `moruna_sys::plane`, each
    producing 0.6.6's output rows (count and content hash) from 0.6.6's input. A float column
    that cannot match bit for bit is named with its tolerance before it is measured;
  - the harness's data OS mode: each job through `moruna run` on `moruna-bench`, timed against
    0.6.6 on the same data (`arrow-core-0.7.md` §9).
- **Tests:** the harness's own unit tests, and the fixtures under KVM.
- **Done when:** the gate runs end to end on `moruna-bench` for the NYC set, then the large
  jobs, with identical rows, boot reported, and both machine types ready, and hands its numbers
  to the PM.

## Order and dependencies

- **Every stream starts at once.** Each builds against the frozen interfaces and keeps today's
  path until its producer lands.
- **Hard dependencies:**
  - B2's entry loop needs B1's wrappers (B1's first commit);
  - the gate needs C, P and D;
  - the gate's tasks are native Rust, so the gate does not wait on B2.
- **Merge order** into `feature/arrow-core-0.7`: A and B1 first (the kernel), then C and P, then
  D, then B2 (with `kernel/linux` retired), and E throughout.
- **The integrator,** at each merge:
  - settles `Cargo.lock`;
  - runs the touched crates' tests and CI's boot under KVM;
  - updates `data-os.md` §0's attack-surface figures.

## Integration log

- **2026-11-16: stream A merged** (fast-forward to `eb4b0b07`). Its CI boot passed 170 of 170
  checks under KVM on 1, 2 and 4 CPUs.
  - It added three calls, two mapping flags, one probe operation, a vendored OSTD 0.19.0
    (about 115 code lines changed) and `moruna-datapath`.
  - It added about 670 code lines to kernel mode.
- **Decided at that merge:**
  - **A builds C5's tickless worker cores next.** The measurement shows the gain the design
    asked for: a pinned core loses 3.8% to 4.5% of its time to about two interruptions a
    millisecond, about 19 us each, even on a quiet VM.
  - **A adds `windows_protect`'s slot form,** the ABI's doc being amended: the engine revokes a
    task's output windows through the output slot, since it holds no handle to the task's
    process (stream P).
  - **A adds a booted test of `POPULATE` zero pages through `process_map`.**
  - **B1 fixes the reclaim of empty 4 KiB tables when a 2 MiB region is unmapped** (`unmap_ptes`
    is B1's), through a cursor locked one level up, as A's `eb4b0b07` does for large pages.
  - **The boot manifest's `MAX_PROCESSES` rises from 32 to 64,** because the booted tests filled
    32.
  - **CI's kernel job host-tests `moruna-datapath` and `moruna-sys`.**
  - **B2's proposed four-way split** (core and processes, vm, fs, images) is accepted. The new
    streams start when B2's core compiles and a session slot is free.
- **2026-11-16: stream B1 merged** (`e7a1fb88`, from `f855154e`). CI's boot passed 191 of 191
  checks under KVM on 1, 2 and 4 CPUs.
  - Every booted B1 test reports ok: the interrupt, snapshot, clone, pager, entry, copy,
    tables, futex and personality tests.
  - Kernel mode grows by 1,372 code lines net, through the nine reserved calls and the
    probe-only `TABLE`.
  - The KB1 ping-pong shows no regression against step 1: 6.09 s, 5.42 s and 0.72 s, against
    6.46 s, 6.19 s and 0.85 s.
  - **Open, and B1's next:**
    - a snapshot mapped in two places shares later writes with a clone, which `fork` cannot
      allow;
    - a clone briefly needs room for its zero pages twice;
    - `process_read` and `process_write` answer `ShouldWait` on a page the pager has not
      supplied.
  - **Later:** the GS base is not kept per thread, and a program thread is interrupted only
    within one tick.
- **2026-11-16: stream A's second round merged** (`5b17702c`, from `7a74b44f`). CI's boot
  passed 197 of 197 checks under KVM on 1, 2 and 4 CPUs, and `deadline-boot.sh` passed.
  - `windows_protect`'s slot form landed, so stream P switches its grouped revoke on.
  - `POPULATE` through `process_map` is booted.
  - C5's stage (b) cut a pinned worker CPU's loss on a quiet VM from 3.8%-4.5% to 1.5%-1.6%.
    Of a tick's ~23 us, about 0.5 us was OSTD's own work; the rest was VM exits.
  - OSTD's RCU now counts user mode and idle as quiescent, so unmapped frames are freed.
  - **Left for stream D:** the remaining 1.5% is the host's own 1 kHz tick on the vCPU threads.
    The launcher should pin QEMU's vCPU threads to host cores isolated from the host's tick
    (`nohz_full`, `isolcpus` or a cgroup cpuset, `rcu_nocbs`), and report the loss measured
    again. It is a host setting for Griot's node, so D documents it and measures it on
    `moruna-bench`.
- **2026-11-16: B1's three follow-ups merged** (`586d7bc4`, from `089a7e48`). CI's boot passed
  193 of 193 checks under KVM on 1, 2 and 4 CPUs.
  - A snapshot binds to the one space that maps it; a clone makes one pair of sides per object,
    so two mappings of one snapshot stay one object in the child, with no later write shared.
  - A clone moves each shared zero page with the bytes already charged for it, so the source's
    account needs room only for 64-byte records.
  - `process_read` and `process_write` wait for a pager's page as a fault does, and answer
    `Fault` on a refusal.
  - Kernel mode: net +90 code lines, no new calls. Ping-pong 6.15 s, 5.30 s and 0.74 s.
  - **For B2-vm:** a `MAP_PRIVATE` file mapping needs its own `memory_snapshot` per process.
- **2026-11-16: `fork` decided** (`data-os.md` §4), on B2's question that a forked child has no
  handles. B1 adds `process_clone`'s `SHARE_HANDLES` (personality processes only, the engine's
  table never shared, no cycle leaks). The personality maps its region's objects into every
  process itself; shared page tables were declined.
- **2026-11-16: B2-fs final** at `1dfe096f` (`feature/data-os-b2-fs`), for B2 to merge.
  - The root is an imagefs client over Moruna's verified layers, not overlayfs.
  - `/tmp` is a tmpfs under the scratch quota. `/proc` and `/sys` are cut to the paths glibc,
    CPython, OpenBLAS and the allocators read. `/dev` is `null`, `zero` and `urandom`. There
    are no terminals.
  - Descriptors 0, 1 and 2 are the input slot, the output slot and the log.
  - New code is at 94.6% line coverage.
  - File times read as zero, as `kernel/linux` reports them today.
  - The mount-family handlers (836 lines) and the fs items only they use go in one pass by B2,
    since `refused.rs` answers first.
- **2026-11-16: stream C merged** (`e42a083c`, from `3141b3d6`). All 15 production-node cases
  pass under KVM on C's head, with A and B1's first round merged.
  - It adds:
    - the engine's arena on granted large-page memory, never given back (C4);
    - the engine driving its data disks' virtio-blk queues (C2, C3, C7);
    - workers placed at start through the scheduler's worker-start hook, which decides
      nothing;
    - no shell as the engine's program, and outputs laid out in the scratch.
  - Coverage: arena 93.1%, runtime 87.0%.
  - **Waits on D:** the `disk://` job under KVM.
- **2026-11-16: the gate is the jobs' functions in Rust** (Brackly: "we have data, then a
  function, then producing some other data; we're supposed to measure that, simple").
  - E ports the seven functions to Rust as native tasks; same input, 0.6.6's output rows, timed
    against 0.6.6.
  - No Python on the gate's path: P's proof is a native Rust task, and B2's 0.7.0 targets are
    musl and glibc. CPython waits for `arrow-core-0.7.md` §8.
  - The gate needs C, P, D and E, and no longer waits on B2.
