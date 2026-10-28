# Moruna SDD 23: The SDK for servers and drivers (`kernel/svc`), the boot image tool, and the kernel's second set of calls

**Document type:** software design document, component 23
**Status:** DRAFT · 2026-10-28 (becomes HANDOFF-READY when section m is empty; the human flips it)
**Parent:** `BOARD.md` section EK, item KB2 (Brackly, 2026-10-28): "a third party adds a driver, a filesystem or a whole new server by writing a process against this SDK and listing it in the boot manifest, without rebuilding the kernel or forking anything"; SDD 20 section m, items 1 to 4
**Decisions:** none new; MK1 (OSTD) stands
**Component location:** `kernel/svc/` (`moruna-svc`), `kernel/tools/image/` (`moruna-image`), `kernel/initramfs/boot.image` (the CI boot's description), `kernel/examples/` (the echo example), `kernel/tools/{build,boot,check-boot,check-examples}.sh`; the kernel additions in `kernel/core/src/{sched,channel,syscall,object,process,boot}.rs`, `kernel/abi`, `kernel/sys` (`heap.rs`, `Handle::narrow`, `Channel::write_with`)
**Consumes:** `moruna-abi` (the ABI, the manifest format, `IMAGE_RANGE`), `moruna-sys` (the syscalls, the start page, the entry, the heap), `moruna-loader` (the image tool checks programs and archives with the kernel's own readers). **Consumed by:** KB3 (drivers as processes), KC1 (the Linux personality server), KC2 (the image filesystem server), KD1 (the data plane on kernel channels), every server a third party writes.

**Decisions worth your eye:** (1) the schema is a declarative macro, `protocol!`: the declaration is the protocol, it generates the client, the server trait and the dispatch, and nothing runs at build time but the compiler; (2) minor versions are negotiated, not refused: a client and a server of one major use what both have; (3) a server never blocks on a client: one that does not read its replies is disconnected; (4) on a host the SDK runs on a model of the channel ABI, so the SDK, its dispatch loop and its conformance harness are tested as ordinary tests; (5) the cycle rule refuses sending an endpoint whose own queue carries an endpoint, which proves that endpoints in flight never form a cycle, instead of collecting cycles after the fact; (6) level-triggered interrupt lines cannot be masked on OSTD 0.19, so KB3 uses MSI-X for virtio.

---

## a. Purpose and boundary

The microkernel (SDD 20) holds processes, memory, threads, interrupts, timers, messages and handles. Everything else is a process speaking a protocol over channels. This component is what makes that practical for someone who is not the kernel's author: a schema to declare a protocol in, the code that speaks it on both ends, a loop that serves it, a heap, a harness that says whether a server implements a protocol, and a tool that puts processes into a boot image from a description. It also carries the kernel additions SDD 20 deferred to KB2: several CPUs, narrowed resources, the cycle rule, and the decision on level-triggered interrupts.

It owns: the protocol model and its wire format; the schema macro; the handshake; the client; the dispatch loop; the conformance harness; the userland heap; the host model of the channel ABI; the boot image description and its tool; the example and the check that it touched no kernel file.

It refuses to know: any particular protocol beyond the example (drivers' protocols are KB3's, the Linux personality's KC1's); the engine; any embedder. The SDK runs on the kernel's ABI and nothing else.

## b. Vocabulary

**Protocol.** A named, versioned set of requests, events and errors over a channel (d.1).

**Frame.** One message of a protocol: an 8-byte header and a body (e.1).

**Connection.** One channel on which a client and a server speak one protocol, from `Hello` until either closes.

**Listener.** A channel on which a server receives new connections, each as one endpoint in a `Connect` frame.

**Suite.** A protocol's conformance cases (f.5).

**Description.** The text the boot image tool reads: processes, programs, handles, links, expectations (e.4).

## c. Invariants

- **SV-I1.** A connection begins with the client's `Hello`. A server answers `Welcome` or `Refuse`, and after `Refuse` closes the channel. A server serves a client of the same protocol name and major version whatever its minor; anything else is refused with its reason.
- **SV-I2.** Neither side uses what the other's minor version lacks: the client does not send a request whose `since` is above the server's minor (`Unsupported`, nothing sent), and the server does not emit an event whose `since` is above the client's.
- **SV-I3.** Every request carries a nonzero id, and its answer (`Reply` or `Failure`) carries the same id. A client files every answer under its id and every event in arrival order, so answers and events read while waiting for another answer are not lost; an answer to an id not in flight is `Malformed`.
- **SV-I4.** A body decodes to exactly one value or to nothing: truncation, an invalid tag, flag or string, a count no message holds, a handle index that names no handle (or one already taken, or one of the wrong type), and bytes or handles left over are each refused.
- **SV-I5.** A frame larger than `limits::CHANNEL_MAX_BYTES` or with more than `limits::CHANNEL_MAX_HANDLES` handles is refused by the sender's SDK before any call is made.
- **SV-I6.** A handle that arrives in a frame and is not taken by the handler is closed when the request has been answered; a handle the handler takes is the handler's.
- **SV-I7.** A server never blocks on one client: a reply or event that finds the client's queue full closes that connection, and a connection that sends a frame a client may not send is closed; the dispatcher serves the others on.
- **SV-I8.** A dispatcher holds at most `limits::WAIT_MAX_ITEMS` handles (connections, listeners and watched objects), waited on with one `wait`; it reads at most 16 frames from one channel per turn; it returns when it holds none.
- **SV-I9.** The schema refuses at compile time a protocol name outside 1 to 64 bytes of `[a-z0-9._-]`, a request or event ordinal that is zero or repeated, an error code outside 1 to 65535 or repeated, and a `since` above the protocol's minor.
- **SV-I10.** A boot image's manifest is rendered by `moruna_abi::manifest` and parsed back with the kernel's parser before the image is written; every program in it parses with the kernel's ELF loader against `IMAGE_RANGE`; the archive reads back with the kernel's cpio reader.
- **SV-I11.** A commit that changes `kernel/examples/` changes no file of `kernel/core`, `kernel/abi`, `kernel/sys`, `kernel/handles`, `kernel/loader` or `kernel/boot`.
- **SV-I12.** (Kernel.) A resource made by `resource_narrow` covers a sub-range of its source's range and carries its source handle's rights; `device_memory_create` and `interrupt_create` refuse what lies outside the resource's range with `AccessDenied`.
- **SV-I13.** (Kernel.) An endpoint is moved into a message only while no message queued towards it carries an endpoint; so every endpoint in flight is held by an endpoint that holds no endpoint, and no set of endpoints in flight keeps itself alive (f.11).
- **SV-I14.** (Kernel.) Every CPU OSTD reports runs process threads; a CPU's boot context is never preempted; a CPU with nothing to run halts until an interrupt.
- **SV-I15.** (Kernel.) The machine powers off only when every process has ended and the boot has started every manifest process.
- **SV-I16.** A deferred request is answered exactly once: a handler that defers must answer `Deferred`, one that answers `Deferred` must have deferred (the connection closes otherwise), and the dispatcher refuses a second answer to a `Pending` (`Malformed`) and an answer to a connection that has gone (`PeerClosed`).

## d. Interfaces

### d.1 The protocol model

A protocol is a name, a `major.minor` version, requests, events and errors:

- a **request** has an ordinal (1 to 65535), arguments and one reply type, and the minor version it arrived in (`since`, 0 by default);
- an **event** has an ordinal, fields and its `since`; the server sends it at any time after the handshake;
- an **error** has a code (1 to 65535); a request answers with its reply or one error.

A minor version only adds requests and events. A change to an existing request, event or error is a new major, which is a different protocol to every client of the old one.

### d.2 The schema: `protocol!`

```rust
moruna_svc::protocol! {
    pub protocol echo {
        name = "moruna.example.echo";
        major = 1;
        minor = 1;
        requests {
            1 => fn echo(text: Vec<u8>) -> Vec<u8>;
            3 since 1 => fn send_to(to: Channel, text: Vec<u8>) -> u32;
        }
        events { 1 => echoed { total: u64 }; }
        errors { 1 => Empty; 2 => PeerGone; }
    }
}
```

It generates, in module `echo`: `ID`, `REQUESTS`, `EVENTS`; `Error` (implements `ProtocolError`); `Event` (`ordinal`, `since`, `decode`, `encode`, `emit`); `trait Server` with one method per request, `fn echo(&mut self, cx: &mut Context, text: Vec<u8>) -> Result<Vec<u8>, Error>`; `Serve<S>`, the `Handler` that decodes a request's arguments, calls the method and encodes its answer; `factory`, a listener's handler factory; and `Client`, with `connect`, `next_event`, `core` and one method per request. A field's type is anything that implements `Wire` (e.2).

**Why a declarative macro.** The requirement is one source of truth that gives versioning, compatibility checks and generated Rust types, with no code generator binary. Three forms were weighed:

| form | generates both ends from one declaration | compile-time checks | build cost |
|---|---|---|---|
| a Rust trait plus derive macros | the trait is one end only; the other end needs a second macro reading the trait | in the proc macro | a proc-macro crate with `syn` and `quote`, built for the host in every kernel build |
| an IDL file with a build-script generator | yes | in the generator | a generator run at build time; generated files to keep in sync or regenerate |
| a declarative macro | yes | `const` assertions in the expansion | none: the compiler expands it |

The declarative macro is the simplest that meets every requirement: the declaration is ordinary Rust inside the crate that uses it, the expansion is visible with `cargo expand`, it needs no dependency, and the same expansion compiles for the kernel and for a host. What it cannot do (concatenate identifiers, so a request's reply is a type, not a named struct) costs nothing a protocol needs.

### d.3 The client

`ClientCore::connect(channel, id, deadline)` makes the handshake; `send(ordinal, body)` returns a request id; `reply(id, deadline)` waits for that id's answer; `call` is the two within the client's timeout; `next_event(deadline)`. `connect(listener)` opens a connection through a listener. `CallError<E>`: `PeerClosed`, `TimedOut`, `TooLarge` (refused at the sender), `Full` (the server is not reading), `Refused(Refused)`, `Unsupported` (SV-I2), `Remote(E)` (the protocol's error), `Status(Status)` (the SDK's: `UnknownMethod`, `BadMessage`, `ReplyTooLarge`), `Malformed`, `Kernel(Error)`.

### d.4 The server

`Handler` (`protocol`, `handle(cx, ordinal, body) -> Outcome`, `closed`), `Outcome` (`Reply`, `Failure(code)`, `Status`, `Deferred`), `Context` (`connection`, `client_minor`, `emit`, `defer`, `complete`, `close`), `Dispatcher` (`serve`, `listen`, `watch`, `complete`, `turn(deadline)`, `run`), `Watch` (`handle`, `signals`, `signaled(seen, completions) -> keep`), `Pending`, `Completions`.

A driver is the case that needs the last four: it waits on its interrupt alongside its clients (`watch`, with an object that owns the interrupt and acknowledges it), and answers a request when its device completes it, not when it arrives (`defer` in the handler, `complete` from the watch). The generated `Server` trait answers synchronously; a server that defers implements `Handler` itself, using the generated `ID`, `REQUESTS` and `Event`.

### d.5 The conformance harness

`conformance::Suite { id, requests, cases, timeout }`, `Case { name, check: fn(&mut Probe) -> Result<(), Fail> }`, `Probe` (`request`, `send_raw`, `next_event`), `Answer` (`reply`, `failure`, `status`), `Connector` (`Listener` is one), `run(suite, connector, report) -> Summary`.

### d.6 What a program gets

On the kernel `moruna-svc` re-exports `main!`, `println!`, `Console`, `start`, `exit`, `Memory`, `DeviceMemory` and `Interrupt` from `moruna-sys`, and turns on `moruna-sys`'s `heap`, so a server written only against `moruna-svc` has its entry, its start handles, its console and `alloc`.

### d.7 Kernel calls added (ABI 1.1; SDD 20 d.2)

| nr | call | arguments | returns | rights needed |
|---|---|---|---|---|
| 24 | `resource_narrow` | resource, base, len | resource | `DUPLICATE` |

`base` and `len` are physical addresses (page aligned) for a device memory resource and GSIs for an interrupt resource; `len` is not zero; the range lies inside the source's (`AccessDenied` otherwise); a process resource is `InvalidArgs`. A root device resource covers `0 .. 2^52`, a root interrupt resource GSIs `0 .. 256`.

`moruna-sys` gains `Handle::narrow`, `Channel::write_with` (moves the handles of a slice of `Option<Handle>`, leaving each `None` on success and in place on failure), and the feature `heap`.

## e. Data model, formats and state machines

### e.1 Frames

```text
offset 0  u8   kind      1 Hello, 2 Welcome, 3 Refuse, 4 Request, 5 Reply, 6 Failure, 7 Event, 8 Connect
offset 1  u8   reserved  zero
offset 2  u16  ordinal   a request's or event's; zero otherwise
offset 4  u32  txid      a request's id, echoed by its answer; zero otherwise
offset 8  body
```

| kind | sent by | body |
|---|---|---|
| Hello | client, first | name (`u8` length, bytes), `u16` major, `u16` minor |
| Welcome | server | `u16` major, `u16` minor (the server's) |
| Refuse | server, then it closes | `u32` reason (1 unknown protocol, 2 another major, 3 not a hello), then the server's name, major and minor as in Hello |
| Request | client | the arguments in order |
| Reply | server | the reply value |
| Failure | server | `u32` code: 1 to 65535 the protocol's error; `0x10001` unknown method, `0x10002` bad message, `0x10003` reply too large |
| Event | server | the fields in order |
| Connect | to a listener | empty; exactly one handle, a channel endpoint |

### e.2 Bodies

Little endian, fixed width: `u8` to `u64`, `i8` to `i64` as themselves; `bool` one byte, 0 or 1; `()` nothing; `String` and `Vec<T>` a `u32` count (at most `limits::CHANNEL_MAX_BYTES`) and the items; `Option<T>` a tag byte, 0 or 1, and the value; tuples of up to four their fields; `Handle` and `Channel` (and `Memory` on the kernel) a `u8` index into the frame's handles, checked for its object type on decode.

### e.3 A connection's states

`opened` → (`Hello` welcomed) `serving` → (either side closes, a protocol violation, or a full client queue) `closed`. In `opened` anything but a well-formed `Hello` is refused (`NotHello`) and closed. In `serving` a client may send only `Request` with a nonzero id.

### e.4 The boot image description

```text
moruna-image 1
process NAME [PROGRAM]
  handle SOURCE SLOT [rights=R,...]     SOURCE: console | root.process | root.device | root.interrupt | program:PROGRAM
  expect TEXT
  exits CODE
  faults TEXT
link PROCESS.SLOT[:R,...] PROCESS.SLOT[:R,...]
expect TEXT                             (before any process: a line of the boot itself)
```

`#` begins a comment line. A process's program is the ELF `PROGRAM` (default `NAME`) in the programs directory, placed at `bin/PROGRAM`. `program:PROGRAM` gives a read-only memory object holding that program's image, for a process that starts others. A `link` is a channel named `PROCESS.SLOT` of its first end (or `linkN` when that is not a valid name or is taken), endpoint 0 to the first end, 1 to the second, each with its own rights. `exits CODE` expects `kk: process NAME exited with code CODE (`, `faults TEXT` expects `kk: process NAME faulted: TEXT`. The tool adds the kernel's boot line (`... abi 1.1: started N processes from the manifest`) and power-off line to the expectations.

## f. Algorithms and policies

### f.1 The handshake

The client sends `Hello` and waits for one frame. The server accepts a `Hello` whose name and major are its own, records the client's minor, and answers `Welcome` with its own version; otherwise it answers `Refuse` with the reason and its own identity, and closes. A client takes `Welcome` with the same major (anything else is `Malformed`) and records the server's minor.

### f.2 The dispatch loop

Each turn waits on every channel for `READABLE` or `PEER_CLOSED`; for each that is ready it reads up to 16 frames. On a listener, each `Connect` brings an endpoint, served with a fresh handler from the listener's factory; a malformed frame or a closed peer closes the listener. On a connection, the first frame is the handshake (f.1); after it, each `Request` is decoded and handed to the handler, whatever handles the handler did not take are closed (SV-I6), and the answer is sent with the request's id. A `Reply` that does not fit the bound becomes `Failure(ReplyTooLarge)`. A send that finds the client's queue full, a closed peer, or a frame a client may not send closes the connection (SV-I7). New connections that arrive while the dispatcher holds 64 handles are closed. A watched object is called with the signals observed when one it waits for is raised, and dropped when it says so. A handler that defers gets a `Pending` (its connection and the request's id), which the connection records; the answer, given through `Context::complete` or a watch's `Completions` during a turn, or `Dispatcher::complete` between turns, is sent at the end of the turn, once (SV-I16).

### f.3 Calls

`send` assigns the next id (1 upward, skipping 0 on wrap) and records it in flight. `reply(id)` returns the filed answer for `id` or reads frames until it arrives: an `Event` is queued, an answer to an id in flight is filed, anything else is `Malformed`. A `Failure` becomes `Remote`, `Status` or `Malformed` by its code.

### f.4 Listeners

A server's start page may hold several listener endpoints; the example server serves every slot named `listen*`. A client's endpoint to a listener is its only way to the server, so the description's `link` lines are the access policy: a process can reach exactly the servers it is linked to.

### f.5 The conformance harness

A suite's own cases cover every request with valid and invalid arguments. `run` adds, from the id and ordinals alone, for every protocol:

| check | passes when |
|---|---|
| handshake | `Welcome` with the suite's major and a minor not below the suite's |
| another protocol, another major, a request before `Hello` | `Refuse` with that reason and the server's identity, then the connection closes |
| an older and a newer minor of the same major | `Welcome` |
| an unknown request | `Status(UnknownMethod)`, twice on one connection |
| every request, with a body of `0xff` filling the frame to the bound | `Status(BadMessage)` (no request's arguments decode from it: every length or count is too large, every bool and tag invalid, every handle index absent; a fixed-width request would need 65528 bytes of fixed fields); this is also the frame at the bound |
| over the bound | the SDK refuses a frame one byte over and a 65th handle, the kernel refuses a message one byte over (`Limit`), the server sees nothing, and still serves |
| handle transfer | a handle sent with an unknown request, and with every request's bad body, is closed (its peer sees `PEER_CLOSED`) |
| clients that disappear | eight clients that go: before `Hello`, after `Hello` before `Welcome`, and with requests unanswered (one carrying a handle); a new client is then served |
| a client that does not read | sending requests without reading, the connection is closed by the server within 4 x 64 requests; a new client is then served |

Each check runs on its own connection, so one failure does not spoil the next. A suite is passed when every check passes.

### f.6 The heap

`moruna-sys` feature `heap` installs the global allocator: eight size classes (16 to 2048 bytes, each a free list through its free blocks) carved from 64 KiB chunks made by `memory_create` and `memory_map` (the memory object's handle is closed; the mapping keeps the pages); a larger block is its own mapping, unmapped by `dealloc`. Alignment up to a page; a larger one is refused (null). One spin lock.

### f.7 The host model

`moruna_svc::sim` is the channel ABI on a host: channels with the kernel's bounds, depth, moves, own-endpoint refusal, the cycle rule and `PEER_CLOSED`; `wait` with deadlines over a change counter and a condition variable; `needed` and `read` as `moruna-sys`'s. It has no rights, no handle values and no objects but channels and an inert object for tests. `moruna_svc::os` exports `moruna-sys` on the kernel (`target_os = "none"`) and `sim` elsewhere, and the rest of the SDK names only `os`.

### f.8 The boot image tool

`moruna-image DESCRIPTION PROGRAMS OUT` parses the description (refusals by line), builds the `Manifest`, renders it and parses it back with `Manifest::parse` and compares, reads each program and parses it with `moruna_loader::elf::parse` against `IMAGE_RANGE`, writes a newc archive (directories, `bin/*`, `manifest`) and reads it back with `moruna_loader::cpio::files`, and writes `OUT/initramfs.cpio`, `OUT/manifest` and `OUT/expect`. `build.sh` builds the kernel, then every userland crate of the workspace in one invocation (the user linker script `kernel/sys/user.ld` passed as link arguments), then runs the tool. `check-boot.sh LOG STATUS EXPECT CPUS` requires every expected line, `kk: CPUS CPUs online`, `kk: process threads ran on CPUS of CPUS CPUs`, no `FAIL:`, no refused boot, no panic, and QEMU status 33.

### f.9 Level-triggered interrupts (SDD 20 m.1)

Not possible on OSTD 0.19, for three reasons read from its source: `IoApic::enable` writes a redirection entry holding only the vector (trigger mode edge, polarity active high) and offers no way to set either; neither `IrqChip` nor `MappedIrqLine` has a mask or unmask (the only masking is `disable` when a `MappedIrqLine` drops, which also frees the line); and the chip's lock is an ordinary spin lock, so remapping a line from its own interrupt handler could deadlock against a task holding it. A PCI INTx line (level, active low on q35's GSIs 16 to 23) is therefore not a line a driver can use on OSTD 0.19 at all.

The plan, for KB3: virtio devices are PCI devices with MSI-X, and MSI-X is edge-like (a message per event, no line to mask). KB3 adds a call that allocates an OSTD IRQ line without routing a GSI and returns the interrupt object with the MSI message (address and data) a driver writes into its device's MSI-X table, which it reaches as device memory through a narrowed resource. `interrupt_ack` stays as it is. Lines through the I/O APIC remain for edge-triggered ISA devices.

### f.10 Narrowing a resource (SDD 20 m.3)

`Resource` carries a kind and a range. A root resource's range is its kind's whole space (e.4 of SDD 20 lists the roots); `resource_narrow` checks the source's type, its `DUPLICATE` right, the kind (a process resource is `InvalidArgs`), alignment and a non-empty range (`InvalidArgs`), containment (`AccessDenied`), and inserts a handle to the new resource with the source handle's rights. `device_memory_create` checks containment after its argument checks and before OSTD claims the range; `interrupt_create` checks the GSI is at most 255 (`InvalidArgs`), then containment. A supervisor gives an untrusted driver a resource over its device's pages and lines only, so the driver can claim nothing else even before OSTD's own checks.

### f.11 The cycle rule (SDD 20 m.4)

An endpoint in a message keeps that message's target endpoint's queue alive, and the target endpoint is itself kept alive by whoever holds it. Draw an edge from `E` to `F` when `F` is carried by a message queued towards `E`. A set of endpoints that no process holds and that is closed under these edges keeps itself alive: a leak that outlives every process, which SDD 20 m.4 understated.

Two rules were weighed: refuse sending a channel's own endpoint through itself (which the kernel already did, and which stops only the one-channel case), or collect cycles when a process ends (which needs a graph walk under every channel's lock, and finds nothing when the cycle belongs to no process). The rule taken is a third, which refuses every cycle at the moment it would form: a write may move endpoint `F` only while no message queued towards `F` carries an endpoint (`BadState` otherwise). An edge `E → F` is then only ever added towards an `F` with no outgoing edge, and adding an edge into a node with no outgoing edges cannot close a cycle; removing edges (a read, a close) cannot create one. So the graph has no cycle, every endpoint in flight is reachable from one some process holds or is dropped with its holder, and closing a holder closes everything it held.

Each channel counts the endpoints queued towards each end (`carried`); the count grows only under a global lock, `TRANSFER`, which a write takes after the caller's table and the channel, and only when it carries an endpoint, between its check and its count; it shrinks on a read and when an end closes. Holding `TRANSFER`, no other write can add an edge, so the check stays true until the count is made. The own-endpoint refusal stays (`InvalidArgs`). The rule's cost to a program: an endpoint with queued messages that carry endpoints must be drained before it is passed on.

### f.12 Several CPUs (SDD 20 m.2)

OSTD 0.19 boots every application processor before the kernel's entry runs and holds each in its boot context until the kernel registers an entry; its default scheduler queues every task on the boot processor. The kernel therefore injects its own scheduler (`core/src/sched.rs`) first:

- one run queue per CPU, each with an idle task that halts until an interrupt (`ostd::task::halt_cpu`, which handles a reschedule that arrives as it halts);
- a task that becomes runnable and names no CPU goes to the CPU with the fewest runnable tasks, the current CPU on a tie, read from per-CPU counters without locks; a task that still names a CPU (it is parking) goes back to it, as OSTD's own scheduler does;
- first in, first out on each CPU; a task that has run 10 ticks (10 ms) while another waits on its CPU is preempted; a CPU running its idle task is asked to reschedule when work is queued to it (OSTD sends the inter-processor interrupt);
- a CPU's boot context is never preempted and never chosen to reschedule; it moves to a task when it yields at the end of its entry, so the boot processor runs the whole boot.

The other CPUs enter through `ostd::boot::smp::register_ap_entry`, enable their time slice, and yield. The boot counts as a live process from before the first manifest process starts until it prints its boot line, so a process that ends on another CPU before the last one has started cannot power the machine off (SV-I15). The post-schedule hook records which CPU ran a process thread, per process and in all; the kernel prints `kk: N CPUs online`, each exit line ends `(it ran on cpus 0,2)`, and the power-off line is preceded by `kk: process threads ran on K of N CPUs`.

## g. Concurrency within the component

The SDK is single threaded per dispatcher and per client: a `Dispatcher` and a `Client` are used by one thread; a server that wants several threads runs a dispatcher in each, on different channels. The heap's lock is a spin lock held for a free-list operation or a chunk's creation. In the kernel, `TRANSFER` is taken last (after a process's table and a channel's state) and only by writes that carry an endpoint; a run queue's lock disables interrupts and is taken alone.

## h. Behaviour

A server ends when its dispatcher holds no channel: every client and every listener has gone. The CI boot runs every process of the description to its end, the echo server last among the example's, and powers off.

## i. Configuration

None at run time. The constants: 16 frames per channel per turn; the 10-tick slice; the heap's classes and chunk size; the description's format version 1.

## j. Observability

Processes print through their console handle. The kernel adds `kk: N CPUs online`, the CPUs each process ran on, and how many CPUs ran process threads.

## k. Tests

Host tests run with `cargo test` in `kernel/` for `moruna-svc` and `moruna-image`; boot tests run in CI's `kernel` job on 1, 2 and 4 CPUs.

| id | test | where | asserts |
|---|---|---|---|
| SV-T1 | `sv_t1_*` | `svc/tests/model.rs` | every `Wire` type round trips through a frame; each refusal of SV-I4; the encoder's bounds; names, ordinals, codes and `since` as the schema checks them; the acceptance rule |
| SV-T2 | `sv_t2_*` | `svc/tests/model.rs`, `service.rs` | a frame at the bound passes and one byte over is refused before sending (SV-I5); malformed frames; a typed call over the bound is `TooLarge` and the connection serves on |
| SV-T3 | `sv_t3_*` | `svc/tests/service.rs` | refusals with their reasons; minor negotiation both ways (SV-I1, SV-I2) |
| SV-T4 | `sv_t4_*` | `svc/tests/service.rs` | typed calls, typed errors, statuses, handles in requests, events before replies, replies matched out of order (SV-I3), a gone server |
| SV-T5 | `sv_t5_*` | `svc/tests/service.rs`, `driver.rs` | several protocols and listeners on one dispatcher; protocol violations close; the 64-handle bound; timeouts; a driver's shape: an interrupt watched beside the clients, requests answered when it fires, a deferred request answered once and only to a live connection (SV-I7, SV-I8, SV-I16) |
| SV-T6 | the schema's `const` assertions | every `protocol!` use | a declaration that breaks SV-I9 does not compile; the `const fn`s are tested in SV-T1 |
| SV-T7 | `sv_t7_*` | `svc/tests/service.rs` | the reference server passes its suite; a server that answers everything, one that keeps handles, one of another protocol, and one that never answers each fail the checks aimed at them |
| SV-T8 | `sv_t8_*` | `svc/tests/model.rs` | the host model keeps the kernel's channel rules, the cycle rule included, and `wait`'s |
| SV-T9 | `sv_t9_*` | `tools/image/tests/image.rs` | a description renders the manifest the kernel parses; refusals by line; programs checked; the archive reads back; the command writes the image and reports failures (SV-I10) |
| SV-T10 | the echo exchange | boot: `echo-client` | `KB2 echo: ok:` and `KB2 echo-server: ok:`; a typed call, an event, a typed error, a channel sent and written through, a 60000-byte echo, a 70000-byte echo refused before sending |
| SV-T11 | the echo suite in the VM | boot: `echo-conformance` | `KB2 conformance moruna.example.echo: ok: N checks passed against echo-server` |
| SV-T12 | several CPUs | boot on 2 and 4 CPUs | every KB1 and KB2 line, `kk: N CPUs online`, `kk: process threads ran on N of N CPUs`, the supervisor's success (SV-I14, SV-I15) |
| SV-T13 | narrowing | boot: `supervisor`, `driver` | `KB2 narrowing: ok:`: the driver claims its page and line and is refused the pages either side, the next line, wider resources and each resource for the other kind; the supervisor is refused narrowing the process resource and an unaligned window (SV-I12) |
| SV-T14 | the cycle rule | boot: `cycles` | `KB2 cycles: ok:`: own-channel refusal, refusal while the queue carries an endpoint, passage once drained, and closing the holder closes the held endpoint (SV-I13) |
| SV-T15 | the examples touch no kernel file | CI: `tools/check-examples.sh` | SV-I11 over every commit of the pull request |

Coverage on the host (`cargo llvm-cov`): `moruna-svc` 93.3% of lines, `moruna-image` 97.4%. The kernel additions are covered by the boot tests above.

## l. Implementation notes for the agent

- Clippy for the userland crates runs only for `x86_64-unknown-none` in CI (the examples and `moruna-sys` do not build for a host).
- `build.sh` excludes the kernel crates and the tool from the userland invocation so the kernel's features are not unified into programs.
- The workspace's `examples/*` member glob means a new example crate needs no change to `kernel/Cargo.toml`, only to `kernel/Cargo.lock`.

## m. Open items

1. **MSI-X for KB3.** The call that allocates an interrupt line for a message-signalled device and returns its message (f.9) is KB3's, as is the PCI configuration access a virtio driver needs.
2. **Partial unmap and protection changes** stay KC1's (SDD 20 m.5).

## n. Traceability

| requirement (BOARD EK KB2 brief) | here |
|---|---|
| a protocol: named, versioned requests, replies, events, handles | d.1, e.1, e.2 |
| one schema format with versioning, compatibility checks, generated types, no generator binary, justified | d.2, SV-I9 |
| handshake with refusal; request ids; typed errors; handles in messages; bound refused at the sender | f.1, f.3, d.3, e.2, SV-I1 to SV-I5 |
| dispatch loop over many channels and other handles with `wait`; typed proxies | f.2, d.3, d.4 |
| a userland heap | f.6 |
| boot manifest tooling with `moruna_abi::manifest`, replacing the hand-written manifest, built in CI | e.4, f.8; `kernel/initramfs/boot.image` |
| the conformance harness, one suite per protocol | f.5, SV-T7, SV-T11 |
| narrowing (m.3), several CPUs (m.2), level-triggered lines (m.1), handle cycles (m.4) | f.10, f.12, f.9, f.11 |
| the echo example, its CI boot, and the check that it touched no kernel file | `kernel/examples/`, SV-T10, SV-T11, SV-T15 |

## o. Deferred

MSI-X and PCI configuration access (KB3); drivers' protocols (KB3); a dispatcher across threads; a schema for protocol evolution across majors (a new major is a new protocol).
