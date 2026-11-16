# A lean, contract-only Moruna for 0.7.0

Status: plan, revised 2026-11-16 on Brackly's direction.

- **No engine or library code in Moruna.** People write whatever they want, as long as it follows
  the contract. Library code is removed from the repository, not put behind feature flags. That
  includes NumPy: users convert from Arrow themselves.
- **Data formats may be exceptions.** Moruna may keep a file format it reads or writes natively.
  It may not keep an engine.
- **Analytical workloads, with tensors in scope as data.** That is where the speed-up comes from.
  Fewer translations and less deserialization, adaptiveness and CPU memory use all stay.
- **No regressions.** Every benchmark 0.6.6 ran has to hold, and §7 is the gate that proves it.
- **One platform: Linux on x86_64** (Brackly, 2026-11-16), Griot's Azure Standard_D4ls_v5
  (4 vCPU, 8 GiB), for staging and production. Releases build only the Linux x86_64 wheels, for
  CPython 3.14 and 3.14t; macOS arm64 and Linux aarch64 stop at 0.6.5 (`8018c96a` on
  `fix/adaptive-runtime`). 0.7 does no macOS or aarch64 work; a Mac stays a development machine.

Read from `main` at `d4165127` (0.6.5). Aligned with the 0.6.6 release session on 2026-11-16: its
hot paths, its sink rule and its CI changes are folded in below. Every 0.6.6 commit cited is on
`fix/adaptive-runtime`.

## The rule

Moruna keeps three things:

- **What it is good at.** Memory-aware execution: the arena, scheduler, controller, placement,
  reactor, storage, resume and the report.
- **The contract.**
- **Native I/O for Arrow's own formats, plus the data formats kept as exceptions.** That means
  safetensors and npy for tensors.

Everything that exists to serve one engine or library leaves. Code written with any library runs
through the contract.

**The contract:**

- **Tables are Arrow.**
  - In Python: any object that offers `__arrow_c_array__` or `__arrow_c_stream__` (the Arrow
    PyCapsule interface).
  - In Rust: `RecordBatch`.
  - Between processes: Arrow IPC.
- **Tensors.** A tensor is a DLPack tensor, as everywhere else. Moruna hands tensors out and
  takes them in beside Arrow, and converts between the two without copying where layout allows.
  It never defines its own (see "Tensors").
- **What Moruna hands user code.** A `pyarrow.RecordBatch`, which every Arrow library reads
  zero-copy through the same interface. `pyarrow`, Arrow's own Python library, remains the one
  Python dependency (`pyproject.toml:13`).

## Decisions (Brackly's, all settled 2026-11-16)

- **D1. Tensors.** Settled 2026-11-16, see "Tensors" below. Arrow is the main format. Tensors are
  first-class beside it, because the cost is small.
- **D2. Vortex.** Dropped.
  - **Why.** The rule would have allowed it as a file format, but it is the largest dependency: 131
    crates on its own, and 25 MB (17%) of the machine code (§5).
  - **Who uses it.** No benchmark and no microkernel fixture.
  - **What users do instead.** Read it with Vortex's Python package in a `moruna.Source`.

## Tensors

**Brackly's direction.**

- Arrow is the main format, and Moruna gives an interface from Arrow to tensors.
- If accepting tensors is cheap, Moruna gives them the same interfaces as Arrow, saving to
  safetensors included.
- A tensor means what it means everywhere: a DLPack tensor that Torch, JAX, CuPy and NumPy take
  as their own.
- Moruna defines no tensor of its own.

**Verdict: cheap.**

- It costs 3 crates (`dlpark`, `snafu`, `snafu-derive`) and about 116 KB of machine code.
- Most of the interfaces exist. The gaps below are wiring, not new subsystems.
- 0.7 does it.

**Tensor interfaces against Arrow's:**

| Interface | Arrow | Tensors today | 0.7 |
|---|---|---|---|
| Read files | Parquet, Arrow IPC | safetensors, npy and `MRB1`, into the arena by first-axis row ranges (`crates/moruna-sources/src/tensor/read.rs:35`) | Keep safetensors and npy. Delete `MRB1`, Moruna's own format (`plan.rs:63, 130`). |
| Write files | Parquet, Arrow IPC | safetensors and `MRB1`, with no copy (`crates/moruna-sinks/src/tensor_sink.rs:1-5, 31-37`) | Keep safetensors and delete `MRB1`. Add npy writing, so a format read is a format written. |
| Function input | `pyarrow.RecordBatch` | `accepts="tensor"` gives a DLPack object (`crates/moruna-adapters/src/python/tensor_obj.rs:1-10`) | Keep. Also wire Arrow to tensor when the input is a table (below). |
| Function output | Arrow | Any `__dlpack__` object (`crates/moruna-adapters/src/python/cross.rs:68`) | Keep |
| Python `moruna.Source` / `IteratorSource` | Returns Arrow | Refused: `read` takes only Arrow (`crates/moruna-sources/src/py_source.rs:169`), and a tensor schema is a plan error (`py_iter.rs:57-61`) | Accept `__dlpack__`, imported as `cross.rs` already imports a function's result |
| Python `moruna.Sink` | `write(batch)` | Refused (`crates/moruna-sinks/src/py_sink.rs:163-168, 206-211`) | Hand the sink the DLPack object functions already get |
| Parquet and IPC sinks, `image` tasks, `moruna.std` | Native | Refused: they take tables (`crates/moruna-sinks/src/parquet_sink.rs:449-451`, `ipc_sink.rs:645-649`) | Through tensor to Arrow (below), or refused at plan time, as now |
| Memory, placement, resume | Arena, tiers, manifest | Same machinery: placement keeps tables and tensors alike (`crates/moruna-placement/src/state.rs:143`; manifest `manifest.rs:217-225`) | Nothing to do |

**Arrow to tensor (`Payload::as_tensor`, `crates/moruna-kernel/src/payload.rs:374-435`).**

- **When there is no copy:**
  - one numeric column with no nulls, giving a 1-D tensor;
  - one fixed-size-list column of numbers with no nulls, giving `[rows, width]`.
- **The dtypes:** `i8` to `i64`, `u8` to `u64`, `f16`, `f32`, `f64` (`payload.rs:106-140`).
- **Several same-dtype columns as `[rows, columns]`** work only when their buffers sit side by
  side, and are refused otherwise rather than copied (`payload.rs:421-426`).
