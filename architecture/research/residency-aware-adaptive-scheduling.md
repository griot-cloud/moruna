# Residency-Aware Adaptive Scheduling for Memory-Constrained Analytical DAGs

**Status:** Research proposal / conceptual design (22 October 2026)

## Abstract
We propose an Arrow-native analytical runtime that coordinates task scheduling, data residency across RAM and disk, and dynamically emerging concurrency. Given a fixed computational DAG, finite RAM, available disk capacity, and a worker pool, the runtime selects which eligible function–batch executions to admit based on estimated immediate cost and remaining cost-to-go. The goal is to reduce end-to-end wall-clock time while enforcing memory and execution dependencies. The central research question is whether state-aware scheduling and bounded lookahead outperform conventional FIFO, pipeline-first, and residency-unaware scheduling.

## 1. Motivation
Analytical systems routinely process datasets larger than physical RAM. Conventional pipelines stream batches efficiently, but their scheduling decisions may not fully account for shared input reuse, expensive evictions and reloads, or alternate eligible tasks that could exploit resident data. A fixed DAG specifies dependencies, not necessarily a unique execution order. When multiple function–batch tasks are eligible, selecting one changes the resident working set and the cost of future tasks.

The initial motivating example is a Parquet dataset larger than available RAM, processed through Arrow-compatible filtering and aggregation. A linear pipeline may already be near-optimal under conventional streaming; more discriminating test cases involve branching DAGs, shared inputs, converging branches, and intermediate results with multiple consumers.

## 2. Scope and assumptions
- A logical DAG of functions is supplied and remains fixed during the first research phase.
- The primary execution interface consumes and emits Arrow record batches or equivalent Arrow-native data structures. A source function may read Parquet and produce Arrow batches.
- Tasks are instances of functions applied to particular batches or partitions. Stateful operators (e.g., aggregation and joins) must expose dependencies and state ownership explicitly; Arrow-in/Arrow-out alone does not make them independent.
- There is a finite RAM budget, finite CPU worker capacity, and a disk tier for source data and spillable intermediates.
- Resource and execution cost estimates are assumed available initially; learning them is a later research problem.
- Decisions must preserve correctness, deterministic dependencies where required, and operator semantics.
- The initial prototype operates in user space; kernel customization is not a prerequisite.

## 3. Core abstraction
The runtime manages *data availability*, not merely memory allocation. Each Arrow buffer or batch has a lifecycle: produced or read, resident, potentially shared/pinned by consumers, optionally spilled, reloaded, and eventually released. The runtime maintains a catalog of buffer identity, size, location, remaining consumers, reload cost, and ownership. It coordinates this catalog with a dependency-aware scheduler.

This is not literally infinite RAM: disk reads, Arrow decoding, transfer bandwidth, and memory allocation have nonzero latency. Prefetching and scheduling can overlap these costs but cannot guarantee zero-delay access.

## 4. Scheduling state and actions
At decision time t, define state S_t as:
- Ready function–batch tasks and their DAG dependencies;
- Buffer residency, location, size, reference counts, and estimated next uses;
- RAM currently occupied, reserved, and reclaimable;
- Available workers/CPU capacity and running tasks;
- Storage bandwidth, outstanding I/O, and relevant contention;
- Predicted execution time, peak scratch memory, and output sizes for candidate tasks.

An action may admit a ready task, reserve its memory, initiate a prefetch, spill an eligible buffer, release an unneeded buffer, or defer a task. Worker distribution across functions emerges from admission decisions and is not statically pinned.

## 5. Cost model
For a candidate action a in state S, estimate marginal cost from:
1. CPU execution time and any fixed task overhead;
2. Missing input bytes, read bandwidth, and decode/transfer overhead;
3. Output bytes and peak transient/scratch memory;
4. Spill bytes, write bandwidth, and later reload penalties;
5. Contention from concurrent tasks for CPU, memory bandwidth, and storage;
6. Changes in memory pressure and the opportunity cost of evicting reusable data;
7. Critical-path effects and downstream tasks unlocked by completion.

A conceptual one-step score is

    score(S,a) = estimated incremental completion-time impact
                 + lambda * resource-pressure risk
                 + estimated future residency penalty.

Memory feasibility is a hard constraint, not merely a soft penalty: reserved plus peak live memory must stay within the budget. A task's wall time cannot simply be summed with other tasks when operations overlap; predictions must account for concurrent execution and critical-path effects.

For lookahead depth h, define an approximate cost-to-go:

    V_h(S) = min_{a feasible in S} [ c(S,a) + V_{h-1}(transition(S,a)) ]

with a terminal heuristic at depth zero. Here c must represent incremental elapsed time or another consistent objective under a concurrency-aware state transition. This is a research formulation rather than a proven optimal algorithm. A practical scheduler may use beam search, short-horizon simulation, or a simpler heuristic.

## 6. Adaptive concurrency
A global worker pool executes admitted function–batch tasks. The scheduler chooses tasks based on dependencies, residency, predicted resource requirements, and contention. More workers can be assigned to slow I/O stages when useful, then reallocated as the bottleneck moves. More I/O workers do not automatically increase throughput: storage saturation, memory reservations, and excessive prefetching can make added concurrency harmful.

