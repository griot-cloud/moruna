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
    own. The 6 GiB cap is the Mac's rule, kept for booted tests; the gate's VM on
    `moruna-bench` runs at the benchmark's budget (24 GiB on the D8ds_v6, as 0.6.6's table did).
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
    0.6.6 on the same data (`arrow-core-0.7.md` §9);
  - the PM's controls (2026-11-16), so the result says something about the engine and the OS,
    not only about leaving Python:
    1. **The same function on 0.6.6.** If 0.6.6 can run the Rust port (the `rust` entry from
       strict-guest-kernels, a kernel compiled into the binary), 0.6.6 is timed with it too. The
       report gives both: 0.6.6 with the Python functions (the published table) and 0.6.6 with
       the Rust functions. If 0.6.6 cannot run them, the report says so.
    2. **A plain Rust baseline** for the "better than other options" runs: the same Rust function
       in a plain Rust thread pool (arrow-rs and parquet reading the same row groups, rayon,
       writing the same zstd level). Ray and Spark keep the Python functions, labelled as such.
    3. **The same output settings for every run:** zstd level, row group size, files and splits.
    4. **CPU-seconds and peak memory beside wall time.**
    5. **A time split per job:** read and decode, the function, encode and write, and waits.
    6. **A record of each port's equivalence:** the Python function it replaces, its output rows
       matched by count and content hash, and any float tolerance named before measuring.
  - **The content hash is taken on a canonical form** (integrator, 2026-11-16), the same for
    0.6.6 and 0.7, because `driver.py`'s `checksum` is only a row count and column sums:
    - a row-wise function's output is sorted by all its columns, then hashed;
    - a per-batch aggregate, whose rows depend on where batches split (Occupancy's partial
      counts), is first re-aggregated by its group keys (`zone`, `minute_of_day`, summing
      `car_minutes`), then sorted and hashed;
    - the row count compared is the canonical form's.
  - **The PM's NYC matrix** in the harness's report (`arrow-core-0.7.md` §9): the five labelled
    row groups (native expressions included), the columns, and 0.7's speedup against the best
    Rust row and the best native-expression row.
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
- **2026-11-16: B2-vm final** at `94a30bfd` (`feature/data-os-b2-vm`), merged into B2's branch
  (`bf6f8db4`).
  - Frames are pages of memory-object chunks (2 to 64 MiB) from an injected source (B2's region
    allocator), kept as (object, offset) plus address.
  - `process_map`, `process_unmap` and `process_protect` are batched per cursor: fault-around's
    16 pages are 1 call, and fork through `process_clone` is 0 calls a page.
  - Private-file copy-on-write is done on frames, not `memory_snapshot`.
  - Host tests: ostd 91, core 147. Coverage 85.7% to 100% per file, ostd 92.3%.
  - Timings wait for B2's slot (`kernel/linuxlayer/vmcost`).
  - **Open for the core, decided after those timings:** each `process_map` creates one area
    (96 bytes charged), and adjacent areas of one object are never merged, so anonymous memory
    faulted a page at a time costs an area a page.
