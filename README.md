# Morsel

A single-node, adaptive batch runtime for out-of-core workloads: tables and tensors larger than the memory budget, transformed by functions a query engine cannot express, at close to the budget's capacity, with no tuning by the user.

Morsel is being designed in the open before it is built. The design documents in [`architecture/`](architecture/) are the product at this stage; code follows them.

## The problem

You have a dataset in object storage or on local disk that is larger than the memory of the process that must transform it. The transformation is a Python function, a model, a tokenizer, a domain rule set: something that decides during the computation, not a SQL expression. The process has a fixed memory budget (a pod's cgroup limit, a single-node cluster's driver, a laptop) and a fixed CPU allocation. Today you hand-write a loop over row groups with a guessed batch size and a guessed worker count, and either the process is killed or the machine runs at a fraction of what was paid for.

Morsel replaces that loop. It reads the host's limits, looks ahead at what the source is about to deliver, learns how the transformation amplifies its input, and adjusts batch size, worker count, read-ahead and staging continuously, so the pass completes inside the budget without anyone choosing a number.

## What it is

- A Rust core: a role-free worker pool, byte-bounded queues that place morsels across device memory, pinned host memory, host memory and local disk by DMA rather than by the CPU, a single resource controller, and a per-morsel trace that is both the run report and the controller's training data.
- Arrow record batches and DLPack tensors as the only two payload layouts, so a kernel written for Morsel is also a Polars expression plugin or a DataFusion function, and a Python function receiving a `pyarrow.RecordBatch` or a `torch` tensor is a kernel without copies in either direction.
- Python bindings through PyO3, with full parallelism for Python kernels on free-threaded CPython.

## What it is not

Not a distributed engine, not a query planner, not a streaming system with per-record latency, and not a replacement for DuckDB, Polars or DataFusion: it hosts them as kernels rather than competing with them.

## Status

Design. See [`architecture/README.md`](architecture/README.md) for the document index and the order in which components are being designed and built.

## Licence

Apache License 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