- **A bug to fix before wiring this.** That multi-column view takes buffers laid out column by
  column, but builds the tensor with row-major strides (`crates/moruna-kernel/src/tensor.rs:311`).
  So the shape is right and the values are transposed. The test checks only shape, dtype and
  pointer (`crates/moruna-kernel/tests/ct_t4_as_tensor_zero_copy.rs:114-120`). The fix is strides
  `[1, rows]` (column-major).
- **Not wired.** The plan check already accepts a numeric table feeding an `accepts="tensor"`
  function (`payload.rs:233-250`). But at run time the function gets a pyarrow batch, because
  `export` passes payloads through as they are (`cross.rs:30-37`). Wiring it is one call where a
  stage that asks for tensors receives a table.

**Tensor to Arrow (`Payload::as_column`, `payload.rs:437-493`).**

- **Possible, with no copy, when all of these hold:**
  - the tensor is in host memory;
  - it is contiguous;
  - its dtype is one Arrow has.
- **How it avoids the copy.** The Arrow buffer is the tensor's own bytes, and it keeps the DLPack
  tensor alive (`Buffer::from_custom_allocation`, `payload.rs:464-474`).
- **Today's limit.** Rank 1 becomes a primitive column. Rank 2 `[n, k]` becomes `n` rows of
  fixed-size lists of width `k`. Higher ranks are refused (`payload.rs:456-458`).
  - For rank 3 and above, 0.7 uses Arrow's own canonical fixed-shape tensor type, which records
    the shape so the column converts back to the same tensor.
  - arrow-rs 59 has it behind `canonical_extension_types`, which needs only crates already in the
    graph (`arrow-schema-59.3.0/src/extension/canonical/fixed_shape_tensor.rs`).
**Zero copy or refused (Brackly, 2026-11-16).**

- **The rule.** Every conversion between Arrow and a tensor, and every DLPack hand-over, either
  shares the bytes or is refused. Moruna never falls back to a copy.
- **What the refusal says.** It names the column or stage, the reason, and what the user changes.
- **When it happens.**
  - At plan time, for what the schema shows: dtype, nullability, rank. `payload.rs:233-250` already
    refuses nullable and non-numeric tables there.
  - At run time, naming the stage, for what only the bytes show: contiguity, alignment, device.

| Refused | Why there is no zero-copy form | What the user does |
|---|---|---|
| A nullable or non-numeric column, to a tensor | A tensor has no null slots and no strings | Fill nulls or cast before the stage, or keep it Arrow |
| Columns of mixed dtypes, or not side by side, as one `[rows, columns]` tensor (`payload.rs:413, 421-426`) | One tensor needs one dtype and one block of memory | Ask for one column, or one fixed-size-list column |
| A `bool` tensor, to Arrow | Arrow packs booleans as bits, and DLPack stores a byte each | Cast to `uint8` in the function |
| A `bf16` tensor, to Arrow | Arrow has no bfloat16 type (`payload.rs:119`) | Cast to `f16` or `f32` in the function, or keep it a tensor |
| A strided (non-row-major) tensor, either way | Arrow buffers and Moruna's tensors are contiguous (`payload.rs:453-455`; the boundary refuses it too, `cross.rs:132-147`) | Call `.contiguous()`, or its equivalent, before returning |
| A device (GPU) tensor, to Arrow | Arrow's C Device interface is not in arrow-rs 59 or pyo3-arrow 0.19 (`cross.rs:38-40`) | Keep it a tensor, or move it to host in the function |
| A buffer not aligned for its type, to Arrow | arrow-rs requires natural alignment | Return a freshly allocated tensor; libraries align by default |
| A 0.x-only DLPack capsule, in or out | Moruna speaks DLPack 1.x (`tensor.rs:181-190`, `tensor_obj.rs:65-77`) | Use a library version that speaks DLPack 1.x |

- **What the rule does not cover: the arena landing copy.** That is a copy into Moruna's memory,
  not a conversion. 0.7 removes it for pyarrow's columns, and removes the Parquet decode copy, by
  making those producers allocate in the arena (see "Memory", items 1 and 2).

**Is tensor to Arrow wanted?** Yes, as an interface rather than as the tensor's home. It is what
gives tensors the Arrow-only interfaces (Parquet, IPC, `image` tasks, user sinks) without a tensor
twin for each. A tensor stays a DLPack tensor inside Moruna, and converts only when it meets
something that takes Arrow. It does not matter for the benchmarks, which never use tensors.

**The work for 0.7:**

1. **Fix the multi-column strides bug** and test the values, not just the shape.
2. **Zero copy or refused.** One test per row of the table above: each asserts the refusal and its
   message, and that no buffer was allocated.
3. **Wire Arrow to tensor** for stages that ask for tensors.
4. **Python sources, iterators and sinks take and give DLPack**, reusing the adapters' import and
   export.
5. **Tensor to Arrow:** past rank 2 through the canonical type, wired where a tensor meets an
   Arrow-only sink or task.
6. **Delete `MRB1`.**
7. **npy writing.**
8. **Move `safetensors` to dev-dependencies.** The reader parses headers itself, so this removes 3
   crates from the wheel (`crates/moruna-sources/src/tensor/safetensors.rs:1-7`).

**Dependencies to support:**

- **Rust:** `dlpark` 0.8.0, with `snafu` and `snafu-derive`.
- **Python:** none. Users bring their own tensor library.
- **The protocol: DLPack 1.x both ways.**
  - Export always hands a versioned capsule (`tensor_obj.rs:65-77`).
  - Import requires one (`dlpark-0.8.0/src/python/capsule.rs:186-208`).
  - A library version that speaks only the 0.x capsule cannot exchange tensors with Moruna.
  - The cached NumPy 2.5.4 speaks 1.x. Minimum Torch, JAX, CuPy and NumPy versions go in the docs
    once verified.
- **GPU tensors** need the optional `cuda` feature, which the wheel does not build
  (`pyproject.toml:35`).

## Memory: the design for no copies

Brackly, 2026-11-16: the batches are Arrow, so copying them into the arena is a waste of memory,
and wrong for large data. That applies on a Python host, and on the Moruna kernel, which is
Moruna's own.

### The principle

