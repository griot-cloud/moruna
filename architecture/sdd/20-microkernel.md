# Moruna SDD 20: The microkernel core (`kernel/`)

**Document type:** software design document, component 20
**Status:** DRAFT · 2026-10-28 (becomes HANDOFF-READY when section m is empty; the human flips it)
**Parent:** `BOARD.md` section EK, item KB1 (Brackly, 2026-10-28); `architecture/optimized-runtime.md` section 3, as the EK direction amends it: the kernel is a microkernel, and Linux compatibility is a server (KC1), not kernel syscalls
**Decisions:** MK1 (`DECISIONS.md`: OSTD as the hardware layer, MPL-2.0)
**Component location:** `kernel/` (its own Cargo workspace): `abi/` (`moruna-abi`), `loader/` (`moruna-loader`), `handles/` (`moruna-handles`), `core/` (`moruna-microkernel`), `boot/` (`moruna-microkernel-boot`, the kernel image), `sys/` (`moruna-sys`), `programs/` (the KB1 test programs), `initramfs/manifest`, `tools/` (`build.sh`, `boot.sh`, `check-boot.sh`)
**Consumes:** OSTD 0.19.0 (boot, page tables, the frame allocator, interrupts, the timer, task switching, user mode entry), OSDK 0.19.0's frame and heap allocators and its x86_64 linker script. **Consumed by:** KB2 (`moruna-svc`, the SDK, which is built on `moruna-sys` and `moruna-abi`), KB3 (drivers as processes), KC1 (the Linux personality server), KD1 (the data plane on kernel channels), KE (the kernel as a node).

**Decisions worth your eye:** (1) the kernel holds nine object types and 24 calls, and nothing else: no files, no names, no network, no Linux syscall; (2) a handle value is salted per process, so a value from another process names nothing here, and every call looks its handles up before it checks anything else; (3) the boot manifest is a line-oriented text file in the initramfs, so adding a server is a line and an ELF, and nothing in the kernel is rebuilt; (4) only the first process, the supervisor, may receive the three root resources (process creation, device memory, interrupts), and it delegates them by duplicating and sending handles; (5) the kernel builds with plain cargo: the crate `cargo osdk` would generate is written out in `boot/`.

---

## a. Purpose and boundary

Moruna 1.0 runs a task as a process on its own kernel (BOARD EK). This component is that kernel's core: the smallest set of mechanisms every other part is built from. It owns: the kernel objects and their lifetimes; handles, rights and the per-process handle table; the native syscall ABI and its version; boot from an initramfs and its manifest; static ELF loading; the debug console as a handle.

It refuses to know: drivers (KB3), filesystems and images (KC2), the Linux ABI (KC1), the network (KE2), the engine and the data plane's record check (KD1), and any embedder. Each of those is a process speaking a published protocol over channels. The test of every object and call here is extensibility: a third party adds a driver or a server by writing a process against the SDK and listing it in the boot manifest; nothing in the kernel is rebuilt or forked.

OSTD is the hardware layer: all the unsafe code (boot, paging, interrupts, the timer, context switches, user mode entry and the fallible user copies) is OSTD's. `moruna-microkernel` is `#![forbid(unsafe_code)]`. The only unsafe in the kernel image outside OSTD is in `boot/src/main.rs`: three exported symbols OSTD looks up by name and the call to OSTD's panic handler, the same code OSDK generates for every OSTD kernel.

## b. Vocabulary

**Object.** A kernel entity of one of the nine types in e.1.

**Handle.** A process's reference to an object, with rights (e.2). A process names objects only by the values of the handles in its own table.

**Rights.** What a handle lets its holder do (e.2). Rights never grow.

**Signals.** Bits an object raises (e.3); `wait` sleeps until one of them is raised.

**Resource.** A root capability: the right to create objects of one kind from nothing (processes, device memory, interrupt lines).

**Manifest.** The boot manifest (e.4): the processes the kernel starts and the handles each starts with.

**Supervisor.** The first process in the manifest, the only one that may receive resources.

