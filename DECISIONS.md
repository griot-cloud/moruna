# Decisions

The open questions the architecture document puts to its owner (`architecture/amoru-runtime-design.md`, section 12), with their status. The PM agent keeps this file current: when a question is decided, the status becomes `decided <date>: <one line>`, the date column is set, and the document or SDD that carries the decision is cited. An escalation item in the preamble (`architecture/sdd/00-preamble.md`, section 7) that traces to one of these questions takes its assumption until the row here says otherwise. Every decision is one line here and as many as it needs in the document it changes; this file is an index, not the argument.

| Id | Question | Owner | Status | Date |
|---|---|---|---|---|
| Q1 | Name of the runtime, its crates and its Python package | Brackly | decided 2026-09-15: Amoru (Adaptive MOrsel RUntime); crates `amoru-kernel`, `amoru-runtime`, `amoru-py`, `amoru-polars`, `amoru-datafusion`; package `amoru`; namespace and trademark checks still to run | 2026-09-15 |
| Q2 | Ordering default: unordered with an opt-in flag, or ordered by default with a bounded buffer (preamble E3) | Brackly | open; assumption: unordered, `ordering.required` opt-in per sink | |
| Q3 | Error policy default: terminate on first kernel error, or skip with a per-run error budget (preamble E4) | Brackly | open; assumption: `terminate` | |
| Q4 | Linear chains only in v1, or DAG support as a Phase 1 requirement (preamble E5) | Brackly | open; assumption: linear chain only | |
| Q5 | Profile store location and sharing across tenants (preamble E6) | Brackly | open; assumption: per-user local directory | |
| Q6 | Databricks budget discovery: explicit budget, or read the driver's intended memory from the cluster environment (preamble E7) | Brackly | open; assumption: explicit budget required, discovery refuses to run on Databricks without one | |
| Q7 | Weight-major execution: write its design document now, in parallel with v1, or after Phase 5 (preamble E8) | Brackly | open; assumption: out of scope for v1, nothing precludes it | |
| Q8 | Reference GPU host and reference NVMe host for the direct-path gates (preamble E1) | Brackly | decided 2026-09-22: the Griot bare-metal server in Nairobi is the reference host; no GPU host, so device and GDS tests are skipped and listed by id | |
| Q9 | Checkpoint cadence (5 s default) and whether the Griot Cloud pod profile sets the staging directory to a persistent volume claim declared `durable_staging=present` | Brackly | open; assumption: 5000 ms, `durable_staging` treated `Absent` unless declared | |
| Q10 | When to write the multi-node design: after v1 ships on the reference host, or brought forward by a named job (preamble E11) | Brackly | open; assumption: after v1; every reserved path returns `Unsupported("rdma")` | |