- **2026-11-16: stream E's host work merged** (`a52dc581`, from `bf5c1115`).
  - All seven functions are ported (`images/gate/functions`) and match 0.6.6's Python outputs on
    the fixtures, five bit for bit. Score's `predicted_tip` is within abs 1.1e-6 (BLAS order),
    and Overture's areas and volumes within rel 1e-9 and 1e-12, the observed differences near
    machine precision. The tolerances are confirmed again on x86.
  - The canonical hash is an order-independent multiset hash (the sum of row SHA-256s mod
    2^128), with Occupancy regrouped first.
  - **Control 1:** 0.6.6 cannot run a `rust` kind (removed in 0.5.8, KD-I7). Its row is
    "0.6.6, Rust function via pyo3" (`images/gate/py066`, GIL released), labelled so.
  - The baseline (`gate-baseline`, rayon, arrow-rs, parquet, Moruna's sink settings) is built,
    as are the harness's engines, plan, report and the KVM mode for the node fixtures.
  - Coverage: functions 94.2%, record 86.1%, baseline 90.8%.
- **2026-11-16: decisions on E's gaps** (integrator; Brackly away, technical calls):
  - **A native program in an image** is declared by its kernel (`abi: moruna`; the default
    stays Linux). The runner starts it with `process_create` and its plane attached, never
    through the personality, after checking the ELF matches the declaration. Owner: P.
  - **Output size:** a task may put several output records for one input batch, so no window
    is sized from a guess of the stage's amplification. Owner: P.
  - **A native task's log** goes through `moruna_sys::plane` onto the job updates, as a Linux
    task's descriptor 2 does. Owner: P, after the gate.
  - **`moruna run`'s form** follows E's harness: `moruna run JOB --output DIR --report FILE`,
    with local inputs staged as `disk://` names and `vm.boot_s` in the report. Owner: D.
  - **The time split:** the engine's report times decode apart from read, and encode apart from
    write. Owner: C.
  - **AVX:** the gate's tasks build with `+avx,+avx2` (no FMA, so the bits are unchanged). Both
    gate machines have AVX2, 0.6.6's NumPy used the hardware, and OSTD saves AVX state with
    `xsave`. A booted test of AVX state across preemption goes into A's next run. The
    `x86_64-moruna-user` default stays SSE2 for other programs.
  - **The gate's VM time:** about 15 h in all. The NYC D8ds_v6 groups 1, 2, 3 and 5 (about
    1.5 h) go first, once P and D are merged; the Python engines, the D4ls_v5 and the large jobs
    follow on later days.
- **2026-11-16: B2's first boots under KVM** (18:42 to 18:54 UTC, head `d0383bc5` with
  `9e8ab3e1`'s line patched in): the Linux layer in user space runs real programs, with no shell.
  - Static musl: "hello from musl", reading the image and writing `/tmp`, exit 0.
  - Dynamic glibc 2.39 through `ld-linux`: LX-T12's two regression checks pass, exit 0. One
    refused call, `rseq` (334), which glibc tolerates.
  - Fork: a child and a grandchild in their own processes, a pipe between them, exit 7 as
    expected, each process exiting 0.
  - **A kernel panic reachable from user space**, found there and fixed by the integrator in
    `9e8ab3e1`: `ProgramTable::unmap` built a backwards range for a protect or unmap inside one
    table region, and `BTreeSet::range` panicked in kernel mode. B1 adds the booted regression
    test and audits the same class of range.
  - The gVisor suite (it needs a Bazel build of gVisor) and B2-fs's booted checks are next.
- **2026-11-16: B2-vm's costs under KVM** (`kernel/linuxlayer/vmcost`, all 25 lines in the
  shared notes, `b2-first-boots.md`):
  - `process_map`: 842 ns for 1 page a call; 70 ns a page at 16; 21 ns a page at 4,096.
  - A fault that maps 1 page costs 3,142 ns a page; fault-around of 16 costs 1,046 ns; an
    already-mapped page 869 ns.
  - The pager: 61.8 us a page at 1 page a request; 5.6 us at 16.
  - `process_clone` of 4,096 pages: 0.86 ms against 2.26 ms for create plus replay.
  - **Decided:** fault-around of 16 stays the default. File-backed faults always ask the pager
    for 16 pages or more. The core does not merge adjacent areas: with fault-around, an area is
    about 6 bytes a page, and merging would be new kernel-mode code for little. None of this is
    on the engine's path, which uses populated granted memory.
