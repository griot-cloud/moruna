# Amoru: Architecture Design

**Document type:** full architecture design (not an ADR)
**Status:** DRAFT for review · revision 3 · 2026-09-16 (revision 2: 2026-09-15; revision 1: 2026-09-12)
**Revision 2 adds:** tensor payloads alongside Arrow (DLPack), tensor sources and sinks, a pinned memory arena, tiered morsel placement across device memory, host memory and disk with hardware-direct movement between tiers, and a deferred design for weight-major execution when a model does not fit its accelerator.
**Revision 3 adds:** the honest limit of the memory guarantee for kernel-internal allocations (section 8) and the seams that narrow it (state footprint in the trace and the budget, a safety margin derived from evidence across runs, allocator interposition as a later design); compressed staging and promotion as a scan as a post-v1 direction with `StagingCodec` reserved (5.6); an engine baseline in the benchmarks; recovery by lineage (a run manifest written by the placement engine, resume on the same or another node from what is already on disk plus source re-reads; section 10, S17, D13) and the reservations that let the same framework extend to many nodes without changing a signature (a remote memory tier, node identity, locality-aware admission, a shuffle-free boundary; section 11, S16, D12).
**Scope:** a single-process runtime that runs a full pass over a dataset, tabular or tensor, larger than the process's memory budget, applying a transformation a query engine cannot express, at close to the budget's capacity, with no tuning by the user. Explicitly OUT: distributed execution, query planning or optimisation, transformations that need random access across the whole dataset (shuffles, exact pairwise, hierarchical clustering), streaming with per-record latency requirements, and the packing or reclaim policies of any host platform that runs this runtime inside a pod, VM or notebook. Griot Cloud's compute plane is a consumer of this runtime, not part of this document.
**Companion:** none yet. A Griot Cloud compute plane design (pod sizing, packing, idle reclaim) will consume this document.

The runtime is named Amoru (Adaptive MOrsel RUntime). "Morsel" is the unit of work throughout this document, not the product.

---

## 1. Problem statement

**P1. The full-pass, opaque-transform, out-of-core workload has no runtime.** An engineer with a Parquet dataset larger than memory and a Python function that a query engine cannot express (a model, a tokenizer, a domain rule set, anything that decides during the computation) has two choices today: hand-write a loop over row groups with guessed batch sizes and a guessed worker count, or adopt a cluster framework to run on one machine. Neither is a runtime for the job. The engineer pays in tuning time and in runs that fail after hours, and the organisation pays for the over-provisioned machine that made the job survive.

**P2. Batch size and worker count are chosen by guess, and the guess is made once.** The memory a batch costs is not its size on disk: Parquet expands several times when decoded, and the transformation's intermediates multiply it again by a factor nobody knows before the function runs. So the batch size is set to what worked last time, on different data, with a different function. Too large and the process is killed; too small and the machine runs at a fraction of what was paid for. The job pays with its life, or the bill pays with idle capacity, and the choice of which is made blind.

**P3. Engines host user functions, but at the wrong granularity or with fixed sizing.** DuckDB, Polars and Spark all accept custom functions. Row-at-a-time hosting is two orders of magnitude slower than native and is not an option at scale. Vectorised hosting is efficient at the batch boundary, but the batch size is the engine's, fixed by its own operators' known footprints (2048-row vectors, thread-derived morsels), and the engine has no model of a black-box function's amplification. A function that expands its input twentyfold gets the same batch as one that does not. Throughput and memory safety pay.

**P4. The one runtime that does adapt carries a cluster with it.** Ray Data's streaming executor has the right scheduling policy and a dynamic memory budget, and it brings a distributed object store, a scheduler process, serialisation between every task, and Python in the control loop. Inside a process with a fixed budget, that overhead is paid out of the budget. The user pays in memory that never reaches their function and in operational surface that has nothing to do with their job.

**P5. Memory-bounded hosts kill without diagnosis.** In a container, a pod, or a small VM, exceeding the memory ceiling is not a warning; it is SIGKILL from the OOM killer, with no record of which batch, what footprint, or how far the job had progressed. The operator's only recourse is to rerun with a bigger allocation, which is the cost the bounded host existed to avoid.

**P6. There is no trace of resource behaviour, so efficiency cannot be verified.** Nobody can say what fraction of the allocated memory and CPU a job actually used, per batch, over time. A platform that sells resource-hours cannot show that the hours were full; a user cannot see why a job was slow; and a controller cannot be improved without the data it would learn from.

There is no inherited half-solution to retire. This runtime is greenfield. Where it lands, it replaces hand-written batch loops in notebooks and scripts, and it must never coexist with a second sizing mechanism in the same process.

---

## 2. Context

### 2.1 The shape that constrains the design

**One process, one budget, no scale-out.** The runtime lives inside a single OS process whose memory and CPU are fixed for the run: by a cgroup when the host is a pod or container, by the machine when it is a laptop or a single-node cluster, or by an explicit number when the caller says so. Consequence: there is no escape valve. Every mechanism in this design must degrade inside the budget, and the only parameter a user may need to supply is the budget itself when it cannot be discovered.

**Arrow is the interchange, and everything relevant speaks it.** arrow-rs, pyarrow, Polars, DuckDB and DataFusion all consume and produce Arrow record batches, and the Arrow C Data Interface moves a batch between Rust and Python without copying. Consequence: the transformation's contract is Arrow in, Arrow out. That single choice is what lets a kernel written for this runtime also be a Polars plugin or a DataFusion function, and what keeps Python kernels zero-copy.

**Tensors have their own agreed layout, and it is compatible with Arrow's.** A tensor is a contiguous, homogeneous, row-major block of numbers with a shape; DLPack is the C-level convention (a header, `dlpack.h`) by which PyTorch, JAX, NumPy and CuPy hand such a block to one another by pointer, on host or device memory, without copying. Arrow's numeric columns without nulls are already such blocks, and Arrow defines a FixedShapeTensor extension type and a C Device Interface for batches that live on a GPU. Consequence: the runtime's payload is either an Arrow batch or a DLPack tensor, tagged with where it lives; a numeric column becomes a tensor by pointer and a prediction tensor becomes a column the same way; and a kernel that wants a GPU-resident Arrow batch (cuDF, for example) receives one through the C Device Interface. Strings, nested types and nulls do not cross into tensors; the runtime says so at plan time rather than copying.

