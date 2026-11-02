# Moruna SDD 20: The microkernel core (`kernel/`)

**Document type:** software design document, component 20
**Status:** DRAFT · 2026-10-28, amended by KB2 the same day (ABI 1.1, several CPUs, the cycle rule; SDD 23), by KB3 on 2026-10-29 (ABI 1.2, the calls drivers need; SDD 25) by KC1 the same day (ABI 2.0: the native numbers moved to `0x1_0000 + n`, the forward and its fast calls, an address space of areas, futexes, a thread's FS base and FPU registers, the realtime clock; SDD 28) and by KD1 the same day (ABI 2.1: slots, the engine's calls on them and the data plane's calls; SDD 29) and by KD2 the same day (ABI 2.2: `process_grant`; SDD 30) and by KB5/KE1/native storage (ABI2.3 accounts,2.4 contiguous DMA,2.5 immutable Account/Memory origin) (becomes HANDOFF-READY when section m is empty; the human flips it)
**Parent:** `BOARD.md` section EK, item KB1 (Brackly, 2026-10-28); `architecture/optimized-runtime.md` section 3, as the EK direction amends it: the kernel is a microkernel, and Linux compatibility is a server (KC1), not kernel syscalls
**Decisions:** MK1 (`DECISIONS.md`: OSTD as the hardware layer, MPL-2.0)
**Component location:** `kernel/` (its own Cargo workspace): `abi/` (`moruna-abi`), `loader/` (`moruna-loader`), `handles/` (`moruna-handles`), `core/` (`moruna-microkernel`), `boot/` (`moruna-microkernel-boot`, the kernel image), `sys/` (`moruna-sys`), `programs/` (the test programs), `initramfs/boot.image` (the CI boot's description, built by `moruna-image`, SDD 23), `tools/` (`build.sh`, `boot.sh`, `check-boot.sh`)
**Consumes:** OSTD 0.19.0 (boot, page tables, the frame allocator, interrupts, the timer, task switching, user mode entry), OSDK 0.19.0's frame and heap allocators and its x86_64 linker script. **Consumed by:** KB2 (`moruna-svc`, the SDK, which is built on `moruna-sys` and `moruna-abi`), KB3 (drivers as processes), KC1 (the Linux personality server), KD1 (the data plane on kernel channels), KE (the kernel as a node).

**Decisions worth your eye:** (1) the kernel holds eleven object types and 50 calls (ABI 2.2) and the data plane's five, and nothing else: no files, no names, no network, no Linux syscall number (a forward's table, given by its server, says which foreign numbers reach which fast call, f.14); (2) a handle value is salted per process, so a value from another process names nothing here, and every call looks its handles up before it checks anything else; (3) the boot manifest is a line-oriented text file in the initramfs, so adding a server is a line and an ELF, and nothing in the kernel is rebuilt; (4) only the first process, the supervisor, may receive the three root resources (process creation, device memory, interrupts), and it delegates them by duplicating and sending handles; (5) the kernel builds with plain cargo: the crate `cargo osdk` would generate is written out in `boot/`.

---

## a. Purpose and boundary

Moruna 1.0 runs a task as a process on its own kernel (BOARD EK). This component is that kernel's core: the smallest set of mechanisms every other part is built from. It owns: the kernel objects and their lifetimes; handles, rights and the per-process handle table; the native syscall ABI and its version; boot from an initramfs and its manifest; static ELF loading; the debug console as a handle.

It refuses to know: drivers (KB3), filesystems and images (KC2), the Linux ABI (KC1), the network (KE2), the engine, the Arrow format and the data plane's record check (KD1: the kernel moves a record's pages and the engine checks it, SDD 29 f.3), and any embedder. Each of those is a process speaking a published protocol over channels. The test of every object and call here is extensibility: a third party adds a driver or a server by writing a process against the SDK and listing it in the boot manifest; nothing in the kernel is rebuilt or forked.

OSTD is the hardware layer: all the unsafe code (boot, paging, interrupts, the timer, context switches, user mode entry and the fallible user copies) is OSTD's. `moruna-microkernel` is `#![forbid(unsafe_code)]`. The only unsafe in the kernel image outside OSTD is in `boot/src/main.rs`: three exported symbols OSTD looks up by name and the call to OSTD's panic handler, the same code OSDK generates for every OSTD kernel.

## b. Vocabulary

**Object.** A kernel entity of one of the eleven types in e.1.

**Handle.** A process's reference to an object, with rights (e.2). A process names objects only by the values of the handles in its own table.

**Rights.** What a handle lets its holder do (e.2). Rights never grow.

**Signals.** Bits an object raises (e.3); `wait` sleeps until one of them is raised.

**Resource.** A root capability: the right to create objects of one kind from nothing (processes, device memory, interrupt lines, message-signalled interrupts, pins for devices).

**Manifest.** The boot manifest (e.4): the processes the kernel starts and the handles each starts with.

**Supervisor.** The first process in the manifest, the only one that may receive resources.

**Start page.** One read-only page the kernel maps into each process before its first thread runs, listing the handles it starts with by slot name (d.4).

## c. Invariants

