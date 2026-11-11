# Moruna SDD 30: The engine on its kernel (`crates/moruna-micro`, `kernel/engine`, ABI 2.2, `moruna.linux` 1.3, `moruna.imagefs` 1.2)

**Document type:** software design document, component 30
**Status:** DRAFT · 2026-10-29 (becomes HANDOFF-READY when section m is empty; the human flips it)
**Parent:** `BOARD.md` section EK, item KD2 (Brackly, 2026-10-28): "the engine as the first process after the supervisor", with the brief of 2026-10-29: one engine codebase for Linux nodes and the kernel, the lowest latency and the highest throughput; the supervisor starts the engine from the boot manifest with the vsock to the embedder, the image filesystem, a memory budget and the right to create processes and slots; the job arrives over vsock in the host protocol; tasks are processes driven through `KernelPlane` behind `DataPlane`; port calls forwarded to the embedder; a boot test of a real job; `architecture/optimized-runtime.md` sections 5 and 8
**Decisions:** EN1 (the engine is the root workspace's engine, an unmodified Linux program the personality runs with granted handles; f.1); EN2 (the vsock is a vhost-user device whose back end is moruna-vmm's vsock device in a host program, so a boot needs no host kernel module; f.11); EN3 (an image's identity is checked on the kernel against the record the launcher writes and the layers the image filesystem mounted, and the verified config gives the command; f.12); DP1 and MK1 stand
**Component location:** `kernel/core/src/plane.rs` (a slot's `SIGNALED`), `kernel/linux/server/src/{plane,events}.rs` (descriptors answered later), `crates/moruna-micro` (the engine's side: the start page, the vsock transport, an image stage as a task of the data plane), `crates/moruna-runtime` (`src/bin/moruna-engine.rs`; `host/transport.rs`, `kinds/image.rs`, `packaged.rs` choose the kernel's adapters there), `kernel/abi`, `kernel/core/src/{process,syscall}.rs`, `kernel/sys` (ABI 2.2: `process_grant`, `Process::grant`, `Mapping::bytes`; the crate built for a Linux target), `kernel/svc` and `kernel/plane/core` (feature `native`; `KernelPlane::load_with`), `kernel/linux/{core,protocol,server}` (`moruna.linux` 1.2: `run_granted`, the start page in the auxiliary vector, a machine's memory, eventfd, epoll, rename, a program ending with its connection), `kernel/imagefs/{fs,protocol,server}` (`moruna.imagefs` 1.1: `rename`; 1.2: `layers`, a layer's seal), `kernel/linux/{protocol,server}` (`moruna.linux` 1.3: `layers`), `crates/moruna-image/src/{record,launch,derive}.rs` and `moruna-layer derive`/`stack` (the attached image's record; an image's process from its config; the boot's derived image), `crates/moruna-vmm/src/vhost_user/` and `moruna-vhost-vsock` (the vsock's vhost-user back end), `kernel/engine` (the engine's supervisor), `kernel/linux/programs/{granted,tap,badrecord}.c` and `kernel/linux/programs/kd2/` (the boot's tasks), `crates/moruna-testkit/src/bin/kd2-fixture.rs`, `kernel/tools/{build,boot,check-boot}.sh`, `kernel/tools/{embedder,vsock-peer}.py`, `kernel/initramfs/{boot.image,engine.conf}`
**Consumes:** SDD 20 (ABI 2.1), SDD 23 (`moruna-svc`), SDD 25 (`moruna.driver.vsock`, the directory), SDD 27 (the image filesystem), SDD 28 (the Linux personality), SDD 29 (the data plane, `KernelPlane`), MH 4.2, 4.3, 4.10 (the host protocol and its ports), SDD 19 (the `image` kind's contract). **Consumed by:** KD3 (conformance across adapters), KD4 (the launcher's devices), KD7 (the scheduler on the plane).

**Decisions worth your eye:** (1) the engine on the kernel is the same engine as on a Linux node, built static for musl and run by the Linux personality; it is not a second engine and not a fork (EN1, f.1); (2) a Linux program may hold handles only when its starter grants them, which is the engine and nothing else in the boot, and it reaches the kernel's own calls on them with the `syscall` instruction, as a task already reaches `batch_get` (f.1); (3) the engine is told its budget as its machine's memory, which it discovers as it does inside a Linux guest (f.3); (4) an image stage's batches move as pages between the engine's plane and the task, and the engine pays one copy in and one copy out at the stage boundary, the same count as on Linux, until the scheduler's arena is the plane's (f.6, m.1); (5) the data disk has no layout on the kernel, so a sink's files stay in the engine's scratch directory, and the boot judges the rows at the tap before the sink (f.9, m.3); (6) the vsock is QEMU's vhost-user device served by moruna-vmm's own vsock device in a host process, so the proof runs on any host QEMU runs on, with no `/dev/vhost-vsock` (EN2, f.11); (7) an `image` entry on the kernel names its image by manifest digest and is refused, naming both digests, when the image attached is another; its command and environment then come from the verified config (EN3, f.12).

---

## a. Purpose and boundary

A run on the Moruna kernel is one engine and its tasks in one VM (optimized-runtime 8). This component makes the engine that process: its start from the boot manifest, its handles, its budget, its connection to the embedder, and its `image` stages as tasks of the data plane. It owns: the rule for which process may hold handles and how a Linux program reaches native calls (`process_grant`, `run_granted`, the start page in the auxiliary vector); the engine's supervisor and its configuration; the engine's adapters to the kernel (`moruna-micro`): the host protocol's transport over the vsock driver, an image stage over `KernelPlane` and the personality's `run_plane`, its port calls carried to the host protocol's ports; what the personality adds so the engine runs unmodified (eventfd, epoll, rename, a machine's memory, a program bound to its connection).

It refuses to know: how a VM is launched and its layers attached (KD4), what the scheduler decides (KD7: today's scheduler drives the stage), the data plane's mechanism (SDD 29), the image filesystem's format (SDD 27), and any embedder: the test embedder is the host's, outside the VM.

## b. Vocabulary

**Engine.** Moruna's runtime (`moruna-runtime`) and its command line, `moruna-engine`. **Supervisor (the engine's).** The native process the manifest starts that starts the engine (`kernel/engine`). **Grant.** Handles a starter moves into a process before it runs, named on a start page. **Granted program.** A Linux program started with a grant; the engine. **Task.** One process of a stage, started by the personality's `run_plane` with a plane. **Embedder.** The program on the host's side of the vsock that sends a job and serves host ports (MH 4.3). **Machine.** What a program is told it runs on: its memory budget and the CPUs online.

## c. Invariants

- **EN-I1. One engine.** The engine on the kernel is built from the same crates as on a Linux node, with the same command line; what differs is the adapter each port of the engine chooses when it finds a start page (the transport, the image stage), and nothing else (f.1).
- **EN-I2. Handles only by grant.** A Linux process holds no handle unless its starter granted it; it holds exactly what it was granted, named on its start page (SDD 28 LX-I2 as amended; KD2-T4, KD2-T8). A task holds none (SDD 29 DP-I2).
- **EN-I3. A grant is whole or nothing.** `process_grant` moves every handle it names or none, once per process, before the process runs; a handle without `TRANSFER` is refused (KD2-T8).
- **EN-I4. A program lives no longer than the connection that asked for it.** Closing a `moruna.linux` connection ends every program its requests started; a cancelled run's tasks end with it (f.8; KD2-T9 run 4).
- **EN-I5. A task's end, a refused record and a cancel are the stage's, as on Linux.** Each ends the stage with one terminal event that names its cause: the task's exit status or fault and the last lines of its standard error, the check's sentence, `cancelled` (f.8; KD2-T9 runs 2 to 4).
- **EN-I6. A port call reaches the embedder or fails the stage.** A task's port `n` is the `n`th of its entry's ports in name order; its request goes to the host protocol as `port_call`, and the answer comes back as a read-only memory object; an unlisted number, a lost call, a deadline and a cancel fail the stage, naming the port (f.7).
- **EN-I8. A task runs only the image its entry names.** Before an image stage's task is made, its reference (a manifest digest) is checked against the attached image's record, the record against itself (the config's and the layers' digests) and the record's layer seals against the seals of the layers the personality's image filesystem mounted, every page of which it checks against the root its seal covers; a mismatch refuses the document (exit 2) naming what was named and what was found (f.12; KD2-T9 run 5, KD2-T13).
- **EN-I7. No blocked call holds up another task.** A call the personality answers later (`epoll_wait`, a read of an empty eventfd, a plane task's read of descriptor 0 before its next batch, its write of descriptor 1 before its window) is answered when it can be or at its deadline, while the server goes on serving every task; the server never waits inside a request for the engine, which is one of its programs (f.4, f.10).

## d. Interfaces

### d.1 `process_grant` (ABI 2.2, SDD 20 d.2 row 49)

`process_grant(process, entries, count) -> address`: the handles `entries` names (40-byte `start_entry`s, as `process_start`'s) move into the created process, which has not run, under their slot names, and a start page naming them (SDD 20 d.4) is mapped into it read only where the kernel chooses; the page's address is returned. Once a process; `process_start` refuses a granted process. `moruna-sys`: `Process::grant(&mut [(Option<Handle>, &str)]) -> Result<u64>`. The ABI version is 2.2.

### d.2 `moruna.linux` 1.2 and 1.3

```rust
// moruna.linux 2.0 (KB5): old major is refused, no legacy body fallback.
3 => fn run_granted(path: Bytes, argv: Vec<Bytes>, env: Vec<Bytes>, grant: Grant) -> Ended;
Grant { handles: Vec<(Bytes, Handle)>, account: Handle, scratch: Option<Handle> }
errors { ...; 6 BadGrant; }
pub const AT_MORUNA_START: u64 = 0x4d4f_5255;
// 2.0 base requests include layer seals.
4 => fn layers() -> Vec<Bytes>;
errors { ...; 7 NoImage; }
```

The program runs as `run` runs it, its process granted `grant.handles` (d.1); its auxiliary vector carries the start page's address under `AT_MORUNA_START`, a type outside every one Linux defines, which `getauxval` reads and a C library ignores; `moruna_sys::start::init` takes it. `grant.account` is the actual runtime account charged by the kernel; `sysinfo` and `/proc/meminfo` report that account's limit/usage. `grant.scratch` carries the actual separate scratch account. The native engine always supplies it; missing/zero scratch configuration is refused rather than falling back to the filesystem service's account. Every request since 1.0: closing the connection ends the programs it started (EN-I4). In major 2, run_plane/run_granted/layers are base-minor requests and old-major bodies are refused. `layers` answers the seals of the layers every program the server runs is started on, lowest first, 32 bytes each, as its image filesystem's `layers` (`moruna.imagefs` 1.2: `16 since 2 => fn layers() -> Vec<Bytes>`) says them: each the SHA-256 of a layer's header page as it was mounted (`Header::seal`), re-made from the header the cache checked; `NoImage` when the image filesystem does not answer, the reason on the console.

### d.3 The engine's supervisor (`kernel/engine`, boot manifest)

A native process with the slots `console`, `config` (a data handle), `linux` (a personality listener) and `directory` (the device manager's). `config` is text (`engine::Config`): `program PATH`, `arg A` (each, in order), `env NAME=VALUE`, `memory BYTES`, `scratch BYTES`, `grant SLOT lookup SERVICE` (a new connection to a directory service), `grant SLOT personality` (a duplicate of the personality's listener), `grant SLOT data FROM` (a read-only duplicate of the supervisor's own data handle `FROM`, taken from its start page once), `grant SLOT service FROM` (a fresh `moruna_svc::connect` connection per run from an explicitly boot-granted supervisor listener `FROM`, with no directory/data fallback), `runs N`. Every line is checked and a refusal names its line. For each run it makes the grants fresh, calls `run_granted`, and prints `KD2 engine: run N of M: the engine exited with code C; S calls served; refused: ...`; then `KD2 engine: ok: M runs, each ended by its own exit`. A grant that cannot be made (no `vsock0` among them) fails the run with its reason. The CI boot's configuration is `kernel/initramfs/engine.conf`: `/moruna/bin/moruna-engine serve --listen vsock://-1:5100`, 1 GiB of memory, 256 MiB of scratch, `vsock` from `vsock0`, `linux` from the personality, `image` from the data handle `image` (`linux/image.record`, f.12), five runs.

The process resource is not granted: no kind the engine runs starts a native process yet (every task is started by the personality), and a grant the engine does not use is authority it does not need (m.5).

### d.4 The engine (`moruna-engine`, `moruna-micro`)

`moruna-engine` is `moruna_runtime::host::cli::main` over `Registry::standard()`: the command line of MH 4.2 without the embedded Python adapter (a Python stage on the kernel is an image whose interpreter is in the image). `moruna-micro` gives the runtime:

- `on_kernel() -> bool`, and `start::{kernel, take, slots}`: the start page found through `getauxval`;
- `vsock::Driver::open(slot)`, `Driver::{listen, connect}`, `Listener::accept`, `Stream::into_parts` (a `Read`, a `Write`, a closer): the host protocol's transport (`host/transport.rs` takes it for a `vsock://` address when `on_kernel()`);
- `image::attached()`: the attached image's record (the granted data handle `image`) and the personality's `layers`, read once a process;
- `task::ImageTask::new(name, identity, path, command, env, ports)` (`moruna_kernel::Kernel`): the `image` kind's stage on the kernel (`kinds/image.rs` makes it when `on_kernel()`, from a verified `moruna_image::Launch`; `Packaged::Task`), with `bind_ports`, `bind_run_ports`, `bind_run`;
- `layout`: the windows, record lengths and port numbering, tested on every target.

The `image` kind's settings gain `ports` (MH 4.10): on the kernel the stage's task calls them; on a Linux node an entry with `ports` is refused, since its process has none.

## e. Data model, formats and state machines

### e.1 A granted program's start

`run_granted` → the personality makes the process and its forward (SDD 28 d.1), `process_grant`s the handles (its page's address kept), sets the scratch quota, loads the program, builds its first stack with `AT_MORUNA_START` beside Linux's entries, launches it. The program's runtime starts as any Linux program's; the engine's first look at `moruna_micro::start::kernel()` reads the auxiliary vector once.

### e.2 An image stage's task on the kernel

`init`: a `KernelPlane` of the stage's own, its arena bounded by the run's ceiling and 1 GiB; one input slot, one output slot, a port when the entry lists ports; a log ring of 64 KiB; a connection to the personality, and `run_plane` sent on it (not waited for). Each `apply`: offered → windowed → put (or refused, or ended). `finish`: the input slot finished → the task's end, then the personality's `Ended` (exit 0 required). The connection's close (the state dropped) ends the task.

## f. Algorithms and policies

### f.1 How the engine runs on the kernel (EN1)

The engine is the root workspace's engine: `moruna-runtime`, its scheduler, sources, sinks, placement, controller, reactor and host protocol, in one static musl program, which the Linux personality runs unmodified. The personality grants it its handles (d.1, d.2); everything the engine does as a Linux program (threads, files, clocks, its tokio event loop) goes through the personality as for any program; what only the kernel has (the vsock driver, the personality's `run_plane`, slots and memory objects) it reaches with the kernel's own calls on those handles, made with the `syscall` instruction at the native numbers, as a task makes `batch_get`. `moruna-sys`, `moruna-svc` (feature `native`) and `moruna-plane` (feature `native`) build for the engine's Linux target and are the same code the native servers use.

Why not a native process. A native engine would be `no_std`: the scheduler, the sources and sinks, Arrow, Parquet, the job document, the host protocol and the controller rewritten without `std`, or a port of Rust's standard library to the kernel's calls (a target of its own, a `sys` layer kept outside the toolchain, nightly only). The first is the second engine the brief forbids; the second is a fork of the standard library to maintain, for what the personality already gives. What a native engine would buy is the forwarded round trip on the engine's file and thread calls (45 to 82 us under TCG, SDD 28 f.1) and none on its hot path: the data plane's calls, the slots, the memory objects and the waits are native calls from the Linux program too, with no forward, so a batch moves at the same cost from either; the clocks, futexes and memory calls are the kernel's fast calls. What it costs the personality is what any Rust program built on tokio needs: eventfd and epoll (f.4), rename (f.9), the machine's memory (f.3).

### f.2 The host protocol's connection

The engine holds one connection to `vsock0`, granted under `vsock`, whose streams and listeners belong to it (SDD 25 d.5). One thread owns it: requests reach it through a queue and a doorbell channel, and it waits in one kernel `wait` on the driver's channel and the doorbell, sending every request as it arrives and routing every answer by its request id, so a `recv` the driver answers only when bytes arrive and a `send` from the writer's thread are in flight together. A read takes up to 60,000 bytes a `recv`; a write sends up to 60,000 bytes a `send` and fails after 5 s without credit, as the Linux socket's `SO_SNDTIMEO` does (MH 4.3); closing sends `close` and does not wait. `moruna-engine serve --listen vsock://-1:PORT` listens on PORT for any cid.

### f.3 The machine's memory

A program granted `memory` is told its machine has that much: `sysinfo`'s `totalram` and `freeram`, `/proc/meminfo`'s `MemTotal`, `MemFree`, `MemAvailable` (no swap); `/sys/devices/system/cpu/online` lists the CPUs online for every program. The engine's discovery reads these as it does inside a Linux guest, where the machine is the budget (MH 2.1); a run's `budget.memory_bytes` still wins. A program started without a budget is told nothing: `sysinfo` is refused and `/proc/meminfo` is not found. KB5 now charges the actual process, pages, objects and reservations to that account and enforces its limit. There is no advertised-only memory ceiling.

### f.4 eventfd and epoll

`moruna_linux::events`: an eventfd is a 64-bit counter (`write` adds, refused at `u64::MAX`, blocks past `u64::MAX - 1`; `read` takes it, or one with `EFD_SEMAPHORE`, and blocks at zero); an epoll instance is an interest list keyed by descriptor and open file description, so a number closed and opened again is another file and its entry goes. Level-triggered entries report while ready, edge-triggered ones once a change (an eventfd's generation moves on every read and write), one-shot ones once until `EPOLL_CTL_MOD`; `EPOLLERR` and `EPOLLHUP` are reported unasked. An eventfd is ready by its counter; a stream or a device for what it is opened for, as `poll` answers; a regular file or a directory is `EPERM` to add, as on Linux; an epoll instance inside another is refused and counted. A call that would block is answered later (EN-I7): the thread waits on its forward, the server records the call, answers it when a later call of the task readies it, or at its deadline, which the main loop's `turn` ends by. `readv` and `pread` of an eventfd take it without blocking (an empty one is `EAGAIN`, counted); `epoll_pwait`'s signal mask is ignored, since no signal is delivered (SDD 28 m.2).

### f.5 An image stage on the kernel

As SDD 19 f.6 and f.7: one record in, one record out, in lockstep; the task's own schema is its first record's. The task is started with `run_plane` on its own connection: its descriptor 0 reads the input slot as an Arrow IPC stream, its descriptor 1 writes the output slot as one (SDD 29 f.8), its standard error is the log ring, which the stage drains into the run's output (MH 4.3 `log`) and keeps the last 20 lines of. On the kernel the entry's `reference` is the image's manifest digest, checked before the task is made (f.12); the task's command, environment, working directory and user are the verified config's, the entry's `command` replacing the command and its `env` added to the environment, as on a Linux node (`moruna_image::Launch`, which the sandbox uses too); the command's first word is looked for on the image's `PATH` in the engine's own view of the image, which is the same image filesystem its tasks start on. An image whose user is not root, or whose working directory is not `/`, is refused: the personality starts every program as root in `/`. The layout is a Linux node's and is not read, and the run's notes say so; `cache` is refused.

### f.6 Windows and copies

Before each batch is offered the task holds one window for its answer, `2 × input + 1 MiB` in whole pages, at most 256 MiB, the kernel's largest memory object; one window is always granted ahead, so the personality's `plane_get`, which waits inside the request (SDD 29 m.2), never waits on the engine. The input batch is laid out as a record by `moruna_kernel::ipc::encode_framing` written straight into an arena object (`KernelPlane::load_with`: the framing and each buffer copied once, the gaps zeroed, through one writable mapping); the plane checks it; the task maps its frames read only. The record the task puts is checked by the plane (DP1), mapped read only into the engine and decoded in place (`Mapping::bytes`, an Arrow buffer owning the mapping), then landed in the run's arena (`land_table`, the one copy of every image's output, as on Linux), after which the engine's hold is dropped and the object goes back to the pool. A batch between the engine and a task is never copied by the kernel; a batch between two image stages crosses the run's arena, which is the scheduler's (m.1).

### f.7 Port calls

A task calls `port_call(n, request)`; the plane gives the engine the call; the stage names it by its entry's ports in name order (`layout::port_name`) and calls `StagePorts::call`, which says the stage waits (`task_waiting`, `port_call`), sends the host protocol's `port_call` and waits for `port_reply`, its deadline or the run's cancel (MH 4.10). An answer is written into a memory object and mapped into the task read only; a refusal is `AccessDenied` to the task and a line of the stage's standard error; a lost call, a deadline, a cancel and an unlisted number fail the stage.

### f.8 A task's end, a refused record, a cancel

The plane says a task has ended once every slot and its port have given everything back (SDD 29 f.6); the stage then takes the personality's `Ended` for it (waiting up to 60 s) and words the failure as on Linux: `image `NAME` exited with status C`, `... was ended by signal S`, or `... was ended by the kernel (CAUSE; the kernel's console line says why)`, with the last lines of its standard error. A record the check refuses ends the task by `slot_refuse` with the check's sentence, which the stage's failure carries. While it waits on its task the stage looks every 250 ms at the run's cancel, and at the personality's answer, so a command the personality could not start fails the stage with why; an answer of the task's own wakes it at once. A cancelled run drops its stages' states: each closes its connection to the personality, which ends its task (EN-I4).

### f.9 Rename, and where a sink's files go

`moruna.imagefs` 1.1 adds `rename` within the scratch directory, with Linux's meaning (`moruna_imagefs::scratch::Scratch::rename`): every sink commits a file by renaming it from its temporary name. `rename`, `renameat` and `renameat2` with `RENAME_NOREPLACE` are served; `RENAME_EXCHANGE` and `RENAME_WHITEOUT` are refused and counted. The data disk has no layout on the kernel yet (optimized-runtime 4 and 7 have the engine write it through virtio-blk, and nothing says how), so a sink's files are the engine's scratch directory's and leave with the VM; the boot judges the rows at the tap, the last task before the sink, which hands every byte of its stream to the embedder (m.3).

### f.10 A plane task's descriptors never stop the personality

KD1's personality served a `run_plane` task's `read(0)` and `write(1)` with `plane_get`, which waits in the kernel for the engine's push (SDD 29 m.2), so one waiting task stopped every other. With the engine one of the personality's programs that is a deadlock: two image stages in a row, the downstream task waiting on descriptor 0, stop the upstream task's descriptors and the engine's own file calls that the next batch depends on. A slot now raises `SIGNALED` on its handles while its task's `batch_get` would not wait (something queued, the slot closed, its task ended; ABI 2.2); the personality keeps its handles to a task's slots (which carry `WAIT`) and looks at `SIGNALED` before it calls `plane_get`. A read that needs the next batch, and a write whose record needs a window, are answered later, when the server's watch on the slot sees `SIGNALED`; a write's bytes are taken at once and the write is answered when they are put, so a task is held back by its windows as by a pipe. `readv(0)` reads into its first buffer (a short read), `writev(1)` is one write, `pread` and `pwrite` are `ESPIPE`. A task is started only while its waited handles fit the server's one `wait` of 64 (two, and one a slot), and is refused `Busy` otherwise.

### f.11 The vsock, a vhost-user device (EN2)

Every boot's machine has its vsock as QEMU's `vhost-user-vsock-pci` (guest cid 3), its flags `machine` lines of the boot description (`kernel/initramfs/boot.image`): the guest's memory a shared `memfd` (`-object memory-backend-memfd,share=on`, `-numa node,memdev`), since a vhost-user back end maps it; the device over a `-chardev socket` that `boot.sh` gives a back end to before QEMU starts. The back end is `moruna-vhost-vsock --socket PATH --cid 3 --uds PATH` (`crates/moruna-vmm/src/vhost_user/`, built by `build.sh` for the host into the root workspace's target directory, where the next build finds it built): the same `VirtioVsock` device and muxer `moruna-vmm` runs in its own VMs, driven by vhost-user messages instead of MMIO. It takes `GET_FEATURES` (`VERSION_1` and `PROTOCOL_FEATURES`), the protocol feature `CONFIG` (the guest's cid by `GET_CONFIG`), `SET_MEM_TABLE` (each region's descriptor mapped), the two rings' size, addresses (the front end's addresses translated through the regions), bases, kicks, calls and enables; QEMU keeps the event queue. A ring runs once both are started and enabled; a kick serves its ring, the muxer's sockets fill the receive ring, and every used buffer signals the rings' call eventfds. One thread, one `poll`. The host side is the muxer's: a host process connects to the `--uds` socket and writes `CONNECT <port>\n` (answered `OK <host port>\n`) to reach a guest port, and a guest connection to host port `P` reaches the socket `<uds>_P`; `vsock-peer.py` and `embedder.py` speak that. The version is the workspace's (`moruna-vmm`, its lockfile): nothing is fetched. CI's QEMU is built with `--enable-vhost-user`.

The confined host back end drops the server's resources and ends with `_exit`.
No C `atexit` callback may open files or change process state after its syscall
filter has been installed. Linux coverage uses continuously mapped LLVM counters
with runtime counter relocation, so the real confined process remains measured
without adding profiler syscalls to its production filter.

### f.12 An image's identity on the kernel (EN3)

The launcher that attaches an image's layers as disks writes its record beside them (`moruna_image::record::Record`, JSON): the manifest and the config byte for byte, and for each of the manifest's layers, lowest first, its digest and the seal of the layer file made from it (the SHA-256 of the file's header page, which carries the root of the hash tree every other page is checked against, SDD 27 d.2). The boot's launcher is `build.sh`: `moruna-layer derive` builds the boot's image from python:3.14-slim (its layers, one more holding the test programs and the engine as an uncompressed, name-ordered, root-owned, zero-dated tar, and `Cmd ["python3", "/kd2/transform.py"]`), and `moruna-layer stack` converts its layers, writes their list, the record (`linux/image.record`, given to the engine's supervisor as the data handle `image`) and the manifest digest (`linux/image.digest`, the embedder's reference). Before an image stage's task is made, the engine reads the record (granted to it as `image`) and asks the personality's `layers` (d.2), then `Record::verify` checks, each refusal naming both sides: the reference is a digest (`a name` is refused: on the kernel there is no index to resolve it in); the manifest hashes to it; the config hashes to the manifest's config digest; the record's layers are the manifest's, in order; the layers served are the record's, in number and seal for seal. The verified config gives the task its command and environment (f.5). The image filesystem mounts a layer only after its header checks itself and reads a page only after it matches the tree under the header's root, so a served layer whose seal matches is byte for byte the layer file the launcher made. What the engine cannot check is the conversion: that a layer file holds its OCI layer's tree; that is the launcher's, as a Linux node trusts its own unpacked cache (SDD 19 IM-I1). The layer list's ids (each file's SHA-256) are the launcher's too and are not checked by the image filesystem (SDD 27 IF-O5).

## g. Concurrency within the component

The engine is multi-threaded as on Linux. A stage's `KernelPlane` and its personality connection are its state's, used by the one worker that holds the state. The vsock connection is one thread's (f.2); readers and writers talk to it through a queue and a doorbell. In the personality, blocked calls are a task's and are answered on its own forward from the single server thread (SDD 28 g).

## h. Behaviour

The CI boot: the supervisor's `KD2 grant` test; `linux-test` runs `granted.c` with `run_granted` (`KD2 granted`); the engine's supervisor starts the engine five times through the personality, each serving one job of `kernel/tools/embedder.py` on the guest's port 5100: the rows, a task that fails, a refused record, a cancel, an image nobody named.

## i. Configuration

`kernel/initramfs/engine.conf` (d.3). Constants: a window of `2 × input + 1 MiB`, at least 1 MiB, at most 256 MiB; a stage's plane at most 1 GiB; a log ring of 64 KiB; 20 lines of standard error in a failure; 250 ms between looks at a run's cancel; 60 s for the personality's answer once a task has ended; 120 s for a hello; 5 s of a write without credit; 60,000 bytes a vsock read or write; 64 runs at most.

## j. Observability

The supervisor prints each run's start (its grants, budget and scratch) and end (code, calls served, calls refused); the personality prints each program's end with its counts, the engine's included; the engine's own standard error goes to the console (`linux lx-moruna-engine: ...`), since its own capture needs `pipe`, which the personality refuses and the run's notes say; each task's lines go to the run's output store. The embedder writes a verdict line per run with the figures: rows, seconds, rows a second, the stages' rates, the tap's port round trips.

## k. Tests

Host tests: `cargo test -p moruna-linux -p linux-protocol -p moruna-plane -p moruna-imagefs -p engine --lib -p moruna-abi` in `kernel/`; `cargo test -p moruna-micro -p moruna-image -p moruna-vmm` and `cargo test -p moruna-runtime --test job jb_t25` in the root. Boot tests in `build.sh`'s image on 1, 2 and 4 CPUs (`boot.sh`, judged by `check-boot.sh`), on any host QEMU runs on: the vsock is vhost-user (f.11).

| id | test | where | asserts |
|---|---|---|---|
| KD2-T1 | `kd2_t1_*` | `moruna-linux` | an eventfd's counter, semaphore mode, its bounds and refusals; epoll's level, edge and one-shot entries, `EPOLL_CTL_MOD` re-arming, `max`, closed descriptors' entries going, entries bound to a description, `EPOLLHUP` unasked; the packed event layout and the timeout |
| KD2-T2 | `kd2_t2_*` | `moruna-linux` | `sysinfo`, `/proc/meminfo` and the CPUs online for a budget; no budget, no figure |
| KD2-T3 | `kd2_t3_*` | `linux-protocol` | a `Grant` round trips with its handles; the account-bearing protocol is 2.0; `AT_MORUNA_START` is outside Linux's types |
| KD2-T4 | `KD2 granted` | boot: `linux-test`, `programs/granted.c` | a static musl program run with `run_granted` finds its start page through `getauxval`, which names the one handle it was granted, and `handle_info` answers on it and not past it; `sysinfo` and `/proc/meminfo` say the 256 MiB granted; an idle `epoll_wait` times out with 0 after its 100 ms; a wait is woken by another thread's write to an edge-triggered eventfd and reports once; a blocking read is woken by a write; an empty nonblocking read is `EAGAIN`; `rename` replaces a file and `RENAME_NOREPLACE` is `EEXIST` |
| KD2-T5 | `kd2_t5_*` | `engine` | the supervisor's configuration read whole (a data grant among its grants), and every refusal naming its line |
| KD2-T6 | `kd2_t6_*` | `moruna-plane` | a record the engine writes in place becomes a batch with its offset and length; a failed write and a refused record give the object back and make no batch |
| KD2-T7 | `kd2_t7_*` | `moruna-micro` | windows are whole pages within their bounds; a record ends at its last buffer; ports are numbered from 1 in name order |
| KD2-T8 | `KD2 grant` | boot: the supervisor | `process_grant` moves a channel into a created process and maps a start page naming it; a handle without `TRANSFER` is refused and kept; a second grant and `process_start` are refused; the granted channel closes with the process |
| KD2-T9 | the engine's runs | boot: `kernel/engine`, `moruna-engine`, `kernel/tools/embedder.py` | five runs of the unmodified engine on the kernel, each a job over vsock through the vhost-user back end, every image entry naming the attached image by its digest: (1) the fixture's 100,000 rows through the Python task (`transform.py`, CPython 3.14 from python:3.14-slim, standard library only: `y := 3x + y`), whose command and environment are the image's config's, found on its `PATH`, and the tap (`tap.c`, which hands its stream to the embedder through port calls), every row checked against the formula and the sink's count against the source's, with the run's seconds, rows a second, the stages' rates and the tap's port round trips (least, median) printed; (2) the Python task raising after one batch: exit 4, `task_failed` `kernel_error` naming `exited with status 1` and its traceback; (3) `badrecord.c` putting bytes that are not a record: exit 4, the failure naming the check's refusal, the kernel ending `lx-badrecord` with the check's sentence; (4) a cancel while the tap waits on its first port call: exit 130, `cancel_acknowledged`, the call given up; (5) an image nobody named (the digest of nothing): exit 2 before any task starts, the diagnostic naming that digest and the attached image's |
| KD2-T10 | the image filesystem's suite | boot: `imagefs-test` | `rename` (1.1): a scratch file moved and replacing another, the image refused; `layers` (1.2): a seal of 32 bytes for each layer, none twice (`moruna.imagefs` conformance) |
| KD2-T11 | `kd2_t11_*` | `moruna-vmm` | the vhost-user messages: a header and its flags, a reply, a ring's state and addresses, a memory table with its descriptors, an address translated through its regions, the config request and reply; every malformed one refused |
| KD2-T12 | `kd2_t12_*` | `moruna-imagefs` | a mounted layer's seal is the SHA-256 of its file's first page; two layers, two seals |
| KD2-T13 | `kd2_t13_*` | `moruna-image` | a record verifies the image its digest names on its own layers, its config's command and environment; a name, another digest, a forged config, swapped layers, a seal short and a wrong seal are each refused naming both sides; a launch from a config with the entry's command and environment, its user, working directory and the paths its command is tried at; every launch that cannot be is refused |
| KD2-T14 | `kd2_t14_*` | `moruna-image` | a derived image adds its layer and command over its base's, the same bytes every time, and unpacks to both; the record of a converted image verifies against the seals of its layer files' mounted headers |
| IF-T20 | `if_t20_*` | `moruna-imagefs` | `Scratch::rename`: across directories, over a file, to itself, every refusal, a directory over an empty one, an open file replaced, the quota held, nothing left charged |
| KK-T8 | isolation | boot | the sweep takes `process_grant` too (40 calls) |
| IM-T27 | `im_t27_the_words_of_a_launch` | `moruna-image` | the user, the environment and the paths a command is tried at (moved from the sandbox with `Launch`) |
| f.10 | two plane tasks in a row | boot: KD2-T9 run 1 | the Python task and the tap, both on descriptors 0 and 1, through one personality that also serves the engine: the run completes, which it cannot when the server waits inside a request |

Controlled proof-runner regression checks (`python3 kernel/tools/test-boot-cleanup.py`, 2026-10-30) passed three cases: normal exit status 33, backend-listen failure status 2, and SIGTERM status 143 during the simulated QEMU process. The fixtures start no VM and test no kernel or GNU timeout deadline. They assert that only owned helper/QEMU PIDs are ended and waited for, their sockets are removed, unrelated processes remain alive, and logs remain. `boot.sh` installs cleanup before starting helpers and bounds termination before waiting. Real build and boot results are still absent (section m, item 6).

`Record::to_bytes` returns the image crate's `Result<Vec<u8>>`; serialization failure is a typed format error propagated by `moruna-layer`, never a shipping `expect`. Existing round-trip fixtures exercise the success path, pending the coordinated Rust check.

## l. Implementation notes for the agent

Integration review, 2026-10-30: KA2 and KD2 both change the image kind, sandbox and its tests. The integrating owner must retain KA2's `plane` setting and sealed-memfd Linux adapter together with KD2's verified image `Launch`, kernel branch and declared ports; choosing either whole file loses required behavior. KA2's consuming Linux `Window::put` helper changes no `DataPlane` method or kernel wire ABI. No merge is implied by this review. Kernel image uid/gid other than root and working directories other than `/` remain explicit refusals in `on_kernel`, not general OCI support.

- `moruna-engine` is built for `x86_64-unknown-linux-musl` as `arrow_echo` is (`build.sh`: `x86_64-linux-gnu-gcc` with musl's specs as the C compiler and linker, `-C link-self-contained=yes -C target-feature=+crt-static`), and stripped.
- `moruna-micro`'s kernel code is compiled only for `x86_64` Linux; on a Mac, `cargo check -p moruna-micro --target x86_64-unknown-linux-gnu` stops at `zstd-sys`, which needs a C compiler for the target, so the check is the build container's.

## m. Open items

1. **The stage boundary copies.** An image stage copies each batch once into its plane and once out of it, because the scheduler's arena is not the plane's; a batch between two image stages is copied twice where the plane could hand it on. The scheduler driving the plane's batches directly is KD7's.
2. **Native accounting proof remains required.** KB5 actual accounts replace the old advertised-only budget. The native supervisor carves runtime and scratch accounts from its own account, uses the runtime account in Grant.account, and supplies a narrowed duplicate of that same child under reserved start slot account. Config grants cannot shadow that slot. It waits boundedly for actual original account used/reserved figures to retire before another sequential run; it never resets accounting while old objects/loans remain. Actual kernel account/start-page/retirement execution is still a required proof.
3. **The data disk.** A sink's files live in the engine's scratch directory and leave with the VM. Writing the run's output to the data disk needs a decision on its layout (a filesystem server over the block driver, or the run's output written as a layer file the host reads with `moruna-layer`): Brackly's.
4. **A layer file's conversion.** The engine checks that the layers served are the ones the record names and that the record is the image named (f.12); that a layer file holds its OCI layer's tree is the launcher's conversion, which only a launcher re-converting the layer could check. KD4's launcher writes the record as `build.sh` does.
5. **Native tasks and the process resource.** No kind starts a native process; when one does, the engine's supervisor grants the process resource (`grant SLOT process`), which the configuration does not have yet.
6. **The proof.** KD2-T4, T8, T9 and T10 have not been booted; the engine's musl build and the back end's build have not been made.

## n. Traceability

| requirement (BOARD EK KD2 brief) | here |
|---|---|
| how the engine runs on the kernel, chosen and justified, with the alternative's costs | f.1 (EN1) |
| a rule for which process holds handles, and how a Linux program reaches native calls | d.1, d.2, f.1; EN-I2, EN-I3; KD2-T4, KD2-T8 |
| the supervisor starts the engine from the manifest with vsock, the image filesystem, a budget; no hard-coded paths | d.3, e.1, f.3; `engine.conf` |
| the job over vsock in the host protocol; the arena sized from the budget | f.2, f.3; KD2-T9 |
| tasks as processes on their image through `run_plane`, wired with `process_plane`, driven through `KernelPlane` behind `DataPlane` | e.2, f.5, f.6 |
| a task's death, a refused record, cancellation as on Linux | f.8; EN-I4, EN-I5; KD2-T9 runs 2 to 4 |
| port calls to the engine, forwarded over vsock, the reply a read-only memory object | f.7; EN-I6; KD2-T9 run 1 |
| results: sinks, the run report and events over vsock | f.9, m.3; KD2-T9 |
| a boot test of a real job on 1, 2 and 4 CPUs, with figures | k; m.6 |
| the boot test runs on this Mac: a userspace vsock back end, its flags in the boot description (coordinator, 2026-10-29) | f.11 (EN2); `boot.image`, `boot.sh` |
| an image's identity checked on the kernel; command and environment from the verified config (coordinator, 2026-10-29) | f.5, f.12 (EN3); EN-I8; KD2-T9 run 5, KD2-T13, KD2-T14 |

## o. Deferred

**EN-O1. One plane for the run** (KD7): the scheduler's batches in the plane's arena, so image stages hand batches on with no copy. **EN-O2. Signals for the engine**: a `cancel` arrives over the host protocol, and no signal is delivered on the kernel (SDD 28 m.2). **EN-O3. `statfs` of the scratch directory**, so the engine's spill budget is the scratch quota rather than nothing.

The service grant preserves the supervisor's listener owner across sequential runs and transfers only each fresh connected channel into that run. Missing start listeners and refused connections fail before `run_granted`; no read-only MAP rights or byte-copy adapter is substituted. A configuration claiming one start slot is both data memory and a listener is rejected with its line. The runtime service client performs the actual service protocol major handshake; connecting the transport alone is not service compatibility proof. Parser checks cover the explicit service role and malformed/mixed-role refusal; the boot assembly must separately prove genuine per-run connections and grants.


The protocol-2.0 engine migration retains the explicit Source::Service connection
path. Memory/scratch are positive whole-page configured budgets; scratch is required.
Each run carves fresh actual runtime and scratch accounts, passes only the runtime
child's READ|WRITE|DUPLICATE|TRANSFER start grant, and never substitutes the native
supervisor or node account. Linux's process charging and the runtime's own start
handle identify the same account. The charging duplicate and scratch duplicate
carry only READ|WRITE|TRANSFER. Errors fail before run_granted; five-second bounded
retirement waits observe actual used/reserved counters. Source formatting and parser
test additions do not claim native account or boot acceptance.


### Private storage admission and actual child accounting

`grant storage admission AUTHORITY CONTROL` takes an already-connected private
`moruna.storage.admission` endpoint and a readonly canonical storage-format
Admission record from explicit boot slots. It does not connect a public listener
or infer authority from the record. Data, listener and private endpoint roles
cannot alias the same boot handle. For each real run, KD2 first carves its actual
runtime and scratch Accounts, validates the sole bounded Admission codec, copies
its control into Memory charged to that runtime Account, then calls bind with the
same actual child Account. Only the returned already-bound connected object
channel is passed to the runtime. A refusal terminates launch; no generic object
connection, parent Account, anonymous credential or compatibility fallback exists.
The storage service's actual designated authority remains responsible for exact
Scope/role/Account authorization. These source changes still require integrated
compilation and native admission/foreign-account tests.
