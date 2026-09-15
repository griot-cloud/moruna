# Architecture

Design documents for Morsel. Code is written from these documents; a change in behaviour is a change here first.

## Documents

| Document | Type | Status |
|---|---|---|
| [morsel-runtime-design.md](morsel-runtime-design.md) | Architecture design: the runtime as a system, its problems, sufficiency criteria, decisions and component map | Draft, revision 2 |
| sdd/00-preamble.md | SDD preamble: shared vocabulary, global invariants, process and concurrency model, configuration, crate layout, escalations, traceability | Pending |
| sdd/01-contracts.md to sdd/12-python-surface.md | One SDD per component, each following the same schema, in build order | Pending |

## Document types

An **architecture design document** decides a system: the problems it exists to solve, the context that constrains it, what would make a solution sufficient, the architecture and its rejected alternatives, failure modes, risks, and an implementation plan whose gates cite the sufficiency criteria.

A **software design document (SDD)** decides one component so completely that a coding agent implements it from the document alone. Every constant has a default and a reason, every format is byte-exact, every failure has a required outcome, every invariant has a test. An agent is handed the preamble, the contracts SDD, and its own component's SDD.

## Components, in build order

Each component is one SDD. Its dependencies are listed; a component can be handed to an agent once its own interface and the interfaces it consumes are frozen in the contracts crate.

1. Contracts crate (`morsel-kernel`): types, traits, interfaces, errors, trace schema. Depends on nothing internal. Designed and frozen first.
2. Memory arena. Depends on 1.
3. Resource discovery and host profile. Depends on 1.
4. Trace writer and run report. Depends on 1.
5. Kernel adapters: Python, Polars, DataFusion. Depends on 1.
6. IO reactor. Depends on 2, 3.
7. Sources: Parquet, tensor. Depends on 1, 2, 6.
8. Sinks: Parquet, tensor, Arrow IPC. Depends on 1, 2, 6.
9. Placement engine (tiered queue). Depends on 1, 2, 3, 6. Load-bearing.
10. Scheduler. Depends on 1, 9.
11. Resource controller. Depends on 3, 4, 9, 10.
12. Python surface. Depends on all.

Test infrastructure (benchmark generators, fakes for every contract) is build machinery and lives in the preamble, not in a component document.

## Conventions

No em dashes. Prose between tables. Every current-state claim names where it was verified or the command that would verify it. Every number has a name, a default, a range and an owner.
