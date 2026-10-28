# Moruna SDD 17: The two runtimes of a packaged kernel (`moruna-wasm`, `moruna-sandbox`)

**Document type:** software design document, component 17
**Status:** DRAFT · 2026-10-15 (becomes HANDOFF-READY when section m is empty and the preamble's E1 and E2 assumptions are accepted; the human flips it)
**Parent:** `architecture/function-packages.md` (FP) sections 2 and 4; `architecture/moruna-hosted-engine.md` (MH) sections 4.3, 4.4, 4.8; preamble E13
**Preamble:** `00-preamble.md`; **Contracts:** `01-contracts.md` d.7 (`Kernel`, `KernelAlloc`, `set_kernel_alloc`), d.12 (`Sampler`, `Sample`, `OutsideMemory`); **Cited:** 05 b, f.9 to f.14, AD-I8 to AD-I15 (the allocator guard), 05 f.3 (the boundary copy), 16 e.1 to e.6
**Component location:** `crates/moruna-wasm`, `crates/moruna-sandbox`, `python/moruna/_sandbox.py` (the template), `crates/moruna-py/src/sandbox.rs` (the child's runner), `guest/` (the standard-build interpreter, the cgroup and the kernel options)
**Consumes:** contracts (1), the guard and the boundary copy (5), the package (16). **Consumed by:** the job document and the facade (12), `moruna check` (16 f.9).

**Decisions worth your eye:** (1) neither runtime has Moruna's access to the machine: a `.wasm` runs in wasmtime with no imports but the world's `read-asset` and `call` (the latter reaching only its stage's listed ports through Moruna, MH 4.10), a `.pex` in a separate process confined by a mount namespace, Landlock, seccomp, no capabilities and a cgroup (FP 4, decided by Brackly 2026-10-15); (2) the child is a fork of a warm template interpreter that holds the data libraries imported and no thread, and batches reach it through a shared mapping, read in place (decided by Brackly 2026-10-15); (3) the allocator guard is Moruna's code and goes where the kernel goes: the same hooks, frame and gate run inside the child against the child's own memory and a ceiling Moruna writes (05 b, E13); (4) wasm linear memory is a fourth source of a call's counts (`linear`), so a wasm call's growth, peak and refusals reach the trace exactly as a Python call's do.

---

## a. Purpose and boundary

Run a packaged kernel (16) so that the kernel cannot read a disk, the job document or the host socket, cannot open a connection, is capped in memory with Moruna told before the cap, is bounded and measured in CPU, and still produces per-call counts, refusals and shrink as an in-process kernel does (FP 4, the table of guarantees).

It owns: the wasmtime engine, its configuration, the resource limiter, fuel; the `.cwasm` reader; the template process, the fork, the child's confinement (mount namespace, Landlock, seccomp, capabilities, cgroup); the control socket and its protocol; the shared mapping and the counters page; the child's runner (the package loaded under the adapter, the guard in the child); the stage's ceiling and its updates; each runtime as a `Kernel`; the guest image's standard-build interpreter and what the guest kernel and init must provide for the sandbox.

It refuses to know: the package format (16), how the run is planned or scheduled (10, 11), and any policy about which kernels to trust: every packaged kernel in a hosted run is confined, whoever owns it.

## b. Vocabulary

**Stage ceiling.** The memory a packaged stage may hold: the run's ceiling in force (03 f.2) minus what the rest of the run holds at that moment (the process's sample less this stage's own in-use). Computed when the stage starts, before every call, and on every limits change (MH 4.4).

**Template.** One standard-build CPython 3.14 process per Moruna process and sandbox interpreter, started on the first `.pex` stage, with `pyarrow` (required) and `numpy` (when its interpreter has it) imported, and one thread. Polars is not imported by the template: its import starts threads (an out-of-core cleaner and its allocator's background threads, measured with polars 2.0.0), and a template that holds a thread cannot fork (SB-I10); a Polars kernel imports it in its own child.

**Child.** One process per `.pex` stage, a fork of the template, confined (f.5), running the package for the whole run.

**Mapping.** A `memfd` both Moruna and the child map shared: the counters page, then the data area where batches are placed.

**Counters page.** The mapping's first 4096 bytes (e.4).

**Control socket.** A Unix stream socket pair; the child's end is the only socket it has.

**Linear source.** The fourth source of `KernelAlloc` (contracts d.7): a wasm instance's linear memory growth.

## c. Invariants

**SB-I1. A wasm kernel reaches nothing but its world.** The linker defines `read-asset` and `call` and nothing else; a component with any other import is refused before instantiation (16 PK-I4). The instance's only inputs are the bytes `process`, `open` and `configure` are given, the files of the assets directory `read-asset` reads, by a relative path that cannot leave it (no `..`, no absolute path, no symbolic link out), and the answers of the ports its stage's entry lists, which `call` returns (f.13, MH 4.10).

**SB-I2. Every growth of a wasm instance's memory is decided by Moruna first.** The resource limiter sees every `memory.grow` and table growth; growth past the stage ceiling, or a growth the run's gate refuses (05 f.9), is refused before the memory exists, counted in the call's `linear.refused`, and a call that then traps ends as `MorunaError::Refused` naming the request (05 AD-I13).

**SB-I3. A wasm call is bounded in CPU.** Each call is given `FUEL_FLOOR + FUEL_PER_INPUT_BYTE × bytes_in` units of fuel; exhaustion ends the call as `MorunaError::Kernel` whose message starts `wasm: fuel exhausted`.

**SB-I4. The child sees only its tree.** After confinement the child's mount namespace holds the tree (read only) at `/pkg`, the assets directory the host named (read only) at `/assets` when there is one, a private `tmpfs` at `/tmp`, the template interpreter's import roots (read only, at their own paths) and `/dev/null`, `/dev/zero`, `/dev/urandom`; nothing else. Landlock allows read and execute under the tree, the assets and the import roots, read and write under `/tmp` and on the three devices, and nothing else. FP 4.

**SB-I5. The child can open no connection and gain no privilege.** seccomp allows the system calls of e.5 and refuses every other with `EPERM`; `socket`, `socketpair`, `connect`, `bind`, `listen`, `accept`, `accept4`, `execve`, `execveat`, `unshare`, `setns`, `mount`, `pivot_root`, `ptrace` and `clone` with any namespace flag are not on it; `kill` and `tgkill` are allowed only towards the child itself. The child has no capability in any set, and `no_new_privs` is set. FP 4.

**SB-I6. The child is capped and weighed by a cgroup.** The child runs in its own cgroup under the sandbox cgroup, created by Moruna before the fork's request, with `memory.max` the stage ceiling (updated with it), `memory.swap.max` 0, `cpu.weight` `CPU_WEIGHT` and `cpu.max` the run's CPU quota in force; it is removed after the child exits. A cgroup that cannot be created or written refuses the stage.

**SB-I7. No batch crosses the socket.** The socket carries only the messages of e.3. An input batch is placed in the mapping by Moruna as an Arrow IPC stream and read in place by the child (zero copy, 05 AD-I1 inside the child); the child writes its output into the mapping and Moruna reads it in place and copies it into the arena once (05 f.3, AD-I2).

**SB-I8. The guard runs inside the child, on the same rules.** The child installs the hooks of 05 f.10 and f.11, opens the frame of 05 f.12 around each call, and its gate (05 f.9) measures the child's memory against the stage ceiling read from the counters page; a call's `KernelAlloc` (`python`, `numpy`, `arrow`) returns with its reply and Moruna hands it to the scheduler with `set_kernel_alloc` (contracts d.7). A refusal in the child ends the call as `MorunaError::Refused` in Moruna with the child's figures.

**SB-I9. The controller sees a packaged stage between batches.** The process's sampler adds, at every sample, each live child's in-use (the larger of the counters page's `in_use` and the child cgroup's `memory.current`) to `anon_bytes`, so the run's ceiling governs Moruna and its children together.

**SB-I10. The template holds no thread when it forks.** Before its first fork, and before every fork, the template checks that it has exactly one thread (`/proc/self/task`) and refuses the request naming the count otherwise.

**SB-I11. A child's death is a kernel error, never a hang.** End of stream on the control socket, or a reply not parsable, ends the call in flight as `MorunaError::Kernel` naming the child's pid and exit status, and every later call of that stage the same way.

**SB-I12. A stateful package's state crosses a checkpoint as bytes.** A `.wasm` checkpoint is `close()` followed by `open(bytes)` with what `close` returned; a `.pex` checkpoint calls the kernel's `checkpoint` in the child (05 e.1). Resume opens the instance with the manifest's bytes.

**SB-I13. An input region is the child's until it lets go.** Each input is placed in a free region of the data area and leased to the child. The child reads it through a foreign Arrow buffer whose base object is the lease; when the last Arrow buffer over the region is gone (the batch, its slices, anything the kernel kept), the lease's release names the region in the next reply. Moruna reuses a region only after its release, places new input only in free regions, and, when no free region is large enough, waits for releases (asking the child, which collects its garbage first), counting the wait in the call's `lease_wait_us`. A call that waited `LEASE_WAIT` with no release is refused as `MorunaError::Refused`, `in_use` the leased bytes and `ceiling` the data area. A kernel that keeps a batch therefore keeps the batch's own bytes, and holds that much of the mapping, which the stage's footprint and its profile show.

**SB-I14. A function's output is untrusted IPC (2026-10-28).** The bytes a `.wasm` returns from `process` and the stream a `.pex` child leaves in its output region are the tenant's code's, and are read through contracts e.8 (`moruna_kernel::ipc::read_stream`), in place: every message is checked before arrow-ipc reads it (contracts CT-I13), so a malformed output, including a compressed one, ends the call as `MorunaError::Kernel` with a sentence naming the field or column and what is wrong, never a panic in Moruna.

## d. Interfaces

### d.1 Exposed (Rust)

```rust
// moruna-wasm
pub const FUEL_FLOOR: u64; pub const FUEL_PER_INPUT_BYTE: u64;
pub fn engine() -> &'static wasmtime::Engine;                         // f.1, one per process
pub struct Compiled { pub component: wasmtime::component::Component, pub notes: Vec<String> }
pub fn compile(package: &moruna_package::Package) -> Result<Compiled>;  // f.3: the .cwasm when it matches
pub fn verify(component: &Component, stateful: bool) -> Result<()>;    // f.2, PK-I4
pub fn declare(component: &Component) -> Result<String>;              // instantiate with no imports, call declare()
pub fn aot(package: &Package) -> Result<PathBuf>;                      // 16 e.6
pub struct WasmKernel { /* the package, the compiled component, the gate cell */ }
impl WasmKernel {
    pub fn new(package: Arc<Package>) -> Result<WasmKernel>;
    pub fn notes(&self) -> &[String];
    pub fn bind_gate(&self, gate: Arc<MemoryGate>);                    // 05 d.1's gate
}
impl Kernel for WasmKernel { /* f.4 */ }

// moruna-sandbox
pub struct SandboxHost { pub python: PathBuf, pub cgroup: PathBuf }
pub struct SandboxKernel { /* the package, the host, the tree, the bound sampler */ }
impl SandboxKernel {
    pub fn new(package: Arc<Package>, host: SandboxHost) -> Result<SandboxKernel>;   // Linux only; elsewhere refused
    pub fn notes(&self) -> Vec<String>;
    pub fn bind_run(&self, sampler: Arc<dyn Sampler>, ceiling: u64);   // f.7: what the stage ceiling is computed from
    pub fn limits_changed(&self);                                      // MH 4.4: recompute and send the stage ceiling
    pub fn outside(&self) -> Arc<dyn OutsideMemory>;                   // SB-I9
}
impl Kernel for SandboxKernel { /* f.8 */ }

pub mod protocol { pub enum ToChild; pub enum FromChild; pub enum ToTemplate; pub enum FromTemplate;
                   pub fn write_frame(..); pub fn read_frame(..); }            // e.3
pub mod mapping  { pub struct Mapping; pub struct Counters; pub const PAGE: usize = 4096; }   // e.4
pub mod regions  { pub struct Regions; pub const ALIGN: u64 = 64; }                           // e.4, SB-I13
pub const LEASE_WAIT: Duration; pub const LEASE_POLL: Duration;                               // i
impl SandboxState { pub fn leased(&self) -> (usize, u64); }                                   // SB-I13
pub mod coverage { pub fn open(); pub fn write(); }                                           // l, Linux
pub mod confine  { pub struct Confinement; pub fn confine(c: &Confinement) -> io::Result<()>; }  // f.5, Linux
pub mod child    { pub trait Runner; pub fn serve(control, mapping, runner) -> i32; }          // f.6
```

### d.2 Exposed (Python, the template and the child's runner)

```python
# python -I -m moruna._sandbox   (fd 3 is the template's end of its socket)
_core._sandbox_child(start_json: str, control_fd: int, memfd: int) -> int   # confine, load, serve; the exit code
```

### d.3 Consumed

`moruna_adapters::guard::{MemoryGate, KernelMemory, FrameEnd}` (05 d.1), `moruna_adapters::land::land_table` (05 f.3, moved out of the `python` feature so both runtimes use the same boundary copy), `moruna_adapters::PyKernel::call_arrow` (05 d.1, the child's call), `moruna_package` (16 d.1), `wasmtime` (component model, cranelift, `ResourceLimiter`, fuel), `landlock`, `seccompiler`, `libc`.

## e. Data model, formats and state machines

### e.1 The stage's life

`new` (package opened, tree chosen) → `init` (`.wasm`: instance and store; `.pex`: template started if needed, cgroup created, child forked and confined, package loaded, `ready` received) → `apply`* → drop (`.pex`: `stop`, wait `STOP_GRACE`, `SIGKILL`, cgroup removed, mount point removed). Both runtimes are `KernelKind::Stateful { max_instances: 1 }`: one instance per stage, created by the scheduler's eager init (PY-I1), calls serialised.

### e.2 wasm memory and fuel

Engine: component model on, fuel on, cranelift at `speed`, threads off, `memory64` off. Store data: the limiter (stage ceiling, gate, guarded flag, the call's linear counts). `memory_growing(current, desired, max)`: `growth = desired − current`; count `requests += 1`, `bytes += growth`, `largest = max(largest, growth)`; refuse when `desired > stage ceiling`, or when the kernel is guarded, the gate is bound and `gate.admit(growth)` refuses; otherwise admit and `settle`; `memory_grow_failed` credits. `peak` is the instance's memory size at the call's end. Tables: refused past `TABLE_ELEMENTS_MAX`; instances past `INSTANCES_MAX`; memories past `MEMORIES_MAX`.

### e.3 The control protocol

Each frame is a little-endian `u32` byte length followed by that many bytes of UTF-8 JSON, one object with an `op` field; a frame over `FRAME_MAX_BYTES` is refused.

To the template (its socket; the request carries the child's control end and the memfd as `SCM_RIGHTS`, in that order):
`{"op":"start","tree":"/abs/tree","target":"cp314-manylinux_2_28_x86_64","entry":"function:shout","manifest":{…16 e.1…},"cgroup":"/sys/fs/cgroup/moruna-sandbox/stage-…","root":"/tmp/moruna-sandbox-…","mapping_bytes":N,"libraries":{…},"args":{…}|null,"assets":"/disk/1/models/churn"|null}`.
The request also carries `"libraries"`, the template's own `{name: version}`, for 17 f.6. `{"op":"status","pid":C}` asks how a child ended. From the template: `{"op":"hello","pid":P,"python":"3.14.7","libraries":{"pyarrow":"25.0.1",…},"glibc":[2,41],"machine":"aarch64"}` once (or `refused` naming why it cannot serve), then per `start` `{"op":"started","pid":C}` or `{"op":"refused","reason":"…"}`, and per `status` `{"op":"exited","pid":C,"status":"exit status 3"}` (or `"running"`). The template reaps its children on `start` and `status`.

To the child: `{"op":"call","lease":N,"offset":O,"len":L,"out_offset":P,"out_len":M}` (the input's lease and region, and the free region the output may be written to), `{"op":"open","offset":O,"len":L}` (state bytes in the mapping), `{"op":"checkpoint","offset":P,"len":M}` (the free region the state may be written to), `{"op":"release"}` (collect garbage and name what is released), `{"op":"stop"}`.
From the child: `{"op":"ready"}` or `{"op":"failed","reason":"…"}` once after loading; per call `{"op":"done","offset":O,"len":L,"alloc":A,"released":[N,…]}` or `{"op":"error","message":"…","refusal":{"requested":R,"in_use":U,"ceiling":C}|null,"alloc":A,"released":[…]}`; per checkpoint `{"op":"state","offset":O,"len":L,"released":[…]}`; per release `{"op":"released","released":[…]}`. `released` lists the leases whose last buffer went since the previous reply (SB-I13). `A` is `{"measured":bool,"refusal_on":bool,"python":[bytes,requests,largest,peak,refused],"numpy":[…],"arrow":[…]}`.

### e.4 The mapping and the counters page

`memfd_create("moruna-sandbox", MFD_CLOEXEC)`, `ftruncate` to `PAGE + DATA_BYTES` where `DATA_BYTES` is the stage ceiling at start rounded up to a page (the pages are touched only by what is written). Page 0, little-endian `u64` at these offsets: 0 magic `0x4d4f52554e415342` ("MORUNASB"), 8 `ceiling` (Moruna writes), 16 `in_use`, 24 `peak`, 32 `calls` (the child writes), 40 `version` (1). Each is accessed as an `AtomicU64` with acquire and release ordering. The data area (`PAGE` to the end) is a set of free ranges and leased regions (SB-I13), kept by Moruna: an input is written into the largest free range, its start aligned to 64 bytes, and that range is leased; the output is written by the child into the largest free range left, which Moruna names in the `call`, and is not leased, because Moruna copies it into the arena before it answers the call. After the copy Moruna gives back the free range that held the output (`fallocate(PUNCH_HOLE)`), and, when a lease's release arrives, the free range the region joins. Only the whole pages inside a free range are punched: a page shared with a leased region keeps its bytes, which the child may still read. The mapping therefore holds pages only for inputs the kernel still refers to and at most one partial page at each end of each.

### e.5 The seccomp allow-list

Two filters. The allow-list allows exactly these calls and refuses every other with `EPERM`:

```text
read write readv writev pread64 pwrite64 preadv pwritev lseek close close_range openat openat2
newfstatat fstat statx statfs fstatfs getdents64 readlinkat faccessat faccessat2 getcwd chdir
fchdir fcntl ioctl dup dup3 pipe2 mkdirat unlinkat renameat renameat2 ftruncate fallocate
fadvise64 fsync fdatasync flock fchmod fchmodat utimensat umask copy_file_range mmap munmap
mremap mprotect madvise mincore brk membarrier futex futex_waitv set_robust_list
get_robust_list rseq set_tid_address clone3 exit exit_group wait4 restart_syscall rt_sigaction
rt_sigprocmask rt_sigreturn rt_sigtimedwait sigaltstack getpid gettid getppid getuid geteuid
getgid getegid getresuid getresgid getgroups getrandom getcpu clock_gettime clock_getres
clock_nanosleep nanosleep gettimeofday sched_yield sched_getaffinity sched_getparam
sched_getscheduler getpriority prlimit64 getrusage sysinfo uname prctl ppoll pselect6
epoll_create1 epoll_ctl epoll_pwait epoll_pwait2 eventfd2 sendmsg recvmsg sendto recvfrom
shutdown getsockopt
```

on x86_64 also these, the older forms its C library still uses:

```text
open stat lstat access readlink poll select pipe dup2 mkdir unlink rename arch_prctl epoll_wait
getdents time getrlimit
```

and these with a condition: `clone` when its flags hold no namespace flag (`CLONE_NEWNS`, `CLONE_NEWCGROUP`, `CLONE_NEWUTS`, `CLONE_NEWIPC`, `CLONE_NEWUSER`, `CLONE_NEWPID`, `CLONE_NEWNET`); `kill` and `tgkill` when their first argument is the child's own pid. The other filter answers `clone3` with `ENOSYS`, so the C library falls back to `clone`, whose flags the allow-list can read (`clone3` passes its flags in memory), and allows every other call, which the allow-list judges. It is installed first: the allow-list does not allow `seccomp` itself. `socket`, `socketpair`, `connect`, `bind`, `listen`, `accept`, `accept4`, `execve`, `execveat`, `unshare`, `setns`, `mount`, `pivot_root`, `ptrace`, `process_vm_readv`, `process_vm_writev`, `bpf`, `perf_event_open`, `memfd_create`, `tkill` and the module and key calls are on neither list.

### e.6 The sandbox host

`--sandbox-python`: a standard-build CPython 3.14 (`sys._is_gil_enabled()` true) with the `moruna` wheel for that build and `pyarrow` installed. `--sandbox-cgroup`: a cgroup v2 directory with `memory` and `cpu` in its `cgroup.subtree_control`, writable by Moruna. The guest provides both (f.10). Nothing is read from the environment.

## f. Algorithms and policies

**f.1 The engine.** One `wasmtime::Engine` per process, built on first use with e.2's configuration; `.cwasm` files are valid only for an engine with that configuration, which the Moruna version pins.

**f.2 Verification.** No imports but `read-asset: func(string) -> result<list<u8>, string>` and `call: func(string, list<u8>) -> result<list<u8>, string>` (amended 2026-10-21; each refused naming it when its type differs, every other import refused naming it). Exports exactly `declare: func() -> string`, `process: func(list<u8>) -> result<list<u8>, string>`, and, when the manifest says `stateful`, `open: func(list<u8>) -> result<_, string>` and `close: func() -> list<u8>`; when it does not, neither. Each mismatch is refused naming the export and the type found. `declare()` is called on a fresh instance with no imports and its manifest compared as 16 f.5.

**f.3 Compile or reuse.** When `<package>.cwasm` exists and its header (16 e.6) names the package's fingerprint and this Moruna's version, the component is `Component::deserialize`d from the bytes after the header; a refusal by wasmtime, a mismatching header or a missing file compiles from the package's bytes with the note `"<package>: compiled (<reason>)"`.

**f.4 A wasm call.** Under the stage's lock: write the input table as one Arrow IPC stream; reset the call's linear counts; set the store's fuel (SB-I3); call `process`; a trap is a refusal when the limiter refused during the call (SB-I2), fuel exhaustion (SB-I3), or otherwise `MorunaError::Kernel` with `wasm: ` and wasmtime's message; `Err(s)` is `MorunaError::Kernel` with `s`. The output is read as an IPC stream holding exactly one batch through contracts e.8, each message checked (SB-I14), and landed into the arena (05 f.3); `set_kernel_alloc` with `measured`, `refusal_on` (guarded and gate bound) and the linear counts.

**f.5 Confinement, in the child, in this order.** 1. Write the child's pid to `<cgroup>/cgroup.procs`; open `<cgroup>/memory.current` and keep the descriptor. 2. `unshare(CLONE_NEWNS)`; make `/` recursively private. 3. Mount a `tmpfs` (mode 0755) on `root`; create and bind-mount the tree at `root/pkg`, each import root at `root/<its path>` and the three devices at `root/dev/<name>`, remounting each bind read-only, `nosuid` (and `nodev` except the devices); mount a `tmpfs` (mode 1777, `nosuid,nodev`) at `root/tmp`. 4. `pivot_root(root, root/.old)`, `chdir("/")`, `umount2("/.old", MNT_DETACH)`, remove `/.old`. 5. `prctl(PR_SET_NO_NEW_PRIVS, 1)`; drop every capability from the bounding set, clear the ambient set, and `capset` effective, permitted and inheritable to empty. 6. Landlock ABI 1 with every filesystem access handled, the rules of SB-I4, `restrict_self`; a kernel without Landlock refuses the child ("Landlock is not available"). 7. seccomp (e.5), default `EPERM`. Any step that fails ends the child with `{"op":"failed","reason":"<step>: <error>"}`.

**f.6 The child's runner.** After f.5: refuse when the tree's `deps/<target>/` holds a distribution (its `*.dist-info/METADATA` `Name` and `Version`) that the template imported at another version, naming both; put `/pkg/src` and `/pkg/deps/<target>` first on `sys.path`; import `entry`; build the adapter's kernel from the attribute and the manifest's declaration and hints (a plain callable, a class with `setup` and `__call__` when `stateful`, a Polars-signature function through the Polars wrapper, MH 4.9; a `@moruna.kernel` object is refused, because the manifest is the declaration); build the child's gate over a sampler that reads `memory.current` through the kept descriptor and the counters page's `ceiling` (`Sample::ceiling_bytes`); send `ready`. Then per `call`: read the IPC stream at the offset through `pyarrow.foreign_buffer` over the mapping whose base is the call's lease (zero copy, SB-I13), run `PyKernel::call_arrow` (the hooks, the frame, the gate, the Arrow pool counts, 05 f.10 to f.14), write the result as one IPC stream into the output region the call names, update `in_use`, `peak`, `calls`, and reply `done` or `error` with the leases released by then, after every object of the call that the kernel did not keep has gone. An output that does not fit its region is an `error`. Per `release`: `gc.collect()`, then `released`.

**f.7 The stage ceiling.** `ceiling_in_force − (sample.anon_bytes − own_in_use)` from a fresh sample of the run's sampler, never below zero, at start, before every call, and on `limits_changed`; when it differs from the page's, Moruna writes the page first and then `memory.max`. Without a bound sampler (a `moruna load` or a `moruna check`), it is the ceiling given to `bind_run`.

**f.8 A pex call (Moruna's side).** Under the stage's lock: refresh the stage ceiling (f.7); write the input as an IPC stream into the largest free range and lease it (SB-I13); when it does not fit and the child holds leases, send `release` and retry, sleeping `LEASE_POLL` between rounds that release nothing, and refuse after `LEASE_WAIT` without a release; when it does not fit and nothing is leased, a `Kernel` error naming the batch and the data area; send `call` with the largest free range left as the output's; read the reply; on `done`, read the output stream in place through contracts e.8, each message checked (SB-I14), and land it into the arena (05 f.3); punch the output's range (e.4); free and punch the released leases; `set_kernel_alloc` with the child's counts and `lease_wait_us`; on `error`, the same counts and `MorunaError::Refused` (with the stage and morsel sentinels of 05) when a refusal is attached, `MorunaError::Kernel` otherwise.

**f.9 The template.** Started with `env_clear()` plus `OPENBLAS_NUM_THREADS=1`, `OMP_NUM_THREADS=1`, `MKL_NUM_THREADS=1`, `-I -m moruna._sandbox`, its socket at descriptor 3 and its stdout on Moruna's stderr. It sets `JE_ARROW_MALLOC_CONF=background_thread:false` (pyarrow's Linux x86_64 wheels link a jemalloc whose constructor starts a background purging thread when libarrow loads), then imports `pyarrow`, `pyarrow.compute`, `pyarrow.ipc` (required) and `numpy` (when its interpreter has it), and `moruna._core`, checks SB-I10, sends `hello`, and serves requests: for `start`, `os.fork()`; the child closes the template's socket and returns `_core._sandbox_child(...)`'s code through `os._exit`; the template closes its copies of the passed descriptors and answers `started`; for `status`, it reaps the child (waiting up to two seconds) and answers how it ended. End of stream on its socket ends it. One template per sandbox interpreter per Moruna process, restarted when it has died.

**f.10 The guest (MH 4.8).** The image carries, beside the free-threaded interpreter at `/opt/python`, the standard build at `/opt/python-std` with `pyarrow` and the `moruna` wheel for `cp314`; the guest kernel has `CONFIG_NAMESPACES`, `CONFIG_SECCOMP_FILTER`, `CONFIG_SECURITY_LANDLOCK` (and `CONFIG_LSM` naming it), `CONFIG_CGROUPS`, `CONFIG_MEMCG`, `CONFIG_SWAP` (without it the kernel has no `memory.swap.*`, and SB-I6 sets `memory.swap.max` to 0), `CONFIG_CGROUP_SCHED`, `CONFIG_FAIR_GROUP_SCHED`, `CONFIG_CFS_BANDWIDTH`, `CONFIG_MEMFD_CREATE`; `moruna-init` mounts `cgroup2` at `/sys/fs/cgroup`, enables `+memory +cpu` in the root's `cgroup.subtree_control`, creates `/sys/fs/cgroup/moruna-sandbox` with the same controllers enabled, and starts `moruna serve` with `--sandbox-python /opt/python-std/bin/python3 --sandbox-cgroup /sys/fs/cgroup/moruna-sandbox`.

**f.11 Arguments and assets (16 b).** A `.wasm` with the hint `args` is called `configure(<arguments as JSON>)` on every new instance, before `open`; an error refuses the stage. `read-asset(name)` reads `<assets>/<name>` whole when `name` is relative, has no `..` component and resolves inside the assets directory; anything else is the `Err` naming why. A `.pex` child bind-mounts the assets directory at `/assets` (17 f.5 step 3) and, for a kernel with `args`, calls the entry once with the arguments as a `dict` after import; what it returns is the kernel (a callable, or for a stateful kernel an object with `setup` and `__call__`).

**f.12 Loopback.** Nothing of Moruna's needs a network interface in the guest, loopback included: the control socket is a socket pair and batches cross a memfd. A host may bring `lo` up for its own use; Moruna neither needs nor uses it.

**f.13 Ports (amended 2026-10-21, MH 4.10).** A run's build binds each `.wasm` stage to its ports (`StagePorts`: the names its entry lists, each a host or a guest route with its deadline, and the stage id), the facade binds them to the run's cancel and its record of waits, and the linker's `call` hands the request to the named route and blocks the call until the answer, the deadline or the cancel. `ok` and the port's refusal return to the component; an unlisted name, a request or answer past 64 MiB, a missed deadline, a lost answer and the cancel trap, and the trap becomes the stage's error (`MorunaError::Kernel` naming the port and the cause; `Cancelled` for the cancel). A stage with no ports bound (a check, a load) refuses every call, saying it called outside a run. A stateful stage that lists ports runs `close` then `open` as each `process` begins and keeps the bytes; while a call of it waits they are its `Kernel::waiting_state`, which the checkpoint writes instead of waiting for the busy instance (MH 4.7). The `.pex` runtime has no `call` and a document that lists ports for a `.pex` is refused.

## g. Concurrency within the component

A stage's instance is used by one worker at a time (`Stateful { max_instances: 1 }`) and every call takes the stage's lock as well. The child is single threaded. The counters page is written by one side per field. The template serves one request at a time; the parent side serialises its requests with a lock.

## h. Behaviour

- A kernel that allocates past the stage ceiling: refused by its gate in the child (counted, `Refused`), or, for memory the hooks cannot see, by the cgroup: the child is killed by the kernel's OOM handling and the call fails as SB-I11 says, naming the exit status.
- A kernel that loops: a `.wasm` runs out of fuel (SB-I3); a `.pex` is bounded by `cpu.max` and the run's deadline, which is the host's.
- A kernel that tries to read `/etc/passwd`, the disk or `/proc`: `ENOENT` (not in its namespace) or `EACCES` (Landlock).
- A kernel that tries `socket()`: `EPERM`.
- The template dies: every child it forked keeps running; the next `start` request restarts it.
- A run on a host that is not Linux: a `.pex` stage is refused when built ("a .pex runs in the sandbox, which needs Linux"); a `.wasm` runs.
- A kernel that keeps a reference to its input batch past the call (a stateful kernel's buffer, a cache, a closure): the batch keeps its own bytes, because its region stays leased until the last buffer over it goes (SB-I13); the stage holds that much of the mapping, which its footprint and the profile show. When the kept inputs fill the data area, a call waits for releases (`lease_wait_us`) and is refused after `LEASE_WAIT`; a kernel that must keep many batches copies the rows it needs.

## i. Configuration

| Name | Value | Reason |
|---|---|---|
| `FUEL_FLOOR` | 20 000 000 000 | a call of a small batch still gets tens of seconds of compute |
| `FUEL_PER_INPUT_BYTE` | 100 000 | fuel grows with the batch; a kernel that needs more per byte is a kernel to rewrite |
| `TABLE_ELEMENTS_MAX` | 1 000 000 | a table is function references, not data |
| `INSTANCES_MAX`, `MEMORIES_MAX` | 64, 16 | a component's core instances and memories |
| `FRAME_MAX_BYTES` | 1 MiB | a control message is never a batch (SB-I7) |
| `STOP_GRACE` | 2 s | a child asked to stop has this long before `SIGKILL` |
| `CPU_WEIGHT` | 100 | the cgroup default: a packaged stage weighs what Moruna's own threads weigh |
| `REPLY_TIMEOUT` | none | a call's length is the kernel's; a dead child ends it (SB-I11) |
| `LEASE_WAIT` | 10 s | a call waits this long without a release before it is refused (SB-I13): longer than a collection, shorter than a stage worth waiting on |
| `LEASE_POLL` | 10 ms | how often a waiting call asks the child again |

## j. Observability

The run report's per-stage `alloc` gains the `linear` source (04). A child's start, ready, refusal and exit are `tracing` events with target `sandbox`; compile notes and tree notes are run notes.

## k. Tests

**SB-T1 identity_component.** A component written in the WAT text format whose `process` returns its input runs through `WasmKernel` and the output equals the input, with `linear.measured`. SB-I1.

**SB-T2 import_refused.** A component importing a function other than the world's two is refused naming the import, and `call` with another type is refused naming its type; one with an extra export, a wrong `process` type, or `open` without `close`: each refused naming it. f.2, PK-I4.

**SB-T3 growth_past_the_ceiling_refused.** A component whose `process` grows its memory one page at a time until `memory.grow` fails and then traps: the call ends as `Refused`, `linear.refused` ≥ 1, `linear.bytes` is the growth admitted. SB-I2.

**SB-T4 gate_refuses_growth.** The same with a gate over `FakeSampler` whose room is smaller than one growth: refused by the gate, and a kernel whose manifest says `memory_guard: false` is not refused by the gate. SB-I2.

**SB-T5 fuel_exhausted.** A component whose `process` loops: `Kernel` error starting `wasm: fuel exhausted`. SB-I3.

**SB-T6 stateful_component.** A component with `open` and `close` keeping a count: the checkpoint is `close` then `open`, a restored instance continues the count. SB-I12.

**SB-T7 cwasm_reused.** After `aot`, the kernel is built with no compile note; with a `.cwasm` whose header names another fingerprint it is compiled and the note says why. f.3.

**SB-T8 protocol_frames.** Every message of e.3 round-trips; a frame over `FRAME_MAX_BYTES` and a frame that is not JSON are refused. e.3.

**SB-T9 counters_page.** The page's fields are at e.4's offsets; a value written by one mapping is read by another of the same memfd; a punched range reads back as zeros. e.4.

**SB-T10 seccomp_list_is_the_documents.** The allow-list in code equals e.5 for the build's architecture. SB-I5.

The following run on Linux with a sandbox host (a privileged container: tests tagged `(Linux, sandbox host)`; they read the interpreter from `MORUNA_SANDBOX_PYTHON` and the cgroup from `MORUNA_SANDBOX_CGROUP`, test configuration only):

**SB-T11 pex_identity.** A packaged identity kernel through `SandboxKernel`: output equals input, counts measured, the child's pid differs from the template's and Moruna's. SB-I7, SB-I8.

**SB-T12 confined_reads.** A kernel that tries to open the job document's path, `/etc/hostname` and `/proc/self/status` and returns what each attempt raised: `FileNotFoundError` or `PermissionError` for each, and `/pkg/src/function.py` readable. SB-I4.

**SB-T13 no_socket.** A kernel calling `socket.socket()` and `os.execv("/bin/sh", …)`: both raise `PermissionError`. SB-I5.

**SB-T14 guard_in_the_child.** A guarded kernel asking `numpy.empty` past the stage ceiling: `Refused` in Moruna with the child's figures, `numpy.refused` ≥ 1; the counters page's `peak` rose. SB-I8.

**SB-T15 cgroup_is_the_hard_line.** The child's cgroup has `memory.max` equal to the stage ceiling and the run's `cpu.max`; it is gone after the stage is dropped. SB-I6.

**SB-T16 dead_child.** A kernel calling `os._exit(3)`: `Kernel` error naming the pid and status 3; the next call fails the same way. SB-I11.

**SB-T17 sampler_sees_the_child.** With a child holding 64 MiB between calls, the process sampler's `anon_bytes` rises by at least that much. SB-I9.

**SB-T18 library_conflict.** A package carrying `pyarrow` at a version the template does not have: refused naming both versions. f.6.

**SB-T19 template_threads.** A template whose interpreter started a thread at import refuses to start, naming the count. SB-I10.

**SB-T20 pex_state_crosses_a_checkpoint.** A stateful class kernel with `checkpoint` and `restore` counting its calls: the checkpoint is the kernel's bytes, a restored child continues the count. SB-I12.

**SB-T21 a_kept_input_keeps_its_bytes.** A stateful kernel that stores its input and returns the stored batch on the next call: the second call returns the first batch's values, not the second's; one lease is held after each call and the footprint counts it. SB-I13.

**SB-T22 backpressure.** With a 256 MiB stage and batches of about 26 MiB: a kernel keeping only its last batch runs 30 calls with no wait; a kernel whose kept batches are garbage in reference cycles (the collector disabled) runs 30 calls and some wait for the collector, counted in `lease_wait_us`; a kernel keeping every batch is refused after `LEASE_WAIT`, naming the leased bytes and the data area, with the wait counted. SB-I13.

**SB-T23 regions.** Leases split the free ranges, releases join them back, the largest free range starts aligned, a range inside a lease or outside the area cannot be leased, and a release twice is nothing. e.4.

**SB-T24 the_call_import (MH 4.10).** A component that calls a listed guest port gets its answer as its output; the port's refusal is the component's; an unlisted name is refused naming it and the list; a call past its deadline fails naming the deadline; the run's cancel ends a waiting call as `Cancelled`; a call outside a run is refused; a waiting stateful instance's `waiting_state` is its state as the call's `process` began, and a restore from it counts on from there. f.13, SB-I1.

**SB-T25 wasm_output_malformed_never_panics.** (2026-10-28; `moruna-wasm` `src/ipc.rs`) An output stream whose schema names type id 229 is `MorunaError::Kernel` naming field `i` (before SB-I14, `read_one` panicked in arrow-ipc's `fb_to_schema`, `Type <UNKNOWN 229> not supported`); every case of `moruna_testkit::ipc::corruptions` over each of its `streams()` is an error or one batch, never a panic (before SB-I14, 17,123 of them panicked). SB-I14.

**SB-T26 pex_output_malformed_never_panics.** (2026-10-28, Linux; `moruna-sandbox` `src/kernel.rs`, over a memfd mapping and no child) The same two cases as SB-T25, written into a mapping's data area and read in place by `read_one`, which before SB-I14 ran the same `StreamDecoder` loop as the wasm side and so panicked on the same streams. SB-I14.

## l. Implementation notes for the agent

`unsafe` is permitted in `moruna-sandbox`'s `mapping.rs` (`mmap`, `memfd_create`, `fallocate`, the atomics over the mapping), `confine.rs` (`unshare`, `mount`, `pivot_root`, `prctl`, `capset`), `fdpass.rs` (`sendmsg` with `SCM_RIGHTS`), `template.rs` (`dup2` and `fcntl` in `pre_exec`), `kernel.rs` (`pidfd_open`, `pidfd_send_signal`), `python.rs` (taking the two descriptors the template passed, the memoryviews and foreign buffers over the mapping) the test of `seccomp.rs` (a forked child that installs the filters) and `coverage.rs` (the profile runtime's buffer), and in `moruna-wasm`'s `cwasm.rs` (`Component::deserialize`, whose bytes are trusted as the tree is: written beside the package by whoever ran `aot` with write access there, 16 PK-I7). The Linux-only modules are `#[cfg(target_os = "linux")]`; on other systems the crate builds and `SandboxKernel::new` refuses.

Coverage counts the forked children. A child of the template and the test's forked child end through `_exit`, which skips the exit handler that writes a coverage build's profile, and a confined child cannot open the profile's path; so in a coverage build (`cfg(coverage)`, which `cargo llvm-cov` sets) the template is given `LLVM_PROFILE_FILE`, each such child creates a file of its own beside that path before it confines itself, and writes the profile runtime's buffer into it before it ends. The crate's coverage is measured on Linux with the instrumented `_core` module the template imports.

Environment facts to verify before starting: Landlock and seccomp filters are available in the test container's kernel (`/sys/kernel/security/lsm` names `landlock`); the container may create cgroups (`cgroup.subtree_control` writable after moving the test into a leaf).

## m. Open items

None.

## n. Traceability

| Parent | Invariant | Test |
|---|---|---|
| FP 4 `moruna-wasm` | SB-I1, SB-I2, SB-I3 | SB-T1 to SB-T5 |
| FP 4 the fork of a warm template | SB-I10, f.9 | SB-T11, SB-T19 |
| FP 4 the namespace, Landlock, seccomp, capabilities | SB-I4, SB-I5 | SB-T10, SB-T12, SB-T13 |
| FP 4 the cgroup | SB-I6 | SB-T15 |
| FP 4 the shared mapping | SB-I7, SB-I13, e.4 | SB-T9, SB-T11, SB-T21, SB-T22, SB-T23 |
| FP 4 the guard across the boundary | SB-I8, SB-I9 | SB-T14, SB-T17 |
| MH 4.7 (state across a checkpoint) | SB-I12 | SB-T6 |
| MH 4.10 (a stage's ports, 2026-10-21) | SB-I1, f.2, f.13 | SB-T2, SB-T24 |
| a function's output is untrusted (contracts CT-I13, 2026-10-28) | SB-I14, f.4, f.8 | SB-T25, SB-T26 |
| FP 2, MH 4.8 (the guest) | f.10 | the guest build |

## o. Deferred (post-v1)

**SB-O1. The child's Arrow allocating in Moruna's arena.** A memory pool over the shared region, shipped as a small extension, removes the outgoing copy (FP 4, "a later refinement").

**SB-O2. A PID namespace for the child.** Signals to other processes are refused by seccomp (SB-I5); a PID namespace would hide them as well.
