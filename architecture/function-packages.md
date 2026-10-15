# Function Packages, One File, Two Runtimes: Architecture Design

**Document type:** full architecture design (not an ADR)
**Status:** DRAFT for review · 2026-10-15
**Scope:** how a kernel becomes one portable file that carries its code and its dependencies, how Moruna runs such a file without giving it Moruna's own access to the machine, and how a program on any machine loads and calls it. OUT: a build system for languages other than Python (an author brings a wasm toolchain), Python compiled to WebAssembly (§7), what a host stores, shares or meters (a host's concern; this document names no host).
**Companion:** `moruna-hosted-engine.md` (MH), §4.9 for the kernel, its declarations and `moruna check`, which this document consumes unchanged; §4.1 for the job document; §4.8 for the guest image.

## 1. Problem statement

**P1. A checkable kernel is not a portable one.** §4.9 makes a function checkable and fingerprinted, but the function is still a Python module whose dependencies are whatever the importing interpreter has, or a Rust crate that has to be compiled into the guest. There is no artefact to move.

**P2. The lockfile is a hash, not an environment.** The lockfile is folded into the fingerprint and never installed. A kernel runs with the interpreter's packages, not its own.

**P3. Tenant code runs in Moruna's process.** A Python kernel is imported into the interpreter beside Moruna, with everything Moruna can see in view: every attached disk, the job document, the host socket. A host that runs one party's kernel over another party's data has no boundary but the machine.

**P4. One language.** A Rust kernel's path ends at a check; nothing else has a path at all.

## 2. The package

One file per function, its kind its extension:

| format | holds | for |
|---|---|---|
| `.wasm` | one WebAssembly component exporting the `moruna:kernel` world (§3), every dependency linked in | any language with a wasm toolchain: Rust, Go, C, Zig, … |
| `.pex` | a Python executable zip: `function.py`, the sibling files, and the **wheels** of the author's lockfile, resolved for the interpreters and platforms the manifest names, bytecode precompiled | Python and Polars |

Both embed `moruna.json`: `{name, entry, declaration: {input_schema, output_schema, hints}, abi, moruna, format, platforms}`, the declaration spelled exactly as §4.9 reads it from a decorator. The package's **fingerprint is `sha256` of the file**. For a packaged kernel it supersedes the source-and-lockfile hash of MH §4.9 step 7: the file *is* the pinned unit; a dependency changed is a different file. `moruna check` prints it, a job document pins it (`kernels[].fingerprint`), the profile store keys on it.

**Python is not compiled to WebAssembly.** The libraries a data function is made of, `pyarrow`, `polars`, `numpy`, are native code with no builds for a WASI runtime, and a 32-bit module's memory is 4 GB. A `.pex` runs a native interpreter: the **standard build** of CPython, not the free-threaded one. A packaged kernel runs in its own process (§4), so free threading buys nothing there, and the standard build is the one the ecosystem publishes wheels for. The guest image (MH §4.8) carries it for packaged kernels.

## 3. The kernel interface, for wasm

One WIT world, `moruna:kernel`:
- `declare() -> string`: the manifest, so a loader needs no file beside the module;
- `process(batch: list<u8>) -> result<list<u8>, string>`: one Arrow record batch in as an IPC stream, one out; an error is the kernel's refusal of that batch, named;
- `open(state: list<u8>) -> result<_, string>` and `close() -> list<u8>` for a stateful kernel (`stateful` hint), so state crosses a checkpoint (MH §4.7) as bytes.

The component imports nothing that reaches the machine: no filesystem, no sockets, no clock beyond what the interface passes in. A loader refuses a module with any other import. (Amended 2026-10-15, for a host's kernels built by arguments, model inference first: a kernel may export `configure(args: string)` to receive its arguments once, and import the world's own `read-asset(name)`, which reads a file of a read-only assets directory the host names and nothing else; a `.pex` sees the same directory read-only at `/assets` in its child. SDD 16 e.5, 17 f.11.) Arrow IPC bytes are the interface because every language has an Arrow IPC reader and none shares a memory layout with the host; the copy is one per batch each way and is what the profile measures.

