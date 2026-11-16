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
  their helpers). The VM has no network, and `kernel-is-the-machine.md` §4.3 makes the
  socket layer a client of `moruna.net` when it comes.
- **Other architectures:** `core/src/arch/{arm,riscv,loongarch}`. Moruna runs on x86-64.
- **Drivers and the cloud platform:** Asterinas's components that drive hardware
  (`virtio`, `nvme`, `pci`, `network`, `input`, `framebuffer`, `mlsdisk`) and the libraries
  only they use (`aster-bigtcp`, `aster-fuse`, `fdt-util`, `logo-ascii-art`). Moruna's
  drivers stay processes.
- **The RTC drivers** of `comps/time` (`rtc/{cmos,goldfish,kvmclock,loongson,pl031}.rs`).
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
  - once the core resumes a fault in the copy routines at their recovery point
    (B1's `personality_set`), copies run in place; until then each goes through
    `process_read` or `process_write` on the personality's own process, which answer
    `Fault` safely;
  - a program's pointer is accepted only below `MAX_USERSPACE_VADDR`, so a program can never
    make the personality copy into its own region.
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
changed with them.

| File | Why |
| --- | --- |
| `comps/softirq/src/taskless.rs` | Its per-CPU `RefCell` lists become locks: threads share CPU 0's state. |
| `comps/time/src/tsc.rs` | The clock source is the core's monotonic clock, not the TSC. |
| `comps/time/src/rtc/mod.rs` | One "driver" reading the wall time the start gives. |
| `comps/time/Cargo.toml` | No RTC drivers' dependencies. |

## What is next: the parts that need B1

The program's own entry, clones, the pager and thread state are stream B1's mechanisms
(`kernel-is-the-machine.md` §4.1). In `ostd` they sit behind one seam each:

- **Running the program** (`ostd::user::set_program_driver`): `UserMode::execute` hands the
  context to the program and returns with the next entry's frame, in ABI 2.6's layout
  (`ostd::arch::cpu::context::frame`, `moruna_abi::personality`).
- **The fault-tolerant copy's recovery point** (`ostd::backend::set_user_copy`,
  `user_copy_range`).

What builds on them is the rewrite of `core/src/{thread,vm,process}` and the start.

`core/` is carried but not yet a workspace member: making it build against `ostd` comes
next, subsystem by subsystem.

## Building and testing

The tree is its own Cargo workspace on the kernel's toolchain, so its carried files keep
Asterinas's lints and formatting. Asterinas's kernel tests (`#[ktest]`) run as host tests
with `--cfg ktest`:

```sh
cd kernel/linuxlayer
RUSTFLAGS="--cfg ktest" cargo test --target x86_64-apple-darwin --workspace   # a Mac, under Rosetta
RUSTFLAGS="--cfg ktest" cargo test --target x86_64-unknown-linux-gnu --workspace
```

For the Moruna user target, as `kernel/tools/build.sh` builds programs:

```sh
RUSTFLAGS="-C relocation-model=static -C relro-level=off -C force-unwind-tables=yes --check-cfg cfg(ktest) -C no-redzone=y" \
  cargo check -Zunstable-options -Zjson-target-spec --target ../targets/x86_64-moruna-user.json \
  -Zbuild-std=core,alloc,compiler_builtins -Zbuild-std-features=compiler-builtins-mem --workspace
```
