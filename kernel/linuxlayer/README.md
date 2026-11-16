# Asterinas's Linux layer, in user space

Moruna's Linux personality for the data OS (`architecture/data-os.md` §4,
`kernel-is-the-machine.md` §4.2): Asterinas's Linux layer, carried out of kernel mode and
run as user code in the upper region of each task's own process. Stream B2 of
`architecture/data-os-streams.md`. It replaces `kernel/linux` when it runs the gate's images.

## Source and licence

- **Carried from** asterinas/asterinas `a0de2258c56b03ba1a6aa417a05f10bdc0f54ff4`
  (main of 2026-11-13, version 0.19.0, the OSTD Moruna's core runs on).
- **Licence:** MPL-2.0, file by file, as DECISIONS.md MK1 accepted for OSTD. Every carried
  file keeps its `SPDX-License-Identifier: MPL-2.0` header and stays MPL-2.0. The new files
  of this tree (the user-space `ostd`, `libs/component`, `libs/osdk-heap-allocator`) carry
  OSTD's API and code and are MPL-2.0 too. `LICENSE-MPL` is Asterinas's copy of the licence.
  The source is this tree; upstream is the commit above.
- **A later Asterinas change** is carried by hand, file by file.

| Here | Upstream | Lines |
| --- | --- | --- |
| `core/src` | `kernel/core/src` | 118,056 |
| `libs/*` (15 crates) | `kernel/libs/*` | 8,370 |
| `comps/*` (8 crates) | `kernel/core/comps/*` | 11,457 |
| `ostd` | OSTD's API, part carried from `ostd/src` | 9,811 |

## What is left out, for 0.7.0

- **Sockets and the network:** `core/src/net/` and `core/src/util/net/` (15,967 lines and
  their helpers), but for `net/uts_ns.rs` (the host and domain names, which `uname`
  needs). The VM has no network, and `kernel-is-the-machine.md` §4.3 makes the socket
  layer a client of `moruna.net` when it comes. The 16 socket calls answer from
  `core/src/syscall/no_socket.rs`: `EAFNOSUPPORT` to make a socket, `EBADF` or `ENOTSOCK`
  on a descriptor.
- **Hardware the personality never sees:** TDX (`security/tsm*`,
  `device/misc/tdxguest.rs`), the virtio RNG, the hypervisor console, virtual terminals,
  input events, the framebuffer, virtiofs, the input-driver listing, and the vDSO (a
  program without `AT_SYSINFO_EHDR` makes the calls instead).
- **Other architectures:** `core/src/arch/{arm,riscv,loongarch}`. Moruna runs on x86-64.
- **Drivers and the cloud platform:** Asterinas's components that drive hardware
  (`virtio`, `nvme`, `pci`, `network`, `input`, `framebuffer`, `mlsdisk`) and the libraries
  only they use (`aster-bigtcp`, `aster-fuse`, `fdt-util`, `logo-ascii-art`). Moruna's
  drivers stay processes.
- **The RTC drivers** of `comps/time` (`rtc/{cmos,goldfish,kvmclock,loongson,pl031}.rs`).
- **What Talos's posture leaves out** (`architecture/data-os.md` §3a):
  - a shell, and the default search for a first program: the personality runs the one
    program a task's command names on its image, or a booted test's `/init`;
  - administrative calls (see "Refused calls");
  - terminals and job control (`device/tty`, a session's controlling terminal), `devpts`
    and ptys, overlayfs, configfs and the systree's `/sys`; `/proc` and `/sys` are cut to
    what a task's programs read, and the process code only the cut files read went with
    them (overflow uid and gid, the UTS names' readers, start time, the init stack's
    reader);
  - mounting: the mount family's handlers and the fs code only they reached (the
    filesystem types' mount-by-name path, mount propagation's slave and unbindable
    states), since `refused.rs` answers those calls;
  - block filesystems as a root: ext2 and exfat stay compiled but unreachable until the
    page cache's write-back leaves `vm/`.
- **`libs/short-vis-path/tests`,** which run the external `cargo expand`.

## The mechanical swaps

Asterinas's Linux layer is written against OSTD. Here it builds against `ostd/`, a crate
with OSTD's API for a user process, imported under OSTD's name, so the carried files keep
their `use ostd::...` lines. The swaps of `kernel-is-the-machine.md` §4.2 are in it:

- **Locks and wait queues on the core's futex** (`ostd/src/sync`):
  - `SpinLock` is a futex lock that spins briefly, then sleeps (`sync/raw.rs`): a user
    process cannot disable preemption, so a holder may be descheduled;
  - `RwLock` sleeps on a futex word when contended (`raw::Parking`);
  - `Waiter` and `Waker` sleep on a futex word (`sync/wait.rs`), so `WaitQueue`, `Mutex` and
    `RwMutex`, built on them, carry unchanged;
  - RCU's grace periods are reader epochs (`sync/rcu/monitor.rs`): an atomic-mode guard is a
    read-side section, and callbacks run on a reclaimer thread that holds no lock.
- **The program's memory through a fault-tolerant copy** (`ostd/src/mm/io/copy.rs`,
  `ostd/src/backend/moruna.rs`):
  - `VmReader` and `VmWriter` keep OSTD's code; their fallible side is the program's memory;
  - copies run in place: the process is given the copy routines with B1's
    `personality_set_with_copy` (`ostd::backend::personality_set`), and the core resumes a
    fault in them at their recovery point; a process given none copies through
    `process_read` and `process_write` on itself (`copy_through_core`);
  - a program's pointer is accepted only below `MAX_USERSPACE_VADDR`, the base of the
    personality's region (`IMAGE_RANGE.end`), so a program can never make the personality
    copy into its own region.