**Start page.** One read-only page the kernel maps into each process before its first thread runs, listing the handles it starts with by slot name (d.4).

## c. Invariants

- **KK-I1.** A process reaches an object only through a handle in its own table. No call takes a process identifier, an address in another process, or a global name.
- **KK-I2.** Every call that takes a handle looks it up in the caller's table before it checks any other argument, so a value that names nothing there returns `BadHandle`, whatever the other arguments are.
- **KK-I3.** A handle value never names an object in another process's table: values are salted per process and carry a generation, so a value learned from another process, or one already closed, is `BadHandle` (f.3).
- **KK-I4.** A thread runs, and the kernel copies to and from user memory, only with the thread's own process's address space active: OSTD's post-schedule hook activates it after every switch, and OSTD's user copies refuse an address space that is not the active one.
- **KK-I5.** Rights never grow: `handle_duplicate` takes a subset of the source's rights, a transfer carries the sender's rights exactly, and `memory_map` maps writable only with `WRITE` and never writable and executable at once.
- **KK-I6.** A transferred handle is removed from the sender's table in the same critical section that queues it; a write that fails moves nothing. A channel's own endpoints never travel through it.
- **KK-I7.** Physical memory and interrupt lines are reached only through objects made from a resource. A device memory region is never RAM nor anything OSTD uses itself; an interrupt object is a line no one else has routed.
- **KK-I8.** Every queue and buffer is bounded: message bytes, handles per message, queue depth, handles per process, wait items, memory object size, image size, debug writes (d.5). Kernel buffers sized by a process are reserved fallibly, so an over-large request is `Limit` or `NoMemory`, never a kernel allocation failure.
- **KK-I9.** The start page is mapped read-only before the first thread runs and stays mapped until the process has ended.
- **KK-I10.** A fault in a process ends that process and nothing else: its threads stop, its handles close, its mappings go, `TERMINATED` is raised on it, and the kernel names the fault on the console.
- **KK-I11.** Only the first process of the manifest may receive a resource; the kernel refuses a manifest that gives one to any other, and refuses to boot from a manifest it cannot parse or an image it cannot load, powering off with the reason.
- **KK-I12.** When the last process ends, the kernel powers off: success if the supervisor exited with code 0, failure otherwise.

## d. Interfaces

### d.1 The calling convention

`syscall` with the number in `rax` and up to six arguments in `rdi`, `rsi`, `rdx`, `r10`, `r8`, `r9`. The kernel returns a value in `rax` (and, where the table says, a second one in `rdx`), or an error as `-(code)` in `rax` (e.5). `rcx` and `r11` are clobbered by the instruction; every other register is preserved. Lengths and offsets of mappings are whole pages (4096 bytes). Deadlines are absolute nanoseconds on `clock_get`'s clock; `0` polls and `u64::MAX` never passes.

A thread starts with `rip` at its entry, `rdi` its argument, and `rsp` 8 below a 16-byte boundary at the top of its stack, as if called. A process's first thread receives the start page's address in `rdi` and a 256 KiB stack the kernel maps.

### d.2 The syscalls, ABI 1.0

`abi_version` returns `MAJOR << 16 | MINOR`. The major number changes when a call's meaning changes; the minor number when a call is added. A number is never reused. Any number at or above 24 is `BadSyscall`.