**Memory is tiered, and the tiers move data by DMA, not by the CPU.** A morsel can live in device memory, in pinned (page-locked) host memory, in ordinary host memory, or on local NVMe. Movement between these tiers is done by DMA engines (the GPU's copy engine for pinned host to device, the NVMe controller for disk to host with O_DIRECT, and GPUDirect Storage for disk straight to device where the hardware and driver allow) provided the destination is pinned or registered and the bytes are laid out on disk exactly as they will be in memory. Consequence: files whose layout equals their memory layout (Arrow IPC, safetensors, GGUF, raw aligned binary) can be moved into their final tier with no CPU involvement, and the runtime should write its own spill and staging files that way; Parquet cannot, and is decoded once at the source boundary into the arena, after which it never passes through the CPU again for movement.

**Python is the user's language, and its parallelism depends on the build.** CPython 3.14 ships an officially supported free-threaded build; NumPy, SciPy, PyArrow, pandas, scikit-learn and Polars publish wheels for it; PyO3 supports it from 0.23 and assumes modules are thread-safe from 0.28. Under a GIL build, a Python function runs on one thread at a time no matter how many workers exist; a single imported extension that has not declared itself thread-safe silently re-enables the GIL for the whole process. Consequence: the runtime's own parallelism must not depend on Python at all (it is Rust), Python kernels are one kind of kernel among several, and the runtime must detect and report the GIL state rather than silently serialise.

**The source is columnar, and its footer knows the future.** A Parquet file's footer carries, per row group and per column, the row count, the uncompressed byte size, null counts and min/max values, readable before any data bytes are fetched. Consequence: the size of what is about to be read is not a forecast, it is a lookup. The runtime's sizing problem reduces to the one thing the footer cannot tell it: how much the transformation amplifies its input.

**Storage is local disk or object storage, and object storage rewards concurrency.** Read throughput from an object store is roughly requests in flight times request size divided by latency. Consequence: read-ahead depth is a control knob, and it costs memory, so it belongs to the same controller as batch size.

**Transformations may hold state, including on a GPU.** A model loaded once and applied to every batch is the canonical opaque transformation, and its batch is bounded by device memory as well as host memory. Consequence: kernels declare whether they are stateless or stateful, stateful kernels are pinned to the worker that holds their state, and device memory is a second budget.

**The host may be a cgroup v2 container with hard limits.** memory.max is enforced by the OOM killer; memory.high is a throttle the kernel applies before that; cpu.max is a quota enforced in 100 millisecond periods with throttling reported in cpu.stat. Naive code inside a container sees the host's core count, not the quota. Consequence: budget and worker count are read from the cgroup when one is present, the runtime holds itself below memory.high, and throttling is a controller input.

### 2.2 Verified current state

The executing agent re-verifies every row before acting on it. Rows marked *unverified* must be settled with the given command before any code that depends on them is written.

| Fact | Where verified |
|---|---|
| Polars `map_elements` on a trivial per-element function: 299 ms vs 2.07 ms for the native expression (about 145x); batch-level UDF in an aggregation: 47 ms vs 33 ms | nikoondata.substack.com, "Polars UDF Guide", fetched 2026-09-11 |
| DuckDB: row-at-a-time Python UDF 5.37 s vs PyArrow vectorised UDF 0.35 s on 10 M integers; vectorised UDF matches an external Python function on time at about one fifth the memory | duckdb.org/2023/07/07/python-udf, fetched 2026-09-11 |
| Spark Arrow UDFs about 10% faster and about 40% less memory than pandas UDFs; pandas conversion copies when nulls are present | databricks.com blog "Introducing Arrow UDFs in PySpark", fetched 2026-09-11 |
| UDFBench (VLDB 2025): Python UDFs 2 to 5x slower than C across engines; thread parallelism does not scale under the GIL; tuple-at-a-time engines move over 20x the data of vectorised ones | vldb.org/pvldb/vol18/p2804-foufoulas.pdf, fetched 2026-09-11 |
| Ray Data streaming executor selects, among runnable operators, the one with the least data in its output buffer; global memory budget recomputed every second from task durations and in/out size ratios; reported 1.3x optimal under memory pressure vs 4.34x for Spark | arxiv.org/abs/2501.12407, fetched 2026-09-11 |
| Polars streaming engine is morsel-driven, spawns async tasks per compute node, falls back to in-memory materialisation for unsupported operators; sizing is not memory-adaptive | deepwiki.com/pola-rs/polars streaming engine page and pola.rs Dec 2025 post, fetched 2026-09-11 |
| SEDA (Welsh 2001): stages with bounded queues, per-stage thread pool controller keyed on queue length, batching controller keyed on throughput | mwhittaker.github.io mirror of the SOSP 2001 paper, fetched 2026-09-11 |
| Morsel-driven parallelism (Leis et al. 2014): workers not bound to operators, pull fixed-size morsels from any runnable pipeline | Leis et al., SIGMOD 2014, via faculty.cc.gatech.edu mirror, fetched 2026-09-11 |
| Free-threaded CPython: 3.14 single-thread performance near the GIL build; NumPy, SciPy, PyArrow, pandas, scikit-learn, Cython 3.1 support it; cffi, cryptography, grpcio among the laggards as of May 2025 | labs.quansight.org "first year of free-threaded Python", fetched 2026-09-11; py-free-threading.github.io/tracking for current status |
| PyO3: free-threaded support from 0.23; from 0.28 modules default to `gil_used = false`; `Python<'py>` no longer implies the GIL; abi3 wheels cannot target free-threaded builds | pyo3.rs/main/free-threading, fetched 2026-09-11 |
| cgroup v2 exposes `memory.max`, `memory.high`, `memory.current`, `memory.stat` (anon vs file split), `cpu.max`, `cpu.stat` (`nr_throttled`, `throttled_usec`) | Linux kernel admin-guide/cgroup-v2; **unverified on target hosts**: `ls /sys/fs/cgroup/` inside a target pod |
| `memory.peak` exists in cgroup v2 from Linux 5.19 | **unverified**: `uname -r && cat /sys/fs/cgroup/memory.peak` on each target host; if absent, the runtime tracks its own peak from `memory.current` samples |
| Rust `std::thread::available_parallelism` honours cgroup v2 CPU quota | **unverified**: run a container with `--cpus=2` on a 16-core host and print the value; if it reports 16, read `cpu.max` directly (the runtime reads `cpu.max` regardless; this only affects the fallback) |
| Parquet crate supports row selection within a row group (needed to sub-split oversized row groups) | **unverified**: check `parquet::arrow::arrow_reader::RowSelection` in the pinned crate version |
| DLPack is a single C header (`dlpack.h`) defining `DLManagedTensor`; Rust wraps it rather than reimplementing it (the `dlpark` crate); PyTorch, JAX, NumPy and CuPy all export and import it | dmlc/dlpack repository README and `dlpark` crate docs; **verify the pinned `dlpark` version supports DLPack v1.0 versioned structs** |
| MinIO AIStor (commercial) supports S3 over RDMA for GetObject and PutObject into client-registered buffers, including CUDA device pointers; community MinIO does not | docs.min.io/aistor/developers/s3-over-rdma, fetched 2026-09-15 |
| Arrow Flight's UCX (RDMA) transport exists as experimental C++ code and has not reached a supported state | ARROW-15706 and apache/arrow issue 40072, fetched 2026-09-15 |
| safetensors file layout: 8-byte header length, JSON header, then raw tensor bytes; tensor data offsets are **not** guaranteed 64-byte aligned | **unverified**: read the format section of huggingface/safetensors README and inspect one file with `xxd`; if unaligned, TensorSource copies into the aligned arena on load |
| GPUDirect Storage (cuFile) requires a supported NVMe path, a supported filesystem with O_DIRECT, and the nvidia-fs kernel module | **unverified on target hardware**: `gdscheck.py -p` on a GPU host; absent, disk to device goes through the pinned host arena |
| io_uring is usable inside the target container runtime (default seccomp profiles in some Kubernetes distributions block it) | **unverified**: run a one-line io_uring probe inside a pod on the target Talos node; if blocked, the local direct-IO path uses `pread` with O_DIRECT on a thread pool |
| FlexGen (Sheng et al., ICML 2023) demonstrates weight-major, tiered (GPU, CPU, disk) offloading for high-throughput batch LLM inference on a single small GPU | arXiv 2303.06865; **verify numbers before citing them in any external material** |

### 2.3 Build versus adopt

The runtime adopts every layer beneath the scheduler and the controller and builds only those two, plus the traits that bind them.

**Adopted.** arrow-rs for the in-memory format and the C Data Interface; the parquet crate for reading with footer metadata, projection and row selection, and for writing with row-group size targets; object_store for S3-compatible, GCS, Azure and local file access with built-in request concurrency and retry; tokio as the IO reactor for prefetch, sink writes and spill; crossbeam for bounded channels and the work queues; PyO3 with maturin for the Python surface; pyo3-arrow or arrow's own FFI for zero-copy batch exchange; `dlpark` for DLPack; the `safetensors` crate for model and tensor files; `cudarc` (or `cust`) for CUDA device memory, pinned allocation and copy engines behind the `cuda` feature; `io-uring` for local direct IO. All are Apache- or community-maintained with monthly or faster release cadence; arrow-rs and parquet are the foundation of DataFusion and Polars' Parquet path, which is the strongest available guarantee they are not going anywhere. The fork-if-needed position: none of these is a candidate; the runtime's dependency on each is through a narrow surface (RecordBatch, a reader builder, a store trait, a channel) and any could be replaced behind that surface.

**Not adopted, and why.** Ray Data has the right scheduling policy and is a cluster runtime; its overhead is P4. Dask's scheduler is Python and its threaded executor is GIL-bound. Polars' streaming engine cannot size for an opaque function's amplification (P3); this runtime hosts Polars as a kernel instead, which makes the eager-versus-streaming distinction disappear inside the scheduler. DuckDB is C++ with fixed vectors and would require the runtime to live inside its extension model. Rayon is a fork-join work-stealing pool; the scheduler here needs stage-priority admission with per-morsel accounting, which is simpler to write over a plain thread pool than to bend rayon into. Each of these has the real advantage of existing and being tested; the runtime borrows their ideas (Ray's admission rule, morsel-driven role-free workers, DuckDB's tail-first eviction) and not their code.

---

## 3. Sufficiency criteria

Each criterion is testable and each implementation gate in section 9 cites the criteria it closes. The benchmark suite referenced below is defined in section 9.0.

**S1. Budget is never exceeded.** For every run in the benchmark suite with a budget B and a dataset larger than B, the process's peak anonymous memory (cgroup `memory.stat` anon plus unevictable, or `ru_maxrss` outside a cgroup) is at or below B. Measured, not estimated.

**S2. The only parameter is the budget, and only when it cannot be discovered.** Every benchmark run completes with `run(source, kernels, sink)` and no batch size, worker count, read-ahead depth or spill threshold supplied. Inside a cgroup, the budget is not supplied either.

**S3. Throughput is within 80% of hand-tuned.** For each benchmark kernel, a static grid search over batch size and worker count establishes the best fixed configuration on the same machine; the runtime with defaults reaches at least 80% of that throughput on the same machine. The report also carries, beside that figure, the same kernel run as a user-defined function inside Polars and DuckDB (the engine baseline, preamble 6.5); it is reported, not gated.

**S4. Capacity is used.** For the compute-bound benchmark kernels, worker busy time is at least 85% of the CPU quota over the run. For the IO-bound kernel (identity), source bandwidth is at least 80% of the measured ceiling of the storage device or network path.

**S5. Budget and parallelism are discovered from the host.** Inside a cgroup v2 container with `memory.max` and `cpu.max` set, the run report shows the budget derived from `memory.high` or 90% of `memory.max`, and the worker ceiling derived from `cpu.max`, with no flags passed.

**S6. Breaches degrade, never kill.** Against the adversarial benchmark kernel whose amplification rises fourfold at the midpoint of the dataset, the run either completes within B or terminates with a diagnostic that names the morsel sequence number, its input size, its measured footprint and the budget. It is never terminated by signal.

**S7. A kernel is portable without change.** A kernel crate written against `amoru-kernel` alone compiles into the runtime, into a Polars expression plugin, and into a DataFusion scalar function through three thin wrapper crates that contain no kernel logic.

**S8. Python kernels are first-class.** Under free-threaded CPython, a GIL-releasing NumPy kernel supplied as a Python callable reaches at least 0.7 times the worker count in speedup over one worker. Under a GIL build, the same run completes correctly and the run report states that Python kernels were GIL-serialised.

**S9. Every morsel leaves a trace.** Each morsel produces a trace record with the schema in 5.9, and the run report's S1 to S4 numbers are computed from that trace alone.

**S10. Spill is bounded.** With a sink throttled to one tenth of kernel throughput, memory stays within B, spill volume stays within the configured disk bound, and reaching the disk bound terminates the run with a diagnostic rather than filling the disk.

**S11. The controller is stable.** On the stationary synthetic dataset, the sign of the morsel-size adjustment changes at most 5 times per 100 morsels after the first 20.

**S12. Overhead is small.** With the identity kernel and 256 MB morsels, wall time is within 5% of a plain Rust loop that reads the same Parquet and writes it back with the same writer settings.

**S13. Tensors cross without copying.** A numeric, null-free Arrow column handed to a Python kernel as a tensor, and a tensor returned as a column, produce zero host-memory allocations of the payload size (measured by the arena's allocation counter); a safetensors source split is delivered to a kernel as a tensor whose data pointer lies inside the memory map or the arena, never in a fresh allocation.

**S14. Movement between tiers does not pass through the CPU.** On a GPU host, a morsel staged in the pinned arena reaches device memory by an asynchronous copy-engine transfer with worker CPU time for the transfer below 1% of its wall time; on a host with GPUDirect Storage, an Arrow IPC spill segment reloads from NVMe into device memory with no host-memory allocation of the payload size; on a host with neither, the same run completes through the fallback path and the report names which path was taken.

**S15. Spill is a tier, not a cliff.** With the throttled sink of S10, throughput after spill engages is at least 70% of throughput before, on NVMe, because spill segments are written and read with direct IO at sequential bandwidth and the queue's head is never on disk.

**S16. The multi-node extension changes no signature.** The contracts crate at v1 contains every type the extension in section 11 needs (`NodeId`, `Tier::Remote`, `Locality`, the manifest), a repository lint proves no `match` on a tier uses a wildcard arm, and the extension's design document, when written, lists its changes to `01-contracts.md` as additions only. Measured by the lint and by a review of that document against this criterion.

**S17. A killed run resumes, and its output is indistinguishable.** For every benchmark with a resumable sink, killing the process with SIGKILL at a random point and resuming from the manifest produces output byte-equal (ordered sink) or row-set-equal (unordered) to an uninterrupted run, re-reads from the source only the morsels the manifest lists for recomputation, and starts the controller from the profile the interrupted run had learned. On a platform that declares durable staging, the same holds when the resume happens on a different node with the staging volume attached.

---

## 4. Architecture

### 4.1 One paragraph

The runtime is a pipeline of stages connected by byte-bounded queues, executed by one pool of worker threads that are not bound to stages, sized and paced by a single resource controller that reads the host's limits, looks ahead at the source's metadata, learns how each kernel amplifies its input, and adjusts four knobs: morsel size, active workers, read-ahead depth, and the spill trigger. Every morsel is an Arrow record batch or a DLPack tensor with a header that says where its bytes live. Every stage speaks one of those two layouts and nothing else. Between stages, morsels are placed across a hierarchy of tiers (device memory, pinned host memory, host memory, local disk) by DMA rather than by the CPU, and disk is a deliberate tier rather than an emergency. Every morsel emits a trace record, and the trace is both the run report and the controller's training data.

### 4.2 The picture

```
                       ┌──────────────────────────────────────────────┐
                       │            Resource controller                │
   cgroup / OS  ─────▶ │  discovery · probe · sizing · bottleneck      │ ◀──── trace records
   (memory.max,        │  classification · envelope · profile store    │       (per morsel)
    cpu.max, GPU)      └───┬──────────────┬──────────────┬─────────────┘
                           │ read-ahead   │ morsel size  │ workers, spill trigger
                           ▼              ▼              ▼
   ┌─────────┐   Q0    ┌────────┐  Q1   ┌────────┐  Qn   ┌─────────┐
   │ Source  │ ──────▶ │Kernel 1│ ────▶ │Kernel 2│ ────▶ │  Sink   │
   │(prefetch│ (never  │        │(spill-│        │(spill-│ (writer │
   │ via IO  │  spills)│        │ able) │        │ able) │  via IO │
   │ reactor)│         └────────┘       └────────┘       │ reactor)│
   └─────────┘              ▲                ▲           └─────────┘
                            │                │
                     ┌──────┴────────────────┴───────┐
                     │  Worker pool (N threads, role-free)│
                     │  next() ← scheduler admission rule │
                     └───────────────────────────────────┘
```

Three kinds of threads exist. The worker pool runs kernels and nothing else. The IO reactor (tokio) runs source prefetch, sink writes and spill segment writes so that no worker ever blocks on storage. The controller runs on its own thread with a 250 ms tick and is also invoked synchronously on every morsel completion, which is when the interesting measurements arrive.

### 4.3 Design decisions and the alternatives that lose

**D1. Workers are role-free; stages are queues, not thread pools.** SEDA gave each stage its own pool and a controller to resize it, and its author later reported the cost: context switching between pools and threads idle in one stage while another starved. Morsel-driven execution removes the problem by never binding a thread to a stage. A worker asks the scheduler what to do next; the scheduler answers from queue state. "Pause the producers and turn them into drainers" is not a mode switch, it is the admission rule declining to hand out source work. The cost is that stateful kernels break the symmetry: a worker holding a loaded model cannot take arbitrary work. The design handles this with instance pools per stateful stage (5.5) rather than by reintroducing per-stage threads.

**D2. One controller, several resources.** A memory controller that shrinks morsels and a CPU controller that adds workers will oscillate, because more workers means more memory. Coupled resources get one controller. Memory and device memory are hard constraints; CPU throttling, IO latency and queue depths are diagnostic inputs that tell the controller where the bottleneck is and which knob relieves it. The cost is a controller with more inputs and a decision function that must be written carefully; that is accepted, and the decision function is isolated behind a trait (5.8) so it can be replaced.

**D3. Look ahead at the data; learn the kernel.** The obvious formulation, forecasting the next morsel's memory from a time series of past morsels, treats the data's future as unknown. It is not: the Parquet footer says what is coming. The only unknown is the kernel's amplification, which is a regression from morsel features to peak footprint, fitted online, predicting an upper quantile. The rule-based sizer ships first; the learned sizer replaces it behind the same trait. The cost is that sources without metadata (CSV, JSON, arbitrary iterators) lose the look-ahead and fall back to probe-and-adjust, which is the rule-based path anyway.

**D4. Rust core, Python at the edge, kernels Arrow-native.** The scheduler, queues, controller and IO run in Rust with no dependency on the Python build. A Python kernel is a callable receiving a pyarrow RecordBatch across the C Data Interface, attached to the interpreter on the worker thread that runs it. Under free-threading it runs in parallel; under a GIL it serialises and says so. The cost is a Rust codebase for a team whose product language is Python; the mitigating fact is that the kernel authors, who are the users, write Python or use existing Rust plugins, and only the runtime's maintainers write Rust.

**D5. Only post-kernel queues spill.** The queue in front of the first kernel holds batches that were just read from durable storage; spilling them rewrites data that exists a read away. Stopping the source is the free spill. Queues holding kernel output hold work that would have to be recomputed, and those spill as Arrow IPC segments to local disk, tail first so the head stays hot. The cost is that a slow sink with a fast kernel can accumulate spill up to the disk bound; that bound is enforced and reaching it is a clean failure (S10).

**D6. A plain thread pool, not rayon or a tokio runtime, for compute.** Workers are std threads in a loop: ask the scheduler, run the task, report. The scheduler's admission logic is the whole point and is easier to state over a plain pool than to express as rayon tasks or tokio futures. Kernels are synchronous and may block for seconds; putting them on an async runtime would be wrong. The cost is a small amount of thread-pool code that rayon would have supplied.

**D7. Learning proposes, rules bound.** Any learned sizer's output is clamped by an envelope derived from the budget and the probe, and the controller reverts to the rule-based sizer when the model's prediction error exceeds a threshold. A confident wrong model at the tail is an OOM; the envelope makes that impossible by construction, at the cost of leaving some throughput on the table when the model is right and the envelope is conservative.

**D8. Linear chains in v1; DAGs later.** The stage graph is a chain: one source, one or more kernels in sequence, one sink. Fan-out and fan-in add ordering and accounting complexity that the sufficiency criteria do not require. The cost is that a job needing two sinks runs twice or writes a struct column; accepted for v1 (Q4).

**D9. Two payload layouts, not one and not many.** A morsel carries an Arrow batch or a DLPack tensor. The alternative of forcing tensors through Arrow's FixedShapeTensor extension keeps one type but makes every tensor kernel unwrap an extension array and every device tensor pretend to be a host column; the alternative of a general payload trait invites a third and fourth layout and destroys the portability guarantee. Two layouts cover tables and maths, they convert to each other by pointer for the numeric case, and every host in section 6 understands both. The cost is a match on every kernel boundary, which is a branch, not a copy.

**D10. Placement is the queue's job, and disk is a tier.** The naive queue is a list in RAM that overflows to disk when full. Here the queue owns where each morsel's bytes are (device, pinned host, host, disk) and moves them toward where the consumer will read them, ahead of the consumer, using DMA engines and files whose layout is the memory layout. Disk becomes a staging tier the controller uses on purpose when the working set exceeds host memory, at sequential NVMe bandwidth, rather than a penalty path. The alternative, RAM-only queues with emergency spill, is simpler and is what every engine does; it loses because on a bounded host it is the emergency path that decides throughput, and because a GPU pipeline needs the staging tier anyway. The cost is a placement engine with tier-specific allocators and copy paths, and hardware-dependent fallbacks that must be tested on every host class in section 6.

**D11. Weight-major execution is a separate design, not a v1 feature.** When a stateful kernel's weights exceed device memory, inverting the loop, streaming layers through the accelerator and running each over all data with activations held in the tiered queues, turns the job into the out-of-core full pass this runtime is built for (FlexGen is the existence proof). It needs the model expressed as a chain of stages and a controller that budgets activations across tiers. It is recorded here so the payload, arena and queue designs do not preclude it, and deferred to its own document (Q7).

**D12. Reserve the seams for many nodes now; build them later.** The multi-node extension (section 11) needs a remote memory tier, a node identity on every origin, locality in admission, a shuffle-free boundary and a manifest that survives a node. Adding those as types and explicit `match` arms costs a few hundred lines in v1 and nothing at runtime; adding them after v1 ships means touching every crate that matches on a tier. The alternative, a clean single-node v1 and a rewrite for multi-node, is what every distributed engine's history looks like. The cost is a handful of variants that return `Unsupported` for a year, and a lint to keep them honest.

**D13. Recover by lineage, not by replication.** Every morsel is a deterministic function of its origin (a split and a row range) and the kernel chain, and the placement engine already knows which morsels are on disk because pressure put them there. So the record needed to resume a run is small (what the sink has committed, what is on disk and where, where the source cursor is) and costs nothing on the normal path but a manifest write every few seconds; lost morsels are re-read from the source and re-run. The alternative, Spark-style replication or forced checkpointing of every stage's output, buys faster recovery of in-memory state at the price of writing everything twice, which on a bounded single node is exactly the bandwidth the runtime is trying to keep. Kernel state that depends on the morsels seen is the one thing lineage does not cover; a kernel declares whether a fresh `init` suffices, whether it checkpoints its own state, or whether it forbids resume.

---

## 5. Component design

Each component is described by what it owns, its interface, how it works, and what it refuses to know. Rust signatures are sketches: names and shapes are binding, exact generics are the implementer's.

### 5.1 Morsel

The unit of everything: what the scheduler hands out, what the controller counts, what queues hold, what spills, and what leaves a trace.

```rust
pub struct Morsel {
    pub seq: u64,              // assigned by the source, monotonic
    pub stage: StageId,        // which stage's input this is
    pub payload: Payload,      // see below
    pub bytes: usize,          // payload footprint at creation, updated when a kernel replaces it
    pub origin: Origin,        // source split id + row/batch range + node id; carried through stages
    pub features: MorselFeatures, // tabular: rows, per-column bytes, string lengths, null ratio
                                  // tensor: shape, dtype, batch dimension
}

pub enum Payload {
    Table(RecordBatch, Tier),          // arrow-rs; Tier says where the buffers live
    Tensor(ManagedTensor, Tier),       // DLPack-backed; shape, dtype, strides, data pointer
}

pub enum Tier { Device(DeviceId), PinnedHost, Host, Disk(SegmentRef), Remote(NodeId, RemoteRef) /* reserved, section 11 */ }
```

`Tier` is the placement engine's field (5.6): a morsel on `Disk` has no resident bytes and is loaded on demand; a morsel on `Device` has no host bytes. `bytes` is computed from Arrow's accounting or from the tensor's shape and dtype. `features` is computed by the source from metadata where available and from the payload otherwise; it is the controller's input vector and is carried unchanged into the trace. Conversion between the two payload forms is a pointer operation and is exposed as `Payload::as_tensor(column)` and `Payload::as_column(name)`, both of which fail at plan time, not run time, for types that cannot cross (strings, nested types, nullable columns). A morsel refuses to know which worker ran it or which queue it sat in; that is the trace's job.

### 5.2 Source

Owns the mapping from a dataset to morsels, the look-ahead, and prefetch.

```rust
pub trait Source: Send + Sync {
    fn schema(&self) -> SchemaRef;
    fn plan(&self) -> Result<Vec<Split>>;            // all splits, with stats, before any read
    fn read(&self, split: &Split, rows: Option<RowRange>) -> BoxFuture<Result<RecordBatch>>;
}

pub struct Split {
    pub id: SplitId,
    pub rows: u64,
    pub uncompressed_bytes: u64,        // from metadata; estimated if unavailable
    pub column_bytes: Vec<u64>,         // per projected column
    pub null_counts: Vec<Option<u64>>,
    pub sub_splittable: bool,           // can `read` take a RowRange?
}
```

**ParquetSource** is the reference implementation. `plan` reads every file's footer through object_store (one small ranged request per file, parallel) and emits one Split per row group with the footer's statistics for the projected columns. A row group larger than the controller's current morsel target is sub-split by row range; a row group smaller is read whole, and the scheduler may coalesce consecutive small splits into one morsel when the kernel benefits from larger batches. `read` decodes with the projected columns only and returns one RecordBatch.

Prefetch runs on the IO reactor. The source keeps `read_ahead` splits in flight, where `read_ahead` is a controller knob; each in-flight read holds its uncompressed size against the memory budget as reserved bytes, so read-ahead cannot silently exceed the budget. Completed reads become morsels in Q0.

**TensorSource** reads safetensors, GGUF, NumPy `.npy` and raw aligned binary. `plan` reads the header (a few kilobytes) and emits one Split per tensor, or per slice of the leading batch dimension when a tensor exceeds the morsel target, with `uncompressed_bytes` exact from shape and dtype and `sub_splittable = true` along that dimension only. `read` memory-maps the file and hands the kernel a tensor whose data pointer lies inside the map when the file's data is 64-byte aligned, and copies into the arena when it is not (safetensors does not guarantee alignment; see 2.2). On a host with GPUDirect Storage and a device-resident consumer, `read` targets device memory directly. There is no look-ahead problem for tensors: the shape says everything, and the controller's prior for a tensor stage is that footprint is linear in the batch dimension.

**Local direct IO.** On local disks, both sources read with O_DIRECT into arena buffers, through io_uring where the host allows it and a `pread` thread pool where it does not, so that file bytes land in the arena without a page-cache copy. This is the same-host version of the CPU bypass and needs no special hardware.

A source refuses to know what the kernel will do with a batch. It exposes statistics; it does not size morsels. Sources without metadata (an iterator of batches, CSV) return splits with `uncompressed_bytes` estimated from the first read and `sub_splittable = false`; the controller then relies entirely on probe-and-adjust.

### 5.3 Kernel

The contract with the transformation, and the crate boundary that makes portability possible. `amoru-kernel` contains these types and depends only on arrow.

```rust
pub trait Kernel: Send + Sync + 'static {
    fn fingerprint(&self) -> Fingerprint;            // stable id: code identity + config hash
    fn output_schema(&self, input: &SchemaRef) -> Result<SchemaRef>;
    fn kind(&self) -> KernelKind;
    fn hints(&self) -> KernelHints { KernelHints::default() }
    fn init(&self, ctx: &InitCtx) -> Result<Box<dyn KernelState>>;   // once per instance
    fn restore(&self, ctx: &InitCtx, state: &[u8]) -> Result<Box<dyn KernelState>>;  // on resume, for kernels that checkpoint (section 10)
    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> Result<Payload>;
    fn accepts(&self) -> PayloadSpec;   // Table | Tensor | Either, and the Tier it wants (Host or Device)
}

pub enum KernelKind {
    Stateless,                                   // any worker, any morsel, state is unit
    Stateful { max_instances: NonZeroUsize },    // bounded instance pool; GPU model => 1
}

pub struct KernelHints {
    pub expected_amplification: Option<f64>,     // author's guess; seeds the probe, never trusted
    pub uses_device_memory: bool,                // controller tracks a second budget
    pub releases_gil: Option<bool>,              // Python kernels only; None = unknown
    pub preferred_rows: Option<usize>,           // e.g. a model's optimal batch; a hint, not a rule
    pub resume: ResumePolicy,                    // Reinit (default) | Checkpoint | Forbid; section 10
}
```

`apply` is synchronous, may take seconds, must not spawn threads that outlive the call, and must be safe to call concurrently on different states (Rust enforces this through `Send + Sync` on the kernel and `&mut` on the state). It returns a new payload; the runtime never assumes the output row count equals the input's, so explode and filter kernels are ordinary kernels. `accepts` tells the placement engine what to deliver: a kernel that declares `Tensor` on `Device` receives its morsel already in device memory, converted from a numeric column by pointer if the upstream payload was a table, and the placement engine did the transfer before the worker was handed the task. A cuDF-style kernel declares `Table` on `Device` and receives an Arrow batch through the C Device Interface.

**Stateful kernels.** `init` runs once per instance on the worker that will own the instance, and the instance stays with that worker until the run ends or the controller retires it. `max_instances` bounds parallelism for that stage; a GPU model declares 1 and the scheduler never runs two morsels of that stage at once. A stateful kernel may declare `uses_device_memory`, in which case the probe measures device memory as well as host memory and the controller sizes against the tighter of the two.

**Python kernels.** `PyKernel` wraps a Python callable or an object with `setup()` and `__call__`. `apply` attaches the worker thread to the interpreter (`Python::attach` in free-threaded PyO3; the GIL acquisition in a GIL build), exports the input across the C Data Interface as a `pyarrow.RecordBatch` or across DLPack as an object any framework can consume with `from_dlpack` (`torch.from_dlpack`, `jax.dlpack`, `cupy.from_dlpack`), calls the object, and imports the returned `pyarrow` batch or any object implementing `__dlpack__` the same way. No copy occurs in either direction, on host or device. `setup()` maps to `init`, so a model loaded in `setup` is a stateful kernel with the instance count taken from a decorator argument. The runtime reads `sys._is_gil_enabled()` at start and records the answer in the run report; a GIL build does not fail the run, it caps the effective parallelism of Python stages at one and says so.

**Portability.** A kernel author implements `Kernel` and nothing else. `amoru-polars` wraps a `Kernel` into a Polars expression plugin by converting the plugin's input Series to a RecordBatch and back; `amoru-datafusion` does the same for a `ScalarUDF`. Both wrappers are under a hundred lines and contain no kernel logic (S7). The runtime is the only host that uses `kind`, `hints` and `init` fully; the others treat every kernel as stateless and call `init` once.

A kernel refuses to know where its input came from, where its output goes, what morsel size it will be given, or how many workers exist.

### 5.4 Sink

```rust
pub trait Sink: Send + Sync {
    fn open(&mut self, schema: &SchemaRef) -> Result<()>;
    fn write(&self, seq: Seq, payload: Payload) -> BoxFuture<Result<()>>;   // runs on the IO reactor
    fn finish(&mut self) -> Result<SinkSummary>;                             // rows, bytes, files
    fn requires_order(&self) -> bool { false }
    // resume support, section 10; defaults make a sink non-resumable and say so
    fn committed_seq(&self) -> Option<Seq> { None }
    fn skip(&self, seq: Seq) {}
    fn checkpoint(&self) -> Result<Option<Vec<u8>>> { Ok(None) }
    fn resume(&mut self, schema: &SchemaRef, state: &[u8], committed_seq: Option<Seq>) -> Result<()>;
}
```

(The normative signatures are in `sdd/01-contracts.md` d.8; this sketch follows them.)

**ParquetSink** writes to object_store with a target row-group size (default 128 MB) and a target file size (default 1 GB), rolling files as needed; the writer is buffered on the IO reactor so kernel workers never wait on storage. **TensorSink** writes safetensors (for interoperability) or the runtime's own aligned binary (an 64-byte-aligned data section behind a fixed header, for files that will be memory-mapped or DMA-loaded later), and **ArrowIpcSink** writes Arrow IPC files with page-aligned buffers for the same reason. A sink that receives a device-resident payload copies it to the pinned arena by copy engine before writing; the CPU never touches the bytes. Unordered by default: morsels arrive in completion order. A sink that returns `requires_order = true` gets a reorder buffer in front of it, bounded in bytes and accounted against the budget; when the buffer is full and the missing sequence number has not arrived, the scheduler stops admitting new source work until it does. Ordering costs memory and throughput; the default is off (Q2).

A sink refuses to know what produced a batch or how far the run has progressed.

### 5.5 Scheduler

Owns the worker pool, the stage graph, the queues between stages, and the admission rule.

**Worker loop.** Each of N workers runs: `let task = scheduler.next(worker_id); let outcome = task.run(); scheduler.report(task, outcome)`. `next` blocks when nothing is admissible. `report` delivers the output batch to the next queue, the measurements to the controller, and the trace record to the trace writer.

**Admission rule**, evaluated in `next`: among stages that have input available and whose output queue is below its high-water mark, choose the stage with the fewest bytes in its output queue; a stateful stage is eligible only if an instance is free for this worker or the worker can create one within `max_instances`. Source reads are admitted through the same rule with Q0 as the output queue, and additionally only when the controller's current `read_ahead` is not already in flight. The rule is Ray Data's operator selection, restated over one process: the stage producing slowest gets the parallelism.

**Why this rule and not round-robin.** Round-robin fills every queue to its high-water mark and then stalls on the slowest stage with all memory occupied. Emptiest-output-first keeps memory concentrated in front of the bottleneck and lets the stages behind it drain, which is exactly the memory the controller wants back when it needs to shrink.

**Stateful instance pools.** Each stateful stage has a pool of up to `max_instances` states. A worker that picks a morsel from that stage acquires an instance (creating one if the pool is below its cap), runs `apply`, and releases the instance. Instances are affinity-tagged to the worker that created them and preferentially reacquired by it, so a worker that loaded a model keeps running that model and its caches stay warm. When the controller lowers the active worker count, workers park in LIFO order so the instances least recently used are the ones that go idle.

**Active worker count.** The pool has N threads where N is the CPU ceiling from discovery, but only `active` of them are allowed to take work; the rest wait on a condition variable. The controller raises and lowers `active` without creating or destroying threads, which makes the adjustment cost nothing.

**Completion.** When the source's plan is exhausted and Q0 is empty, each stage drains in order; when the last queue is empty and all instances are released, the sink's `finish` runs on the IO reactor, and the trace writer flushes.

**Errors.** A kernel error on a morsel terminates the run by default with the morsel's sequence number, origin and features in the diagnostic. An opt-in `skip` policy records the failure in the trace and continues. Panics in Rust kernels and exceptions in Python kernels are both caught at the worker boundary and reported the same way (Q3).

The scheduler refuses to know what a byte budget is. It reads knobs (active workers, high-water marks, read-ahead) and never computes them.

### 5.6 Queues and placement

A queue here is not a list in memory. It is an ordered set of morsels, each with a current tier, and a placement engine whose job is to have each morsel's bytes in the tier its consumer wants by the time the consumer asks. Byte-bounded per tier, not count-bounded, because morsels vary in size.

```rust
pub struct TieredQueue {
    order: VecDeque<MorselRef>,                  // FIFO; each ref names a tier and a location
    budget: TierBudgets,                         // bytes allowed per tier, set by the controller
    target: PayloadSpec,                         // what the consumer kernel declared in accepts()
    water: TierWaterMarks,                       // low/high per tier
    staging: Option<SpillConfig>,                // None for Q0
}
```

**Tiers and movement.** The tiers are `Device`, `PinnedHost`, `Host` and `Disk`, and the legal moves between them are all DMA: pinned host to device and back by the GPU copy engine on a stream owned by the queue; disk to pinned host by O_DIRECT read into the arena (io_uring where available); disk to device directly by GPUDirect Storage when the host has it, else through pinned host; device to disk the reverse. Ordinary `Host` is where a morsel lands when no accelerator is present and is the same as `PinnedHost` on such hosts. The CPU is never in the data path of a move; it issues the move and consumes a completion. Every move is asynchronous and tracked as an in-flight reservation against the destination tier's budget, so a burst of prefetch cannot overshoot.

**Push** never blocks. A morsel arrives in whatever tier its producer left it (a kernel's output tier). If that tier is above its high-water mark and `staging` is set, the placement engine demotes the newest morsels (the tail) one tier down, device to pinned host, pinned host to disk, until the tier is back at its low-water mark. Q0 never demotes below the tier the source delivered to (D5): its backpressure is the admission rule stopping the source.

**Pop** hands the consumer a morsel that is already in `target`'s tier. The placement engine keeps a promotion window of the next `k` morsels in the order, moving them up toward `target` ahead of time, with `k` sized by the controller from the consumer's measured service time and the move latency (the same bandwidth-delay arithmetic as read-ahead). If the head is not yet resident, pop waits on the move's completion, and the wait is recorded in the trace as a placement miss so the controller can raise `k`.

**Disk as a tier.** Staging segments are Arrow IPC files for tables and the aligned binary format of 5.4 for tensors, 64 to 256 MB each, with every buffer page-aligned, written by O_DIRECT sequential writes on the IO reactor to the staging directory (the platform's local scratch: a pod's ephemeral volume, Databricks' `/local_disk0`, `/tmp` on a laptop). Because the on-disk layout is the in-memory layout, reload is a DMA into the arena or, with GPUDirect Storage, into device memory, with no decode. On hosts without direct IO the segment is memory-mapped instead; those pages are file-backed and reclaimable and do not count against the anonymous-memory budget in 5.8. Total staged bytes are bounded by a configured disk limit (default 20% of free space at start, or the pod's ephemeral-storage limit if discoverable); reaching it is a clean failure (S10). The difference from conventional spill is that the controller may choose the disk tier deliberately, for example to hold activations between weight-major passes (D11) or to keep a device tier small on a shared GPU, and the sufficiency criterion S15 requires it to run at sequential bandwidth rather than as a penalty.

**Head stays hot.** Demotion is always from the tail, promotion always toward the head, so the consumer's next morsel is never the one being written out. This is DuckDB's buffer manager eviction order and FlexGen's schedule, restated over a FIFO.

**The engine remembers what it holds.** Because the placement engine is the one component that sees every morsel from push to pop and knows which are on disk, it is also where the run's recovery record lives: a lineage index of every morsel the sink has not yet committed (its origin, its stage, its segment if any), written as a small manifest every few seconds and at every segment roll. Section 10 describes resume; the point here is that the mechanism is a by-product of placement, not an extra pass over the data. The tier set also carries a reserved fifth tier, `Remote`, for memory lent by another node of the same run (section 11); no v1 path produces it.

**Compressed staging and promotion as a scan (post-v1).** Segments are raw today: the in-memory layout written by DMA and read back by DMA, no CPU in the path. A compressed record (Vortex-encoded, the format the Griot lakehouse stores) would cost CPU on demotion and, unless the consumer accepts compressed arrays, on promotion, in exchange for fewer disk bytes and therefore more effective disk bandwidth. That is a trade the controller can make and the format cannot: when the bottleneck classification says the run is sink-bound or disk-bound and workers are idle, the CPU is free and buys throughput; when the run is compute-bound, it would steal from kernels. So the staging codec is a per-queue knob the controller flips, the encode runs as a worker task the placement engine issues, and the exception to "bytes move only by the reactor" is stated the way the Parquet decode exception is stated for G-I2. The contract carries `StagingCodec` now with `Raw` as its only variant, matched explicitly, so the variant is an addition when it comes. Two further steps follow from a lazily-decodable format. First, promotion need not return the whole record: with layouts that resolve a projection and a row selection to byte ranges, and statistics that let a filter run on compressed bytes, promotion becomes a small scan that brings back only the columns and zones the next consumer's `accepts` and predicate need, and the disk tier becomes a queryable store rather than a byte store. Second, when the source itself is a Vortex file on local NVMe, Q0's staging tier and the source are the same bytes: Q0 need not be written at all, a `Disk` segment reference generalises to a row range in the source file, and the lineage manifest gets simpler because the on-disk copy of a source morsel is its origin. For Griot Cloud that makes the local cache of a lakehouse table simultaneously the source, the staging tier and the recovery record. On a GPU host the same format closes the loop the other way: a compressed segment reaches the device by GPUDirect Storage and is decoded there, so fewer bytes cross the bus than with raw Arrow. None of this is v1; the `VortexSource` (sources SO-M1, Phase 7) is the first step and the one that pays on its own.

Queues refuse to know why the controller set a budget or why a consumer wants a tier; they know only tiers, budgets and the order.

### 5.6a Memory arena

All host-side morsel bytes come from one arena the runtime owns for the run: a contiguous, page-aligned region allocated at start to the size of the host budget, backed by huge pages where the host offers them and by pinned (page-locked) memory where an accelerator is present, so that any buffer in it is a valid DMA source or destination without further registration. On hosts with RDMA-capable NICs the arena is additionally registered with the NIC once, which is what makes a future RDMA source (section 6) a zero-copy path. The arena hands out morsel buffers by a size-class allocator with 64-byte alignment, and it is the allocation counter S13 reads. Device memory has a matching arena per device, sized to the device budget. The arena is why the budget is exact rather than sampled: bytes not in the arena are not morsel bytes, and the controller accounts for them under `baseline`. Pinned memory is a scarce resource for the operating system, so the arena pins only up to the budget and never the whole machine; on a host without an accelerator it does not pin at all.

### 5.7 Resource discovery

The only component that knows the difference between a pod and a laptop.

```rust
pub struct Limits {
    pub memory_ceiling: u64,     // memory.high if set, else 0.9 * memory.max, else 0.9 * total RAM
    pub memory_kill: Option<u64>,// memory.max if in a cgroup
    pub cpu_quota: f64,          // cpu.max quota/period; else logical cores
    pub devices: Vec<Device>,    // GPU id, total and free device memory (feature "cuda")
    pub source: LimitSource,     // Cgroup | Os | Explicit
}
pub trait Sampler { fn sample(&self) -> Sample; }   // anon bytes, file bytes, throttled_usec, peak
```

On Linux it reads `/sys/fs/cgroup/` for the process's own cgroup (v2 only; v1 hosts fall back to OS totals with a warning). `memory.stat` supplies the anon/file split; the controller budgets against anon plus unevictable, because file-backed pages (page cache from reads, memory-mapped spill) are reclaimable by the kernel before it reaches the OOM killer. Outside a cgroup, `/proc/self/statm` and `sysinfo` supply the same numbers with less precision. Explicit overrides win over discovery in all cases. An `Explicit` budget inside a cgroup that is larger than the cgroup's ceiling is clamped, with a warning naming both numbers.

### 5.8 Resource controller

Owns the four knobs and the model that sets them. It is the component that does not exist elsewhere.

**Inputs**, refreshed on every morsel completion and every 250 ms tick: memory anon bytes and peak since last tick; per-stage queue bytes; per-worker busy fraction; CPU throttled time delta; source read latency and in-flight count; sink write latency and backlog; device memory used, if any; the features of the next `read_ahead` splits in the source's plan.

**Outputs:** `morsel_target_bytes` (per kernel stage), `active_workers`, `read_ahead`, `spill_trigger` (a bool per spillable queue), and the queues' high-water marks.

**Budget arithmetic.** `budget = memory_ceiling - baseline - reserve`, where `baseline` is the process's anon memory measured after all kernels' `init` have run (this captures loaded models), and `reserve` is 10% of the ceiling held back as headroom. Working-set model, which the controller keeps balanced at every decision:

```
active_workers × morsel_target × A_k × safety  +  Σ queue high-water marks  +  read_ahead × split_bytes  ≤  budget
```

`A_k` is the amplification factor for kernel k, the ratio of peak footprint during `apply` to input bytes. `safety` starts at 1.5 and tightens toward 1.2 as the measured prediction error shrinks.

**Probe.** Before the first real morsel of each kernel stage, the controller runs one morsel of 16 MB (or `preferred_rows` if hinted) through the kernel on a single worker with all other workers parked, and measures the anon-memory delta at peak. That delta divided by input bytes is the initial `A_k`. If the kernel declares device memory, the same probe measures device memory and yields a second factor. The probe's output is a real morsel and proceeds downstream; nothing is wasted. When a profile exists for the kernel's fingerprint and schema (below), the probe still runs, to detect drift (except on a resumed run, section 10, where the profile the interrupted run wrote a few seconds earlier is trusted and only stages without one are probed), but the profile's factor is used until the probe completes.

**Sizing.** With `A_k` known, `morsel_target` follows from the working-set equation at the chosen `active_workers`, clamped to [4 MB, 512 MB]. Look-ahead: the next splits' `uncompressed_bytes` and per-column stats are known, so the source sub-splits or coalesces to hit the target in actual bytes rather than in rows. Then AIMD on measured outcomes: after each morsel, if the observed peak was below 85% of what the model allowed, raise `morsel_target` by 10%; if it exceeded the allowance, halve it and raise `safety`. Adjustments are damped to no more than one change per `active_workers` completions, which is what makes S11 achievable.

**Worker count.** `active_workers = min(cpu_quota, stateful caps, floor(budget_for_workers / (morsel_target × A_k × safety)))`, re-evaluated on each tick. A rising throttled-time delta lowers it by one until the delta stops rising; borrowed CPU that memory cannot feed is not used.

**Bottleneck classification**, once per tick, driving which knob moves:

| Observation | Diagnosis | Action |
|---|---|---|
| Workers busy < 70%, Q0 empty, source in-flight at max | IO-bound on read | raise `read_ahead` while budget allows |
| Workers busy < 70%, Q0 non-empty | memory-bound (workers parked by budget) | shrink queues' high-water marks toward low, then raise workers |
| Workers busy > 90%, last queue growing, sink backlog rising | sink-bound | set `spill_trigger` on the last queue; lower `read_ahead` |
| Workers busy > 90%, all queues near low-water | compute-bound | nothing to do; report |
| Throttled time rising | CPU quota reached | lower `active_workers` by one |

**Decision function trait.** The sizing logic above is `RuleSizer`. The controller talks to it only through:

```rust
pub trait Sizer: Send {
    fn propose(&mut self, obs: &Observation, envelope: &Envelope) -> Knobs;
    fn observe(&mut self, obs: &Observation, outcome: &Outcome);
    fn confidence(&self) -> f64;
}
```

`Envelope` carries the hard bounds derived from the budget and the probe; the controller clamps whatever a sizer proposes to it (D7). `LearnedSizer`, a later phase, fits an online quantile regression from morsel features (rows, per-column bytes, mean string length, null ratio, active workers) to observed peak per input byte, predicting the 95th percentile, and warm-starts from the profile store. When its rolling prediction error exceeds twice `RuleSizer`'s, the controller switches back to `RuleSizer` for the rest of the run and records the switch in the report.

**Profile store.** After every run, the controller writes `{fingerprint, schema_hash, A_k quantiles, morsel_target at end, workers at end, error stats}` as a small JSON record to a profile directory (default `~/.amoru/profiles/`, overridable, disableable). The next run with the same fingerprint and schema starts from those values instead of from defaults, so the second run of a job is efficient from its first morsel. Profiles are advisory: the probe still runs and overrides them on drift.

The controller refuses to know how a source reads or how a queue spills. It sets numbers; the components act.

### 5.9 Trace and run report

Every morsel completion writes one record to the trace, an Arrow IPC stream on the IO reactor, flushed at run end and on failure:

```
seq, stage, worker, t_start, t_end,
rows_in, bytes_in, rows_out, bytes_out,
feat_mean_str_len, feat_null_ratio, feat_col_bytes[],
knob_morsel_target, knob_active_workers, knob_read_ahead,
mem_anon_before, mem_anon_peak, dev_mem_peak,
cpu_time_us, throttled_delta_us,
q_bytes_before[], q_bytes_after[], spill_bytes_delta,
sizer, outcome (ok | error(msg) | skipped)
```

The run report is computed from the trace and the limits alone: peak memory against budget, throughput per stage, worker busy fraction against quota, source bandwidth, spill volume, bottleneck classification over time, GIL state, sizer used and any fallback. It is printed at the end of `run` and returned as an object; S1 to S4 are read directly from it (S9). The trace is also the dataset the learned sizer is fitted on.

### 5.10 Python surface

```python
import amoru

@amoru.kernel(stateful=True, instances=1, device_memory=True)
class Score:
    def setup(self):
        self.model = torch.load("model.pt").cuda().eval()
    def __call__(self, batch: pyarrow.RecordBatch) -> pyarrow.RecordBatch:
        ...

report = amoru.run(
    source=amoru.ParquetSource("s3://bucket/data/", columns=["id", "text"]),
    kernels=[normalize, Score()],
    sink=amoru.ParquetSink("s3://bucket/scored/"),
    budget=None,            # discovered; a string like "6GiB" overrides
    trace="./trace.arrow",  # optional
)
print(report)
```

A plain function is a stateless kernel. Kernels may also be Polars expressions (`amoru.polars(lambda df: ...)`, applied per morsel through the lazy API) or Rust plugins loaded by name. The package is built with maturin as version-specific wheels for CPython 3.13 and 3.14, GIL and free-threaded (abi3 cannot target free-threaded builds). `amoru.run` refuses to start if a kernel is Python and the interpreter reports the GIL enabled, unless `allow_gil=True`, in which case it proceeds serialised and the report says so.

---

## 6. Cross-cutting deep-dive: hosting and portability

The runtime has to be the same library in five hosts, and the design must say what changes in each.

**A pod or container with cgroup v2 limits.** Discovery reads the cgroup; the budget is `memory.high` if the platform set one, otherwise 90% of `memory.max`; workers from `cpu.max`. The runtime holds anon memory below the ceiling and lets the kernel reclaim page cache and spill mappings. Spill goes to the pod's ephemeral volume and is bounded by its limit when discoverable from the downward API, otherwise by the 20% rule. A platform that wants the runtime to behave differently (a tighter reserve, a fixed spill path) sets environment variables `AMORU_BUDGET`, `AMORU_SPILL_DIR`, `AMORU_SPILL_LIMIT`; there is no configuration file.

**A single-node Databricks cluster.** There is no cgroup limit on the driver process that reflects the user's intent, and a JVM holds a large fraction of the machine. Discovery falls back to OS totals, which would be wrong, so this host is the one where an explicit budget is expected: the documented pattern is `budget=` set from the cluster's driver memory minus the JVM's configured heap. Spill defaults to `/local_disk0`. Spark is not used; the runtime reads Parquet from DBFS or the object store directly through object_store. The report notes `LimitSource::Explicit`.

**A laptop or bare VM.** OS totals, 90% ceiling, cores from the OS. This is the developer's host and the benchmark host.

**A GPU host.** Feature `cuda` enables device discovery through the CUDA runtime API; without the feature, kernels that declare `uses_device_memory` run with host-memory sizing only and the report warns. Device memory is a second budget with its own arena, its own probe and its own `A_k`; the morsel target is the smaller of the host- and device-derived targets. Multiple GPUs map to `max_instances` on the stateful kernel, one instance per device, with the instance's `InitCtx` naming its device. The data path on such a host is: source decodes or maps into the pinned arena; the placement engine promotes the next morsels to device memory on a copy stream while the current one computes; the kernel receives a device tensor or a device Arrow batch by pointer; its output is demoted to pinned host for the sink or kept on device for a following device kernel. With GPUDirect Storage present, disk-tier segments and aligned tensor files skip the host entirely.

**A host with RDMA-capable networking.** Not a v1 host. Two uses, both through the reactor. First, a storage peer that speaks RDMA writes morsels straight into the registered arena through an `RdmaSource` behind the Source trait (AIStor's S3 over RDMA is one such peer; a storage daemon that decodes Parquet on the storage side and pushes Arrow morsels is another, and would be Griot's own). Second, other nodes running the same job lend memory and cores: the placement engine sees their registered arenas as a `Remote` tier between pinned host and disk, and the scheduler on each node runs kernels on the morsels it holds; section 11 says how and where the boundary is. On a single machine RDMA is meaningless; the equivalent, a shared-memory Arrow region between the storage process and the runtime, is a `SharedMemorySource` and is the recommended way for any co-located gate or proxy to feed the runtime without a copy.

**Inside another engine.** `amoru-polars` and `amoru-datafusion` host a kernel, not the runtime. In those hosts the engine schedules, the engine sizes, and the kernel is a stateless batch function; the runtime's controller is absent. This is by design: the runtime cannot take over an engine's scheduler, and the kernel crate is what carries across.

**Crate layout** that makes this true: `amoru-kernel` (traits, Morsel, Payload, features; depends on arrow and dlpark only), `amoru-runtime` (everything in section 5 except 5.3's trait and 5.10), `amoru-py` (PyO3 bindings, depends on runtime), `amoru-polars` and `amoru-datafusion` (bridges, depend on kernel only). A kernel author's crate depends on `amoru-kernel` alone.

---

## 7. Failure modes and degraded behaviour

**Approaching the memory ceiling.** Sequence, in order: the controller's AIMD halves the morsel target and lowers active workers; spill triggers on the last spillable queue; read-ahead drops to one; if anon memory still rises (a kernel whose amplification has no relation to input size), the run terminates with a diagnostic naming the morsel, its features, its measured footprint and the budget. Inside a cgroup, `memory.high` throttling slows the process during this sequence and gives the controller time; `memory.max` is never reached because the ceiling is below it. What it looks like: throughput falls, the report shows the sizer's adjustments, and either the run completes or the last trace record explains why not.

**A single row larger than the maximum morsel.** The source cannot sub-split below one row. The morsel is passed at its natural size, the controller drops active workers to what the budget allows for that morsel (possibly one), and the trace marks it. A single row larger than the budget terminates the run with a diagnostic; no runtime can process it.

**Kernel panic or Python exception.** Caught at the worker boundary; the morsel is recorded with the error; default policy terminates, `skip` continues. The stateful instance that raised is retired and re-initialised on next acquisition, since its state may be corrupt.

**Stateful `init` fails.** The run terminates before any morsel is read, with the error; nothing has been written.

**Source read failure.** object_store retries transient errors with backoff; a persistent failure on a split terminates the run naming the split (file and row group). Partial output remains in the sink's completed files; the sink's `finish` is not called, and the report says which splits completed.

**Sink write failure.** Same shape: the reactor surfaces the error, the run terminates, completed files remain, uncommitted buffers are lost, the report lists the last committed sequence number.

**Disk full during spill, or spill limit reached.** The queue reports the failed segment write; the run terminates with the spill volume, the limit, and the queue's state. No unbounded growth.

**GIL-enabled interpreter with Python kernels.** Refused at start unless `allow_gil=True`; then Python stages run one morsel at a time while Rust stages and IO continue in parallel. The report states the effective parallelism.

**CPU throttling in a container.** Throttled time rises; the controller lowers active workers until it stops; the report shows the throttled fraction. Throughput is bounded by the quota, which is correct behaviour.

**Controller oscillation.** Damping bounds adjustments to one per `active_workers` completions; if the sign of adjustment flips more than the S11 threshold, the controller freezes the morsel target at the geometric mean of the last ten values for 100 morsels and records the event.

**Learned sizer divergence.** Prediction error above twice the rule sizer's triggers fallback for the rest of the run; the report records the morsel at which it happened.

**Cancellation.** SIGINT or a Python KeyboardInterrupt stops admission, lets in-flight morsels complete (bounded by the longest `apply`), flushes the trace and the sink's completed files, and returns a report marked cancelled.

**Spill directory on the same device as the source.** Not detected automatically. Documented as a configuration hazard; the report shows spill bandwidth and source bandwidth side by side so a collision is visible.

**Placement miss.** The consumer asks for a morsel that is still in flight from a lower tier. The pop waits, the wait is traced, the controller raises the promotion window. Persistent misses with the window at its maximum mean the tier's bandwidth is the bottleneck, and the report says so.

**Device out of memory.** The device arena is exhausted despite the budget, usually because the kernel's framework allocator holds more than the probe saw. The controller halves the morsel target for that stage, demotes any promoted-but-unconsumed morsels back to pinned host, and retries once; a second failure terminates with the device figures in the diagnostic. Device memory is never allowed to fragment across two arenas.

**Direct IO unavailable.** O_DIRECT rejected by the filesystem, io_uring blocked by the container's seccomp profile, or GPUDirect Storage absent. Each falls back one step (buffered IO, `pread` thread pool, pinned-host bounce) and the report names the path taken, so a run that is slower than expected can be explained without a debugger.

**Process or node loss.** The OOM killer (a foreign process on the host, not this one; G-I8 covers this one), a node reboot, a pod eviction, a Kubernetes node loss. Nothing is done at the moment of loss because nothing can be; the last manifest, written within `checkpoint.interval_ms`, is the recovery point. A new process with `resume=` reads it, removes the sink's uncommitted output, puts the morsels that were on disk back in their queues, re-reads the rest from the source, and continues (section 10). What is lost is bounded: the sink's file in progress, and the morsels that were in memory or in flight, each re-read once. On a platform without durable staging, resume works only on the same node; the report and the exception say which case applies.

**Pinned memory refused.** The host limits locked memory (RLIMIT_MEMLOCK, or the pod lacks the capability). The arena falls back to unpinned huge pages and device transfers go through a small pinned bounce buffer; the report says so.

---

## 8. Explicit risks and accepted positions

**Memory accounting is approximate.** Arrow's `get_array_memory_size` undercounts allocator overhead; anon memory as reported by the cgroup lags the allocator's view; allocators hold freed memory. Accepted position: the runtime budgets against the cgroup's anon figure with a 10% reserve and a safety factor learned from the probe, not against Arrow's numbers, and it uses mimalloc or jemalloc so that freed morsel memory is returned promptly. The residual risk is a burst of allocations between two samples; the 250 ms tick and the per-completion sample bound the window.

**Page cache is charged to the cgroup.** `memory.current` includes file-backed pages and can approach `memory.max` while anon is well under budget. The kernel reclaims those pages before invoking the OOM killer, so this is not a kill risk, but it does mean `memory.current` alone is the wrong signal. Accepted position: budget against anon plus unevictable from `memory.stat`; document the distinction in the report.

**Python parallelism depends on the ecosystem's free-threading progress.** A user whose kernel imports a not-yet-safe extension gets a GIL and a serialised Python stage. Accepted position: detect, refuse by default, allow with a flag, and state it in the report. The runtime's own parallelism and its Rust kernels are unaffected.

**Look-ahead is Parquet-only in v1.** Other sources lose the metadata and fall to probe-and-adjust. Accepted: Parquet is the format the problem statement names; iterators and CSV work correctly with less precision.

**Stateful kernels cap parallelism.** A single-instance GPU kernel serialises its stage; the pipeline's throughput is that kernel's. Accepted: this is the physics of one GPU, and the runtime's job is to keep the GPU fed (read-ahead, prior stages in parallel), which it does.

**Ordering is off by default.** A sink that needs order pays a bounded reorder buffer and possible stalls. Accepted; ordered output is the exception in this workload class, and the flag is explicit.

**Kernel-internal allocations are observed, not governed.** The budget is enforced for every byte the runtime allocates: morsels, queues, staging, the arena. A byte a kernel allocates for itself inside `apply` (a NumPy temporary, a Torch tensor, a Rust `Vec`) is outside the arena and is seen only by the sampler, after the fact. The guarantee for those bytes is therefore indirect: the probe measures what a kernel does to the process, the controller sizes morsels and workers around that measurement with a safety margin, the reserve absorbs a miss, breach handling shrinks the next morsel, and the cgroup is the containment if all of that is wrong at once. This is the honest limit of "never OOM": it holds for any kernel whose footprint is a stable function of its input, which is the class the runtime targets, and it degrades to "diagnosed and terminated" for a kernel whose footprint is not. Closing it fully means mediating the allocators those libraries use (NumPy and PyTorch both allow the allocator to be replaced; adapters AD-M1, escalation E13), which is a change underneath the kernel author's code and is deferred to its own design. The contract's `Allocator` is the seam; nothing in v1 precludes it.

**The learned sizer is not in v1.** The rule sizer with look-ahead and profiles is expected to reach S3 on its own; the learned sizer is an improvement, not a dependency. Accepted, with the trait boundary in place so it can be added without touching the scheduler.

**Hardware-direct paths are hardware-dependent.** GPUDirect Storage, io_uring, huge pages and pinned memory each depend on a driver, a filesystem, a kernel setting or a capability, and each will be absent on some hosts the library must run on. Accepted position: every direct path has a tested fallback that produces identical output, the fallback is chosen at start rather than on failure, and the report names the path. The direct paths are performance, never correctness.

**Two payload layouts is a permanent commitment.** Adding a third later (sparse tensors, ragged arrays) would touch every kernel boundary. Accepted: sparse and ragged data are represented as Arrow list or struct columns, which is what Arrow is for, and are not promised zero-copy into frameworks.

**Tensor and analytics workloads compete for the same design attention.** The tensor path is smaller in code but larger in hardware surface. Accepted position: v1 gates are analytics gates; tensor gates are v1.1 (Phase 5 and 9); nothing in v1 is allowed to preclude them, which is why Payload, Tier and the arena are in the v1 types even though only the host tier is exercised.

**A tiny dataset pays runtime overhead it does not need.** If the source's total uncompressed size is under a quarter of the budget, the runtime skips the probe, sets one morsel per split at full size, and uses all workers; this costs nothing and removes the objection.

**Rust is not the team's product language.** Accepted for the reasons in D4; the surface area maintained in Rust is bounded to the crates in section 6, and kernel authors do not touch it.

**The name is a working name.** Q1.

---

## 9. Implementation plan (appendix)

### 9.0 Benchmark suite (built in Phase 0, used by every gate)

A synthetic Parquet generator with controllable row count, column mix (ints, floats, short strings, long text with configurable mean length and variance), null ratio, and row-group size; it writes to local disk and to an S3-compatible store (MinIO in a container). Five kernels: **identity** (IO-bound, A about 1); **normalise** (regex over a text column in Rust, A about 1.5); **tokenise-explode** (splits text into a list column and explodes, A 5 to 10); **wide-intermediate** (a Python NumPy kernel that materialises a float matrix per batch, A about 20, releases the GIL); **adversarial** (A jumps 4x at the midpoint of the dataset). Two tensor kernels from Phase 5 on: **embed-score** (numeric Arrow columns crossed to a tensor, a small matrix multiply, result crossed back as a column; host and, where present, device) and **torch-score** (a stateful PyTorch model on a GPU host, weights in safetensors read by TensorSource). A tensor-only dataset generator writes safetensors and aligned binary at controllable sizes. A **hand-tuned baseline** script for each: a plain loop over row groups with a fixed batch size and a fixed thread count, grid-searched, reported as rows per second. All gates run inside a container with `--memory` and `--cpus` set, and on the bare host.

### Phase 0: contract and skeleton
Deliverables: `amoru-kernel` with `Kernel`, `Morsel`, `Payload` (both variants, `Tier` with only `Host` exercised), `MorselFeatures`, `KernelKind`, `KernelHints`, `PayloadSpec`; `amoru-runtime` with the host arena (unpinned, huge pages where offered), `ParquetSource` (plan from footers, read with projection and row selection into arena buffers), `ParquetSink`, a single-threaded driver that runs source → kernels → sink with fixed 128 MB morsels; the trace writer with the 5.9 schema; the benchmark generator and the identity kernel. Gate: S12 (overhead within 5% of a plain loop), S9 partial (trace written with full schema), S7 partial (`amoru-kernel` depends on arrow and dlpark only; verified by `cargo tree`).

### Phase 1: scheduler, queues, workers
Deliverables: worker pool with parked/active split; `TieredQueue` with the `Host` tier only; stage chain; admission rule; stateless kernels only; completion and error handling with the default terminate policy. Gate: S4 for compute-bound kernels on the bare host (worker busy at least 85% of cores); S11 trivially (no controller yet, so no oscillation; the test harness exists).

### Phase 2: discovery and rule controller
Deliverables: cgroup v2 and OS discovery; sampler; the probe; `RuleSizer` with look-ahead sizing, AIMD, damping, worker-count formula, bottleneck table; the working-set equation enforced. Gate: S1, S2, S5 in the container; S6 against the adversarial kernel (completes or diagnoses, never SIGKILL); S11 on the stationary dataset; S3 for normalise and tokenise-explode.

### Phase 3: disk tier
Deliverables: `TieredQueue` with `Host` and `Disk` tiers; page-aligned Arrow IPC staging segments written with O_DIRECT on the IO reactor (io_uring with `pread` fallback), tail-first demotion, head promotion window, direct-IO reload into the arena with mmap fallback, disk bound from ephemeral limit or free-space rule, staging trigger in the controller, placement-miss tracing; the run manifest and lineage index in the placement engine, sink commit tracking, and single-node resume (section 10). Gate: S10 with the throttled sink; S15 (post-spill throughput at least 70% of pre-spill on NVMe); S1 re-run with the disk tier active; S17 on the same node (kill and resume, output equal).

### Phase 4: Python surface
Deliverables: `amoru-py` with PyO3 0.28+, maturin builds for 3.13/3.14 GIL and free-threaded, C Data Interface exchange, `@amoru.kernel`, `amoru.run`, report object, GIL detection and refusal. Gate: S8 with the wide-intermediate kernel on both builds; S2 from Python.

### Phase 5: tensors, stateful kernels and device memory
Deliverables: `Payload::Tensor` exercised end to end; `TensorSource` (safetensors, `.npy`, aligned binary; mmap with arena copy when unaligned) and `TensorSink`; column-to-tensor and tensor-to-column pointer conversions with plan-time type checks; DLPack export and import in `amoru-py`; instance pools, affinity, retirement on error; `cuda` feature with device discovery, the device arena, pinned host arena, copy-engine promotion and demotion between `PinnedHost` and `Device`, and the device-memory probe. Gate: S13 (zero-copy crossings measured by the arena counter); S14 on a GPU host for the pinned-to-device path; S1 with torch-score (host and device budgets both respected); S6 with a stateful adversarial variant; embed-score benchmark added to S3.

### Phase 6: report and profiles
Deliverables: run report computed from trace and limits; profile store with warm start and drift override. Gate: S9 in full; S3 measured for all five kernels from the report; a second run of each benchmark shows no probe-phase throughput dip in the trace.

### Phase 7: engine bridges
Deliverables: `amoru-polars` expression plugin wrapper; `amoru-datafusion` ScalarUDF wrapper with `AmoruMemoryPool` (adapters AD-M2) so a bridged DataFusion kernel accounts against the budget; `VortexSource` (sources SO-M1), with the `vortex` crate entering the dependency table in that pull request; the normalise kernel built and run in all three hosts from one crate. Gate: S7; the Vortex source passes the Parquet source's tests over the same generated data.

### Phase 8: learned sizer (post-v1)
Deliverables: `LearnedSizer` with online quantile regression, envelope clamping, error-triggered fallback; trained from accumulated traces; opt-in flag. Gate: S3 improves on at least three of five kernels versus `RuleSizer` with no S1 or S11 regression; fallback exercised by a test that corrupts the model.

### Phase 9: hardware-direct paths (post-v1)
Deliverables: GPUDirect Storage reload of staging segments and aligned tensor files straight to device where `gdscheck` passes, with the pinned-host bounce as fallback; RDMA registration of the arena behind a `rdma` feature and an `RdmaSource` against AIStor's S3 over RDMA as the first peer; `SharedMemorySource` for co-located producers. Gate: S14 in full on a GDS host; an RDMA host test that a 10 GB read completes with worker CPU time below 2% of wall time.

### Phase 10: weight-major execution (separate design document)
Not planned here. D11 records the intent; Q7 asks for the go-ahead to write the document. Preconditions this document guarantees: tensor payloads, a device tier, a disk tier that runs at bandwidth, and a controller that budgets per tier.

Every phase ends with the full gate suite of all prior phases re-run in the container and on the bare host, and no phase may leave a sufficiency criterion that a prior phase closed in a failing state.

---

## 10. Recovery: resume by lineage

The runtime already holds, in normal operation, most of what a checkpoint would need to contain. The placement engine knows every morsel between the source cursor and the sink's last commit, and knows which of them are on disk because it put them there. The source is deterministic for a split and a row range. The kernels are a chain. So the lineage of any morsel is its origin plus the chain, and the only state that needs writing is an index of that lineage, a few numbers, and whatever a sink or a stateful kernel wants to add.

**The manifest.** The placement engine writes `manifest.json` in the run's staging directory, atomically, every `checkpoint.interval_ms` (default 5 s), at every segment roll, and when a run terminates or is cancelled. It lists: the run id, the source plan's digest and the kernel fingerprints (so a manifest cannot be applied to a different job); the committed watermark, the highest sequence number below which the sink has committed everything; the source cursor, where the source drive would issue its next read; for every uncommitted morsel, its origin, its current stage and, if it is on disk, the segment reference; the sink's checkpoint (committed file names and the next index); and the checkpointed state of any kernel that declared it keeps state worth saving. A manifest for a 32 GiB budget of 64 MiB morsels is under 100 KiB. Segments referenced by the current manifest are never deleted until the next manifest no longer references them, so a crash between two writes leaves a consistent pair.

**Resume.** A new process calls `run` with `resume=` naming the run id, the manifest path, or `auto`. Before anything is written: the manifest's identity is checked against the plan and the kernels (a mismatch is refused with the first difference named); the sink is opened in resume mode and removes every output above the watermark (each committed file records its sequence range in its metadata, so this is a listing and a comparison, not a scan); the placement engine rebuilds its queues, putting on-disk morsels back at their stages in sequence order; the scheduler sets its source cursor and sequence counter, restores stateful instances (fresh `init` for kernels that said reinit suffices, `restore` from the checkpoint for kernels that declared it, refusal for kernels that forbid resume), re-reads every listed morsel without a disk copy from its origin and pushes it to the first queue with its original sequence number, and then runs as usual. The controller does not read the manifest; it reads its profile store, which the interrupted run wrote as it learned, so the resumed run starts at the sizes the first run had reached rather than at the probe size.

**What it costs and what it covers.** Normal path: the manifest write, two `fsync`s every few seconds on a file that does not grow with the data, and a small index in memory. Recovery: the sink's file in progress is rewritten, and every morsel that was in memory or in flight is re-read and re-run through every stage up to where it was; nothing that was on disk or committed is redone. What it does not cover: a stateful kernel whose state depends on the morsels it has seen and that neither checkpoints nor forbids; that is declared per kernel and the decorator's documentation names the rule in one sentence. Cross-node resume requires the staging directory to survive the node (a persistent volume, a detachable disk), which a platform declares through `durable_staging`; the runtime cannot probe that, so it treats an undeclared directory as local and says so.

**Why this and not replication.** Spark keeps lineage too, at the granularity of a partition and a DAG, and recomputes on loss; it also offers checkpointing that writes everything. The runtime's version is the same idea at morsel granularity with the placement engine as the lineage store, and it is cheaper for one reason: the placement engine writes to disk anyway, under pressure, in the layout it reads back by DMA, so the recovery record is mostly a description of files that already exist. Replicating morsels to a second node would cost a full copy of the working set over the network for every run, to protect against an event that happens to a small fraction of runs; recomputing costs a bounded re-read only when the event happens.

---

## 11. Extension to many nodes

This section reserves the shape of a later extension so that v1 does not preclude it. It is not a design for that extension; that is its own document, written when there is a job that needs it and a host to test it on. Its purpose here is to state what the extension is, where the boundary lies, and which types exist now because of it.

**The claim.** A job that fits the runtime's model, one source, a chain of kernels, one sink, morsels independent of one another, runs on N nodes as N copies of the single-node runtime that share three things: a manifest that names every node of the run and hands out source splits, a remote memory tier through which a node under pressure lends morsels to a node with room and a node with idle cores pulls work from a node with a backlog, and the lease daemon that makes both safe. Each node keeps its own arena, reactor, placement engine, scheduler and controller; the controller's equations are unchanged because they are about one memory ceiling and one set of cores; the admission rule is unchanged except that a worker prefers the emptiest queue on its own node and, failing that, a queue whose head it can fetch cheaply. A morsel crossing nodes is one RDMA write of an Arrow buffer from arena to registered arena, no serialisation, no driver in the path.

**The boundary.** The runtime does not shuffle. A cross-node join, group-by or sort needs data partitioned by key across nodes and is a query engine's job; the runtime would host such an engine's kernels as it does today, not replace the engine. This is stated so the extension does not drift into being a distributed query engine, which exists and is not the problem being solved. The work that fits, batch scoring and inference, per-row and per-batch transforms, embedding and feature computation, is the work where a distributed engine's costs (task scheduling, serialisation, executor memory management, a driver) are pure overhead, and where N small nodes running this runtime should beat the same N nodes running that engine on throughput per core and, more visibly, on memory used per node.

**Fault tolerance.** Section 10 applies per node: each node's staging directory holds its manifest and segments. The run manifest names the nodes; a node that dies has its disk reattached to a replacement (or the platform's persistent volume is claimed by a new pod) and the replacement resumes that node's share. Morsels the dead node held in memory, including those it had lent to others through the remote tier, are marked lost by the lease daemon and are recomputable like any other; the replacement re-reads them. The design does not replicate.

**What v1 carries for it.** In `01-contracts.md`: `NodeId` (with `LOCAL_NODE` the only v1 value), `RunId`, `Tier::Remote(NodeId, RemoteRef)` with `RemoteRef` holding the address, memory key and length an RDMA read needs, `Origin.node`, `Locality` on `Placement::pop`, `TIER_COUNT` sized for five tiers, and `AmoruError::Unsupported`. In the placement engine: a reserved `OnRemote` entry state and reserved rows in the move table. In the reactor: reserved rows in the copy dispatch table and the statement that the arena's single reservation exists so that registering it with a NIC is one call. In every crate: a lint that fails on a wildcard arm over a tier (CT-T14), so that the day the `rdma` feature is written, the compiler lists every place that needs an arm. In the preamble: E11, which forbids any v1 agent from implementing the reserved paths, and G-I11, which states the aim.

**What is deliberately not reserved.** Peer discovery, the lease protocol, membership changes mid-run, the coordinator's split assignment policy, the wire format of anything: these belong to the extension's document and none of them constrains a v1 signature.

---

## 12. Open questions for Brackly

**Q1. Name.** Decided 2026-09-15: Amoru (Adaptive MOrsel RUntime). Crates are `amoru-kernel`, `amoru-runtime`, `amoru-py`, `amoru-polars`, `amoru-datafusion`; the Python package is `amoru`. "Morsel" remains the unit of work. Remaining checks before publishing: PyPI, GitHub namespace, trademark, domain.

**Q2. Ordering default.** This document makes output unordered by default with an explicit flag for ordered sinks. If the dominant use is writing back to a lakehouse table where row order carries no meaning, unordered is right. If a meaningful share of jobs will produce sequence-dependent output, ordered-by-default with a bounded buffer is the safer surprise.

**Q3. Error policy default.** Terminate on first kernel error is the safe default and the annoying one for long scoring jobs where one malformed row should not cost six hours. `skip` with a per-run error budget (terminate after N failures) is the alternative; the document leaves terminate as default and asks whether the budgeted form should be the default instead.

**Q4. Linear chains only in v1.** A DAG (one source feeding two kernel chains and two sinks) is a real pattern in scoring pipelines that write both predictions and a rejects file. The document defers it; confirm this is acceptable for v1 or it becomes a Phase 1 requirement with ordering and accounting consequences.

**Q5. Profile store location and sharing.** Per-user local profiles are the default. On a platform where the same kernel runs for many tenants, a shared profile store keyed by fingerprint would give every tenant a warm start from the first run. That sharing is a platform decision with a data-leakage angle (profiles contain size statistics, not data) and is not decided here.

**Q6. Databricks budget discovery.** The document expects an explicit budget on Databricks single-node clusters. If a reliable way to read the driver's intended memory from the cluster's environment exists, discovery should use it; confirm whether the Spark configuration exposed to the driver process is trustworthy for this purpose.

**Q7. Weight-major execution.** D11 defers it to its own design document. It is the feature that would let a small GPU run batch inference for a model that does not fit it, at the cost of a controller that budgets activations across tiers and a model expressed as a chain of stages. Confirm whether that document should be written now, in parallel with v1, or after Phase 5 proves the tensor and device paths.

**Q8. Hardware baseline for the direct paths.** The direct-IO, pinned-memory and GPUDirect Storage paths each need a host to be verified on (2.2). Name the reference GPU host and the reference NVMe host the gates in Phases 3, 5 and 9 will run against; without them those gates cannot close and the fallbacks are the only tested paths.

**Q9. Checkpoint cadence and disk.** The manifest write is cheap, but two `fsync`s every 5 s on a slow or shared disk are not free, and `checkpoint.interval_ms` is the trade between them and the size of the replay after a crash. Confirm 5 s as the default, and whether the Griot Cloud pod profile should set the staging directory to a persistent volume claim (declared `durable_staging=present`) by default, which is what makes cross-node resume work there.

**Q10. When to write the multi-node design.** Section 11 reserves the seams and nothing else. The extension needs a job that does not fit one node, an RDMA-capable pair of hosts to test on, and a decision on whether the coordinator is a library role (the first node) or a separate small process. Confirm that this waits until after v1 ships on the reference host, or name the job that brings it forward.