## 4. The two runtimes, neither with Moruna's access to the machine

Today a Python kernel is imported into Moruna's own interpreter and sees what Moruna sees. A packaged kernel in a hosted run never does, whoever owns it and whoever is running it:

- **`moruna-wasm`**: wasmtime inside Moruna's process, one instance per stage, with a linear-memory limit and a fuel limit taken from the stage's budget. The module can touch only its own linear memory and call only what the `moruna:kernel` world hands it; it has no path to Moruna's memory, disks, sockets or another stage. Every growth of its memory passes through wasmtime's resource limiter, which is where Moruna intervenes; CPU is fuel.
- **`moruna-sandbox`**: for `.pex`, a separate operating-system process, one per stage, started once for the run and kept for its state. It is a **fork of a warm template**: the guest keeps one interpreter started with the data libraries imported and idle, and a stage's child is a fork of it, so starting one costs a millisecond and the libraries' pages are shared copy-on-write rather than loaded again. After the fork, before any kernel code, the child enters a mount namespace that holds only the package's extracted tree (read-only), a private temporary directory and the control socket; Landlock over those paths; seccomp allowing the file, memory and socket calls the interpreter needs and no `connect`, no `execve` outside the tree, no namespace calls; no capabilities; a cgroup with `memory.max` at the stage's ceiling and the stage's CPU weight. Then it loads `<package>.pex`. **Batches do not cross the socket:** Moruna places each input batch in a shared mapping (a memfd the child has mapped) and names its offset on the control socket; the child's Arrow reads it in place, zero copy, as it does in-process today; the output goes back into the mapping, one copy, as the boundary copy is today (SDD 05, decision 1). The child never sees the job document, the host socket, or any disk but its own tree.