- **2026-11-16: how `kernel/linux` is retired** (integrator, on B2's question): a drop-in `linux`
  server on the personality, speaking linux-protocol, so its clients (linux-test, KD1, the
  engine, moruna-micro) do not change.
  - `run_granted` stays: the engine is a Linux program on the personality that makes native
    calls on its granted memory, disks, cores and clock page.
  - A program's native calls pass through the personality's entry, which makes them only on
    handles granted to the program. Routing them straight to the core was rejected: the
    program would reach the personality's own handles.
  - LX-T16's "forward" is measured again as the entry crossing. The engine's batch path still
    makes no calls.
  - **Done when, under KVM:** linux-test, KD1 and LX-T16 pass on the new server; C's 15 node
    cases pass with the engine on the personality; B2-fs's booted checks pass through a real
    task start; `kernel/linux/{core,server}` are deleted.
- **2026-11-16: a program's native calls** (`data-os.md` §4): program mode gets a handle table of
  its own, an allowlist of native calls, no handle 0 for a process, and a reserved address range
  for core-placed mappings. B1 builds it after its current slot (ABI 2.6, unreleased: one new
  call, and a new `personality_set` argument). B2 uses it for `run_granted`. It is not on the
  gate's path: the engine runs on today's server until B2 lands.
- **2026-11-16: B1's shared handles merged** (`905258d0`, from `b4116620`). CI's boot passed 203
  of 203 checks under KVM on 1, 2 and 4 CPUs.
  - `process_clone` shares the cloned process's handle table on request (`SHARE_HANDLES`),
    under the §4 guards. A task of processes holding each other's handles is killed whole.
  - **A second kernel panic reachable from user space, fixed** (`fdece684`, a marked B1 hook
    in `boot.rs`, approved). The kernel had never installed OSTD's handler for a kernel copy
    that faults on a user page. Any call given a bad pointer panicked the kernel, as did a
    copy onto a page shared copy-on-write since a clone. Such a copy now answers `Fault`, or
    readies the page and copies again. It had been there since KB1.
  - B1 audited every range and every sum on user values in `vm.rs` and `memory.rs`. The first
    panic's clamp (`9e8ab3e1`) covers every path through `unmap_ptes` (unmap, protect, discard,
    copy-on-write faults, kernel copies, `windows_protect`, clones). Nothing else can run
    backwards or overflow.
  - Kernel mode: net +85 code lines.
  - **Open:** ping-pong on 1 and 2 CPUs read 7.69 s and 6.74 s against 6.15 s and 5.30 s
    before, with the boot now carrying A's, P's and E's tests on the same CPUs. B1 measures it
    alone in its next slot before and after `SHARE_HANDLES`, to rule a regression out.
- **2026-11-16: stream D merged** (`e39b8f0e`, from `e8c18316`). `moruna run JOB --output DIR
  --report FILE` is a VM run: the job's local inputs become `disk://` data disks, the output
  disk is unpacked into DIR, and the report carries `vm` (`boot_s`, `launch_s`, `stage_s`,
  `node_s`, `job_s`, `outputs_s`, `total_s`). Log records ride the run's events at the
  connection's own rate. Coverage on moruna-bench: `moruna-vmm` 84.6%, `moruna-runtime` 83.3%.
  - **Tick isolation measured:** with `nohz_full`, `isolcpus=domain,managed_irq` and
    `rcu_nocbs` on the vCPUs' cores and the launcher's placement, a quiet boot loses
    0.058-0.073% of a 300 ms spin, against 1.45% with the host tick on. Host settings for
    Griot's node are in `kernel/node/HOST.md`.
  - **The KVM end to end did not run.** moruna-bench has no QEMU with both KVM and seccomp
    (`/opt/qemu` lacks seccomp, `/opt/qemu-proof` lacks KVM), and the launcher always jails
    QEMU with `-sandbox`. Under TCG the run reached the engine's first touch of its data disks,
    where the guest panicked on OSTD's missing user page fault handler. That is the panic B1
    fixed in `fdece684`; D's branch predated it, and the merged head has it.
  - **Integrator's call:** the launcher keeps `-sandbox`. moruna-bench gets one QEMU built
    from the repo's pinned 10.2.1 source with KVM, seccomp, vhost-user and slirp, then D's
    harness (`/home/bench/d/d-e2e.sh`) runs at the merged head before E's NYC gate.
  - **For E:** a 24 GiB-budget document gets a 27 GiB VM by default; the gate passes
    `--memory-gib 24` with a budget of 20.6 GiB or less. A run takes at most 40 data disks,
    so Overture's 240 files need C's multi-file input on one disk; NYC (12) and FineWeb (20)
    fit.
  - **Open, for Brackly:** the image tool's `Description::programs()` fails every image build
    on a `-` driver line (D's one-line fix, in `kernel/tools/image/src/lib.rs`, was blocked
    by the permission classifier, so it waits for you). `node-for-job.py` works around it per
    node. `node-for-job.py` runs under `python3` on the host and is to be ported to Rust.