- **KK-I1.** A process reaches an object only through a handle in its own table. No call takes a process identifier, an address in another process, or a global name.
- **KK-I2.** Every call that takes a handle looks it up in the caller's table before it checks any other argument, so a value that names nothing there returns `BadHandle`, whatever the other arguments are.
- **KK-I3.** A handle value never names an object in another process's table: values are salted per process and carry a generation, so a value learned from another process, or one already closed, is `BadHandle` (f.3).
- **KK-I4.** A thread runs, and the kernel copies to and from user memory, only with the thread's own process's address space active: OSTD's post-schedule hook activates it after every switch, and OSTD's user copies refuse an address space that is not the active one.
- **KK-I5.** Rights never grow: `handle_duplicate` takes a subset of the source's rights, a transfer carries the sender's rights exactly, and `memory_map` maps writable only with `WRITE` and never writable and executable at once.
- **KK-I6.** A transferred handle is removed from the sender's table in the same critical section that queues it; a write that fails moves nothing. A channel's own endpoints never travel through it, and an endpoint travels only while no message queued towards it carries an endpoint (the cycle rule, f.4), so endpoints in flight never keep each other alive.
- **KK-I7.** Physical memory and interrupt lines are reached only through objects made from a resource, and only within that resource's range (a root resource covers its kind's whole space; `resource_narrow` derives a sub-range, f.10). A device memory region is never RAM nor anything OSTD uses itself; an interrupt object is a line no one else has routed.
- **KK-I8.** Every queue and buffer is bounded: message bytes, handles per message, queue depth, handles per process, wait items, memory object size, image size, debug writes (d.5). Kernel buffers sized by a process are reserved fallibly, so an over-large request is `Limit` or `NoMemory`, never a kernel allocation failure.
- **KK-I9.** The start page is mapped read-only before the first thread runs and stays mapped until the process has ended.
- **KK-I10.** A fault in a process ends that process and nothing else: its threads stop, its handles close, its mappings go, `TERMINATED` is raised on it, and the kernel names the fault on the console.
- **KK-I11.** Only the first process of the manifest may receive a resource (the three of KB1 and, since ABI 1.2, `root.ecam`, `root.msi` and `root.dma`); the kernel refuses a manifest that gives one to any other, and refuses to boot from a manifest it cannot parse or an image it cannot load, powering off with the reason.
- **KK-I12.** When the last process ends, and the boot has started every manifest process, the kernel powers off: success if the supervisor exited with code 0, failure otherwise.
- **KK-I13.** (2.0) A page has an entry in a process's page table only while its area allows access to it; a page made inaccessible and accessible again holds the bytes it held, and a zero page reads zero until it is written (f.5, f.12).
- **KK-I14.** (2.0) Protection never grows past the handle: an area of a memory object mapped through a handle without `WRITE` can never be made writable, and no area is ever writable and executable at once (f.5).
- **KK-I15.** (2.0) A forwarded process's call reaches its server or the kernel's fast calls and nothing else: a fast call acts only on its caller's thread and process, and the forward's channel is the kernel's, never in any process's table (f.14).
- **KK-I16.** (2.0) Every thread runs with its own FPU registers and FS base, whatever ran on its CPU before it (f.15).

## d. Interfaces

### d.1 The calling convention

`syscall` with the number in `rax` and up to six arguments in `rdi`, `rsi`, `rdx`, `r10`, `r8`, `r9`. Call `n` of the table is number `nr::BASE + n`, `BASE = 0x1_0000` (ABI 2.0); the data plane's calls are `BASE + 0x100` onward, in a range reserved to `BASE + 0x110` (ABI 2.1, d.2 and SDD 29 d.1); every number outside the table and that range is foreign: `BadSyscall` for a native process, and for a forwarded one a message to its server or a fast call (f.14, SDD 28). The kernel returns a value in `rax` (and, where the table says, a second one in `rdx`), or an error as `-(code)` in `rax` (e.5). `rcx` and `r11` are clobbered by the instruction; every other register is preserved. Lengths and offsets of mappings are whole pages (4096 bytes). Deadlines are absolute nanoseconds on `clock_get`'s clock; `0` polls and `u64::MAX` never passes.

A thread starts with `rip` at its entry, `rdi` its argument, and `rsp` 8 below a 16-byte boundary at the top of its stack, as if called. A process's first thread receives the start page's address in `rdi` and a 256 KiB stack the kernel maps.

### d.2 The syscalls, ABI 2.0

`abi_version` returns `MAJOR << 16 | MINOR`. The major number changes when a call's meaning changes; the minor number when a call is added. A number is never reused. Any number at or above `BASE + 53` outside the data plane's range, and any number of that range the second table does not name, is `BadSyscall` for a native process. ABI 1.1 (KB2) added `resource_narrow`. ABI 1.2 (KB3, SDD 25 d.1) added calls 25 to 29 and `memory_create`'s flags, whose zero is the call ABI 1.0 made. ABI 2.0 (KC1, SDD 28) moved every call to `BASE + n` (so a forwarded process keeps the low numbers for its own ABI), made `memory_unmap` take any whole pages of a mapping, and added calls 30 to 42. ABI 2.1 (KD1, SDD 29) added calls 43 to 48 and the data plane's calls of the second table. ABI2.2 adds process_grant49;2.3 adds account_create50/account_info51;2.4 adds contiguous DMA;2.5 adds account_matches52 with READ on both actual handles. The table's `nr` column is `n`.

