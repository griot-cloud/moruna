# An optimized runtime

A runtime that runs untrusted code on data. Native code, any language, with the guarantee wasm gives: the code can do exactly what it is handed and nothing else, and the only thing that crosses its boundary is Arrow. Everything below is Rust.

## The stack

```
+----------------------------------------------------------------+
|  7  images        OCI layers: base, deps, shim, code            |
|  6  tasks         wasm in the engine, or a process on the kernel|
|  5  engine        moruna: sources, scheduler, sinks, arena      |
|  4  data plane    Arrow batches in shared pages, zero copy      |
|  3  kernel        the Moruna kernel: ~60 syscalls, Rust, no_std |
|  2  monitor       moruna-vmm: KVM, virtio-blk, vsock, shm       |
|  1  hardware      one VM per run; CPU, memory, no NIC           |
+----------------------------------------------------------------+
```

One VM per run. Inside it, one kernel that is ours, one engine, and the tasks as processes. Nothing in the VM has a network.

## 1. Hardware

A VM with virtual CPUs, memory, and three virtual devices: a block device for the data disk, a vsock for control, and a shared-memory region for batches. No network device exists. Between tenants the boundary is the hypervisor and the CPU's virtualisation hardware.

## 2. The monitor: `moruna-vmm`

The program on the node that launches a VM. It exists today. It does four things:

- maps the VM's memory and starts its CPUs through KVM;
- attaches the data disk as virtio-blk, with io_uring and direct IO on the node side;
- opens one vsock to the embedder's agent;
- exposes a shared-memory region (virtio-shm) that the kernel uses for the data plane.

It also presents image layers to the VM as read-only block devices, one per layer, content-addressed. It never unpacks an image.

The hypervisor is a port. KVM is the first adapter.

## 3. The kernel

A Rust `no_std` kernel, the only kernel in the VM. It replaces Linux. It is built to be small: the target is under 100,000 lines, so that the code behind the syscall allowlist is of the same order as a wasm engine.

### What it provides

| area | syscalls | notes |
|---|---|---|
| memory | `mmap`, `munmap`, `mprotect`, `brk`, `madvise` | huge pages by default for anything over 2 MiB; Arrow buffers land on them |
| tasks | `clone` (threads and processes), `exit`, `wait`, `futex`, `sched_yield`, `getpid` | a process is an address space; threads share one |
| time | `clock_gettime`, `nanosleep` | the clock comes from the node at boot |
| files | `openat`, `read`, `pread`, `close`, `fstat`, `lseek`, `getdents`, `readlink`, `mmap` on files | the read-only image filesystem, and one scratch directory with a byte quota |
| output | `write` to fd 1 and 2 only | captured as log records |
| random | `getrandom` | seeded from the node |
| signals | `rt_sigaction`, `rt_sigprocmask`, `kill` to self | enough for language runtimes to install handlers |
| process start | `execve`, `exit_group`, `set_tid_address`, `arch_prctl` | what glibc and musl need to start |
| channels | `batch_get`, `batch_put`, `port_call` | the three that are ours; section 4 |

That is the whole interface, about 60 entries. Every one carries Linux's semantics, so an unmodified Linux ELF binary runs: glibc, musl, the CPython and JVM runtimes, a Rust or Go binary. Everything Linux has that is not in the table returns `ENOSYS`, and the kernel counts it against the task.

### What it does not have

- no network stack and no `socket`;
- no users, groups, or permissions; a process owns its address space and nothing else;
- no `ioctl` beyond the terminal size query;
- no `ptrace`, no `/proc`, no `/sys`, no device nodes except `null`, `zero`, `urandom`;
- no shared memory between processes except what the engine grants (section 4);
- no writable filesystem except the scratch directory;
- no loadable modules, no drivers beyond the three virtio devices.

### The filesystem

The image's layers are block devices from the monitor. The kernel mounts them as one overlay, lowest first, applying whiteouts, and pages files in on demand. Nothing is copied into the VM: a 4 GB image with a 2 MB function costs the pages the function touches. The scratch directory is a tmpfs with a quota taken from the task's memory budget.

### Isolation between processes

Each task is a process: its own page tables, its own file descriptors, its own scratch directory. There are no syscalls that reach another process. Two tasks in one VM can affect each other only through the engine, by the batches it moves between them, which is the job's data flow.

### What to build it from

Not from nothing. Two Rust kernels implement the Linux syscall interface already and are the starting points: Asterinas (a "framekernel", Linux-compatible, the closest in intent) and Hermit (a unikernel with a Linux ABI layer). Either is forked, cut down to the table above, and given the three channel syscalls.

## 4. The data plane

Batches never travel through a pipe or a socket inside the VM. They sit in pages.

- The engine owns an arena: a region of the VM's memory, backed by huge pages, with a fixed budget.
- A batch is one Arrow IPC record in the page-aligned framing the engine already writes (`moruna-kernel/src/ipc.rs`): every buffer starts on a page boundary, so a buffer can be mapped, written by direct IO, or handed to a device with no copy.
- `batch_get(slot) -> (ptr, len)`: the kernel maps the batch's pages into the calling task read-only and returns where. The task reads the Arrow arrays in place.
- `batch_put(slot, ptr, len)`: the task hands back a region it was granted write access to. The kernel checks the record's framing (the schema, every node and buffer, no compression) before the engine sees it, unmaps it from the task, and marks it ready. A record that fails the check ends the task with the reason.
- `port_call(port, ptr, len) -> (ptr, len)`: the one non-Arrow channel. The bytes go to the engine, which forwards them over vsock to the embedder's agent, which applies its policy. The task blocks until the answer is mapped in.