- **Tasks are threads** (`ostd/src/task`): the core schedules, so `Task::run` starts a thread,
  and `Task::current` reads a slot at the base of the thread's aligned stack (the program
  owns FS). Atomic-mode guards are RCU read sections.
- **Time** is the core's monotonic clock; jiffies are read off it, and tick callbacks and
  bottom halves run on a ticker thread (`ostd/src/timer`). The wall time at start comes from
  the personality's start (`ostd::boot::BootInfo`), as the engine reads its own (C8).
- **Frames** are pages of the personality's memory with OSTD's per-page counts and metadata
  (`ostd/src/mm/frame.rs`); DMA buffers are plain memory (`mm/dma.rs`).
- **Per-CPU state:** a thread does not know its CPU, so every thread is on CPU 0 for per-CPU
  state; `cpu_local!` values must be `Sync` and `cpu_local_cell!` cells are atomics.
- **Components** register with `#[init_component]` as in Asterinas; `component::init_all`
  runs a stage's (`libs/component`).

### Carried files changed

Moruna's text rule (no em dashes) covers carried files too: the 108 em dashes in
Asterinas's comments became colons after a list item's term and commas elsewhere. No code
changed with them. Beyond that, and the pruning above:

| File | Why |
| --- | --- |
| `comps/softirq/src/taskless.rs` | Its per-CPU `RefCell` lists become locks: threads share CPU 0's state. |
| `comps/time/src/tsc.rs`, `rtc/mod.rs`, `Cargo.toml` | The clock source is the core's monotonic clock; one "RTC" reads the wall time the start gives. |
| `core/src/init.rs` | No other CPU, driver listing, network or vDSO to start; the first program is the task's named one on its image (or a booted test's `/init`), with no default search and no shell, and a missing one ends the personality; the personality ends with its init process. |
| `core/src/arch/x86/power.rs` | No ACPI reset port: powering off ends the process. |
| `core/src/device/registry/block.rs` | No virtio or NVMe request threads. |
| `core/src/process/signal/poll.rs` | A polling task is marked by its address's complement: user addresses are positive. |
| `core/src/syscall/mod.rs`, `syscall/clone.rs` | Every call passes `syscall/refused.rs` first; a call the table lacks is counted; `clone` with a new namespace is refused. |
| `core/src/process/exit.rs`, `core/src/init.rs` | A process's refused calls are reported when it ends, the task's when the personality ends. |
| `core/src/process/posix_thread/mod.rs` | Waking a thread for a signal also interrupts its task (`Task::interrupt`): a thread running the program comes back only when the core interrupts it. |
| `core/src/fs/file/file_handle.rs`, `syscall/{mod.rs,arch/x86.rs}` | No socket hooks; the socket calls answer from `syscall/no_socket.rs`. |
| `core/src/vm/vmar/vmar_impls/fork.rs` | Its device-memory test is ignored: there is no device memory. |
| `core/src/fs/` | Stream B2-fs: the root is the image through the image filesystem (`fs_impls/imagefs`, `rootfs.rs`), with the task's scratch (a tmpfs under a quota), the cut `/proc` and `/sys`, and `/dev` over it; descriptors 0 to 2 are the task's streams (`task_streams.rs`). Their ports reach the start on the target (`fs/mod.rs`, `imagefs/mod.rs`). |
| `core/src/lib.rs` | The module list (with `start`), three unused features and `getset`, and the clocks every host test needs (`ostd::ktest_setup!`). |
| 9 files | Items only the left-out parts used are marked `expect(dead_code)` with that reason. |

New in `core/`: `net/mod.rs` (the UTS namespace only), `vdso.rs` (none mapped),
`syscall/no_socket.rs`, `syscall/refused.rs`, and `start/`, a task's start: the image
filesystem's client, the plane's slots (ported from `kernel/linux/server/src/plane.rs`) and
the log ring, as the ports `fs/` is built on (`start::start_task`).