| nr | call | arguments | returns | rights needed |
|---|---|---|---|---|
| 0 | `abi_version` | | version | |
| 1 | `handle_close` | handle | 0 | |
| 2 | `handle_duplicate` | handle, rights | new handle | `DUPLICATE`; rights a subset of the handle's |
| 3 | `handle_info` | handle | `type << 32 \| rights` | |
| 4 | `channel_create` | | endpoint 0 (`rax`), endpoint 1 (`rdx`) | |
| 5 | `channel_write` | endpoint, bytes, nbytes, handles, nhandles | 0 | `WRITE`; `TRANSFER` on each handle sent |
| 6 | `channel_read` | endpoint, bytes, cap, handles, hcap | nbytes (`rax`), nhandles (`rdx`); on `BufferTooSmall`, `rdx` = `needed handles << 32 \| needed bytes` | `READ` |
| 7 | `memory_create` | size, flags (1.2: `memory_flags::DMA` = 1, the object may be pinned for a device; 2.1: `memory_flags::CONTIGUOUS` = 2, with `DMA` only and at most 4 MiB, its pages one physically contiguous run that a pin maps as one run of device addresses; 0 otherwise) | memory object | |
| 8 | `memory_map` | memory object or device memory, offset, len, flags | address | `MAP` and `READ`; `WRITE` for a writable mapping |
| 9 | `memory_unmap` | address, len | 0 | whole pages of the caller's `memory_map` areas; other area kinds refused (f.5) |
| 10 | `process_create` | process resource, name, name length, account (2.3: zero for the caller, otherwise an account or process handle) | process | `WRITE` on the resource and a nonzero account argument |
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
| 21 | `interrupt_create` | interrupt resource, GSI | interrupt | `WRITE` on the resource; the GSI inside the resource's range |
| 22 | `interrupt_ack` | interrupt | 0 | `WRITE` |
| 23 | `device_memory_create` | device resource, physical address, len | device memory region | `WRITE` on the resource; the range inside the resource's |
| 24 | `resource_narrow` (1.1) | resource, base, len | resource over `base .. base + len` | `DUPLICATE`; the range inside the source's (f.10) |
| 25 | `interrupt_create_msi` (1.2) | MSI resource, slot | interrupt (`rax`); its message, `data << 32 \| address` (`rdx`) | `WRITE` on the resource; the slot inside its range, held by no live interrupt (SDD 25 f.2) |
| 26 | `memory_pin` (1.2) | DMA resource, memory object, flags, addresses, capacity | pin (`rax`), page count (`rdx`); each page's device address written; on `BufferTooSmall`, `rdx` = pages needed | `WRITE` on the resource; `READ` on the memory, and `WRITE` with `pin_flags::DEVICE_WRITES`; the memory made with `memory_flags::DMA` (SDD 25 f.4) |
| 27 | `dma_release` (1.2) | DMA resource | pins released from its quarantine | `WRITE` |
| 28 | `resource_info` (1.2) | resource, 24-byte buffer | 0; `u32` kind, `u32` zero, `u64` base, `u64` length written | |
| 29 | `process_kill` (1.2) | process | 0 | `WRITE`; the process running (`BadState` otherwise) |
| 30 | `memory_protect` (2.0) | address, length, `map` protection bits (none is no access) | 0 | whole pages of the caller's own `memory_map` mappings, every page mapped (`OutOfRange` otherwise); `WRITE` only through a handle that had it (`AccessDenied`), never with `EXECUTE` (`InvalidArgs`) |
| 31 | `process_forward` (2.0) | process, endpoint, table, table length | 0 | `WRITE` on the process, created and not started (`BadState`); `READ`, `WRITE` and `TRANSFER` on the endpoint, which leaves the caller's table; the table of SDD 28 d.1 (`InvalidArgs`) |
| 32 | `process_launch` (2.0) | process, entry, stack pointer, heap start (0 none) | the first thread's id | `WRITE`; the process created and not started; no start page and no handles: its creator built its memory |
| 33 | `process_map` (2.0) | process, memory object (0: zero pages filled on first touch), offset, length, address (0: the kernel chooses), flags (`map` protection bits, `map::FIXED`) | address | `WRITE` on the process; `MAP` and `READ` on the memory, `WRITE` for a writable mapping; at a given address without `FIXED` a range in use is `BadState`, with `FIXED` it is unmapped first |
| 34 | `process_unmap` (2.0) | process, address, length | 0 | `WRITE`; whole pages, any mapped or not |
| 35 | `process_protect` (2.0) | process, address, length, protection bits | 0 | `WRITE`; as `memory_protect`, on any of the process's areas |
| 36 | `process_read` (2.0) | process, address, buffer, length (at most 1 MiB) | 0 | `READ`; every page mapped and readable (`Fault`) |
| 37 | `process_write` (2.0) | process, address, buffer, length (at most 1 MiB) | 0 | `WRITE`; every page mapped and writable (`Fault`) |
| 38 | `futex_wait` (2.0) | address of a `u32`, expected value, deadline | 0 when woken | the word holding another value is `ShouldWait`; the deadline `TimedOut` (f.13) |
| 39 | `futex_wake` (2.0) | address, count | threads woken | (the caller's own words) |
| 40 | `thread_set_fs` (2.0) | FS base | 0 | a lower-half address (`InvalidArgs`) |
| 41 | `forward_clone` (2.0) | process, thread id, stack pointer, FS base (0: the parent's), clear-tid word (0 none), set-tid word (0 none) | the new thread's id | `WRITE`; a forwarded process whose thread `id` waits on its forward (`InvalidArgs` for no such thread, `BadState` for one not waiting) (f.14) |
| 42 | `random_read` (2.0) | buffer, length (at most 4096) | 0 | `BadState` when the CPU has no random source |
| 43 | `slot_create` (2.1) | flags: `slot_flags::INPUT` or `OUTPUT` | slot | (`InvalidArgs` for any other flags) |
| 44 | `slot_push` (2.1) | slot, memory object, offset (a page multiple), length (whole pages for an output slot), cookie | 0 | `WRITE` on the slot; `TRANSFER`, `READ` and `MAP` on the memory, and `WRITE` for an output slot; the handle leaves the caller's table; `ShouldWait` with `SLOT_DEPTH` objects held, `BadState` once closed or its task ended (SDD 29 f.2) |
| 45 | `slot_take` (2.1) | slot, 32-byte buffer | the memory object given back (zero for a release); the event written (`plane::slot_event`) | `READ`; `ShouldWait` with none, `PeerClosed` with none and the task ended |
| 46 | `slot_close` (2.1) | slot | 0 | `WRITE` |
| 47 | `slot_refuse` (2.1) | slot, reason, length (at most 512) | 0 | `WRITE`; the attached task running (`BadState`); it ends with the reason (SDD 29 f.3) |
| 48 | `process_plane` (2.1) | process, slot handle values, count (at most 64), port endpoint (0 none) | 0 | `WRITE` on the process, created and not started (`BadState`), and on each slot, none attached before (`BadState`); `READ`, `WRITE`, `TRANSFER` on the endpoint, which leaves the caller's table (SDD 29 d.2) |
| 49 | `process_grant` (2.2) | process, start entries (as `process_start`'s), count (at most 64) | the address of the start page mapped into it | `WRITE` on the process, created and not started (`BadState`), granted once (`BadState` for a second; `process_start` refuses a granted process); `TRANSFER` on every handle (`AccessDenied`, and nothing moves); valid, distinct names (`InvalidArgs`). The handles move into its table under their names and a start page naming them (d.4) is mapped read only where the kernel chooses: how a server that launches a process with `process_launch` gives it handles, which the Linux personality does for the engine (SDD 30 d.1) |

The data plane's calls (ABI 2.1, SDD 29 d.1), native numbers `BASE + 0x100` onward. The first three name a slot or a port by its number in the caller's own plane, not by a handle: a number the plane does not have is `BadHandle`, as a value that names nothing is (KK-I2).

| nr | call | arguments | returns | needs |
|---|---|---|---|---|
| `0x100` | `batch_get` | slot | address (`rax`), length (`rdx`); `(0, 0)` at the end | an input slot: its next batch mapped read only, the previous one released; an output slot: its window mapped writable; waits while there is none |
| `0x101` | `batch_put` | slot, address (a page of the window), length | 0 | an output slot whose window is mapped (`BadState`); the record inside it (`OutOfRange`); the window leaves the address space before the engine is told |
| `0x102` | `port_call` | port, request, length (at most 64 KiB less 32) | the reply's address (`rax`) and length (`rdx`), `(0, 0)` for none | a port (`BadHandle`); waits for the engine's reply, which is mapped read only; the engine's error code is the call's |
| `0x103` | `plane_get` | process, slot | as `batch_get`, mapped into the caller | `WRITE` on the process, which has a plane (`BadState`) |
| `0x104` | `plane_put` | process, slot, address, length | 0 | as `batch_put`, of a window `plane_get` mapped into the caller |
| `0x10f` | `plane_probe` | operation, argument | a frame's physical address, or the bytes copied for the caller | a kernel built with the `probe` feature (`BadSyscall` otherwise); SDD 29 d.3 |

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
| 12 | `u32` CPUs online (ABI 2.0; reserved before) |
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
| pages one `memory_pin` pins | 65536 (a memory object of the largest size) |
| pins in one DMA resource's quarantine | 4096 |
| MSI slots | 224 |
| `process_read`, `process_write` (2.0) | 1 MiB a call |
| `random_read` (2.0) | 4096 bytes a call |
| a forward's table (2.0) | foreign numbers `0 .. 512` |
| slots of one plane (2.1) | 64 |
| memory objects one slot holds: queued, mapped and given back untaken (2.1) | 64 |
| a `slot_refuse` reason (2.1) | 512 bytes of UTF-8 |

### d.6 `moruna-sys`

Safe wrappers in `no_std` Rust: `Handle` (owned, closes on drop, `duplicate`, `info`, `narrow`), `Channel` (`create`, `write` moves an array of handles and gives them back on failure, `write_with` the same from a slice of `Option<Handle>`, `read`, `needed`), `Memory` and `DeviceMemory` (`map` returns a `Mapping` that unmaps on drop and is read and written by volatile byte accesses, never through a reference, because its pages may be shared), `Process` (`create`, `map_image`, `start`), `Thread::spawn`, `Timer`, `Interrupt`, `Console` and `println!`, `wait` and `wait_one`, `now`, `exit`, `thread_exit`, the start page (`start::take`, `start::peek`), `main!` (the entry), a panic handler that prints on the `console` slot if there is one and exits with 101, and, with the feature `heap`, a global allocator over memory objects (SDD 23 f.6). `raw` carries one safe function per call that passes memory, taking slices; the unsafe `syscall` and the calls that could break the process's own memory (unmap, thread start) are private to the crate.

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
| slot (2.1) | the engine's end of a place batches pass through between it and one task (SDD 29 e.1); closed when its last handle closes | `READABLE` (an event waits), `WRITABLE` (a push would be taken), `PEER_CLOSED` (its task has ended), `SIGNALED` (2.2: its task's `batch_get` would not wait, which a server serving the task's descriptors waits on, SDD 30 f.10) | read, write, duplicate, transfer, wait |
| console | the serial line, write only | | write, duplicate, transfer |
| resource | the right to create processes, or device memory, interrupts, message-signalled interrupts or pins within a range (physical addresses `0 .. 2^52`, GSIs `0 .. 256`, MSI slots `0 .. 224`, PCI requester ids `0 .. 65536` for a root resource; `root.ecam` covers exactly the firmware's PCI configuration space) | | write, duplicate, transfer |
| pin (1.2) | a DMA memory object's pages mapped for the devices a DMA resource covers; when it closes they wait in that resource's quarantine until `dma_release` (SDD 25 f.4) | | (none; it is only closed) |

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
| `root.ecam`, `root.msi`, `root.dma` (1.2) | the device memory resource over ACPI MCFG's window, the MSI resource, the DMA resource (the first process only; SDD 25 d.2) |
| `channel:NAME.0`, `channel:NAME.1` | one endpoint of a declared channel |
| `file:PATH` | a read-only memory object holding the initramfs file at `PATH` |

`rights=` narrows the source's rights to a subset (names as in e.2). The parser refuses, by line, an unknown directive or source, a bad name or path (paths are relative, no `..`, no empty part), a duplicate channel, process or slot, a handle before any process, an endpoint given twice or never, a root resource outside the first process, rights the source does not carry, and more than 32 processes, 64 channels or 64 handles for one process. `moruna_abi::manifest::Manifest` parses and renders the format, so the KB2 tooling writes manifests with the same code the kernel reads them with.

### e.5 Errors

`BadHandle` 1, `WrongType` 2, `AccessDenied` 3, `InvalidArgs` 4, `NoMemory` 5, `ShouldWait` 6, `PeerClosed` 7, `TimedOut` 8, `BufferTooSmall` 9, `OutOfRange` 10, `BadSyscall` 11, `Limit` 12, `BadState` 13, `Fault` 14. Within a call they are checked in the order: handle lookup, the handle's type, its rights, then the other arguments.

### e.6 A process's life

`created` (by `process_create`; no image) → `created with image` (after `process_map_image`, once) → `running` (after `process_start`, or at boot) → `ended`. A manifest process goes through the same states at boot.

## f. Algorithms and policies

### f.1 Boot

OSTD boots by multiboot, initialises memory, interrupts and the timer, and calls the boot crate's `#[ostd::main]`, which calls `moruna_microkernel::main`. The kernel sets OSTD's log level to info (at its default, debug, OSTD prints a line for every FPU save and restore), refuses a CPU without FSGSBASE (f.15), injects its scheduler and registers the other CPUs' entry (f.11), starts its clock (the TSC, whose frequency OSTD measured; no frequency is a refusal) and reads the CMOS clock (f.16), registers the deadline queue on OSTD's 1 kHz timer, installs the pre- and post-schedule hooks of KK-I4 and f.15, prints `kk: N CPUs online`, reads OSTD's initramfs (a cpio newc archive; only regular files and directories), parses `manifest`, creates its channels, and for each process in order loads its ELF, builds its start handles, and starts it. It prints `kk: moruna microkernel, abi 2.0: started N processes from the manifest`, stops counting itself as a live process (KK-I12), and returns, which hands the CPU to the processes. Any refusal prints `kk: boot refused: REASON` and powers off with failure.

### f.2 Loading an image

`moruna-loader::elf` accepts `ET_EXEC`, ELF64, little endian, `EM_X86_64`, at most 64 program headers; takes a `PT_TLS` template (ABI 2.0: a program sets up its own thread-local storage from it with `thread_set_fs`) and refuses one outside the loaded segments; refuses `PT_INTERP` and `PT_DYNAMIC` (a dynamic linker is a server's job, SDD 28), a segment with more file bytes than memory bytes or reaching past the file, a segment outside `0x1000 .. 0x1000_0000_0000`, two segments sharing a page, a segment both writable and executable, and an entry outside an executable segment. Each segment gets fresh zeroed pages, its file bytes copied in, mapped with its permissions. Mappings, stacks and the start page go in `0x1000_0000_0000` up to the top of user space, first fit, with an unmapped guard page on each side.

### f.3 Handle values

A value is `((generation << 16) | (slot + 1)) XOR salt`, the salt drawn per process from the CPU's random source, the TSC and the process's serial number. Slot zero decodes from no value, so zero is never a handle; an encoding that would be zero takes the next generation. Closing a slot advances its generation, so the old value names nothing. Ending a process drains its table, and the handles are dropped outside the table's lock, since dropping one may close a channel endpoint.

### f.4 Channels

Two queues, one towards each endpoint, and an open flag per endpoint, under one lock. Signals are recomputed under that lock after every change: `READABLE` while an endpoint's queue is not empty, `WRITABLE` while its peer is open and the peer's queue has room, `PEER_CLOSED` once the peer is closed. A write copies its bytes and handle values in from user memory first, then, holding the caller's table lock and then the channel's lock (that order everywhere), checks every handle (present, `TRANSFER`, not an endpoint of this channel, not listed twice), the peer open and its queue not full, and only then moves the handles out of the table and queues the message. A read holds the same two locks, checks the caller's buffers and the table's room, copies the bytes out, inserts clones of the handles, copies their values out, and only then pops the message; a fault at any step leaves the message queued and removes the clones. When an endpoint closes, the messages queued to it are taken out under the lock and dropped outside it.

**The cycle rule.** A message carrying endpoint `F` towards endpoint `E` keeps `F` alive for as long as `E` is open, so endpoints in flight could keep each other alive with no process holding any of them. A write may therefore move an endpoint only while no message queued towards it carries an endpoint (`BadState` otherwise). Each channel counts, per end, the endpoints its queued messages carry; a write that carries an endpoint takes the global `TRANSFER` lock after the table and the channel, checks every endpoint it moves has a count of zero, and adds what it queues to its peer's count; a read and a close subtract. Every endpoint in flight is then held by one that holds none, and no cycle forms (proof: SDD 23 f.11).

### f.5 Memory

A memory object's pages are allocated, zeroed, at creation, and mapping maps those same frames, so every mapping of one object shares its bytes with no copy: a process can hand another a read-only view of a buffer by duplicating its handle without `WRITE` and sending it. A process's address space (`core/src/vm.rs`, ABI 2.0) is a map of areas, each a run of whole pages with one protection and one backing: pages of a memory object (writable only if the handle mapped through carried `WRITE`), zero pages (f.12), or device memory. Unmapping or protecting part of an area splits it, so any whole pages can be unmapped or protected; `memory_unmap` and `memory_protect` act on the caller's own `memory_map` mappings only, so segments, stacks and the start page cannot be unmapped or changed by a native process, while the `process_*` calls act on any area of a process their caller holds with `WRITE`. The page table holds an entry only for a page that is accessible (an area with no access has none), because OSTD tracks a frame only while its entry is present; making pages accessible again maps them again from their backing, so they keep their bytes. A device memory region is OSTD's `IoMem`: OSTD hands out only ranges in its MMIO windows that hold no RAM and nothing OSTD reserved (the APICs, the IOMMU), and the kernel first refuses a range outside the resource's (`AccessDenied`) and a range a live region holds (`AccessDenied`). OSTD never takes a range back, so the kernel keeps the ranges it acquired and hands out slices of them again once no live region holds them (SDD 25 f.3).

### f.6 Time

`clock_get` is the TSC scaled to nanoseconds since boot. Timer deadlines and `wait` deadlines live in one queue ordered by (deadline, sequence), under a lock that disables interrupts; every tick of OSTD's 1 kHz timer pops what is due, raising `SIGNALED` on a timer or waking a waiting thread. A deadline is met to within one tick plus scheduling delay.

### f.7 Interrupts

`interrupt_create` refuses a GSI above 255 (`InvalidArgs`) or outside the resource's range (`AccessDenied`), allocates an OSTD IRQ line, has it raise `SIGNALED` on the object when it fires, and routes the GSI to it through the I/O APIC; a GSI the chip does not have or one already routed is `AccessDenied`. A driver waits for `SIGNALED`, services its device and calls `interrupt_ack`, which clears it. OSTD 0.19 programs every line edge triggered and active high and cannot mask one, so a level-triggered line is not usable; a PCI driver uses `interrupt_create_msi` (ABI 1.2), an OSTD line with no pin that its device raises by writing the message the call returns into its MSI-X table (SDD 25 f.2).

### f.8 Waiting

`wait` reads its items, looks every handle up (KK-I2), registers one OSTD waker with every object's signaller and, for a finite deadline, with the deadline queue, and then loops: if any item's awaited signals intersect the object's current ones, it returns how many; if the deadline has passed, `TimedOut`; if the thread is being killed, it returns so the thread can end; otherwise it sleeps until woken. Any change of an object's signals wakes every registered waker. It unregisters everything and writes every item's observed signals back.

### f.9 Faults and ending

A CPU fault or trap in user mode (OSTD's `ReturnReason::UserException`) ends the faulting process with a description (for a page fault, the address, read or write, and the instruction pointer). Ending records the first reason, marks every thread killed and wakes any that sleeps in `wait`; each thread leaves user mode at its next kernel entry. The last thread to end runs the termination of KK-I10, prints `kk: process NAME exited with code N` or `kk: process NAME faulted: WHAT; it has ended and the others run on`, and, if it was the last process, powers off (KK-I12).

### f.10 Narrowing a resource

`resource_narrow` makes a resource of the source's kind over `base .. base + len` with the source handle's rights: page-aligned physical addresses for device memory, GSIs for interrupts. It is refused for a process resource or an empty or misaligned range (`InvalidArgs`) and for a range not inside the source's (`AccessDenied`), so rights and ranges never grow. A supervisor gives an untrusted driver a resource over its own device only.

### f.11 Several CPUs

The kernel injects its own scheduler into OSTD before any task runs (OSTD 0.19's default queues every task on the boot processor): a run queue and an idle task per CPU, a new task on the CPU with the fewest runnable tasks, a woken one on its waker's CPU when nothing else is runnable there, a 10 ms slice, and a boot context that is never preempted. The application processors enter through OSTD's AP entry. The boot counts as a live process until it has started the whole manifest (KK-I12). Detail: SDD 23 f.12.

### f.12 Zero pages (ABI 2.0)

A zero-page area (`process_map` with no memory object, a forwarded process's anonymous `mmap`, its heap and stack) allocates nothing when mapped. A thread's first touch of one of its pages is a page fault on a page with no entry; the kernel finds the area, checks the access against its protection, allocates a zeroed frame, keeps it with the space under the page's address and maps it, and the thread goes on. Any other fault ends the process (KK-I10). A copy the kernel makes into or out of such a page (a syscall's argument, `process_read`, `process_write`) populates it first. Unmapping the page frees its frame; `madvise(DONTNEED)` (f.14) frees it so it reads zero again.

### f.13 Futexes (ABI 2.0)

A futex queue is keyed by the process and a word's address, so no word of one process names a queue of another (KK-I1). A wait reads the word and queues the thread under the queues' lock, after populating the word's page so the read needs no other lock; a wake takes the lock, so no wake falls between a wait's read and its sleep. Each waiter carries a bitset (all ones for the native call); a wake wakes the first `count` waiters whose set meets its own, in arrival order. A requeue moves waiters between words and records the move on each, so a timed-out waiter leaves the queue it is in. A waiter killed while it sleeps returns `BadState` and its thread ends.

### f.14 Forwarded processes and fast calls (ABI 2.0; SDD 28 f.1)

`process_forward` gives a created process a forward: the kernel's end of a channel and a table naming, for each foreign number below 512, a fast call or none. A forwarded thread's foreign call is either a fast call, which the kernel answers itself in the x86-64 System V convention (arguments in `rdi`, `rsi`, `rdx`, `r10`, `r8`, `r9`, a result or `-errno` in `rax`), or a request on the forward's channel (SDD 28 d.1) after which the thread waits for the reply with its token; replies for other waiting threads stay queued for them, anything else is dropped, and the server's end closing ends the process with the reason. While a thread waits its registers are kept, so `forward_clone` can start a thread from them (result register zero, the stack pointer given) and write its id to the set-tid word before it runs. A thread that ends while its process goes on clears its clear-tid word and wakes one waiter on it. The fast calls are `futex` (wait, wake, their bitset forms, requeue and compare-requeue), `clock_gettime`, `clock_getres`, `gettimeofday`, `time`, `nanosleep`, `clock_nanosleep`, anonymous `mmap`, `munmap`, `mprotect`, `brk`, `madvise`, `sched_yield`, `arch_prctl` (the FS base), `set_tid_address`, `getrandom`, `exit` and `gettid`: each acts only on the calling thread and its process, and one it does not take (a mapping of a file, a futex operation it does not know, another `arch_prctl` code) is forwarded instead. No foreign number is the kernel's: the table is the server's.

### f.15 A thread's registers across switches (ABI 2.0)

OSTD's pre-schedule hook saves the leaving thread's FPU registers (XSAVE) and FS base (RDFSBASE), and charges it the CPU time of its slice; the post-schedule hook activates the arriving thread's address space and loads its FPU registers and FS base. A thread starts with the FPU's reset state and an FS base of zero (or its parent's, or the `forward_clone` argument). A boot on a CPU without FSGSBASE is refused with the reason. The process and thread CPU clocks of `clock_gettime` are these charges.

### f.16 The realtime clock (ABI 2.0)

At boot the kernel reads the CMOS clock once (the node's time, in UTC; the century register when ACPI's FADT names one), and the realtime clock is that instant plus the monotonic clock. A CMOS clock that holds no valid date refuses the boot with the reason.

### f.17 The data plane (ABI 2.1; SDD 29)

A slot is a kernel object the engine makes and a created process is given before it starts (`process_plane`). The kernel moves pages between them and reads none of the bytes: `slot_push` queues a memory object (a batch for an input slot, a window for an output one); `batch_get` maps the next one into the task as an area of its own kind, `plane`, which no call of the task (nor a server's on it) unmaps, protects or maps over, so the plane always unmaps what it mapped; `batch_put` unmaps the window from the task before the engine's `slot_take` gives its memory object back; a task's end gives back every object its slots held. A port call is the forward's rendezvous (f.14) on the task's port channel, its reply a memory object mapped read only. The kernel copies no byte of a batch or a window (DP-I1); `plane_probe`, in a kernel built with `probe`, reports the frames and the copies its tests measure. Detail: SDD 29 f.1 to f.6.

## g. Concurrency within the component

Locks, outermost first: a process's handle table; a channel's state; an object's signaller (interrupts disabled). The deadline queue (interrupts disabled) is taken alone, or before a timer's signaller in the tick. A process's region map and its state are leaves. No lock is held while a handle is dropped, except where the drop cannot be the last reference (the clones of f.4). User memory is copied with OSTD's fallible copies, which never sleep, so a copy may run under a spin lock. The kernel runs on several CPUs (every shared state is behind a lock, address spaces are activated per switch, OSTD flushes TLBs on unmap, and its scheduler is the kernel's, f.11); CI boots it on one, two and four. `TRANSFER` (f.4) is taken after a channel's state, only by writes that carry an endpoint; a run queue's lock is taken alone.

## h. Behaviour

A boot runs the manifest's processes until the last one ends, then powers off (KK-I12). A process that never ends keeps the machine up, which is what a supervisor is for.

## i. Configuration

None at run time. The bounds of d.5 are constants of `moruna-abi`. The toolchain is pinned in `kernel/rust-toolchain.toml` (nightly-2026-07-21, OSTD 0.19.0's); the build flags are OSDK's, in `kernel/tools/build.sh`.

## j. Observability

The kernel writes to the serial line: the CPUs online, its boot line, a boot refusal, an image refusal, each process's end with its code or its fault and the CPUs it ran on, how many CPUs ran process threads, and the power-off line. Processes write only through a console handle. OSTD's own panic prints a backtrace and powers off with failure.

## k. Tests

Host tests run with `cargo test` on the host target; boot tests run in CI's `kernel` job, which builds the image from `kernel/initramfs/boot.image` (`kernel/tools/build.sh`, SDD 23 f.8), boots it under QEMU 10.2.1 with KVM when the runner has it and TCG otherwise on one, two and four CPUs (`boot.sh`), and judges each serial line (`check-boot.sh`): every line the description expects present, every CPU online and having run a process thread, no `FAIL`, no `panic`, and QEMU's status 33, the kernel's success. The KB2 boot tests are SDD 23's SV-T10 to SV-T14.

| id | test | where | asserts |
|---|---|---|---|
| KK-T1 | `kk_t1_*` | `abi` | error codes round trip and are distinct; rights, signals and types take only defined bits; layout helpers bounded |
| KK-T2 | `kk_t2_*` | `abi::manifest` | the format parses, renders and round trips; every refusal of e.4 by line; every bound |
| KK-T3 | `kk_t3_*` | `loader::cpio` | files read and directories skipped; every refusal (truncated, magic, field, name, trailer, type) |
| KK-T4 | `kk_t4_*` | `loader::elf` | a static executable parses; every refusal of f.2 |
| KK-T5 | `kk_t5_*` | `handles` | values name only live entries; a closed or reused slot's value is bad; values differ between salts; zero is never issued; the table is bounded and drains |
| KK-T6 | ping-pong | `programs/ping`, `pong` | `KB1 ping-pong: ok: 10000 round trips over one channel`, each echo checked |
| KK-T7 | handle transfer | `programs/xfer-a`, `xfer-b` | A writes a pattern, sends a duplicate without `WRITE` (and loses it from its table); B maps it, checks the pattern, is refused a writable mapping and a duplicate with `WRITE` with `AccessDenied` |
| KK-T8 | isolation | `programs/iso-holder`, `isolation` | every handle-taking call (36, ABI 2.1: the 29 of ABI 2.0, the five slot calls, `process_plane`, `plane_get`, `plane_put`) and the task's own `batch_get`, `batch_put` and `port_call` in a process with no plane, on every value 0 to 4096 and on six live values of another process, return `BadHandle`, first holding only a console, then holding nothing (the exit code counts failures; the kernel prints `exited with code 0`) |
| KK-T9 | wait with deadline | `programs/deadline` | a timer wakes a waiting second thread no earlier than its deadline and within the bound of the machine it runs on: 50 ms under KVM, 900 ms under QEMU's TCG (below); a wait nothing satisfies is `TimedOut` no earlier than its deadline |
| KK-T10 | a faulting process | `programs/fault` | reading address 0 ends the process; the kernel prints `faulted: page fault at 0x0 on read`; every other test's line and the final success line still appear |
| KK-T11 | the supervisor | `programs/supervisor`, `child` | creates, loads and starts a child from a `file:` image and sees it end, after a start giving a handle without `TRANSFER` is refused with `AccessDenied` and the handle stays the supervisor's; maps a device memory region in high MMIO and is refused the same range a second time; is refused RAM as device memory, and an interrupt from the process resource; waits on an interrupt line until its deadline and acknowledges it |
| KK-T12 | the boot | the whole log | the boot line (`abi 2.0`) with the description's process count; every process but `fault` exits 0 (`isolation`'s code counts its failures); the power-off line says the supervisor succeeded |
| KK-T13 | ABI 2.0's own calls | `programs/vm` | `KC1 kernel calls: ok:`: a mapping made read only keeps its bytes and made writable again takes writes; one of a handle without `WRITE` cannot be made writable; a partial `memory_unmap` keeps the first page and the object the rest; a futex wake wakes a sleeping thread, a wait on a changed word is `ShouldWait`, one nothing wakes `TimedOut` at its deadline; `thread_set_fs` takes a lower-half address and refuses a non-canonical one; `random_read` gives bytes that are not all zero and refuses a request past its bound |
| KK-T14 | another process's memory | `programs/supervisor` | `KC1 remote memory: ok:`: zero pages mapped at a chosen address, written and read across a page boundary, zero where untouched; a second mapping there refused without `FIXED` and replacing with it; an object's pages through a read-only handle readable, not writable, not protectable to writable; writable and executable refused; pages protected to no access unreadable and back again readable; a partial `process_unmap` faults the page gone and keeps the next; protecting over a hole `OutOfRange`; a handle without `WRITE` reads and nothing more |

KK-T9's bound is the scheduler's, not the clock's. The timer fires on the 1 ms tick; the woken thread then waits its turn behind the runnable threads on its CPU, each for up to a 10 ms slice (f.11), so the lateness grows with the work the rest of the boot is doing and with how fast the CPU does it. The program reads the CPUID hypervisor leaf (0x4000_0000) itself: `KVMKVMKVM` gets 50 ms, `TCGTCGTCGTCG` gets 50 ms times the emulation factor, and any other answer (no hypervisor bit, another hypervisor) gets 50 ms, the strict bound, never a loose one it has not earned. The factor is 18, measured on 2026-10-29 as the ratio of KK-T6's time for 10000 round trips under TCG to under KVM, the worst of the CPU counts: TCG on QEMU 10.2.1 in Apple's `container` on an arm64 Mac, ten boots each on one, two and four CPUs (1.85 to 2.05 s, 0.97 to 1.24 s, 0.86 to 1.05 s); KVM on CI's ubuntu runner, two boots each (327 to 347 ms, 71 to 77 ms, 102 to 120 ms); worst ratio 1240 / 71 = 17.5 on two CPUs. Under TCG the wake was 0.6 to 85 ms late on that host with nothing else running, and 134 to 238 ms late in earlier boots there beside other work, against the old 50 ms; under KVM, 8 to 32 ms. Either bound is far below the waiter's own 5 s deadline, so a lost wake still fails the test: that wait ends `TimedOut`, not `SIGNALED`, and the program prints `FAIL`. The CI job boots with KVM, so the bound it judges is 50 ms.

Coverage: the repository's 90% line gate is for the root workspace; it is not applied here, because the kernel crate runs only under QEMU. What is measured is the host coverage of `moruna-abi`, `moruna-loader`, `moruna-handles`, `moruna-svc` and `moruna-image` (`cargo llvm-cov`), and for the kernel, the boot tests above and SDD 23's, which between them cover the 52 native calls on one, two and four CPUs (including KB5 account creation/info and quota assertions); SDD 25's DR-T7 makes the five of ABI 1.2, and its driver tests take MSI interrupts that fire; KK-T13 and KK-T14 make the native calls of ABI 2.0 and SDD 28's LX-T10 to LX-T16 make the forward, `process_launch`, `forward_clone` and every fast call under real programs; SDD 29's DP-T10 to DP-T17 make the six calls of ABI 2.1 and the data plane's.

## l. Implementation notes for the agent

- OSTD's `#[ostd::main]` expands to an exported symbol, which `forbid(unsafe_code)` rejects; it lives in `boot/`, which is OSDK's generated base crate, so the kernel crate stays safe.
- QEMU's `-kernel` loads only 32-bit multiboot ELFs, so `build.sh` writes a copy of the kernel with `e_machine` set to `EM_386`, as OSDK does for `qemu-direct`.
- The kernel and the programs are built by separate cargo invocations, so the manifest parser (which allocates) is not unified into programs that have no allocator.
- The programs link with `programs/user.ld`: one segment per permission, each on its own pages (f.2).

## m. Open items

1. **A pager.** Zero pages fill on first touch (f.12), but a memory object's pages are all allocated when it is made, so a file mapping is read whole before it is mapped (SDD 27 m.2). A memory object whose pages a server fills on a fault is the next kernel change (SDD 28 m.1).

Resolved by KC1 (SDD 28): partial unmap and protection changes (f.5, `memory_protect`, `process_protect`, a partial `memory_unmap`). Resolved by KB2 (SDD 23): more than one CPU (f.11, booted on 2 and 4 CPUs); narrowing a resource (f.10, `resource_narrow`); handle cycles (the cycle rule, f.4). Resolved by KB3 (SDD 25): level-triggered interrupts, by MSI (`interrupt_create_msi`).

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

The kernel as a node (KE), aarch64. The data plane is SDD 29 (ABI 2.1). The engine as a granted Linux program is SDD 30 (ABI 2.2, `process_grant`). The Linux personality is SDD 28. The image filesystem (KC2) is SDD 27; it added no call: the long-lived pin its page cache needs is a pin its driver keeps (SDD 27 m.1, SDD 25 d.3). What it needs of the kernel later is a pager, a memory object whose pages a server fills when a task first touches them (SDD 27 m.2), which is a kernel change of its own.