A buffer's bytes are written once, by whatever produces them, straight into memory Moruna counts
and can hand on. After that a batch moves only by reference inside a process, or by page mapping
between processes. The only byte writes left are format changes (decoding Parquet, encoding for a
sink), and those write directly into that memory.

### Today's copies

| Copy | Where | Why it exists |
|---|---|---|
| Parquet decode | `crates/moruna-sources/src/parquet/decode_copy.rs:1-8` | arrow-rs allocates through Rust's global allocator (`arrow-buffer-59.3.0/src/buffer/mutable.rs:133`), not the arena |
| A Python function's new columns | `crates/moruna-adapters/src/python/copy.rs:1-7`, `land.rs`; limited by 0.6.6 to columns the arena does not own (`19aaefc6`) | pyarrow's pool cannot be reached from Moruna's wheel without a per-release C++ shim (`architecture/sdd/05-adapters.md:399`, AD-O6). A process-wide allocator was rejected (`:395`, AD-O4). |
| A Python task's input and output on the Moruna kernel | Descriptors 0 and 1 (`python/moruna/_shim.py:348-360`); copies "as any read and write are" (SDD 29 f.8) | The shim speaks Arrow IPC over stdio instead of the plane's three calls |
| A task's buffers into its output window, on the kernel | The task writes the record into the window | The buffers were built in the task's heap. A task's anonymous memory is zero pages with no shareable memory object (SDD 20, call 33). |

The stated need for the host copy, "downstream DMA needs arena buffers" (`05-adapters.md:10`), does
not hold for tables. A table never moves between memory tiers (`crates/moruna-placement/src/moves.rs:607-623`),
and spills as Arrow IPC (`moves.rs:612-614`), which reads from any memory.

Keeping foreign buffers and only counting them is not enough either. The arena is mapped and
touched at start (`crates/moruna-arena/src/region.rs:3-5`), so its pages would sit idle while the
foreign ones are held.

### The primitive: the arena as an allocator

- **The interface.** The arena gets a thread-safe C interface, `alloc(size, align)` and
  `free(ptr, size)`, over the allocator it already has. Rust code and foreign allocators can both
  call it.
- **Recognising arena memory.** A buffer is recognised as arena memory by its address
  (`Allocator::tier_of`, `crates/moruna-kernel/src/buffer.rs:351`). Landing a batch already
  checks every buffer this way (`crates/moruna-adapters/src/land.rs:56-58`, `Payload::table_with` at
  `crates/moruna-kernel/src/payload.rs:300-307`),
  so a batch born in the arena lands with no copy and no new code path.
- **Lifetime.** The arena stays mapped until its last allocation is freed, so a buffer that
  outlives its run never dangles.

### On a Python host

1. **Moruna's own Rust code.** Moruna's binaries (`_core`, `moruna-engine`) install a global
   allocator that sends allocations above a size threshold to the arena, and the rest to the
   system allocator.
   - arrow-rs then decodes Parquet straight into the arena, and the decode copy finds the buffers
     already there.
   - The threshold starts at 64 KiB and is set by measurement.
   - **AD-O4, answered.** The cost per allocation is one size comparison. Freed memory goes back
     to the arena's own free lists, not to a third allocator that holds it. The §7 gate measures
     both against 0.6.6.
2. **pyarrow.** A C++ `arrow::MemoryPool` over the arena's C interface, installed with
   `pyarrow.set_memory_pool` for the run.
   - Columns a function computes are then born in the arena, and land with no copy.
   - Arrow's memory figures become exact instead of a lower bound (AD-I15).
   - It is built in CI against each supported pyarrow release (AD-O6), with the range pinned in
     `pyproject.toml`. pyarrow is the contract's own library and the one Python dependency, so
     this is the one native shim Moruna carries.