**The guard across the boundary.** The allocator guard (SDD 05, E13) is Moruna's code, not the kernel's, and it goes where the kernel goes. The child is not a bare interpreter: it is Moruna's runner, which loads the package under the adapter, installs the same hooks (CPython's allocator domains, NumPy's data allocator), opens the same per-call frame, counts the same way and refuses the same way, on the same fast path, inside the child. What changes:
- **The gate measures the child**, which is exactly the kernel's memory, not Moruna's process with the kernel's allocations mixed in. Moruna sends the stage's ceiling at start and whenever it changes (a resize, the controller's shrink target); the child's gate holds that line.
- **The counts reach the trace as before.** Each call's `KernelAlloc` travels back with the batch's reply, so the call's trace record and the report's per-stage figures are built unchanged; peak and in-use are also in a shared-memory page the sampler reads at its tick, so the controller sees the stage between batches.
- **A hard ceiling the hooks never gave.** What a C extension allocates past the hooks (raw `malloc`, invisible to E13 today) the cgroup refuses; per-stage CPU is exact. The adapter's GIL serialisation (SDD 05 decision 2) has no work across processes.
- **Arrow is exact.** The child has its own pyarrow pool, so a call's Arrow figures are the kernel's alone (AD-I15's shared-pool caveat is gone).

**The guarantees, side by side.** What a host relies on when it runs one party's kernel over another party's batches:

| guarantee | `.wasm` in `moruna-wasm` | `.pex` in `moruna-sandbox` |
|---|---|---|
| cannot read a disk, the job document or the host socket | it has no way to name them | its namespace holds only its own tree; Landlock and seccomp refuse the rest |
| cannot open a connection | no such import | no socket call but the inherited one |
| memory capped, Moruna told before the cap | the linear-memory limit and the resource limiter | the guard in the child, and the cgroup for what the guard cannot see |
| CPU bounded and measured | fuel | the cgroup |
| per-call counts, refusal, shrink | yes | yes, over the socket and the shared page |
| residual risk | a wasmtime escape | a Linux kernel escape from a confined process |

Both residuals sit inside the microVM that is the host's tenant boundary. The costs of the child are not guarantees, and with the fork and the shared mapping they match the in-process path: a millisecond to start, the libraries' pages shared, a batch read in place and copied once on the way out, as today. A later refinement makes the child's Arrow allocate its output in Moruna's arena (a memory pool over the shared region, which pyarrow takes only from C++, so Moruna ships it as a small extension), and the outgoing copy disappears too; the arena stays Moruna's.

The in-process `pyo3` path of SDD 12 is not removed: it is the **library** path, `moruna.run(...)` over a function object in the author's own interpreter, and the path of the standard kernels (MH §4.9). A **hosted** run (MH §4.3) loads a kernel through a package and one of the two runtimes above, never in-process; strict mode refuses a job document that names a Python module by import path.

## 5. The job document

`kernels[]` gains `{ "kind": "package", "path": "<file on a disk the run attaches>", "fingerprint": "sha256:…" }`, with optional `args` and `assets` (amended 2026-10-15; SDD 16 d.4). Before a stage starts, Moruna hashes the file and refuses the run on a mismatch; it then picks the runtime from the extension and the budget from the stage. The host decides where the file is and how it got there; Moruna reads it from a disk like any other input.

## 6. The commands

- **`moruna package <project>`**: a Python project (`function.py`, `function.yaml`, a lockfile) becomes `<name>.pex`, its wheels resolved for the interpreter tags and platforms given with `--platform` (the guest image's, by default: MH §4.8 names them) and never for the machine it runs on; refused when a wheel the lockfile pins does not exist for a named platform, naming it.
- **`moruna package --wasm <module.wasm> <project>`**: a component the author built with their own toolchain is verified against the interface (exports, imports, `declare()` agreeing with `function.yaml`), the manifest is embedded, and the result is `<name>.wasm`.
- **`moruna check <package>`**: the seven steps of MH §4.9 **through the package's runtime**, not in-process, so the verdict is the one a hosted run would give.
- **`moruna aot <package>.wasm`**: writes `<package>.cwasm`, wasmtime's precompiled form for this Moruna version and CPU target, named by the package's fingerprint and the Moruna version inside it; a loader uses it when both match and otherwise compiles and says so. For a `.pex` the equivalent is the extracted tree with its `.pyc` files, written once beside the package by whoever runs it first with write access to that place. Either way a second load is milliseconds, which is what lets one serving process hold many kernels.
- **`moruna load <package>`**: returns a kernel the library path can call, running under that same runtime on this machine: wasmtime in-process, or the sandboxed child. This is what a client library's "run it here" mode is.

## 7. Explicit risks and accepted positions

- **Not a build system for other languages.** The author brings a wasm toolchain; `moruna package --wasm` verifies, it does not compile.
- **Not Python-in-wasm.** Accepted for the reasons in §2; revisit when the data libraries publish for a WASI runtime.
- **Two residual risks, one class.** A wasm kernel's boundary is wasmtime; a pex kernel's is the Linux kernel under seccomp and Landlock. Both are inside the host's microVM. Accepted.
- **The wheel ecosystem decides what a `.pex` may contain.** A pin with no wheel for a named platform is refused by name, never substituted.

## 8. Component amendments, by SDD

| SDD | amendment |
|---|---|
| `12-python.md` | `moruna package`, `moruna aot`, `moruna load`; the library path unchanged |
| `15-check.md` | a package checked through its runtime (§6) |
| **new `16-package.md`** | the package (§2): the two formats, `moruna.json`, the `moruna:kernel` world (§3), the fingerprint, the three commands (§6); `kernels[].kind = package` in the job document (§5) |
| **new `17-sandbox.md`** | `moruna-wasm` and `moruna-sandbox` (§4): the wasmtime limits and limiter; the warm template and the fork, the child's namespaces, Landlock, seccomp, the cgroup, the control socket and the shared mapping for batches, the guard in the child and the shared counters page; strict mode's refusal of an unpackaged kernel |
| `14-vmm.md` | the guest image carries the standard-build interpreter for packaged kernels (§2) |

## 9. Implementation plan

1. `16-package.md`: the manifest, the world, `moruna package` for a project and for a wasm module, the fingerprint; `moruna check` over a package.
2. `17-sandbox.md`: `moruna-wasm`, then `moruna-sandbox` with the guard inside it; strict mode.
3. `moruna aot` and `moruna load`.
4. The guest image: the standard-build interpreter beside the engine's own.
5. A release, which a host pins.