- **2026-11-17: stream P merged** (`7cf337b2`, from `5ffded0f`, which already held D). Under KVM
  (r7, `c2ff1da6`): DS-P passed on 1, 2 and 4 CPUs, 356 batches of 131,072 rows judged row by
  row, and the kernel copied for the task only its bells' futex words; the 15 node cases
  passed; r8 (`5ffded0f`, 1 CPU) passed 210 of 210 with the task's log line read back from its
  log ring. Coverage: `moruna-plane` 89.0%.
  - **A native program from an image** (`abi: moruna`, which needs `plane: true`): the engine
    checks the program's ELF against `kernel/sys/user.ld`'s layout, both ways, and starts it
    itself with no personality: `Process::create_in`, `process_map_image`, `plane::attach`,
    `start_with` the log ring. Ring protocol 4 adds the task's exit word (`TASK_EXIT`), read
    after the process's TERMINATED signal; no word means a fault or a kill. Several output
    records a batch; a native task's log lines go through `moruna_sys::plane::Log`.
  - **Integrator's call (security), on P's question:** the job's engine gets a WRITE|TRANSFER
    duplicate of the root process resource (`grant process root.process` for the initial job,
    `grant process process process` in engine.conf). Accepted: a process resource only lets
    its holder create or clone processes, each charged to an account the holder has WRITE on
    (`process_create`, `process_clone`); it reaches no existing process. TRANSFER is what the
    grant chain needs to move it. Each native task is charged to the task account the engine
    carves, the same bound a personality-started task has.
  - **Open:** `ImageTask`'s ring path (`plane: true`) has not run in a boot; the NYC gate is
    its first. Decode-in-place is not wired: `ImageTask` copies each batch into the task's
    region once, counted, and the gate's time split shows it.
  - **Seen in P's boots, not P's:** DS-A `share` fails on 2 and 4 CPUs in r7 (25 ms and 47 ms
    against 20 ms; 1 CPU passes), and the 9 DS-A lines after it are then missing. A finds the
    cause; E checks whether the NYC numbers move with one vCPU left free. B1's `futex` ("the
    waker woke no one") failed once on 4 CPUs in r6 and passed in r7.
- **2026-11-17: the bar is every benchmark, then pruning** (Brackly, through the PM;
  `arrow-core-0.7.md` section 9). A NYC job passes only at 1.5x the best of every row, on wall
  and with CPU-seconds no worse than any row; a miss gets another round on the stage its time
  split names. **Stream U** (integrator's agent, its own branch) builds the untimed usage
  build. Kernel-mode coverage comes from OSTD's existing `coverage` feature (minicov, dumped at
  system exit, read by the host), and the services' from minicov too. Region counts on the
  dispatchers' arms are the per-call counts, so no hand-written counters go in. It is off by
  default, and the timed builds never carry it. U runs it on the gate's jobs after E's timed
  matrix and lists every zero-use call, OSTD part, server subsystem and driver as a removal
  candidate. Removal waits for the large datasets.
- **2026-11-17: stream A's wake queue merged** (`bdf24e1f`, from `12f1ce40`, with `da345685`
  and `fae6a8cc`), before the NYC gate's timed runs, since the stall it fixes is on the gate's
  path. DS-A `share` failed in P's boots on 2 and 4 CPUs (25 and 47 ms against 20). The cause
  is the queue order, not the stretched tick: an enqueue onto a stretched CPU already kicks it.
  A woken task joined the back of one first-in, first-out queue and waited out every slice
  ahead of it, up to 10 ms each, behind threads that had first run on that core and stayed.
  - **The fix:** a woken task waits in its CPU's wake queue, taken before the preempted and new
    tasks, at most four in a row while a preempted one waits. A wake waits at most the rest of
    the current slice plus a tick, and a thread that never sleeps still runs one turn in five.
    New test `DS-A wake`: a sleeper shares a core with three pinned spinners, is woken 20 times,
    and fails if a wake takes over 15 ms.
  - **Placement:** while some core has no pinned thread, unpinned threads (a reactor, a ticker,
    a helper) are placed on such cores, and one queued on a pinned worker's core moves.
  - Checked on the Mac: kernel clippy, the user programs' clippy and the host tests. Booted by
    the integrator under KVM on 1, 2 and 4 CPUs in its moruna-bench slot before the gate. A's
    own slot (after B2) boots it with the old order and placement off, for the with-and-without
    figures.
