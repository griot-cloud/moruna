# Moruna waves A, B and C: the brief for Codex

Brackly, 2026-10-27. Three waves of Moruna work, in order. Every item is built in this repository, in Moruna's own terms, and released here. Griot pins the release; Griot's PM proves the work afterwards against this brief.

Read before starting: `CONTRIBUTING.md` (its rules hold for every item), `architecture/sdd/00-preamble.md`, `architecture/sdd/01-contracts.md`, and the SDD of each component an item touches. `DECISIONS.md` records what is decided; this brief decides four open things there (section 2).

## 1. Laws that bind every item

1. **Moruna knows nothing of Griot.** No Griot name, field, URL or concept enters Moruna. Anything Griot needs is a general capability, described in Moruna's terms, that any embedder could use.

2. **The adapter pattern, everywhere.** Every technology Moruna touches sits behind a trait, and each technology is one adapter of it. This matters most for **what Moruna can run**, because that list will keep growing: containers, VMs, API calls, binaries, Rust, and whatever comes next.
   - **The interface is a port.** Some already exist: `Source`, `Sink` and `Kernel` (`moruna-kernel`), `KernelLoader` (`moruna-runtime/src/job/build.rs`), `Placement`, `Reactor`, `Allocator`.
   - **Each kind is one adapter.** It lives in its own module or crate, registers itself in a registry, and is the only code that knows its technology.
   - **One conformance suite per port** says what "correct" means for every adapter of that port. A new adapter is accepted when it passes the suite. The suite is written once, before the second adapter.
   - **Adding a kind touches no existing code.** Not the scheduler, not the job builder, not the other adapters. If an item would add an arm to a `match` over kinds anywhere outside the registry, the design is wrong; stop and fix the design.
   - **Closed enums at the boundary go.** Today the job document names kinds through fixed enums: `SourceDoc`, `SinkDoc` and `KernelKindDoc` (`moruna-runtime/src/job/mod.rs`), even though `Source`, `Sink` and `Kernel` are traits underneath. A document names a kind; the registry resolves it to an adapter, or refuses with the kinds it knows.

3. **Formats.** Batches are Arrow. The job spec (wave C) and Moruna's log and event records (wave B) are **FlatBuffers**: read in place, schema evolution across releases, a verifier for untrusted bytes. JSON stays only as a rendering for people (a `moruna run` file, a debug print), converted at the edge.

4. **This repository's rules:**
   - behaviour is specified before it is written, in the SDD, in the same pull request;
   - 90% line coverage per crate;
   - no stubs;
   - no `unwrap` or `expect` outside tests;
   - tests named after the SDD's test ids;
   - tests prove properties, not machines.

   One pull request per component; a release is a version bump, with its CHANGELOG section.