The runtime should reserve peak memory before admitting work, provide backpressure, and account for long-running stateful operators. CPU and I/O can be modeled as separate resource demands even if a common scheduler coordinates both.

## 7. Illustrative workloads
**Linear pipeline:** Parquet scan -> filter -> aggregate. Tests whether adaptive prefetching and worker allocation improve a well-streamed baseline.

**Shared-input branching:** One resident batch feeds several independent functions. Scheduling all eligible consumers before eviction can reduce redundant reads.

**Selective retention:** A filter produces a small subset; the runtime compares retaining a zero-copy view (which may pin a large parent allocation) against copying into compact buffers.

**Converging partitions:** Two branches feed a partition-compatible join. Interleaving matched partitions may lower peak intermediate residency, subject to join semantics.

**Spill-intensive aggregation:** The runtime chooses between continuing production and freeing space by consuming or spilling intermediate state.

## 8. Research hypotheses
H1: Residency-aware task admission reduces disk traffic and wall-clock time on DAGs with substantial shared data reuse.

H2: Short bounded lookahead improves on greedy marginal-cost selection when immediate cheap tasks cause expensive future evictions or critical-path delays.

H3: Dynamic concurrency allocation improves throughput over fixed per-stage worker allocations under changing I/O and CPU bottlenecks.

H4: Benefits are workload-dependent; linear streaming pipelines may show little improvement and scheduler overhead can outweigh gains for small batches.

## 9. Experimental design
Compare against: (A) FIFO ready-task scheduling; (B) pipeline-first batch streaming with fixed concurrency; (C) residency-aware greedy scheduling; (D) residency-aware bounded-lookahead scheduling. Hold the DAG, hardware, memory budget, and operator implementations constant.

Vary RAM-to-data ratio, batch size, DAG branching factor, consumer reuse, CPU-to-I/O cost ratio, skew, output selectivity, and worker count. Report end-to-end wall-clock time, CPU utilization, storage read/write bytes, spill/reload bytes, peak and reserved RAM, worker idle time, task queue delay, scheduler overhead, and correctness.

Perform ablations for residency signals, lookahead, contention estimation, and dynamic worker allocation. Use synthetic DAGs with known optimal small cases plus representative Parquet/Arrow analytical queries.

## 10. Proposed architecture
1. **DAG registry:** Tracks functions, batch-level dependencies, readiness, and stateful barriers.
2. **Arrow buffer catalog:** Tracks ownership, residency, location, size, liveness, and consumers.
3. **Resource estimator:** Predicts execution duration, output size, scratch peak, and I/O demands.
4. **Admission and scheduling engine:** Scores feasible task/actions and controls the worker pool.
5. **Memory and spill manager:** Reserves capacity, prefetches, spills, reloads, and releases buffers.
6. **Execution workers:** Execute Arrow-compatible operators and emit runtime telemetry.
7. **Telemetry and model updater:** Measures actual costs and recalibrates predictions.

## 11. Key technical risks and open questions
- **Cost objective:** How should the scheduler estimate *incremental makespan* under overlap, rather than incorrectly adding concurrent task durations?
- **Granularity:** How small can batches become before scheduling and Arrow allocation overhead dominate?
- **Stateful operators:** How are joins, aggregates, ordering, and partition barriers represented safely?
- **Spillability:** Which buffers and operator states can be persisted and reconstructed efficiently?
- **Residency value:** How should the runtime quantify reuse likelihood, time until next use, and eviction opportunity cost?
- **Prediction error:** How robust is bounded lookahead to inaccurate memory and duration estimates?
- **Fairness:** How can critical-path priority avoid starving other ready branches?
- **OS interaction:** How do page cache, memory mapping, allocator behavior, and asynchronous I/O affect observable residency?

## 12. Phased research plan
**Phase 1: Simulator:** Model small DAGs, explicit buffer lifetimes, hard memory admission, disk transfer cost, and finite worker capacity. Establish correctness and compare greedy vs bounded lookahead.

**Phase 2: User-space prototype:** Build a minimal Arrow-batch runtime with a buffer catalog, spill/prefetch hooks, ready-task queue, and dynamically allocated worker pool.

**Phase 3: Cost calibration:** Profile Arrow functions across batch shapes and sizes; update duration, scratch memory, output cardinality, and contention models online.

**Phase 4: Evaluation:** Run controlled workload matrices, report performance and overhead, and identify regimes where scheduling materially helps.

**Phase 5: Advanced mechanisms (conditional):** Investigate kernel-level assistance or physical-plan adaptations only if measured bottlenecks justify them.

## 13. Precise research problem
Given a fixed DAG G of Arrow-batch task instances, a finite memory budget M, a worker budget W, a set of disk-resident and resident buffers, and estimated state-dependent task and data-movement costs, construct an online, dependency-correct schedule and residency policy that minimizes total completion time while respecting hard memory and resource constraints. Evaluate whether bounded lookahead and adaptive task admission provide measurable improvements over streaming and greedy baselines, and characterize the workload regimes in which they do.

## 14. What is not claimed
This proposal does not claim that databases lack cost models, that operating systems lack caching or prefetching, or that joint scheduling is inherently novel. Novelty, if any, must be demonstrated through a clear comparison with existing dataflow, cache-aware, out-of-core, and adaptive execution systems. The immediate contribution is a testable problem formulation and prototype plan.