- **2026-11-17: A's wake queue reverted** (revert of `bdf24e1f`). Booted under KVM on
  moruna-bench, it failed its own test and did not fix `share`. `DS-A wake`: longest wake
  33 ms on 1 CPU (average 16 ms) and 10.0 s on 4 CPUs (average 532 ms). `DS-A share` was still
  28 ms on 2 CPUs. The NYC gate runs on the kernel of `5ffded0f` (P's, booted clean except
  `share`), built apart as `/data/boot-gate`. A refixes on its branch and boots it in its own
  slot before it is merged again.
  - **D's `moruna run` under KVM, end to end** (QEMU 10.2.1 from the repo's pinned source, with
    KVM and seccomp, and the launcher's `-sandbox` on; at `bdf24e1f`'s code): stream C's
    `disk://` job, Parquet to Parquet with the rows unchanged; KD3's image task to Arrow IPC with
    its rows checked; a failing task whose log records ride the update stream; and a run with
    its vCPUs placed. A tiny job's VM costs launch 0.6-0.7 s and boot 3.2-4.0 s; its total is
    8.7-14.0 s. `stage_s` is 0, since the inputs are local files.
  - Not run under KVM: the launcher's booted test (`data_disks_boot`) compiled to no test
    without D's `native-tcg-proof` feature. D ran it under TCG.
- **2026-11-17: imagefs's scratch store, the gate blocker E found** (integrator's agent,
  branch `worktree-agent-a18d4664dfd2c8f66`). Every NYC run with about 27 MB of Parquet
  output or more ended with `imagefs: panicked: memory allocation of 8192 bytes failed` and
  the node stopped.
  - **The cause, read from the code (not booted):** a /tmp file was 16 KiB chunks, one
    memory object and one handle each, in imagefs's one table of 4,096 handles shared by
    every connection. At the run's end the engine's scratch holds the output three times:
    the sinks' files under `/tmp/out`, the layer writer's spill (`.data-part`) and the
    layer (`moruna_image::layerfile::write`). For 27 MB that is about 3 x 1,648 chunks,
    over the table. The layer is copied through a `BufReader` and a `BufWriter`, 8 KiB a
    call: a write's growth took the table's last handle, and the next read's 8 KiB reply
    (`Scratch::read`'s buffer, a heap block over 2 KiB, so its own memory object) found no
    handle, and `alloc` aborted. The outputs that passed (12.9 MB, about 2,400 chunks)
    stayed under it; the line between is near 21 MB.
  - **The store** (`moruna_imagefs::chunks`, host-tested; `imagefs/server/src/pages.rs`
    is its kernel backing). A file's chunks grow with it: the first is what the first
    growth needs (at least a page), each later one the largest of 16 KiB, an eighth of
    the file and the growth, at most 16 MiB, and never more than the account has room
    for, so a file fills its account to the last growth that fits.
    - Waste past a file's end: under `max(16 KiB, size / 8 + 4 KiB)`, and under 16 MiB,
      besides the kernel's record (256 bytes a chunk, 16 a page). A small file costs a
      page, not 16 KiB.
    - Handles: 27 MB is 48 chunks, 4 GiB 309, 16 GiB 1,077 (in 32 KiB writes). The
      chunks of all scratch directories share a budget of 3,072 handles (one file of
      47 GiB); imagefs keeps 1,024 for itself, a bound checked at compile time against
      its own holders (start slots, layer devices, connections and their accounts, kept
      mappings, a message's handles each way, the heap's next block).
  - **No panic on exhaustion:** a growth is counted against the budget and the account's
    room before any chunk is made, and is `NoSpace` to the client when either is short;
    `QuotaExceeded`, `Limit` and `NoMemory` from the kernel are `NoSpace` too. A default
    account (no `scratch_account`) is carved only while imagefs keeps 16 MiB of its own
    account for its heap and messages.
  - **The copy path:** a read or a write mapped every 16 KiB chunk it touched, whole,
    and copied a byte a call (reads) or a word a call (writes): per MiB of 32 KiB
    requests, 64 to 96 map and unmap pairs, and 1,048,576 single-byte loads or 131,072
    word stores, each through a bounds check. Now a call maps only the pages it touches,
    usually in one chunk, and copies them with one `memcpy` (the chunks are imagefs's
    alone: no client maps a scratch file): 32 pairs per MiB. Counted, not timed.
  - Tests: 8 host tests in `chunks` (offsets across chunk boundaries, resizes up and down,
    zeros after a cut, the budget's and the account's refusals with nothing made, a failed
    chunk undone, a scratch directory on chunks); `chunks.rs` 99.4% of lines,
    `moruna-imagefs` 98.4%. imagefs's clippy and release build on the user target.
  - **For the integrator's boot:** a NYC run with 27 MB or more of output completes, and
    imagefs prints no panic. imagefs's boot test (`imagefs-test`'s scratch quota of
    256 KiB) still refuses within 32 KiB of its limit.
  - **Open, for E:** the output layer needs about three times the output in the engine's
    scratch account (plus up to an eighth for growth), so 1 GiB holds outputs to about
    300 MB. Streaming the layer onto `disk.out` without the spill and the layer file is
    the engine's change.