5. **The cost model is empirical** (wave C, but it shapes wave B's declarations). Costs are measured, not guessed. Every kind is profiled on synthetic Arrow batches generated from its input schema, at growing sizes (1k → 10k → 100k → 1M rows), recording time, peak memory, output size and selectivity. That happens when a function or kind is checked, which already runs it (`moruna check`). Relational work is costed by DataFusion. Profiles live in the existing profile store (`profiles_dir`), and every real run refines them. Where a table's statistics exist (min/max, null counts, distinct counts), generated batches follow them; value-dependent behaviour (selectivity, skew) is corrected by real runs.

## 2. Decisions this brief records (add them to `DECISIONS.md`)

- **Q4** (linear chains only, or DAGs): **DAGs**, decided by Brackly 2026-10-27. The job spec v2 (MC2) is a DAG; v1's linear chain stays readable as a DAG with one path.
- **The adapter pattern** (law 2), as a decision with its own id.
- **FlatBuffers** for the job spec and for log and event records (law 3).
- **The empirical cost model** (law 5).

## 3. Wave A: blocking. Do first; release as 0.5.7.

### MA1. A source with zero rows never finishes

Reproduction, found by Codex: `SELECT 42 AS answer WHERE false` through a `datafusion` source hangs until cancelled.

Cause, read from `crates/moruna-scheduler/src/source_drive.rs` at `6b0ec41`:
- `close_when_exhausted` (line 508) closes the source queue only when the shared cursor's `split_index` has reached `plan.len()`.
- `peek_next_range` (line 120) steps over a split whose `rows` is 0, but only on a local copy of the cursor; it never advances the shared cursor.
- No range is ever taken from a zero-row split, so nothing else advances it either.

So any plan containing a zero-row split, an empty one included, leaves the cursor short of the end, and the queue never closes. A zero-row split in the middle of a plan would stall the same way, since the cursor cannot pass it.

Do:
1. A test that fails today: a plan of one zero-row split, and a plan with a zero-row split between non-empty ones. Each must finish with exactly the rows of the non-empty splits.
2. The fix: the shared cursor moves past zero-row splits (or exhaustion is decided by whether any range remains), so `close_when_exhausted` sees the end.
3. A property test: any plan of splits with any row counts, zeros included, finishes, and the sink receives exactly the sum of the splits' rows.

### MA2. Every task gets a terminal event

Codex found that a cancelled run can leave tasks with no terminal fact. An embedder then shows them as incomplete forever, and must never invent the missing end.

The rule: every task that started gets exactly one terminal event (completed, failed with its cause, or cancelled), whatever ends the run (cancel, failure, a kernel error under each error policy, shutdown on drop).

Where: the reverse shutdown in `crates/moruna-runtime/src/cancel.rs`, the run's events (event v3, released in 0.5.6), the report (`report.rs`).

Tests:
- cancel at many points of a run, as a property, and assert one terminal event per started task;
- the same for failure, and for drop without cancel.

### Release

0.5.7 carries MA1 and MA2, with its CHANGELOG section.

## 4. Wave B: not blocking; fits the current design

### MB1. Keep each stage's output when asked

Today a run keeps only what reaches the sink, plus captured stdout and stderr. An embedder wants each stage's actual rows: Griot's run history shows every step's output, not only the last.

Add a per-stage `retain` setting in the job document. A retained stage's output batches are written as Arrow IPC into the run's output store (the directory of MH 4.3, `job/mod.rs:841`). They are bounded by a size cap that the document states, and readable after the run.

This is the linear-chain form of MC2's per-edge `retain`; build it so MC2 widens it rather than replacing it.

### MB2. Log and event records as FlatBuffers

Today `crates/moruna-runtime/src/host/logs.rs` turns attributed output into records. Its rules stay exactly as they are:
- whole lines;
- `partial`, `continued` and `truncated` lines;
- a Python traceback kept as one record;
- per-producer rate limits;
- drops counted.

The change: records are written as FlatBuffers, in a schema of Moruna's own (`architecture/` gets the schema), and the run's events use the same treatment. A consumer reads a record in place. The schema is Moruna's: no field names any embedder.

### MB3. New kinds of runnable: containers, VMs, API calls, binaries

Brackly and Codex own the scope. The shape is fixed by law 2 and MC1. Build each as **an adapter of one runnable-kind port**, with a **declaration**:
- its name (`container`, `vm`, `http`, `binary`, …);
- its settings, as a schema (FlatBuffers or JSON Schema; pick one, and MC1 uses the same);
- its inputs and outputs: Arrow batches in and out, files, or nothing;
- its needs: network, the egress it requires, a GPU, a model port;
- whether it is deterministic, and whether it preserves columns.

If MC1 is not built yet when MB3 starts, build MC1's registry and declaration first (section 5), then the kinds on it. Do not add kinds to `KernelKindDoc`.

Each kind states its isolation in its SDD:
- a binary runs confined like a `.pex` (`moruna-sandbox`: `confine.rs`, `seccomp.rs`, cgroup);
- an API call goes out only through a named port (`call(port, bytes)`, as `.wasm` does in `moruna-wasm/src/calls.rs`), never by opening sockets itself;
- a container or a VM states what runs it inside the guest, and what that adds to the guest image.

Each kind has its own SDD section and its own adapter tests, and all of them pass the runnable-kind conformance suite.

### MB4. Rust kernels

The `rust` kernel kind is reserved and refused (`KernelKindDoc::Rust`, `krate`, `symbol`). Build it as an adapter of the same port, isolated like the other packaged kernels (decision FP1).

### MB5. Port calls from `.pex` and `moruna-py`: HELD

Do not start. Today `call(port, bytes)` exists for `.wasm` only. A `.pex` child talks over a pipe with control frames of at most 1 MiB, with large payloads in shared memory, and a waiting stateful `.pex` needs the rule that a checkpoint takes its state as of the batch's start. This is held for Brackly with the "models: true" function flag.

### MB6. The rest of the run-events audit

Griot's audit of what a run reports lists P0 and P1 gaps: per-task lifecycle and correlation, attributed live logs, progress, memory pressure, location. It was written against Moruna 0.5.1, and event v3 (0.5.6) closed part of it. The audit is `docs-site/web/content/design/job-execution-observability.md` in the Griot repository. Brackly can give you a copy; describe the gaps in Moruna's terms, never Griot's.

Read it against 0.5.6, list what is still open in this repository's BOARD.md, then close each with the event, its test, and the SDD change.

## 5. Wave C: the direction

Build in order: MC1, then MC2, then MC3. MC4 starts only when Brackly says so.

### MC1. Moruna declares what it can run

One registry of runnable kinds. Each entry is an adapter (law 2) with a declaration (MB3's fields). Sources and sinks become kinds too: Parquet, Vortex, tensor, Arrow IPC, `datafusion`, and the `peql` sink are adapters of their ports, registered the same way, and `SourceDoc`/`SinkDoc` stop being closed enums.

**Deliverables:**
- the registry;
- the declaration schema;
- a `moruna kinds` listing (each declaration, machine-readable), which an embedder reads to know what can run and how to configure it;
- the conformance suites: one for sources, one for sinks, one for runnable kinds;
- migration of every existing kind onto the registry.

An embedder draws its configuration screens from the declarations: Griot's Canvas will. So a new kind becomes usable without embedder code.

### MC2. Job spec v2: a DAG, in FlatBuffers

`moruna_spec: 2`. This decides Q4.

**Nodes.** Each node is a kind from MC1, with settings checked against the kind's declaration. It also states facts the scheduler needs:
- stateful or not (an aggregate or a join is a barrier);
- its partitioning (so two branches with the same partitioning can be interleaved);
- whether its state can be spilled;
- whether output order matters.

**Edges.** Named output port → named input port. One output may feed several nodes; a node may take several inputs. Each edge carries Arrow batches and has a `retain` flag (MB1, widened).

**Cost hints.** Optional per node (time per row, selectivity, scratch memory). Measured profiles override them.

**The job.** As today: budget, staging, object store, checkpoint, error policy, contracts, report.

**Rules:**
- The JSON form is a rendering of the FlatBuffers schema, for people.
- v1 documents are read as a one-path DAG.
- A job with one node is valid: an embedder testing one task submits exactly that.
- The canonical form and its digest are defined, so an embedder stores a saved job as its spec, by digest.

### MC3. A planner: logical DAG → optimised physical DAG

**Input:** MC2's logical form, or Substrait for relational regions.
**Output:** a physical DAG (FlatBuffers) that the scheduler runs.

**Relational regions** (SQL, standard kernels) become DataFusion logical plans; this repository already embeds DataFusion (`moruna-datafusion`). DataFusion optimises them:
- filter and projection pushdown into Parquet, Vortex and table sources;
- filter reordering and constant folding;
- join strategy and partition alignment.

**Black-box nodes** (functions, containers, API calls, binaries) become extension nodes, optimised through what their declaration states: columns read and written, deterministic, column-preserving.

**Across the whole DAG:**
- a subexpression shared by two branches is computed once;
- adjacent stateless steps are fused, with no batch materialised between them;
- materialisation is chosen.

**Enforcement points are barriers.** A node can declare itself an enforcement point: the `peql` contract boundary, masking. No filter or projection on a column it governs moves across it, because a filter evaluated before masking leaks the masked value. This is a general rule of the planner, not an embedder's.

**`moruna explain`** returns the optimised plan and its estimated cost (from the profiles) without running it. Plans are cached by the logical DAG's digest.

### MC4. A residency-aware adaptive scheduler (when Brackly says)

The research proposal is in this branch: `architecture/research/residency-aware-adaptive-scheduling.md`. It builds on what `moruna-scheduler` already does:
- admission when a stage's input is resident (`pick.rs`);
- the disk tier (`staging`);
- the budget;
- the morsel sizer;
- `profiles_dir`.

A DAG (MC2) is what gives it choices between tasks. Its phases:
1. a simulator on MC2 specs;
2. a user-space prototype: buffer catalog, admission scored by immediate cost plus cost-to-go with bounded lookahead, hard memory reservations, spill/prefetch/release, workers following the bottleneck;
3. cost calibration (law 5);
4. evaluation against FIFO, pipeline-first and greedy baselines;
5. kernel help only if measured.

## 6. How the work is handed back

- One pull request per component, against `main`, with the SDD changes in it. Item ids (MA1 … MC3) go in the titles.
- This repository's `BOARD.md` gets a row per item, with its status.
- For each item, the PR says:
  - which SDD sections changed;
  - which tests prove it (by id);
  - the coverage of each crate it touched;
  - anything this brief got wrong.
- Releases: 0.5.7 for wave A. Later waves release when Brackly asks.
- Griot's PM then proves each release from the Griot side: the pin, the guest image, conformance, the e2e. The PM reports what fails, and fixes on the Griot side what is Griot's.