| nr | call | arguments | returns | rights needed |
|---|---|---|---|---|
| 0 | `abi_version` | | version | |
| 1 | `handle_close` | handle | 0 | |
| 2 | `handle_duplicate` | handle, rights | new handle | `DUPLICATE`; rights a subset of the handle's |
| 3 | `handle_info` | handle | `type << 32 \| rights` | |
| 4 | `channel_create` | | endpoint 0 (`rax`), endpoint 1 (`rdx`) | |
| 5 | `channel_write` | endpoint, bytes, nbytes, handles, nhandles | 0 | `WRITE`; `TRANSFER` on each handle sent |
| 6 | `channel_read` | endpoint, bytes, cap, handles, hcap | nbytes (`rax`), nhandles (`rdx`); on `BufferTooSmall`, `rdx` = `needed handles << 32 \| needed bytes` | `READ` |
| 7 | `memory_create` | size | memory object | |
| 8 | `memory_map` | memory object or device memory, offset, len, flags | address | `MAP` and `READ`; `WRITE` for a writable mapping |
| 9 | `memory_unmap` | address, len | 0 | (the mapping must be one `memory_map` made, exactly) |
| 10 | `process_create` | process resource, name, name length | process | `WRITE` on the resource |
| 11 | `process_map_image` | process, memory object holding a static ELF | entry point | `WRITE` on the process, `READ` on the image |
| 12 | `process_start` | process, entry, start entries, count | 0 | `WRITE` on the process; `TRANSFER` on each handle given |
| 13 | `process_exit` | code | does not return | |
| 14 | `thread_create` | entry, stack top, argument | thread | (in the caller's own process) |
| 15 | `thread_exit` | | does not return | |
| 16 | `wait` | items, count, deadline | number of items satisfied; each item's observed signals written | `WAIT` on each handle |
| 17 | `clock_get` | | nanoseconds since boot | |
| 18 | `timer_create` | | timer | |
| 19 | `timer_set` | timer, deadline (0 disarms) | 0 | `WRITE` |
| 20 | `debug_write` | console, bytes, len | len | `WRITE` |
| 21 | `interrupt_create` | interrupt resource, GSI | interrupt | `WRITE` on the resource |
| 22 | `interrupt_ack` | interrupt | 0 | `WRITE` |
| 23 | `device_memory_create` | device resource, physical address, len | device memory region | `WRITE` on the resource |

`channel_read` and `channel_write` never block: an empty queue is `ShouldWait` (or `PeerClosed`), a full peer queue is `ShouldWait`; a process waits for `READABLE` or `WRITABLE`. `wait` is the only call that blocks.

A process ends when its last thread ends (code 0), when a thread calls `process_exit`, or when a thread faults (KK-I10).

### d.3 `process_start` entries and `wait` items

A start entry is 40 bytes: `u32 handle` (moved into the new process), `u32` reserved, `[u8; 32]` slot name, zero padded. A wait item is 16 bytes: `u32 handle`, `u32 signals awaited`, `u32 signals observed` (written by the kernel), `u32` reserved. All little endian.

### d.4 The start page

One page, read-only, at an address the kernel chooses, passed in `rdi`:

| offset | field |
|---|---|
| 0 | `u32` magic `0x31534b4d` ("MKS1") |
| 4 | `u32` ABI version |
| 8 | `u32` count of entries |
| 12 | `u32` reserved |
| 16 | `[u8; 32]` process name |
| 48 + 40 i | entry i: `u32` handle, `u32` rights, `[u8; 32]` slot name |

`moruna-sys::start::take(name)` takes the handle in a slot once.

### d.5 Bounds (`moruna_abi::limits`)

| bound | value |
|---|---|
| message bytes | 64 KiB |
| handles per message | 64 |
| messages queued towards one endpoint | 64 |
| handles per process | 4096 |
| items per `wait` | 64 |
| memory object size | 256 MiB |
| image for `process_map_image` | 64 MiB |
| device memory region | 1 GiB |
| process and slot names | 1 to 32 bytes of `[a-z0-9._-]` |
| start handles | 64 |
| `debug_write` | 4096 bytes, UTF-8 |
| a process's first stack | 256 KiB |

### d.6 `moruna-sys`

Safe wrappers in `no_std` Rust: `Handle` (owned, closes on drop, `duplicate`, `info`), `Channel` (`create`, `write` moves an array of handles and gives them back on failure, `read`, `needed`), `Memory` and `DeviceMemory` (`map` returns a `Mapping` that unmaps on drop and is read and written by volatile byte accesses, never through a reference, because its pages may be shared), `Process` (`create`, `map_image`, `start`), `Thread::spawn`, `Timer`, `Interrupt`, `Console` and `println!`, `wait` and `wait_one`, `now`, `exit`, `thread_exit`, the start page (`start::take`, `start::peek`), `main!` (the entry), and a panic handler that prints on the `console` slot if there is one and exits with 101. `raw` carries one safe function per call that passes memory, taking slices; the unsafe `syscall` and the calls that could break the process's own memory (unmap, thread start) are private to the crate.

## e. Data model, formats and state machines

### e.1 Objects

| type | what | signals | default rights |
|---|---|---|---|
| process | an address space and a handle table | `TERMINATED` | read, write, duplicate, transfer, wait |
| thread | an OSTD task running user code in its process | `TERMINATED` | read, write, duplicate, transfer, wait |
| channel endpoint | one end of a pair; closes when its last handle closes | `READABLE`, `WRITABLE`, `PEER_CLOSED` | read, write, duplicate, transfer, wait |
| memory object | zeroed pages, allocated at creation; every mapping maps the same pages | | read, write, map, duplicate, transfer |
| timer | a deadline on the kernel clock | `SIGNALED` | read, write, duplicate, transfer, wait |
| interrupt | a global system interrupt routed to an OSTD IRQ line | `SIGNALED` until acknowledged | read, write, duplicate, transfer, wait |
| device memory region | physical MMIO pages, mapped uncached | | read, write, map, duplicate, transfer |
| console | the serial line, write only | | write, duplicate, transfer |
| resource | the right to create processes, device memory or interrupts | | write, duplicate, transfer |

A file a manifest gives (`file:PATH`) is a memory object holding a copy of the file, with read, map, duplicate and transfer.

### e.2 Rights

`READ` 1, `WRITE` 2, `MAP` 4, `DUPLICATE` 8, `TRANSFER` 16, `WAIT` 32. A raw value with any other bit is `InvalidArgs`.

### e.3 Signals

`READABLE` 1, `WRITABLE` 2, `PEER_CLOSED` 4, `TERMINATED` 8, `SIGNALED` 16.

### e.4 The boot manifest

The file `manifest` at the root of the initramfs, ASCII, at most 64 KiB, lines of at most 512 bytes, `#` to the end of a line a comment:

```text
moruna-manifest 1
channel NAME
process NAME PATH
handle SOURCE SLOT [rights=R,R,...]
```

The first directive is `moruna-manifest 1`. `channel` declares a channel whose endpoints are `NAME.0` and `NAME.1`. `process` starts a process from the ELF at `PATH` in the initramfs, in manifest order; the `handle` lines under it give it handles. `SOURCE` is one of:

| source | gives |
|---|---|
| `console` | the debug console |
| `root.process`, `root.device`, `root.interrupt` | a resource (the first process only) |
| `channel:NAME.0`, `channel:NAME.1` | one endpoint of a declared channel |
| `file:PATH` | a read-only memory object holding the initramfs file at `PATH` |

`rights=` narrows the source's rights to a subset (names as in e.2). The parser refuses, by line, an unknown directive or source, a bad name or path (paths are relative, no `..`, no empty part), a duplicate channel, process or slot, a handle before any process, an endpoint given twice or never, a root resource outside the first process, rights the source does not carry, and more than 32 processes, 64 channels or 64 handles for one process. `moruna_abi::manifest::Manifest` parses and renders the format, so the KB2 tooling writes manifests with the same code the kernel reads them with.

### e.5 Errors

`BadHandle` 1, `WrongType` 2, `AccessDenied` 3, `InvalidArgs` 4, `NoMemory` 5, `ShouldWait` 6, `PeerClosed` 7, `TimedOut` 8, `BufferTooSmall` 9, `OutOfRange` 10, `BadSyscall` 11, `Limit` 12, `BadState` 13, `Fault` 14. Within a call they are checked in the order: handle lookup, the handle's type, its rights, then the other arguments.

### e.6 A process's life

`created` (by `process_create`; no image) → `created with image` (after `process_map_image`, once) → `running` (after `process_start`, or at boot) → `ended`. A manifest process goes through the same states at boot.

## f. Algorithms and policies

### f.1 Boot

OSTD boots by multiboot, initialises memory, interrupts and the timer, and calls the boot crate's `#[ostd::main]`, which calls `moruna_microkernel::main`. The kernel starts its clock (the TSC, whose frequency OSTD measured; no frequency is a refusal), registers the deadline queue on OSTD's 1 kHz timer, installs the post-schedule hook of KK-I4, reads OSTD's initramfs (a cpio newc archive; only regular files and directories), parses `manifest`, creates its channels, and for each process in order loads its ELF, builds its start handles, and starts it. It prints `kk: moruna microkernel, abi 1.0: started N processes from the manifest` and returns, which hands the CPU to the processes. Any refusal prints `kk: boot refused: REASON` and powers off with failure.

### f.2 Loading an image

`moruna-loader::elf` accepts `ET_EXEC`, ELF64, little endian, `EM_X86_64`, at most 64 program headers; refuses `PT_INTERP` and `PT_DYNAMIC` (a dynamic linker is a server's job, KC1), `PT_TLS`, a segment with more file bytes than memory bytes or reaching past the file, a segment outside `0x1000 .. 0x1000_0000_0000`, two segments sharing a page, a segment both writable and executable, and an entry outside an executable segment. Each segment gets fresh zeroed pages, its file bytes copied in, mapped with its permissions. Mappings, stacks and the start page go in `0x1000_0000_0000` up to the top of user space, first fit, with an unmapped guard page on each side.

### f.3 Handle values

A value is `((generation << 16) | (slot + 1)) XOR salt`, the salt drawn per process from the CPU's random source, the TSC and the process's serial number. Slot zero decodes from no value, so zero is never a handle; an encoding that would be zero takes the next generation. Closing a slot advances its generation, so the old value names nothing. Ending a process drains its table, and the handles are dropped outside the table's lock, since dropping one may close a channel endpoint.

### f.4 Channels

Two queues, one towards each endpoint, and an open flag per endpoint, under one lock. Signals are recomputed under that lock after every change: `READABLE` while an endpoint's queue is not empty, `WRITABLE` while its peer is open and the peer's queue has room, `PEER_CLOSED` once the peer is closed. A write copies its bytes and handle values in from user memory first, then, holding the caller's table lock and then the channel's lock (that order everywhere), checks every handle (present, `TRANSFER`, not an endpoint of this channel, not listed twice), the peer open and its queue not full, and only then moves the handles out of the table and queues the message. A read holds the same two locks, checks the caller's buffers and the table's room, copies the bytes out, inserts clones of the handles, copies their values out, and only then pops the message; a fault at any step leaves the message queued and removes the clones. When an endpoint closes, the messages queued to it are taken out under the lock and dropped outside it.

### f.5 Memory

A memory object's pages are allocated, zeroed, at creation, and mapping maps those same frames, so every mapping of one object shares its bytes with no copy: a process can hand another a read-only view of a buffer by duplicating its handle without `WRITE` and sending it. `memory_unmap` takes exactly one mapping `memory_map` made; segments, stacks and the start page cannot be unmapped by a process. A device memory region is OSTD's `IoMem`: OSTD hands out only ranges in its MMIO windows that hold no RAM and nothing OSTD reserved (the APICs, the IOMMU), and refuses a range already handed out.

### f.6 Time

`clock_get` is the TSC scaled to nanoseconds since boot. Timer deadlines and `wait` deadlines live in one queue ordered by (deadline, sequence), under a lock that disables interrupts; every tick of OSTD's 1 kHz timer pops what is due, raising `SIGNALED` on a timer or waking a waiting thread. A deadline is met to within one tick plus scheduling delay.

### f.7 Interrupts

`interrupt_create` allocates an OSTD IRQ line, has it raise `SIGNALED` on the object when it fires, and routes the GSI to it through the I/O APIC; a GSI the chip does not have or one already routed is `AccessDenied`. A driver waits for `SIGNALED`, services its device and calls `interrupt_ack`, which clears it.

### f.8 Waiting

`wait` reads its items, looks every handle up (KK-I2), registers one OSTD waker with every object's signaller and, for a finite deadline, with the deadline queue, and then loops: if any item's awaited signals intersect the object's current ones, it returns how many; if the deadline has passed, `TimedOut`; if the thread is being killed, it returns so the thread can end; otherwise it sleeps until woken. Any change of an object's signals wakes every registered waker. It unregisters everything and writes every item's observed signals back.

### f.9 Faults and ending

A CPU fault or trap in user mode (OSTD's `ReturnReason::UserException`) ends the faulting process with a description (for a page fault, the address, read or write, and the instruction pointer). Ending records the first reason, marks every thread killed and wakes any that sleeps in `wait`; each thread leaves user mode at its next kernel entry. The last thread to end runs the termination of KK-I10, prints `kk: process NAME exited with code N` or `kk: process NAME faulted: WHAT; it has ended and the others run on`, and, if it was the last process, powers off (KK-I12).

## g. Concurrency within the component

Locks, outermost first: a process's handle table; a channel's state; an object's signaller (interrupts disabled). The deadline queue (interrupts disabled) is taken alone, or before a timer's signaller in the tick. A process's region map and its state are leaves. No lock is held while a handle is dropped, except where the drop cannot be the last reference (the clones of f.4). User memory is copied with OSTD's fallible copies, which never sleep, so a copy may run under a spin lock. The kernel is written for several CPUs (every shared state is behind a lock, address spaces are activated per switch, and OSTD flushes TLBs on unmap), but KB1 boots and tests it on one (m.2).

## h. Behaviour

A boot runs the manifest's processes until the last one ends, then powers off (KK-I12). A process that never ends keeps the machine up, which is what a supervisor is for.

## i. Configuration

None at run time. The bounds of d.5 are constants of `moruna-abi`. The toolchain is pinned in `kernel/rust-toolchain.toml` (nightly-2026-07-21, OSTD 0.19.0's); the build flags are OSDK's, in `kernel/tools/build.sh`.

## j. Observability

The kernel writes to the serial line: its boot line, a boot refusal, an image refusal, each process's end with its code or its fault, and the power-off line. Processes write only through a console handle. OSTD's own panic prints a backtrace and powers off with failure.

## k. Tests

Host tests run with `cargo test` on the host target; boot tests run in CI's `kernel` job, which builds the image (`kernel/tools/build.sh`), boots it under QEMU 10.2.1 with KVM when the runner has it and TCG otherwise (`boot.sh`), and judges the serial line (`check-boot.sh`): every line below present, no `FAIL`, no `panic`, and QEMU's status 33, the kernel's success.

| id | test | where | asserts |
|---|---|---|---|
| KK-T1 | `kk_t1_*` | `abi` | error codes round trip and are distinct; rights, signals and types take only defined bits; layout helpers bounded |
| KK-T2 | `kk_t2_*` | `abi::manifest` | the format parses, renders and round trips; every refusal of e.4 by line; every bound |
| KK-T3 | `kk_t3_*` | `loader::cpio` | files read and directories skipped; every refusal (truncated, magic, field, name, trailer, type) |
| KK-T4 | `kk_t4_*` | `loader::elf` | a static executable parses; every refusal of f.2 |
| KK-T5 | `kk_t5_*` | `handles` | values name only live entries; a closed or reused slot's value is bad; values differ between salts; zero is never issued; the table is bounded and drains |
| KK-T6 | ping-pong | `programs/ping`, `pong` | `KB1 ping-pong: ok: 10000 round trips over one channel`, each echo checked |
| KK-T7 | handle transfer | `programs/xfer-a`, `xfer-b` | A writes a pattern, sends a duplicate without `WRITE` (and loses it from its table); B maps it, checks the pattern, is refused a writable mapping and a duplicate with `WRITE` with `AccessDenied` |
| KK-T8 | isolation | `programs/iso-holder`, `isolation` | every handle-taking call (15) on every value 0 to 4096 and on six live values of another process returns `BadHandle`, first holding only a console, then holding nothing (the exit code counts failures; the kernel prints `exited with code 0`) |
| KK-T9 | wait with deadline | `programs/deadline` | a timer wakes a waiting second thread no earlier than its deadline and within 50 ms of it; a wait nothing satisfies is `TimedOut` no earlier than its deadline |
| KK-T10 | a faulting process | `programs/fault` | reading address 0 ends the process; the kernel prints `faulted: page fault at 0x0 on read`; every other test's line and the final success line still appear |
| KK-T11 | the supervisor | `programs/supervisor`, `child` | creates, loads and starts a child from a `file:` image and sees it end; maps a device memory region in high MMIO and is refused the same range a second time; is refused RAM as device memory, and an interrupt from the process resource; waits on an interrupt line until its deadline and acknowledges it |
| KK-T12 | the boot | the whole log | the boot line with 10 processes; every process but `fault` and `isolation` exits 0; the power-off line says the supervisor succeeded |

Coverage: the repository's 90% line gate is for the root workspace; it is not applied here, because the kernel crate runs only under QEMU. What is measured is the host coverage of `moruna-abi`, `moruna-loader` and `moruna-handles` (`cargo llvm-cov`), and for the kernel, the boot tests above, which between them make each of the 24 calls; what they do not reach is listed in m (an interrupt that fires, a second CPU).

## l. Implementation notes for the agent

- OSTD's `#[ostd::main]` expands to an exported symbol, which `forbid(unsafe_code)` rejects; it lives in `boot/`, which is OSDK's generated base crate, so the kernel crate stays safe.
- QEMU's `-kernel` loads only 32-bit multiboot ELFs, so `build.sh` writes a copy of the kernel with `e_machine` set to `EM_386`, as OSDK does for `qemu-direct`.
- The kernel and the programs are built by separate cargo invocations, so the manifest parser (which allocates) is not unified into programs that have no allocator.
- The programs link with `programs/user.ld`: one segment per permission, each on its own pages (f.2).

## m. Open items

1. **Level-triggered interrupts.** OSTD 0.19's mapped IRQ line has no mask or unmask, so a level-triggered line that stays asserted refires until the device is serviced. KB3 needs mask-on-fire and unmask-on-ack, from OSTD or by routing virtio devices through MSI-X.
2. **More than one CPU.** The kernel is written for SMP (g) but KB1 registers no AP entry and boots with one CPU; a multi-CPU boot test is KB2's.
3. **Narrowing a resource.** The supervisor delegates a resource whole (duplicate and send). A call that derives a resource over a sub-range (a physical window, a GSI range) is needed before untrusted drivers get one (KB3).
4. **Handle cycles.** An endpoint queued in a message on another channel whose endpoint is queued on the first keeps both alive until one side's process ends (no leak past a process's life, since ending drains the tables, but a leak while both run).
5. **Partial unmap and protection changes.** `memory_unmap` takes whole mappings; `mprotect`-like changes and unmapping part of a mapping are the Linux personality's needs (KC1).

## n. Traceability

| requirement (BOARD EK KB1) | here |
|---|---|
| objects: process, thread, channel, memory object, timer, interrupt, device memory region | e.1; `core/src/{process,channel,memory,clock,interrupt}.rs` |
| handles with rights; naming only through one's own table; transfer moves | e.2, f.3, f.4; KK-I1 to KK-I6 |
| the native syscall ABI, numbered and versioned | d.1 to d.5; `abi/src/lib.rs` |
| boot from an initramfs and a manifest; the supervisor alone gets root handles | e.4, f.1; KK-I11 |
| ELF loading of static binaries | f.2; `loader/src/elf.rs` |
| the debug console as a handle | e.1, d.2 (`debug_write`) |
| `moruna-sys` | d.6; `sys/` |
| the tests, booted in CI | k; `.github/workflows/ci.yml` job `kernel` |

## o. Deferred (post-KB1)

The SDK and protocol harness (KB2), drivers as processes (KB3), the Linux personality (KC1), the image filesystem (KC2), the data plane on kernel channels (KD1), the kernel as a node (KE), aarch64.