- **2026-11-17: stream E's gate harness merged** (from `f444b0d6`). Its native tasks run on
  P's rings, with answers in parts. The gate image is FROM scratch with the engine in it
  (55 MB; the engine is 49.8 MB stripped). It also brings the setup-time and clean-install
  benchmarks, and a block0 stack attached without the kit's fixture disks (launch 0.26 s to
  0.14 s).
  - **Integrator's call, for the gate:** a native stage may run several instances, each a
    child carving its own account inside the stage's share (`native_account::native_children`,
    with a host test). C reviews it.
  - **Checks:** `moruna-runtime` clippy is clean and its tests pass, apart from one.
    `test_configure.py` and the gate harness's tests pass. The one failure is
    `registration_and_live_keys_preserve_distinct_actual_package_arguments`
    (`tests/package.rs:548`, its warm-up observed nothing in 250 ms). It is not E's: it failed 3
    of 3 on `764bfe7a`, before the merge, and 2 of 3 after, on a Mac short of disk with the uv
    cache just emptied. It is left open, to be run again on a quiet machine.
- **2026-11-17: the benchmark moves to Central India** (Brackly). The rows must come from one
  machine. The German moruna-bench's 51 control runs are discarded, and its THP A/B never ran.
  The new machine is the same: Standard_D8ds_v6, Ubuntu 24.04.202610250, host kernel
  7.0.0-1017-azure, nested KVM on Azure, in resource group moruna-bench-20261117in, zone 2.
  Its tree is public main (`d4165127`) plus the integrator's patch, sent through run-command
  in pieces checked by SHA-256 (no SSH). 0.6.6's tree is `8018c96a`. QEMU is rebuilt there from
  the pinned 10.2.1 source.