## Refused calls

The data OS takes Talos's posture (`architecture/data-os.md` §3a): no shell, and the layer
keeps what a job's programs need (processes, threads, files, pipes, signals, epoll, `fork`
and `execve`) and refuses administrative calls. `syscall/refused.rs` holds the list:

- **`EPERM`, before the handler runs**, as Linux answers a process without the privilege:
  - mounts: `mount`, `umount2`, and the new mount API (`open_tree`, `move_mount`, `fsopen`,
    `fsconfig`, `fsmount`, `fspick`, `mount_setattr`);
  - `pivot_root`, `chroot`;
  - namespaces: `unshare`, `setns`, and `clone` or `clone3` with a `CLONE_NEW*` flag;
  - `ptrace`, and the calls that need its permission: `process_vm_readv`,
    `process_vm_writev`, `pidfd_getfd`;
  - modules: `init_module`, `finit_module`, `delete_module`;
  - `kexec_load`, `kexec_file_load`, `reboot`, `swapon`, `swapoff`;
  - names: `sethostname`, `setdomainname`;
  - time: `settimeofday`, `clock_settime`, `adjtimex`, `clock_adjtime`.
- **`ENOSYS`:** every number the table does not have.
- **The socket calls** answer from `syscall/no_socket.rs`.

Each refusal is counted by number for the process that made it, as `kernel/linux` counts
its refusals: `linux: process PID refused: mount(165) x1, ...` when the process ends (if it
had any), and `linux: the task refused: ...` when the personality ends. The refused
handlers' carried code stays in the tree, unreachable: removing it would leave parts of
`fs/` unused.

## Launching the personality

- **The contract** (`launch/`): the personality's region is `[IMAGE_RANGE.end,
  + 0x6000_0000_0000)`, above every address a Linux program links at. Its image is linked at
  `0x5000_0000_0000` inside the region and opens with a table of three `u64`s: the copy
  routines' start and end, and their recovery point. The first thread's stack is 256 KiB,
  aligned to its size, past the image; the start page's address is at its stack pointer.
- **The launcher** (`launch/src/launcher.rs`) creates the process, maps each segment as an
  object of its own, maps the first stack, grants the handles it is given plus the
  process's own as `self`, gives the process its region with the copy routines
  (`personality_set_with_copy`) and launches it. `launch_task` gives a task's process its
  data plane first. `linuxlayer-boot` launches it with the slots `console`, `initramfs`
  and, if given, `cmdline`.
- **A task's slots** (`linuxlayer_launch::slot`): `filesystem`, its own connection to the
  image filesystem, which becomes the root; `scratch`, the account whose limit `/tmp`
  holds; `input` and `output`, the plane's slots to wait on; `log`, the engine's log ring;
  and `cmdline`, `init=PROGRAM -- ARGS`. Without `filesystem` the personality boots on its
  initramfs.
- **The image** (`personality/`) is built from position-independent code
  (`-C relocation-model=pie`, so it reaches its addresses) and linked at its address
  (`personality.ld`). Its start records the start page and the first thread's block, runs
  the constructors `inventory` registers with, reads its slots, gives the layer its boot
  information (the command line, the initramfs, a 1 GiB memory budget, the clocks) and runs
  the layer's boot in a task, with the core's console as the layer's serial console. The
  layer ends the process when its init process ends.
- **The booted test** (`boot/boot.sh`) builds the kernel, the image and `linuxlayer-boot`, an
  initramfs whose `/init` is a static musl program, renders `boot/linuxlayer.image` and
  boots it under QEMU, then checks the lines it expects.

## A task's processes: shared memory and `fork`

One Linux process is one Moruna process, and the task's personality is one instance whose
state every process reaches at the same addresses (`architecture/data-os.md` §4, the
integrator's decision of 2026-11-16):

- the task's processes share one handle table (B1's `process_clone` with
  `process::SHARE_HANDLES`), so every handle value means the same in each;
- memory the personality hands out comes from objects mapped at an address it picks in
  `linuxlayer_launch::SHARED`, in every process of the task, before any of it is used
  (`ostd::backend::shared_map`, `backend/shared.rs`); one lock covers the addresses, the
  objects and the processes, and `clone_process` holds it across a `process_clone`;
- which process a thread is in goes in its block.

`fork` itself waits for B1's `SHARE_HANDLES`; the personality's heap moves onto the shared
memory once `moruna-sys`'s heap takes its chunks from it.

## Running the program: B1's entry

The program's own entry, clones, the pager and thread state are stream B1's mechanisms
(`kernel-is-the-machine.md` §4.1, `moruna_sys::personality`):

- **Running the program:** `UserMode::execute` hands the context to the program with
  `personality_return` and returns with the next entry's frame (`backend::run_program`).
  The core switches general registers, RIP, RFLAGS and FS base only, so the entry loads the
  program's vector state (XRSTOR) just before the call and saves it (XSAVE) just after, and
  OSTD's `FpuContext::save` and `load` read and write that saved state.
- **Each thread's block** sits at the base of its aligned stack: its task, the program's
  frame (inside the region, as the core requires) and the program's vector state.
- **Interruption:** a signal for a thread that runs the program interrupts it
  (`Task::interrupt`, B1's `thread_interrupt`), so it enters the personality and takes the
  signal, as a kernel's timer interrupt would bring it back. Each thread's block holds its
  own handle for that, written by its spawner before the thread runs anything.
- A driver installed with `ostd::user::set_program_driver` takes the entry's place: a
  scripted program in host tests.

## The program's address space

`vm/` drives OSTD's `VmSpace` through its cursors. Here `ostd`'s `VmSpace` is the
personality's record of the program's pages, with OSTD's cursor API, so `vm/` carries
unchanged, Asterinas's own copy-on-write `fork` included. Making the program see those pages
(the core's `process_map`, `process_unmap` and `process_protect` of the frames' memory
objects, with B1's clone and pager where they measure better) is stream B2-vm's. In host
tests a thread's activated space backs program addresses below 4 GiB, so `vm/`'s faults,
mappings and copies run on the host.

A copy of program memory the personality has not mapped yet asks the layer's fault handler
(`ostd::mm::fault`) to map it and goes on, as OSTD's fault path does.

## The gVisor system-call tests

`test/gvisor` runs gVisor's system-call tests (release-20260622.0, the build Asterinas
a0de2258 runs) in a booted personality:

- `applies` lists the 66 tests that apply: Asterinas's 83, less sockets and the network,
  mounting, terminals, `ptrace`, namespaces and the vDSO, which 0.7.0 leaves out;
- `blocklists/` is Asterinas's (`test/initramfs/src/conformance/gvisor/blocklists`,
  MPL-2.0), one file a test, for the tests that apply;
- `runner/` takes the place of Asterinas's `run_gvisor_test.sh`, since there is no shell.
  It is the first program (`init=/gvisor/runner -- /gvisor`): it runs each test with its
  blocklist as the gtest filter in an empty `/tmp`, reaps what the test leaves behind, and
  prints `gvisor: ok: NAME` or `gvisor: FAIL: NAME ...`, then a count. It is a Linux
  program (static musl), outside the workspace; its host tests use stand-in test
  binaries.

The binaries are built from gVisor's source at that tag (`bazel build
--test_tag_filters=native //test/syscalls/...`) and run on dynamic glibc. Which cases
fail here, beyond Asterinas's blocklists, is the judgement Brackly's rule asks for.

