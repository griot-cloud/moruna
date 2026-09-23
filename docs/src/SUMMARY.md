# Summary

[Moruna](README.md)

# Introduction

- [What Moruna is](introduction/what-moruna-is.md)
- [What Moruna is not](introduction/what-it-is-not.md)
- [How a pass runs](introduction/how-a-pass-runs.md)

# User guide

- [Install](guide/install.md)
- [First run with moruna.run](guide/first-run.md)
- [Sources](guide/sources.md)
  - [Parquet, local and object store](guide/sources-parquet.md)
  - [Tensor files: safetensors and NumPy](guide/sources-tensor.md)
  - [A Python iterator as a source](guide/sources-iterator.md)
- [Sinks](guide/sinks.md)
  - [Parquet sink](guide/sinks-parquet.md)
  - [Tensor sink](guide/sinks-tensor.md)
  - [Arrow IPC sink](guide/sinks-arrow-ipc.md)
- [Ordering and error policies](guide/ordering-and-errors.md)
- [Budgets and the environment variables](guide/budgets.md)
- [Reading the run report](guide/run-report.md)
- [Reading the trace](guide/trace.md)
- [Cancellation and resume](guide/cancellation-and-resume.md)

# API reference

- [API reference](api/README.md)
  - [Python API](api/python.md)
  - [Rust API: moruna-kernel](api/rust.md)
  - [Configuration table](api/configuration.md)

# Kernel author guide

- [Kernel author guide](kernels/README.md)
  - [A Rust kernel against moruna-kernel](kernels/rust-kernel.md)
  - [The same kernel as a Polars plugin](kernels/polars.md)
  - [The same kernel as a DataFusion function](kernels/datafusion.md)
  - [A Python kernel](kernels/python-kernel.md)
  - [The GIL and free threading](kernels/gil-and-free-threading.md)
  - [Stateful kernels, instances and ResumePolicy](kernels/stateful-kernels.md)
  - [Hints and footprint](kernels/hints-and-footprint.md)
  - [The zero-copy rules and what breaks them](kernels/zero-copy.md)

# Operator and hosting guide

- [Operator and hosting guide](hosting/README.md)
  - [A laptop or bare VM](hosting/laptop.md)
  - [A cgroup v2 container or Kubernetes pod](hosting/container.md)
  - [A single-node Databricks cluster](hosting/databricks.md)
  - [A Griot Cloud pod profile](hosting/griot-cloud.md)
  - [A GPU host](hosting/gpu.md)
  - [Sizing the staging disk](hosting/staging-disk.md)
  - [The profiles directory](hosting/profiles.md)
  - [Diagnosing a Budget termination](hosting/budget-termination.md)
  - [Direct paths and how the report names the path taken](hosting/direct-paths.md)

# Architecture and internals

- [Architecture and internals](internals/README.md)
  - [The component map and the waves](internals/component-map.md)
  - [How a morsel moves](internals/morsel-lifecycle.md)
  - [How the controller decides](internals/controller.md)
  - [How to read an SDD and where the tests live](internals/reading-an-sdd.md)
  - [Escalations and the agent prompts](internals/escalations-and-agents.md)
  - [Benchmark methodology](internals/benchmarks.md)

# Tutorials

- [Tutorials](tutorials/README.md)
  - [Score a Parquet table with a NumPy kernel](tutorials/score-parquet-with-numpy.md)
  - [Embed a text column to a tensor](tutorials/embed-text-to-a-tensor.md)
  - [Kill a run and resume it](tutorials/kill-and-resume.md)
  - [Run inside a container with a budget](tutorials/container-with-a-budget.md)
