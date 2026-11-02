# Moruna SDD 29: The data plane (`kernel/plane`, the kernel's slots, ABI 2.1)

**Document type:** software design document, component 29
**Status:** DRAFT · 2026-10-29 (becomes HANDOFF-READY when section m is empty; the human flips it)
**Parent:** `BOARD.md` section EK, item KD1 (Brackly, 2026-10-28): "the data-plane adapter on kernel channels: shared pages, ownership handoff, the record check in `batch_put`", with the brief of 2026-10-29: the three calls `batch_get`, `batch_put` and `port_call`, batches exchanged as shared pages and never copies, the lowest latency and the highest throughput; `architecture/optimized-runtime.md` section 4; `architecture/adaptive-runtime.md` sections 0 and 9 (the scheduler that drives it)
**Decisions:** DP1 (the record check runs in the engine's process, on pages the task can no longer reach; f.3); MK1 (OSTD) stands
**Component location:** `kernel/core/src/plane.rs` (the slot object, a process's plane, port calls), `kernel/core/src/{vm,syscall,process,foreign,usercopy,object}.rs` (the plane's areas, the calls, the rendezvous shared with the forward, the copy counter), `kernel/abi` (ABI 2.1: `nr`, `plane`, `limits`), `kernel/sys/src/plane.rs` (`moruna-sys`: the engine's `Slot`, `attach`, a task's `Plane`, `Input`, `Output`, `Port`, a server's `plane_get` and `plane_put`), `kernel/plane/check` (`moruna-ipc-check`: the IPC check in `no_std`, shared with `moruna-kernel`), `kernel/plane/core` (`moruna-plane`: the engine's side and the data-plane port), `kernel/plane/test` (`plane-test`: the boot's engine and tasks), `kernel/linux/server/src/plane.rs` (a task's descriptors 0, 1 and 2), `crates/moruna-kernel/src/ipc*` (the check, now on `moruna-ipc-check`), `crates/moruna-testkit/src/bin/plane-fixture.rs` (the boot's fixtures)
**Consumes:** SDD 20 (memory objects, address spaces of areas, the forward's rendezvous), SDD 01 e.7 and e.8 (the page-aligned record and the IPC check), SDD 21 (`moruna-record`'s ring), SDD 28 (the Linux personality). **Consumed by:** KD2 (the engine as a process on the kernel), KD3 (conformance across adapters), KD7 (the scheduler), KA2 (the port's Linux adapter).

**Decisions worth your eye:** (1) the kernel moves pages and reads no record: the check runs in the engine's process, after `batch_put` has unmapped the window from the task and before anything else of the engine is told of the record (DP1, f.3); (2) a batch moves by moving handles: a window's handle leaves the engine for the slot, comes back with the record, and a consumer gets a duplicate without `WRITE`, so fan-out is the same frames mapped read only into each consumer and is freed once, after the last share and the engine's hold (f.5); (3) a task names its slots and its port by numbers in its own plane, not by handles, and holds no handle at all; (4) `batch_get` on an output slot is how a task gets its window, so the task's interface is three calls; (5) pages are 4 KiB: OSTD 0.19 maps a user page table at its last level only, so large pages are not the kernel's to give yet (m.1).

---

## a. Purpose and boundary

A task on the Moruna kernel exchanges Arrow batches with the engine as pages. This component owns: the slot, the kernel object batches pass through; the five data-plane calls and the six of the engine (ABI 2.1); the plane's areas of an address space, which only the plane unmaps; port calls on the forward's rendezvous; the record check's placement; the engine's side of all of it (`moruna-plane`: the arena, slots, ownership moved between tasks, fan-out, residency) and the data-plane port the engine is written against; a Linux task's descriptors 0 and 1 as Arrow IPC streams and its text as log records.

It refuses to know: Arrow (the kernel's half), the scheduler's decisions (KD7: this component carries them out), how a port call leaves the VM (KD2 forwards it over vsock to the embedder, optimized-runtime section 4), how a task's image is attached (KD4), and the engine's sources and sinks.

## b. Vocabulary

**Batch.** One page-aligned Arrow IPC record (contracts e.7): a schema message, a record batch message, then the body, every buffer starting on a page from the record's start. **Slot.** A kernel object: a queue of memory objects the engine pushes, the one its task has mapped, and the events that give them back. **Input slot**, **output slot**: the engine pushes batches into the one and windows into the other. **Window.** Whole pages of a memory object an output slot's task writes one record into. **Plane.** A process's slots, numbered as `process_plane` gave them, and its port. **Port.** The kernel's end of a channel to the engine on which a task's port calls go. **Arena.** The memory objects the engine's batches and windows live in. **Share.** A consumer's hold on a batch. **Hold.** The engine's own. **Event.** What `slot_take` gives back: a release, a record put, a window returned.

## c. Invariants

- **DP-I1. No copy.** The kernel copies no byte of a batch or a window between processes: `batch_get` maps the batch's frames, `batch_put` unmaps the window and gives its memory object back, and a consumer's mapping is of the same frames the producer wrote (DP-T10: same physical frames, zero bytes copied by the kernel for either task).
- **DP-I2. A task reaches what its plane names.** A task names slots and its port by numbers in its own plane; a number it was not given is `BadHandle`; a task of the engine holds no handle, so no call reaches another process (DP-T13, KK-T8).
- **DP-I3. The check sees what the consumers will.** The engine's check runs on a record whose window has left the task's address space, so no task can write the bytes after they are checked; a record that fails ends its task with the reason, goes back to the arena, and is never a batch (DP-T3, DP-T12).
- **DP-I4. A batch is read only once it is a batch.** Every consumer maps it without `WRITE`, and the engine never writes a batch it has pushed or a port reply it has sent (a write by a task faults, DP-T13).
- **DP-I5. A plane's areas are the plane's.** No call of the task, nor a server's on its process, unmaps, protects, maps over or writes an area the plane mapped; the plane unmaps exactly what it mapped (DP-T13).
- **DP-I6. A batch is freed once, after its last holder.** A batch's memory goes back to the arena when its last share is released and the engine has dropped its hold, and not before (DP-T2, DP-T11).
- **DP-I7. A slot is bounded.** One slot holds at most `SLOT_DEPTH` (64) memory objects, queued, mapped and given back untaken; a push past it is `ShouldWait`, and nothing the slot does after its creation allocates (its queues are reserved whole).
- **DP-I8. A task's end gives everything back.** When a task ends, each slot gives the engine a release for every batch it held or had queued and every window back, and then its end (`PEER_CLOSED`), whatever the task did.
- **DP-I9. A task's only ways out are its three calls and its text.** A task of the engine is a process with a plane and no handle: its bytes leave it through `batch_put` (checked), `port_call` (to the engine) and, for a Linux task, its writes to descriptors 1 and 2, which become records in the engine's log ring (f.7). A native task has no descriptors and no handle, so it has no other way out (KK-T8 and DP-T13 probe every call that takes a handle or a slot); a Linux task's other calls are the personality's (SDD 28), which reach its own image and scratch directory only (LX-I2, LX-I3).

## d. Interfaces

### d.1 A task's calls (ABI 2.1, native numbers)

| nr | call | arguments | returns | errors |
|---|---|---|---|---|
| `0x1_0100` | `batch_get` | slot | address (`rax`), length (`rdx`); `(0, 0)` at the end | `BadHandle` for a slot the plane does not have; for an input slot, the next batch mapped read only, the previous one released (an event); for an output slot, its window mapped writable (the one mapped now, or the next); waits while there is none and the slot is open |
| `0x1_0101` | `batch_put` | slot, address, length | 0 | `InvalidArgs` on an input slot, a length of zero, an address not on a page; `BadState` with no window mapped; `OutOfRange` past the window; the window is unmapped before the engine sees the event |
| `0x1_0102` | `port_call` | port, request, length | the reply's address and length, `(0, 0)` for an empty reply | `BadHandle` with no port; `Limit` past `plane::request::MAX` (65504 bytes); the engine's error code as the call's; `PeerClosed` when the engine has gone; the thread's previous reply is unmapped first |
| `0x1_0103` | `plane_get` | process, slot | as `batch_get`, mapped into the caller | `WRITE` on the process; `BadState` when it has no plane |
| `0x1_0104` | `plane_put` | process, slot, address, length | 0 | as `batch_put`, of a window `plane_get` mapped into the caller |
| `0x1_010f` | `plane_probe` | operation, argument | d.3 | `BadSyscall` in a kernel built without `probe` |

`moruna-sys::plane` wraps them: `Plane::take()` once a process, then `input(n)`, `output(n)` and `port()` each once, so no two values can unmap each other's batches; `Input::get() -> Batch` and `Output::window() -> Window` borrow their slot, so a batch or a window cannot outlive the call that unmaps it; `Batch::bytes()` is the record in place, `Window::bytes()` the window to write, `Window::put(offset, len)`; `Port::call(port, bytes) -> Reply`.

### d.2 The engine's calls (ABI 2.1, the native table, SDD 20 d.2 rows 43 to 48)

`slot_create(flags)`; `slot_push(slot, memory, offset, len, cookie)` (the handle moves into the slot; an input slot's batch needs `READ` and `MAP`, an output slot's window `WRITE` too and whole pages); `slot_take(slot, out[32])` (the event: `RELEASED` with the batch's cookie, `PUT` with the window's cookie, the record's offset and length, and the window's memory object back, `RETURNED` with a window never used; `ShouldWait` with none, `PeerClosed` with none and the task ended); `slot_close(slot)` (no more pushes: `batch_get` answers the end once what is queued is taken; the engine's last handle closing does the same); `slot_refuse(slot, reason)` (the task ends: `faulted: its record on slot N was refused by the engine's check: REASON`); `process_plane(process, slots, count, port)` (a created process's slots in order, the caller keeping its handles, and the port endpoint, which leaves its table; a slot is attached once). A slot raises `READABLE` while an event waits, `WRITABLE` while a push would be taken, `PEER_CLOSED` once its task has ended, and (ABI 2.2, SDD 30 f.10) `SIGNALED` while its task's `batch_get` would not wait.

A port request is 32 bytes (`u32` kind 1, `u64` token at 8, `u64` port at 16, `u64` length at 24) and then the task's bytes; the reply is 24 bytes (`u64` token, `u64` status, `u64` length) and, with status 0 and a length, one memory object with `READ` and `MAP`, which the kernel maps into the caller. The rendezvous is the forward's (SDD 20 f.14): the thread waits for the reply with its token, replies for other threads stay queued for them.

### d.3 `plane_probe` (the `probe` feature)

`FRAME` (0): the physical address of the page holding the argument, an address of the caller's. `COPIED` (1): the bytes the kernel has copied into or out of the caller's memory since it started; every user copy and every copy through a page's frame counts (`vm::Space::count_copy`). `OBJECT_FRAME` (2): the physical address of page `argument >> 32` of the memory object whose handle is `argument & 0xffff_ffff`. The CI boot's kernel is built with it (`build.sh`); a kernel built without it answers `BadSyscall`.

### d.4 The data-plane port (`moruna_plane::DataPlane`)

```rust
pub trait DataPlane {
    type Task;                                                    // what starting a task needs: on the kernel, its port end
    fn open(&mut self, shape: Shape) -> Result<(TaskId, Self::Task), PlaneError>;   // Shape { inputs, outputs, port }
    fn offer(&mut self, task: TaskId, input: u32, batch: BatchId) -> Result<(), PlaneError>;  // a share, no copy
    fn finish(&mut self, task: TaskId, input: u32) -> Result<(), PlaneError>;
    fn grant(&mut self, task: TaskId, output: u32, bytes: u64) -> Result<(), PlaneError>;     // a window from the arena
    fn next(&mut self, deadline: u64) -> Result<Option<Event>, PlaneError>;
    fn answer(&mut self, call: CallId, reply: Result<&[u8], Error>) -> Result<(), PlaneError>;
    fn drop_batch(&mut self, batch: BatchId) -> Result<(), PlaneError>;                       // the engine's hold
    fn residency(&self) -> Residency;   // budget, made, resident, batches, shares, windows, pooled
}
pub enum Event { Put { task, output, batch, rows, bytes }, Released { task, input, batch }, Freed { batch },
                 Refused { task, output, reason }, Call { call, port, request }, Ended { task } }
pub enum PlaneError { Unknown(&str), Budget { asked, free }, Full, Closed, Limit(&str), Kernel(Error), Refused(String) }
```

The engine above (KD2) is written against this trait alone. On the kernel the adapter is `KernelPlane<C: Calls>` over the kernel's calls (`moruna_plane::kernel::Kernel` through `moruna-sys`, or a model of them on a host), which adds `slots(task)` (the handles `process_plane` attaches), `batch(id)` (a batch's memory object, offset and length, for a sink and a test), `load(record)` (a record of the engine's own, checked, held as a batch) and `load_with(len, write)` (the same for a record the engine writes straight into the arena object, KD2, SDD 30 f.6). On Linux (KA2) the adapter is the sandbox's channel: `offer` writes the batch to the task's standard input as an Arrow IPC stream, a record read from its standard output is a `Put`, `grant` bounds what is read, a task's end is `Ended`; the events and the refusals are the same, which KD3's conformance suite runs both through.

## e. Data model, formats and state machines

### e.1 A slot

The engine's end (`SlotEnd`, what its handles name) and the slot it shares with the task's plane. State: `ready` (pushed, in order), `current` (the one mapped: by whom, where, how much), `events`, `closed`, `attached`, `ended`, its task and its number. Every pushed object becomes exactly one event; `ready + current + events` is at most `SLOT_DEPTH`, and both queues are reserved whole at creation.

An input slot's batch: `ready` → `current` (mapped read only by `batch_get`) → `RELEASED` (at the next `batch_get`, or the task's end). An output slot's window: `ready` → `current` (mapped writable) → `PUT` (unmapped by `batch_put`) or `RETURNED` (the task's end).

### e.2 A batch in the engine (`KernelPlane`)

An arena object (its handle while the engine holds it, none while a slot does, its size), and for a batch: the object, the record's offset and length, its shares, the engine's hold. A window granted is an object pushed with its id as the cookie; it comes back as a record (a batch with the hold) or a window returned (the pool). A batch offered is pushed as a duplicate without `WRITE`, with the batch's id as the cookie, and its share ends at the release.

## f. Algorithms and policies

### f.1 The arena

`Arena::take(bytes)` gives the smallest pooled object that fits, or a new memory object while the budget allows (`Budget { asked, free }` otherwise: the scheduler waits for a release, adaptive-runtime 9.2). An object that comes back goes to the pool, so a run in steady state makes no memory: DP-T15 moved 1,000 batches through four windows' objects. Memory objects are 4 KiB pages zeroed when made (m.1).

### f.2 Moving a batch

`batch_get` maps the slot's next memory object into the caller as a `plane` area (SDD 20 f.17): a page-table change of `len / 4096` entries and no copy. `batch_put` checks the record lies in the window, unmaps the window's area, and queues `PUT` with the window's memory object; `slot_take` puts that object's handle, with the rights it was pushed with, into the engine's table. The engine offers the batch by pushing duplicates of the handle without `WRITE`. A consumer's mapping is of the frames the producer wrote (DP-T10).

### f.3 Where the check runs (DP1)

The kernel does not check records; the engine's `moruna-plane` does, in `KernelPlane::arrived` (`verify.rs`), before the event that tells the rest of the engine of the record. Why there, and why it is sound:

- the window has left the task's address space before the engine is told (`batch_put` unmaps it first), and the engine moved its own handle into the slot when it pushed the window, so when the check runs no task can write the record, and what it checks is what every consumer maps;
- the check is `moruna-ipc-check::record::check_record`, the IPC check of contracts e.8 (`moruna-kernel`'s `ipc::check` now runs the same code: one check, `no_std`); it reads the framing only (the two messages, a page or so) through a mapping of the first pages, and judges the body by its buffers' offsets and lengths against the region put, which it never reads;
- the kernel stays without the Arrow format and without a flatbuffer parser of untrusted input (SDD 20 a: the kernel refuses to know the record check), and a fault in the check is a fault in the engine's process, not in the kernel;
- the check runs off the task's path: `batch_put` returns as soon as the window is unmapped, so a producer writes its next record while the engine checks the last;
- a refusal is `slot_refuse`: the kernel ends the task with the check's sentence on the console, the engine's `Refused` event carries it to the task's terminal event, and the record's object goes back to the pool unread by anything else.

A data-plane record is never compressed and carries no dictionaries: `check_record` refuses both (a compressed record may inflate 0 bytes, contracts e.8 `Inflate::none()`), so KA10's bombs are refused before any inflation is planned (DP-T12).

### f.4 Port calls

`port_call` copies the request (at most 65,504 bytes; this is the one channel that is not Arrow) into a message on the task's port channel and waits for the reply with the thread's token, on the forward's rendezvous (`foreign::exchange`, shared with SDD 28 f.1). The reply's memory object is mapped read only into the task until the thread's next port call or its end. The engine answers with bytes (a memory object it writes and shares without `WRITE`), nothing, or an error code. Forwarding a call to the embedder's agent over vsock is KD2's.

### f.5 Fan-out and release

A batch offered to `n` consumers is `n` pushes of duplicates of one handle: one set of frames, `n` read-only mappings. Each release ends a share; the engine's `drop_batch` ends its hold; at zero the object goes back to the pool and the engine sees `Freed`, once (DP-I6). The engine may drop its hold at once after offering (DP-T11 does), so the batch lives exactly as long as its consumers need it, which is KA5's shared-batch rule (contracts CT-I15, placement PL-I16).

### f.6 A task's end

The kernel ends a process's plane before it clears its address space: each slot gives back what it held (DP-I8), unmapping a batch or window a server had mapped for it, and raises `PEER_CLOSED`. `KernelPlane` takes every event, then the end of every slot and of the port channel, and emits `Ended` once.

### f.7 A task's text: the log ring

A `run_plane` task's writes to descriptor 2, and to 1 when it has no output slot, are split into lines (at most 4,096 bytes each), and each line is pushed as a record (`moruna-record`, SDD 21, byte for byte the bus's format) into the ring the engine formatted in a memory object and gave the personality: message id the task's process name, subject the descriptor, header `stream` `stdout` or `stderr`. A ring with no room counts the line in `Ended::dropped`. Nothing of a plane task's text reaches the console or `Ended::output` (DP-T17).

### f.8 A program with no Moruna library (`run_plane`)

The two descriptors of optimized-runtime section 7, in the Linux personality (`kernel/linux/server/src/plane.rs`): `read(0)` gives the input slot as an Arrow IPC stream, the first record whole (its schema message, its record batch message and its body, which is a valid stream message as the record lays it) and every later one from its record batch message on, then the end-of-stream marker once the engine has closed the slot; `write(1)` is parsed a message at a time, the schema message kept, and each record batch message and its body laid out as a record (`moruna-ipc-check::record::reframe`, exactly `encode_framing`'s layout) in the output slot's window, which the server maps with `plane_get` and puts with `plane_put`. A dictionary message on 1 is `EINVAL`. These are copies, as any `read` and `write` are; a program that makes the three calls moves no byte. Rust's runtime asks `poll` of descriptors 0 to 2 at its start and aborts on `ENOSYS`, so the personality serves `poll` (SDD 28 d.2).

## g. Concurrency within the component

Locks, outermost first: a process's handle table, a slot's state, then an address space's areas and a signaller (leaves). `slot_push` and `slot_take` take the table and then the slot; `batch_get` and `batch_put` take the slot and then the mapper's space; a task's end takes each slot and then the mapper's space. A task waiting in `batch_get` observes the slot's task signaller under the slot's lock, so a push after it wakes it; a kill wakes it through the thread's waker. The engine's side is single threaded: `next` takes every event, then waits on every open slot and port channel at once (`WAITED_MAX`, 64).

## h. Behaviour

The CI boot runs the engine `plane` (`plane-test`) beside every other test: it starts each task as a process from `plane-task` (or `plane-ro`) with a plane and no handle, answers its role on port 1, and judges what it reports on port 2.

## i. Configuration

None at run time. Constants: `SLOT_DEPTH` 64, `PLANE_SLOTS` 64, `REFUSAL_MAX` 512 (`moruna-abi`); `WAITED_MAX` 64, `FRAMING_MAX` 1 MiB (`moruna-ipc-check`); a log line 4,096 bytes; the boot engine's arena budget 128 MiB.

## j. Observability

The kernel prints a refused task's end with the check's sentence; the engine sees every event, its residency (budget, made, resident bytes, batches, shares, windows, pooled), and every refusal's reason. The boot engine prints a line per test and its arena at the end.

## k. Tests

Host tests: `cargo test -p moruna-ipc-check -p moruna-plane -p moruna-linux -p linux-protocol` in `kernel/`, and `cargo test -p moruna-kernel` (CT-T18, CT-T22 to CT-T29: every IPC read of the root workspace through the shared check). Boot tests run in `build.sh`'s image on 1, 2 and 4 CPUs (`boot.sh`, judged by `check-boot.sh`).

| id | test | where | asserts |
|---|---|---|---|
| DP-T1 | `dp_t1_*` | `moruna-plane` | a window granted, a record put, checked and a batch; its object, offset and length; the task's end; the batch freed into the pool |
| DP-T2 | `dp_t2_*` | `moruna-plane` | one batch offered to three consumers: three read-only shares of the same bytes; freed once, after the last release, the engine's hold dropped first |
| DP-T3 | `dp_t3_*` | `moruna-plane` | garbage, a buffer outside the region, a body past it, a framing past it: each refused with its reason, the task told, nothing a batch, one object reused for all |
| DP-T4 | `dp_t4_*` | `moruna-plane` | a port request becomes a call; answers with bytes (a read-only memory object), nothing, an error; a message that is no request |
| DP-T5 | `dp_t5_*` | `moruna-plane` | the budget refuses past itself; what comes back is reused, the smallest that fits |
| DP-T6 | `dp_t6_*` | `moruna-plane` | a full slot, a closed one, unknown tasks, slots and batches, the kernel's refusals |
| DP-T7 | `dp_t7_*` | `moruna-plane` | 64 slots a task, 64 waited on at once; nothing to wait on |
| DP-T8 | `dp_t8_*` | `moruna-plane` | the engine's own record loaded and checked; a bad one refused with its reason |
| DP-T9 | `dp_t9_*` | `moruna-plane` | the check reads the first page and never the body; a framing past a page read to its end; an unreadable record |
| DP-T10 | zero copy | boot: `plane`, `plane-task` | `KD1 zero copy: ok:` a producer writes a record of 64 MiB of values in its window and puts it, a consumer gets it: the frames of pages 0, middle and last are the same in the producer, the arena and the consumer (`plane_probe`), and the kernel copied 0 bytes for either task across its calls |
| DP-T11 | fan-out | boot | `KD1 fan-out: ok:` one batch to three consumers in one frame; three shares, three releases, freed once |
| DP-T12 | the check refuses | boot | `KD1 check: ok:` garbage, a body past the region, a buffer outside it, a dictionary column and every one of KA10's compressed bombs, each refused with its reason and its task ended (`kk: process plane-bad-N faulted: its record on slot 0 was refused by the engine's check: ...`) |
| DP-T13 | isolation | boot | `KD1 isolation: ok:` 4,096 slot numbers it was not given and 4,097 handle values for `batch_get`, `batch_put`, `plane_get`, `plane_put`, `slot_push`, `slot_close` are `BadHandle`; a put on its input slot is refused; an unserved port refuses the call; its batch cannot be unmapped, protected, mapped over or written through its process; `plane-ro` writes into its batch and faults (`kk: process plane-ro faulted: page fault at ... on write`); KK-T8 sweeps the calls of ABI 2.1 too |
| DP-T14 | port calls | boot | `KD1 port: ok:` 2,000 round trips to the echo port, each reply checked, least and median |
| DP-T15 | throughput | boot | `KD1 throughput: ok:` 1,000 batches of 1 MiB from a producer to a consumer, in order |
| DP-T16 | the shimless path | boot: `/kc1/bin/arrow_echo` | `KD1 shimless: ok:` KA1's `arrow_echo`, built static with musl, reads 1,000 rows on descriptor 0 from an input slot and writes them back with its column `n` on descriptor 1: columns `x`, `s`, `n`; `x` as read; `n` the row numbers |
| DP-T17 | text as records | boot: `/kc1/bin/hello` | the same line: a plane task's standard output with no output slot reaches the log ring as records (`stream: stdout`), and none of it the console or the answer |

Results, booted under TCG (QEMU 10.2.1, `-cpu max`, emulating x86-64 in an arm64 Linux VM on an Apple host; the boot runs every test at once):

| CPUs | DP-T10: window mapped (16,641 pages) | put (unmap) | `batch_get` of the 64 MiB batch | DP-T14 port call, least (median) | DP-T15: 1,000 batches of 1 MiB | producer `batch_get` of a window, least (median) | `batch_put`, least (median) | consumer `batch_get`, least (median) |
|---|---|---|---|---|---|---|---|---|
| 1 | 6.1 ms | 0.6 ms | 6.0 ms | 188 us (1.0 ms) | 1.43 s: 701 batches/s | 93 us (99 us) | 107 us (113 us) | 199 us (218 us) |
| 2 | 62 ms | 23 ms | 5.9 ms | 72 us (129 us) | 0.73 s: 1,374 batches/s | 93 us (99 us) | 108 us (111 us) | 201 us (224 us) |
| 4 | 6.3 ms | 1.3 ms | 6.3 ms | 77 us (80 us) | 0.70 s: 1,437 batches/s | 92 us (102 us) | 108 us (124 us) | 200 us (241 us) |

The boots ran one at a time; with three at once on a loaded host the same tests gave 134, 232 and 318 batches/s and `batch_get`s of the 64 MiB batch of 270, 47 and 9 ms, which is the host's contention and not the plane's: the per-call costs of DP-T15 hardly moved (a window's `batch_get` 86 to 116 us least in every run). Across calls a 1 MiB batch costs about 100 us to map (257 entries), about 110 us to put (the unmap and its TLB shootdown) and about 200 us for the consumer's `batch_get` (an unmap of the last batch and a map of the next); the 64 MiB batch maps in about 6 ms, about 0.37 us an entry under emulation. The forwarded round trip of SDD 28 f.1 was 45 to 82 us least; a port call is the same rendezvous plus a reply's memory object mapped, 72 to 188 us least.

On every count the producer's window, the arena's object and the consumer's batch were the same physical frames at pages 0, 8,320 and 16,640 of the 68,161,536-byte record (64 MiB of values after its framing and its 1 MiB validity bitmap), and the kernel's copy counter for each task moved by 0 across its `batch_get` and `batch_put`. A batch is 257 pages a side for 1 MiB: its cost is the page-table change and the TLB shootdown of unmapping, which is why the consumer's `batch_get` (an unmap of the last batch and a map of the next) costs about twice the producer's. DP-T15 ran on four windows and made 3 MiB of objects beyond one reused from DP-T10: the steady state makes no memory. DP-T12 refused 43 records (garbage, a short body, a buffer 1 MiB outside the region, a dictionary column, and KA10's 39 bombs) and printed each reason. DP-T16's `arrow_echo` made no refused call once `poll` was served. KK-T8 swept 4,103 values over 39 calls. KB1's `wait-deadline` passed on every count in these boots and missed its 50 ms bound in earlier ones, on 1, 2 or 4 CPUs (87 to 332 ms late), when the host was loaded: it was a known failure on 1 CPU before KD1; KC2's `tree` passed. KB2's echo suite now waits 30 s a call, as the block suite does, because its 5 s missed twice while DP-T10 ran beside it.

Coverage on the host (`cargo llvm-cov`, lines): `moruna-plane` 94.7% (`arena.rs` 100%, `plane.rs` 96.1%, `verify.rs` 74.1%); `moruna-ipc-check` 97.1%. The kernel's half, the engine and the tasks are covered by the boot.

## l. Implementation notes for the agent

- `plane-ro` holds the one deliberate unsafe write of the component, the pattern of KB1's `fault`; every other crate here is `forbid(unsafe_code)` but `moruna-sys`, whose `plane` module makes a mapped batch, window or reply a slice, sound because the borrow ends before the call that unmaps it.
- `arrow_echo` is built for `x86_64-unknown-linux-musl` with `x86_64-linux-gnu-gcc` as the linker, musl's self-contained start files and its C dependencies through `musl-gcc.specs` (`build.sh`); the root toolchain needs that target.

## m. Open items

1. **Large pages.** OSTD 0.19's user page tables take 4 KiB entries only (`UserPtConfig::item_from_raw` asserts level 1), so a 64 MiB batch is 16,385 entries to map and to unmap. Mapping 2 MiB pages for arena objects made of contiguous frames is an OSTD change and then a kernel one.
2. **A server waits inside a request.** Resolved by KD2 (SDD 30 f.10): a slot raises `SIGNALED` while its task's `batch_get` would not wait, and the personality answers a task's descriptor 0 and 1 later rather than waiting inside the request.
3. **Zeroed objects.** A memory object's pages are zeroed when made; the arena reuses objects, so this is paid once an object, not once a batch.

## n. Traceability

| requirement (BOARD EK KD1 brief) | here |
|---|---|
| the arena, slots, large pages where the kernel can give them | f.1, e.1, e.2; m.1 |
| `batch_get` by page-table change, measured | d.1, f.2; DP-I1; DP-T10, DP-T15 |
| `batch_put`, the check before the engine sees the bytes, its placement justified | d.1, f.3; DP1, DP-I3; DP-T3, DP-T12 |
| `port_call` on the forward's rendezvous, measured | d.1, d.2, f.4; DP-T4, DP-T14 |
| every channel out of a task is one of these, and text as records through the ring | DP-I9, f.7; DP-T13, DP-T17, KK-T8 |
| images without a shim: descriptors 0 and 1 | f.8; DP-T16; SDD 28 d.3 |
| `moruna-plane`, `no_std` and `alloc`, fan-out, residency, the port trait | d.4, f.5; DP-T1 to DP-T9 |
| the IPC check `no_std`, shared | f.3; `moruna-ipc-check`; contracts e.8 |
| ABI rows | SDD 20 d.2 rows 43 to 48 and the data plane's table |

## o. Deferred

**DP-O1. Ports to the embedder** (KD2): the engine forwards a port call over vsock to the embedder's agent, which applies its policy. **DP-O2. Spill** (KD7): a batch evicted from the arena written to the data disk through the engine and read back. **DP-O3. Devices on the same pages**: a sink handing a batch's frames to the block driver (`memory_pin`) with no copy.