- **2026-11-17: C's arena on growth and P's three gate fixes merged** (`c363d2ef`, from
  `ec9d67b3` and `c2a6d0cc`). Checked on the Mac: clippy is clean and the tests of
  moruna-arena, moruna-scheduler, moruna-micro and moruna-kernel pass. On the India VM they are
  in the gate's tree before any Moruna row is timed.
  - **C:** a granted arena commits as the run uses it: a 32 MiB first region, then regions as
    large as all made so far, up to the backing's largest. The budget is still counted in full
    for admission. Before the fix the guest committed the whole budget before the first row,
    at 2.2 s per GiB.
  - **P, the ~40 s tail:** it was not the log drain. Each native task's cleanup waited the full
    5 s for its account to empty while its process handle was still held, and then marked the
    instance "cleanup uncertain". With 8 tasks that came to about 40 s. The handle is now
    released first.
  - **P, Kernel(Limit):** the kernel makes no memory object over 256 MiB, and a task's rings
    object (two output windows of 128 MiB plus the ring pages) went just over it. The windows
    are now capped, with a compile-time check against the kernel's limit.
  - **P, the feeding:** one worker keeps every instance of a native stage busy, two batches in
    flight per task, dealt across the instances.
  - **Process lesson:** both fixes sat finished for over an hour, because the streams' reports
    stayed in their own transcripts. The integrator now checks asked-for branches itself, and
    the streams SendMessage their results.
- **2026-11-17: the output layer goes straight onto disk.out** (integrator, on C's plan).
  Today a run's output is copied four times after the last row and held three times in scratch,
  which caps it at about a third of the scratch account. Instead, sinks write
  `disk://out/<dir>/` objects by DMA from their arena buffers, and the finish writes only the
  metadata, the hash tree and, last, the header. The layer format and the host's unpacker do not
  change, and a failed run leaves no header. A 16 MiB metadata reserve holds about 150,000 files.
  C owns it, including D's two lines (the in-VM sink URL and disk.out's size), marked as hooks
  in D's files since D's agent is finished.
- **2026-11-17: the NYC gate's core controls, India VM** (10:05-10:48 UTC; 106 runs, each
  row 5 of 5, no OOM; every output equal to 0.6.6's under gate-canon/1). Medians:

  | Job | Rust pool | 0.6.6, Rust fn | 0.6.6, Python fn | DuckDB / Polars | 0.7 must reach |
  | --- | --- | --- | --- | --- | --- |
  | Score | 14.2 s, 99.5 CPU-s | 14.6 s | 27.4 s | none | 9.5 s |
  | Occupancy | 2.4 s, 8.7 | 3.0 s | 8.0 s | 7.0 s | 1.6 s |
  | Enrich | 6.6 s, 39.6 | 7.8 s | 7.4 s | 12.8 s / 9.4 s | 4.4 s |
  | Audit | 3.0 s, 12.7 | 3.4 s | 28.0 s | none | 2.0 s |

  - **Out of reach with boot counted:** 0.7's launch plus boot is 2.65 s, so Occupancy and
    Audit cannot pass with boot counted in the wall. Score's function alone is about 11 s on
    8 threads, the same code in every Rust row. Only Enrich is reachable: 1.7 s for the job
    after boot, where the pool spends 30.4 of its 39.6 CPU-s on Parquet encode and write. The
    column the bar is judged on (with boot or without) is Brackly's call. Every Moruna row is
    reported both ways, and cutting boot is work regardless (fast deployment).
  - **Shmem THP `advise`** changed nothing: QEMU does not madvise its memfd, so
    ShmemHugePages stayed 0. A `within_size` run follows.
- **2026-11-17: P's placement, account cap and plane sizing under KVM** (`c06344c2`, E's
  one-month Score smoke, 8 processes).
  - **At 20.5 GiB:** completes, total 21.1 s (launch 0.14, boot 2.52, job 17.7). The tasks ran
    one per CPU. After the first 2 s the run does about 1.4M rows/s, against 0.35M before (the
    pool: 2.7M). First rows came at 11.1 s, against 24.1 s. The commit step halved, to 7.25 GiB
    at about 10 s, and is still there; it is no longer the task accounts (C).
  - **At 2 GiB:** refused. A native stage's footprint, eight 112 MiB planes plus a morsel, is
    981 MB against its 625 MB budget. This is new with the plane sizing (P).
  - The month went out as 34 batches of 4 MiB morsels, and two tasks got none (P).
  - **Shmem THP, `within_size` against `never`:** no huge pages (ShmemHugePages 0) and no
    change in the first row. The cost before the first row is the guest's own commit, not the
    host's page size. Host huge pages (a hugetlb memfd with reserved pages) are left aside.