The three syscalls are the only way bytes leave a task. `write` to fd 1 and 2 goes to the log, with the existing rules: whole lines, rate limits, drops counted. That is the "only Arrow flows" guarantee, enforced by the kernel rather than by inspection.

Spill goes the same way: a batch evicted from the arena is written to the data disk by the engine through virtio-blk with direct IO, and read back into the arena when needed. Later, a GPU or a DMA engine is another consumer of the same pages.

## 5. The engine: `moruna`

Process 1 in the VM. It exists today and keeps its shape: sources, the scheduler, sinks, placement, the controller, the budget. What changes is what it sits on: instead of Linux pipes and a seccomp sandbox, it uses the kernel's slots and the three syscalls. The engine:

- owns the arena, the data disk, and the vsock;
- starts each task as a process with its image overlay and its budget;
- moves batches between tasks by changing slot ownership, never by copying;
- turns every task's end into one terminal event;
- is the only process that may open the data disk or speak on the vsock.

## 6. Tasks

A task is a kind plus its settings. Two kinds:

- **wasm**: runs inside the engine process. No syscalls at all; the exported functions are `batch_get`, `batch_put`, `port_call`. For code that must have no kernel beneath it.
- **image**: a process on the Moruna kernel, started from an image's command. Any language.

Both see the same three channels. The conformance suite runs one function through both and requires the same rows, the same refusals, and the same ending when the code tries something it may not.

A task's budget is memory (arena pages plus scratch), CPU time, and a syscall count. Exceeding any ends the task with the reason.

## 7. Images

An image is an OCI image: a manifest, a config with the command, and content-addressed layers. There is no container runtime anywhere; an image is a filesystem and a command.

### The four layers

| layer | holds | shared by |
|---|---|---|
| 1 | the language runtime and its userland (CPython, a JVM, nothing for a static binary) | every image in that language |
| 2 | the resolved, locked dependencies | every image with the same lockfile |
| 3 | the shim: receives a batch, calls the function, returns a batch, in that language | every image in that language |
| 4 | the function | that function |

Functions in one job share 1 to 3. Jobs in one language share 1 and 3. A dependency change rebuilds 2; a code change rebuilds 4, a few kilobytes.

### Two ways in

- **Bring an image.** Any OCI image from any registry, plus a command. The shim is absent, so the contract is `batch_get` and `batch_put` through the two file descriptors the kernel pre-opens for them, as a plain Arrow IPC stream. No library needed.
- **Build from code.** `@moruna.kernel` on a function in a supported language. The builder resolves the runtime, locks the dependencies, adds the shim, and writes the four layers. The user never sees a Dockerfile. Each layer's digest is its content's hash, so two users with the same lockfile produce the same layer 2 bit for bit.

The builder runs in its own VM on the same kernel, with one port open: the package resolver. It is the only build-time network access, and it goes through the embedder's policy like any port call.

### Caching

Layers live on the node by digest. The monitor hands a VM the layer devices it needs; a layer already present is never fetched again. The kernel pages files in lazily, so the cost of a cold image is the bytes the task touches, not the image's size.

## 8. A run, start to finish

1. The embedder's agent asks the monitor for a VM with the data disk, the job's image layers, a budget, and one vsock.
2. The monitor boots the kernel, which starts the engine as process 1.
3. The engine reads the job over vsock, sizes the arena, and starts each task as a process on its image overlay.
4. A source task's batch lands in the arena; the engine marks its slot ready for the next task.
5. The next task calls `batch_get`, reads in place, writes its output into its granted region, calls `batch_put`. The kernel checks the record and hands the slot on.
6. A task that calls `port_call` blocks while the engine forwards the bytes over vsock and back.
7. A sink task writes to the data disk through the engine.
8. Each task ends with exactly one terminal event over vsock.
9. The engine exits; the monitor tears the VM down; the arena and scratch are gone.

## 9. The guarantee

What a task can do is the table in section 3, and nothing else. What stands between a task and the node is: the Moruna kernel (under 100k lines of Rust), the monitor, and KVM. That is the same order of size as a wasm engine, which is the point.

| boundary | enforced by |
|---|---|
| task to task | address spaces and the absence of any cross-process syscall |
| task to engine | the three channel syscalls, with the record check in `batch_put` |
| task to node | nothing reaches the node but vsock from the engine and virtio-blk from the engine |
| tenant to tenant | the hypervisor |

## 10. What exists and what is built

| piece | state |
|---|---|
| the monitor, `moruna-vmm` | exists; gains virtio-shm and per-layer block devices |
| the engine | exists; moves from pipes and seccomp to slots and the three syscalls |
| the page-aligned Arrow record | exists (`ipc.rs`); becomes the data plane's unit |
| the record check | exists (the IPC check); moves into the kernel's `batch_put` |
| wasm kind | exists; unchanged |
| the kernel | built, from Asterinas or Hermit |
| the three channel syscalls | built |
| the overlay filesystem over layer devices | built |
| the image builder and `@moruna.kernel` | built; the current `.pex` packaging is its first, Python-only form |
| the conformance suite across kinds | built |