## How the tree is judged

Brackly's rule (data-os-streams.md, 2026-11-16): files carried from Asterinas are judged by
Asterinas's own tests and the gVisor system-call suite, which pass and which fail. Code
Moruna writes or rewrites around them (`ostd/`, `libs/component`, the OSDK stand-ins, and
rewritten files such as the start, the entry loop and the `vm/` adapters) keeps the 80%
per-crate line floor by host tests.

## Building and testing

The tree is its own Cargo workspace on the kernel's toolchain, so its carried files keep
Asterinas's lints and formatting. Asterinas's kernel tests (`#[ktest]`) run as host tests
with `--cfg ktest`, one at a time as the kernel runs them (some share global registries),
each in a task of its own, after the setups registered with `ostd::ktest_setup!`:

```sh
cd kernel/linuxlayer
RUSTFLAGS="--cfg ktest" cargo test --target x86_64-apple-darwin --workspace -- --test-threads=1   # a Mac, under Rosetta
RUSTFLAGS="--cfg ktest" cargo test --target x86_64-unknown-linux-gnu --workspace -- --test-threads=1
```

The booted test, on Linux x86-64 with QEMU and musl (KVM when usable):

```sh
kernel/linuxlayer/boot/boot.sh [OUT] [TIMEOUT_SECONDS] [INIT_C] [CMDLINE]
```

For the Moruna user target, as `kernel/tools/build.sh` builds programs:

```sh
RUSTFLAGS="-C relocation-model=static -C relro-level=off -C force-unwind-tables=yes --check-cfg cfg(ktest) -C no-redzone=y" \
  cargo check -Zunstable-options -Zjson-target-spec --target ../targets/x86_64-moruna-user.json \
  -Zbuild-std=core,alloc,compiler_builtins -Zbuild-std-features=compiler-builtins-mem --workspace
```