3. **Libraries Moruna cannot reach** (Torch, an engine's own allocator). Their results keep one
   copy, counted and named per stage in the run report.

### On the Moruna kernel

The kernel already counts every page (`kernel/quota/src/lib.rs:1-7`), and already moves batches
as pages (SDD 29, DP-I1). The design closes the two gaps.

4. **Input.** The Python shim uses the plane's three calls through `moruna._core`, which already
   links `moruna-sys` on Linux x86_64.
   - `batch_get` maps the batch read-only.
   - pyarrow reads it in place, with `pyarrow.ipc.read_record_batch` over
     `pyarrow.foreign_buffer(address, length)`.
   - Nothing is copied.
5. **Output.** A new call, `batch_put_runs(slot, header, runs)`, replaces writing the record into
   a window.
   - **What the task passes.** The shim lays out the record's messages in its window, and passes
     the address and length of each buffer in its heap.
   - **Whole pages.** For every buffer that starts on a page and is above the threshold, the
     kernel makes a memory object of its whole pages, shared copy-on-write between the task and
     the batch.
   - **What is still copied.** The buffer's last partial page, and any buffer that is small or
     does not start on a page, are copied into the window. That is at most one page per buffer.
     The tail is copied rather than shared because consumers map a batch read-only and fan-out
     maps it into other tasks (DP-I4). Sharing the whole last page would show them the producer's
     other heap bytes.
   - **What the consumer sees.** The kernel maps the messages and the runs one after another in
     the consumer, which sees today's page-aligned record unchanged (SDD 29 b, contracts e.7).
   - **Copy-on-write keeps the task correct.** If the task writes a page again, or its allocator
     reuses it, only that page is copied. `madvise` free just drops it.
   - **No pyarrow shim is needed here.** pyarrow's Linux wheels allocate with jemalloc
     (`python/moruna/_sandbox.py:32`), whose large allocations start on a page. A release where
     that stops holding only falls back to the copy path, so it is checked by test, not assumed.
6. **Engine to task.** The engine's decoded columns are arena runs (item 1). It pushes a batch as
   its messages plus those runs, the same way, so a task's input is never copied either. Today
   `slot_push` takes one range of one memory object (SDD 20, call 44), so it gains a list of runs.

**Kernel changes.** These are amendments to SDD 20 and SDD 29, as the next ABI version:

- a memory object made from a range of a process's anonymous pages, shared copy-on-write;
- `batch_put_runs`;
- `slot_push` with runs.

For accounting, a shared run is charged once, to the batch, while it is shared (SDD 20 f.18).

### Memory mapping

- **Arrow IPC files.** An uncompressed IPC file's message bodies are already Arrow buffers.
  - **On a host,** Moruna maps uncompressed IPC inputs and spill files read-only, instead of
    reading them into the arena. Their pages are the OS page cache, clean and reclaimable, and are
    counted as mapped bytes. Every message is still checked (CT-I13).
  - **On the kernel,** the image filesystem already maps file ranges read-only (SDD 28, LX-I5).
  - **Compressed IPC** (zstd, lz4) decodes into the arena (item 1).
- **Parquet** is compressed and encoded, so it cannot be usefully mapped. Decoding writes new
  bytes either way, and item 1 makes it write them in the arena.

### What is still copied, by design

- Format changes, which write straight into the arena: decoding, and encoding for sinks.
- On the kernel, buffer tails of at most one page, and small buffers.
- On a host, results from libraries Moruna cannot reach. They are counted and reported.

### How it is proved

- **On a host:** `boundary_copies_total` is zero for a pyarrow-computed column and for a decoded
  Parquet batch.
- **On the kernel:** the `COPIED` counter (SDD 29) grows by no more than the tails when a Python
  task passes a batch.
- **Then the gate.** The NYC jobs, then the full §7 gate, with `cg_peak_bytes` expected to fall.

## 1. What leaves

| What | Where | Goes with it |
|---|---|---|
| peQL and DataFusion | `crates/moruna-datafusion` (all of it, including the UDF bridge); `crates/moruna-runtime/src/kinds/governed.rs`; the `peql` features (`crates/moruna-runtime/Cargo.toml:130`, `crates/moruna-py/Cargo.toml:76`, `pyproject.toml:35`) | Kinds `datafusion` and `peql`, `Embedding::bindings` (`kinds/mod.rs`); `crates/moruna-job/catalogue/peql.json` and its constant (`crates/moruna-job/src/catalogue.rs:4-17`); `crates/moruna-runtime/tests/governed.rs`; the peQL code in `crates/moruna-py/src/bin/kd3-fixture.rs`; the gate steps `test_bridges`, `clippy_peql`, `test_peql`, `clippy_peql_iceberg`, `test_peql_iceberg` and their `lean` calls (`tools/quality/check.sh:315-328` on `main`, `:331-345` on 0.6.6); the pins at `Cargo.toml:88-91` (`datafusion`, `peql`) and parcel's in `crates/moruna-runtime/Cargo.toml` |
| Seams in the core that exist only for peQL | Job document `contracts` (`crates/moruna-job/src/doc.rs:125-131`, `job.rs:75-77`, `schema/job.fbs:150-156`); `Embedding::admit` and `Context::contracts` (`kinds/mod.rs`); `Source::read_report` and `SourceRead` (`crates/moruna-kernel/src/source.rs:91-102`); `SinkWrite` (`crates/moruna-kernel/src/sink.rs:18-45`); the report's engine read and write (`crates/moruna-trace/src/report.rs:589-600`); the engine shares and `EngineMemory` (`crates/moruna-runtime/src/run.rs:1304-1340`, `spec.rs:146-184`) | The job document and the run report each lose fields, so both versions are bumped |
| Polars | `crates/moruna-polars` and its pin (`Cargo.toml:87`); `moruna.polars` (`python/moruna/__init__.py:256-275`); the Polars signature, wrapping and type names (`python/moruna/_declare.py:22-56, 109-198`) | `_check.py:30, 75`, `_job.py:313-314`, `_standin.py:78-97` (the image stand-in); the `polars` extra (`pyproject.toml`); the polars and datafusion entries in `deny.toml` |
| Vortex (D2) | `crates/moruna-sources/src/vortex/`, `crates/moruna-sinks/src/vortex_sink.rs`, `crates/moruna-runtime/src/kinds/vortex.rs`; pin `Cargo.toml:127` | `moruna.VortexSource` and `VortexSink` (`crates/moruna-py/src/sources.rs`, `sinks.rs`, `handles.rs`, `run.rs`; `python/moruna/__init__.py:41-62`); its tests and `deny.toml` entries |
| `MRB1`, Moruna's own tensor file format | `crates/moruna-sources/src/tensor/plan.rs:63, 130`; the `Amb1` writer in `crates/moruna-sinks/src/tensor_sink.rs:35-36` | Tensors stay first-class ("Tensors"). Only the in-house format goes. |
| pyarrow-only duck typing | A function result is accepted through pyarrow's `to_batches` (`crates/moruna-adapters/src/python/cross.rs:65-67`) | Replaced by `__arrow_c_stream__` (§3) |
| NumPy (decided 2026-11-16) | The NEP 49 allocator handler (`crates/moruna-adapters/src/python/hooks.rs`, `python/kernel.rs:171`, the counter at `guard.rs:416`). `numpy` as one of the four allocation sources in the contract (`crates/moruna-kernel/src/kernel.rs:83-101`, `trace.rs:118`), the trace (`crates/moruna-trace/src/alloc.rs:32-83`, `render.rs:239-248`), `moruna check` (`crates/moruna-runtime/src/check/mod.rs:306-312`, `check/warmup/bench.rs:135-140`) and the sandbox's wire counts (`crates/moruna-sandbox/src/protocol.rs:33-59`). The sandbox's preload of `numpy` (`python/moruna/_sandbox.py:31`). | The allocation sources become `python`, `arrow` and `linear`, so the report and the sandbox protocol change, in the same version bump as the peQL rows. NumPy's memory is then counted by the controller's out-of-arena model, as Polars' and Torch's already are (`crates/moruna-controller/src/lib.rs:87-93`). The npy reader stays: it is Moruna's own Rust reading a file format, with no NumPy. |

About 9,500 lines of Rust leave with the peQL, DataFusion, Polars and Vortex crates and modules
(about 3,700 of them Vortex's). On the Python side, Polars handling goes from four modules.

## 2. What stays, and why

| What | Why |
|---|---|
| Arena, scheduler, controller, placement, reactor, storage, record, trace, event, job, plan | This is the engine |
| Native Parquet and Arrow IPC | This is where Moruna beats a plain thread pool, the loop, Ray and Spark (0.6.6 benchmark on the bench VM: Overture shapes 723 s, against 870 s for the loop and 941 s for Ray). The source plans from footers, with decoded bytes per split (`crates/moruna-sources/src/parquet/mod.rs:4-7`), and splits by row group. Byte ranges go through the reactor into arena buffers (`parquet/mod.rs:6-9`), and several reads are in flight at once (`parquet/read.rs:19-25`). The Parquet sink writes several files at once in lanes, with a ledger watermark. Arrow IPC is also the wire format between processes (`crates/moruna-runtime/src/kinds/image.rs:1-6`). |
| The Python call path as 0.6.6 left it | One thread state per worker for its life (`30c359ca`). Small requests skip the guard (`4dd8b1ab`, `2e24261b`). A returned batch copies only the columns the arena does not own (`19aaefc6`). Deferred refcounts on a function's shared objects (`22ef8131`). Two glibc heaps per CPU (`f3351bc4`). Repeat runs start from measured profiles (`5d01c17e`). Every new Python boundary in §3 has to go through this path, not around it. |
| `moruna.std` (`crates/moruna-kernels`) | Moruna's own code, with no third-party crates and 154 KB of machine code. Its stages fuse and never enter the interpreter (`src/lib.rs:8-12`). `join` and `union` are the only stages a Python job can give several inputs (`crates/moruna-kernel/src/kernel.rs:275-318`; `moruna-adapters` overrides none of those hooks). |
| `moruna.Source`, `Split`, `Sink`, `IteratorSource`, `@moruna.function`, `@moruna.kind`, `image` | These are how people bring their own code (§3) |
| Tensor source and sink: safetensors and npy (`crates/moruna-sources/src/tensor/`, `crates/moruna-sinks/src/tensor_sink.rs`) | Data-format exceptions. Neither needs a crate: both readers are Moruna's own code, and the `safetensors` crate moves to dev-dependencies. They read into, and write from, DLPack tensors. |
| Function packages (`.wasm`, `.pex`), images, sandbox, VM, microkernel | Moruna's own isolation, not a library integration |

## 3. Making "write whatever follows the contract" true

**The aim (Brackly, 2026-11-16).** The API must be easy and intuitive to extend, and stay fast.

**One protocol per role.** It is the same whether the data is Arrow or tensors. A user returns
anything that speaks Arrow (`__arrow_c_array__`, `__arrow_c_stream__`) or DLPack (`__dlpack__`),
and Moruna tells them apart. The only Moruna type a user ever builds is `moruna.Split`.

| Role | The least a user writes | Each optional step, and what it buys |
|---|---|---|
| Source | An iterable of batches (`moruna.IteratorSource`) | `plan()` returning `moruna.Split(id, rows)` and `read(split_id, start, end)`: look-ahead, sub-splits, resume. Then `bytes=` on each split: exact morsel sizing. Then `concurrent = True` (item 2): several reads in flight. |
| Function | `@moruna.function` on `batch -> batch` | `accepts="tensor"`: a real tensor in. Schemas declared: `moruna check` can check the job. |
| Sink | `write(batch)` and `finish()` | `checkpoint()` and `restore(state)`: exactly-once resume (item 4) |
| Anything, without Python, or on the microkernel | An `image` reading and writing Arrow IPC | A plan request (item 3): splits and resume |
| Rust embedder | `moruna_kernel::Source`, `Sink` or `Kernel`, registered with `Registry::register_*` (`kinds/mod.rs:504-575`) | Native speed, through the same traits the built-ins use |

**Fast by default.** The user does nothing for any of these:

- the 0.6.6 call path: one thread state per worker, small requests past the guard;
- returned columns the arena owns land without a copy;
- a stream is pulled in Rust;
- zero copy or refused ("Tensors").

**The run report names the next step.** When a user's code leaves speed on the table, the report
says which step would buy it. For example: "source `X` read one split at a time; if `read` is
thread-safe, set `concurrent = True`". It goes out through the build's notes, which already reach
the report (`kinds/mod.rs:268`, `crates/moruna-runtime/src/report.rs:67-68`). A refusal works the
same way: it names the fix.

Each gap below is a 0.7 item, landed before or with the removal it replaces.

1. **Take the whole PyCapsule interface at every Python boundary.**
   - Sources take only `__arrow_c_array__` today. `read` is extracted as `pyo3_arrow::PyRecordBatch`
     (`crates/moruna-sources/src/py_source.rs:169`), and iterator items the same way
     (`py_iter.rs:93`).
   - A function may return `__arrow_c_array__`, pyarrow's `to_batches` or `__dlpack__`
     (`crates/moruna-adapters/src/python/cross.rs:62-73`).
   - An object that offers only `__arrow_c_stream__` is refused everywhere. Several libraries
     export frames and query results that way.
   - pyo3-arrow already has the type that takes both: `AnyRecordBatch`
     (`pyo3-arrow-0.19.0/src/ffi/from_python/input.rs:8-22`).
   - The refusals stop naming `pyarrow.RecordBatch` (`py_source.rs:173`).
   - **Hot-path rule.** A stream is pulled in Rust, through the C stream's `get_next`: one Python
     call to take the stream, not one per batch. A returned batch lands as 0.6.6 lands one. The
     columns the arena owns land without a copy. Buffers another library allocated make the one
     copy that charges them to the arena (`19aaefc6`).
2. **Concurrent user sources.**
   - Every call into a `moruna.Source` is serialised under one mutex, whatever the interpreter
     build (`py_source.rs:53-55`). So a user's source decodes one split at a time, while Parquet
     decodes several.
   - Add an opt-in `concurrent = True`: reads are then issued like Parquet's, several in flight. On
     free-threaded 3.14 that is parallel Python. On the standard build, any library that releases
     the GIL decodes in parallel.
   - A Rust or C++ library with Python bindings then reaches native speed through the capsule. No
     plugin ABI of Moruna's own is needed.
   - **Hot-path rule.** A concurrent read runs on the worker's own thread state (`30c359ca`), never
     on a new one per read.
   - **Two known losses, unchanged by this plan.**
     - Free-threaded GC stops every thread, and hits dict-heavy `to_pylist` code.
     - Decode on one core and compute on another costs cache locality.
     - A concurrent user source behaves like Parquet on both counts, no worse.
3. **Plannable `image` sources.**
   - An image source is one Arrow IPC stream today. It is not repeatable, so it has no split plan,
     no row ranges and no resume (`crates/moruna-sources/src/stream.rs:1-6, 209`).
   - Add a plan request: the process prints its splits, with rows and optional bytes. Then it reads
     one split or row range per invocation, chosen by an environment variable beside
     `MORUNA_ROLE` (`kinds/image.rs:33-39`).
   - An image source then gets what a Python `moruna.Source` gets, from a job document and without
     an interpreter.
4. **A stated rule for user sinks.**
   - A `moruna.Sink`'s `write` returning is the commit, and calls come in order from one writer
     (`crates/moruna-sinks/src/py_sink.rs:14-21`). So it keeps 0.6.6's default `Sink::lagging() ==
     false` (`crates/moruna-kernel/src/sink.rs:146` on 0.6.6). `PySink` does not override it.
   - **The rule:** what `write` accepted is the sink's once it returns. `checkpoint()` runs at every
     manifest write, so a sink that buffers either flushes there or puts the buffer in its
     checkpoint bytes. Otherwise resume is not exactly-once.
   - It goes into `docs/python-sinks.md` and the `moruna.Sink` docstring, with a test of a buffering
     sink across a resume.
   - A user sink that ever gets several writers must report `lagging`, as 0.6.6's lane sink does
     (`1286f2dd`, tests SC-T46 and SI-T34, SDD 08 f.12 and SDD 10 f.12).
5. **User sources and sinks on the microkernel.**
   - Image sources and sinks are refused on the Moruna kernel today; only functions run there
     (`kinds/image.rs:277, 295, 304-307`).
   - After the removals, that would leave the microkernel reading only Parquet and Arrow IPC.
   - Lift the refusal, so user-written sources and sinks run on host, VM and microkernel alike.

**What user code still does not get, compared with native Parquet.** I/O into arena buffers
through the reactor, and footer-exact split sizes: a user gives bytes, or Moruna samples 1,024 rows
(`py_source.rs:34`). The decoded batch is charged to the arena on every route. It is the same one
copy Parquet makes (`crates/moruna-sources/src/parquet/decode_copy.rs:20-22`; `py_source.rs:279`;
`stream.rs:122`). Sinks already work this way: a `moruna.Sink` gets zero-copy batches and
exactly-once resume (`crates/moruna-sinks/src/py_sink.rs:1-21`).

The job document's `kind` field needs nothing new. User code in a document is an `image`, or a
served function (`@moruna.kind`, `--kinds FILE`: `crates/moruna-runtime/src/host/cli.rs:73`). Rust
embedders register their own kinds (`kinds/mod.rs:504-575`). A removed kind is refused with the
list of kinds the build has (`kinds/mod.rs:693-709`). Add one line that names the replacement: "use
a `moruna.Source` or an `image`".

### Logs ride the job updates (Brackly, 2026-11-16)

**Today.** Everything a task prints is captured, an image task's output included, and kept as
`log` records in the run's output store (`crates/moruna-runtime/src/host/logs.rs:1-30`). The job
updates Moruna posts to its host carry only how far each producer's records reach, never the lines
themselves (`crates/moruna-runtime/src/host/protocol.rs:21-23`). No user-facing API reads them:
the run report does not expose them, there is no `moruna logs`, and `moruna.read_events(path)`
needs a segment file the user has to find.

**0.7.** Log records go out on the same update stream as the run's other events.

- Each record carries its producer (task or engine), its level and its line.
- They stay rate-limited per producer, as the capture already is (`RATE_RECORDS`, `RATE_BYTES`).
- A host such as Griot forwards them with the other updates (to Kora, on Griot's side). Moruna
  carries no Kora code.
- In Python, the run report gains `logs(task=None)`, which reads the run's records, and
  `moruna.run(..., on_log=...)`, which gets each record as it arrives.

### Containers: `moruna.image` is the interface

`moruna.image(reference, role=..., entry=..., params=..., needs=..., takes=..., gives=...)` runs
any OCI image, by digest, as a source, a function or a sink (`python/moruna/_job.py:1923-1939`).

- **What a task can set:** `command` and `env` (`crates/moruna-runtime/src/kinds/image.rs:73-94`).
- **How it runs:** confined, with no network (`docs/images.md`). Its batches cross as Arrow IPC.
- **So any function in any language that reads and writes Arrow runs today.**
- **The gaps:** image sources plan no splits (item 3), and image sources and sinks are refused on
  the microkernel (item 5). Both are already in this plan.

## 4. What a 0.6.x user changes

| 0.6.x | In 0.7 |
|---|---|
| `"kind": "datafusion"`, `"kind": "peql"`, a document with `contracts` | Refused. Read and write through peQL in your own code: a `moruna.Source` or `moruna.Sink` that calls peQL, or an `image`. Such an adapter belongs to peQL, not Moruna. |
| `PlanSource`, `PeqlSink` (Rust, `crates/moruna-datafusion/src/lib.rs:32-41`) | Gone. Implement `moruna_kernel::Source` or `Sink` in your own crate and call `Registry::register_source` or `register_sink`. No Python class ever existed. |
| `moruna.polars`, a function annotated `pl.DataFrame -> pl.DataFrame`, Polars type names in declarations | Call `pl.from_arrow(batch)` inside the function and return the frame. It is taken through `__arrow_c_stream__` (§3, item 1). Declarations use Arrow type names. |
| `"kind": "vortex"`, `moruna.VortexSource`, `VortexSink` | A `moruna.Source` or `Sink` over Vortex's Python package that returns Arrow. Or convert the data to Parquet. |
| `"kind": "tensor"`, `moruna.TensorSource`, `TensorSink`, `accepts="tensor"` | Unchanged, except that `MRB1` files are refused: convert them to safetensors. New in 0.7: an `accepts="tensor"` function fed Arrow gets a real tensor, and Python sources and sinks can take and give DLPack. |
| A function that relied on Moruna counting NumPy's allocations | Nothing to change. That memory is counted by the controller's out-of-arena model. |
| `moruna.std`, Parquet, Arrow IPC, `image`, packages | Unchanged |
| The `peql` wheel feature and the `polars` extra (`pyproject.toml`) | Gone. One wheel, one build. |

**Tests that change:**

- **KD3 matrix.** These cases go: `source.datafusion`, `sink.peql`, and `source.vortex` and
  `sink.vortex` (`crates/moruna-py/tests/fixtures/kd3-matrix.json`, checked by
  `tools/kd3-matrix.py:17-29` and node acceptance, `tools/proof-lab/node-acceptance.py:406`). The
  tensor cases stay, without `MRB1`, plus cases for Arrow to tensor, tensor to Arrow, and a Python source and sink over DLPack.
- **New KD3 cases:**
  - a user source returning a stream-only object;
  - a concurrent user source;
  - a plannable image source;
  - an image source and sink on the microkernel;
  - a buffering user sink across a resume.

**The Moruna kernel.**

- **Its command line.** `moruna-engine` (`crates/moruna-runtime/src/bin/moruna-engine.rs`) is built
  with no features (`kernel/tools/build.sh:154`), so it never had the peQL kinds.
- **KD3 runs there too.** KD3's `native` adapter is the Moruna kernel
  (`crates/moruna-runtime/src/bootstrap.rs:275-277`, `crates/moruna-py/src/bin/kd3-fixture.rs:316-321`).
  The fixture binary is built with `peql`, so the removed cases leave both adapters.
- **Kernel tooling.** Its own boot fixtures in `kernel/` and `tools/proof-lab/` name none of the
  removed kinds.
- **What changes for it.** It loses the Vortex built-ins and gains user sources and sinks (§3,
  item 5).
- **The guest image** installs the release wheels (`guest/build.sh:21-23`), so it changes as the
  host does.

## 5. What it buys

Measured with `cargo tree --offline --locked -e normal,build`: distinct third-party packages, plus
local crates.

| Build | 0.6.5 | 0.7 | Platforms |
|---|---|---|---|
| Wheel | 556 (+4 git pins) | **330** (0 git pins) | linux x86_64, the one platform released |
| Microkernel engine (musl) | 439 | **305** | linux only |

0.7 keeps `dlpark` and moves `safetensors` to dev-dependencies ("Tensors"). Dropping NumPy removes no
crate: the hook is Moruna's own code.

**Machine code.** Measured in the 0.6.5 manylinux aarch64 cp314 wheel the quality gate built on
2026-11-12 (`target/quality/sandbox-wheel/`, `tools/quality/check.sh:153`). That wheel was built
with `peql`.

- The wheel is 83.7 MB, and its `_core` 271.8 MB.
- By `llvm-nm` symbol sizes, with each generic instance counted to the first crate it names:
  - peQL, DataFusion, parcel and sqlparser: 69.6 MB (47.6%);
  - Vortex: 25.2 MB (17.2%);
  - arrow, parquet and object_store: 28.3 MB;
  - wasmtime: 7.9 MB;
  - Moruna itself: 6.9 MB.
- Tensor support (DLPack, safetensors, npy, and Moruna's tensor modules) is 116 KB.
- The lean core's code is about 51 MB of today's 146 MB. These are estimates: no release build was run, because the Mac has 27 GiB free.

**CI.** These figures come from the 0.6.6 session; I did not time them here.

- The Rust gate spends about 35 of its roughly 100 minutes, and most of the shared agent's disk,
  on the Polars, DataFusion, peQL and peQL-Iceberg steps.
- Those steps leave with the crates (§1). The gate's time and disk should fall in proportion.
  Measure that on the first lean run.

**Found in passing.** The release builds do not strip symbols (on `main`:
`.github/workflows/ci.yml:720, 840`; 0.6.6 moves CI to `.azure-pipelines/`; no `strip` in
`Cargo.toml`). `.strtab` and `.symtab` are 81.5 MB of the 271.8 MB `.so`.

## 6. Order of work

Each step ships with its tests and meets the coverage gate (`tools/quality/coverage_gate.py`). Steps
1 and 2 come first, so every removal lands with its replacement already shipped.

Steps 1 to 5, 7 and 8 touch the run path or the `@moruna.function` path. Each of them runs the four
NYC jobs against 0.6.6 by the §7 method before it merges; each job takes 7 to 25 s. The full gate
runs once, at step 11.

**Before step 1.** Branch from `main` only after 0.6.6 (`fix/adaptive-runtime`, Azure PR 2) has
merged. It touches files this plan also changes:

- `crates/moruna-scheduler` (`worker.rs`, `2073f71c`; `checkpoint.rs`, `shared.rs`);
- the sink trait and the moruna-sinks checkpoint ledger;
- `crates/moruna-runtime/tests/elastic.rs` and `crates/moruna-vmm/src/microkernel.rs`;
- CI, now `.azure-pipelines/ci.yml` and its templates;
- `tools/quality/check.sh`, which gains `lean()` (`5383f55c`).

The griot-ci pool runs one CI run at a time and is shared with Griot. Nothing is pushed or queued
until the 0.6.6 session says the release is out.

1. **The whole PyCapsule interface at every Python boundary** (§3, item 1). Tests: a source, an
   iterator and a function each return an object that offers only `__arrow_c_stream__`, and one
   that offers only `__arrow_c_array__`. The refusal names the interface.
2. **Concurrent user sources and the user-sink rule** (§3, items 2 and 4).
   - Tests: the split, resume and malformed-input tests of `crates/moruna-sources/tests/py_source.rs`,
     run with `concurrent = True`.
   - A test that several reads are in flight, on the worker's thread state.
   - A buffering `moruna.Sink` resumed exactly once.
   - The report's next-step note for a serial source, and its absence once `concurrent = True`.
3. **Remove peQL and DataFusion with their core seams.** Bump the job document and report
   versions. Tests: refusals of the removed kinds and of `contracts`, the schema tests, and KD3
   without the two cases.
4. **Remove Polars:** the crate, the Python special-casing and the extra. Tests: the Polars user
   path of §4, written as a plain function, in `python/tests`.
5. **Vortex and tensors.**
   - **Remove Vortex (D2).** Tests: a user `Source` over Vortex's Python package in
     `python/tests`, which runs when the package is installed and skips otherwise, so the gate never
     needs it.
   - **Tensors** (the work list in "Tensors"): fix the multi-column strides bug, wire Arrow to
     tensor, give Python sources, iterators and sinks DLPack, take tensor to Arrow past rank 2 and
     wire it, delete `MRB1`, add npy writing, and move `safetensors` to dev-dependencies.
     - Tests: the `[rows, columns]` view's values, not just its shape.
     - A Parquet source feeding an `accepts="tensor"` function, which gets `numpy.from_dlpack` of
       its columns with the data pointer unchanged.
     - A rank-3 tensor written to Parquet and read back as the same tensor.
     - A Python `Source` and `Sink` over DLPack.
6. **Plannable image sources, and image sources and sinks on the microkernel** (§3, items 3 and 5).
   Tests: the new KD3 cases on `linux`, `native` and the microkernel.
7. **Remove NumPy (§1).**
   - The NEP 49 handler, the `numpy` allocation source in the contract, trace, report, `moruna check`
     and the sandbox protocol, and the sandbox preload.
   - It shares the report version bump with step 3.
   - Tests: the guard and trace tests with three allocation sources, and the sandbox protocol round
     trip.
8. **No copies ("Memory").**
   - **8a, host.** The arena's C interface; the size-routed global allocator; the pyarrow pool,
     built per supported release; mapped IPC reads.
   - **8b, kernel.**
     - The ABI amendments: copy-on-write memory objects from anonymous ranges, `batch_put_runs`,
       and runs for `slot_push`.
     - The Python shim on the three calls.
     - The engine pushing arena runs.
   - Tests as in "Memory: How it is proved". The NYC check after 8a and after 8b.
9. **Logs on the job updates.** Records on the update stream, and `logs()` and `on_log` in
   Python. Tests: a task's lines reach the peer in order with their producer and level, the rate
   limit holds, and `report.logs(task=...)` returns that task's lines.
10. **Docs:** `docs/sources-and-sinks.md`, `docs/python-sources.md`, `docs/python-sinks.md`,
   `docs/job-document.md`, `docs/python-functions.md`, the README and the CHANGELOG, with §4 as
   the migration.
   - `docs/python-sources.md` replaces its `moruna.VortexSource` section with a worked
     `moruna.Source` over Vortex's own Python package: one split per file with its row count, and
     `read` returning that row range as Arrow.
   - The same example is the step 5 test, so reading Vortex stays proven after the built-in goes.
11. **The no-regression gate (§7), then the version bump.** A benchmark that does not hold blocks
   the release.

## 7. The no-regression gate

0.7.0 is not versioned until every benchmark 0.6.6 ran holds against 0.6.6, on one VM.

**Baseline.** The table is in `CHANGELOG.md` on `fix/adaptive-runtime`, lines 10-18. It was
measured on one fixed node with 8 CPUs and 24 GiB.

| Benchmark | 0.6.6 | Loop | Ray Data |
| --- | --- | --- | --- |
| NYC Score (NumPy) | 25.2 s | 27.0 s | |
| NYC Occupancy (pyarrow) | 7.6 s | 8.0 s | |
| NYC Enrich (vectorised, filter) | 7.0 s | 7.5 s | |
| NYC Audit (pure Python) | 14.8 s (14.2 to 14.8) | 14.5 s | |
| Overture shapes | 723 s | 870 s | 941 s |
| Overture attributes | 1,118 s | 1,142 s | 1,111 s |
| FineWeb MinHash | 2,144 s | 2,184 s | |

**Method.**

- **Two machine types, each with both builds side by side.** The D8ds_v6 the 0.6.6 numbers came
  from, and the D4ls_v5 that staging and production run on (4 vCPU, 8 GiB). On the D4ls_v5 there
  is no published baseline, so 0.6.6 is measured there in the same session. Within a type, VMs
  differ by about 15%. A number from another
  VM or another day proves nothing.
- **Alternate the order.** Whichever build reads a set of files first looks slower, because of the
  page cache.
  - NYC runs in pairs, 0.6.6 then 0.7, then 0.7 then 0.6.6.
  - The large jobs change which build goes first from one benchmark to the next.
- **Measure as 0.6.6 measured.**
  - The NYC figures are repeat runs that start from what the first run learned. So each build gets
    one learning run, then five measured runs per NYC job.
  - The large jobs run once per build. That is about 66 minutes per build, so the run has to finish
    before the VM's 02:00 UTC shutdown wipes `/data`.
- **Measure the noise in the same session.** Run two NYC pairs of 0.6.6 against itself. Audit's
  published range, 14.2 to 14.8 s, shows that noise is about 4% on Audit.
- **Pass.** On every benchmark, 0.7's median `wall_s` and median `cg_peak_bytes` are no worse than
  0.6.6's beyond that same-session noise, and there are no OOM kills (`bench/vm/driver.py:112-120`).
  Anything outside the band is a regression and blocks the release.
- **NumPy jobs.** Score, Occupancy and MinHash do their work in NumPy
  (`bench/vm/workloads.py:39-76`, `bench/vm/ooc_workloads.py:7-14`), and on 0.7 they run without
  the NumPy hook. Their peak memory and wall time show whether the controller's out-of-arena model
  holds them. If it does not, the fix goes into that model, which serves every library, never back
  into a NumPy hook.

**Where.** The D8ds_v6 is `moruna-bench`, in resource group `moruna-bench-20261114g` on the
griot-stg subscription. It is deallocated, and kept for this run by the 0.6.6 session. A
D4ls_v5 is made beside it for the run, from the same setup script.

- **On the D4ls_v5,** the harness's defaults of 8 CPUs and 24 GiB (`bench/vm/make_plan.py:11`)
  scale to the machine. It has no local temporary disk, so `/data` is a managed data disk, and
  its I/O figures are not comparable with the D8ds_v6's NVMe; only 0.6.6 against 0.7 on that
  machine is.

- `/home/bench/setup8.sh` on its OS disk recreates `/data`, downloads the datasets and mirrors the
  results.
- The harness is `bench/vm/`, deployed to `/home/bench/bench` (`bench/vm/README.md`).

**Watch these when implementing.** Each touches the hot path the benchmarks run.

- **Function results.** `__arrow_c_array__` must stay the first check on a function's result
  (`crates/moruna-adapters/src/python/cross.rs:62`), so a pyarrow result takes the same path as
  today.
- **Budget split.** Removing `EngineMemory` and the engine shares must leave the budget split
  unchanged for a job with no engine source (`crates/moruna-runtime/src/run.rs:1304-1340`).
- **The NumPy hook's removal (step 7).** It moves the accounting of the three NumPy jobs to the
  generic model.
- **Arena allocation (step 8).** It changes how every batch's memory is allocated. Its per-allocation
  cost (AD-O4) and peak memory are the things to watch.
- **Tensors (step 5).** The conversions run only for stages that ask for tensors or receive them,
  so the table path the benchmarks run is untouched. The NYC check after step 5 confirms it.
