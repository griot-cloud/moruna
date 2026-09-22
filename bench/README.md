# Amoru benchmark suite

Placeholder created in wave 0 (F0.1). This directory is owned by the `bench`
agent (preamble 6.5) and is filled in two parts:

- Wave 1: the data generator (Parquet with a controllable row count, column mix,
  null ratio and row-group size, to local disk and to an S3-compatible store;
  safetensors and aligned binary tensors of controllable shape) and the kernels
  (identity, normalise, tokenise-explode, adversarial, wide-intermediate,
  embed-score; torch-score once the reference GPU host exists, E1).
- Wave 5: the hand-tuned baselines (what S3 is measured against) and the engine
  baseline (the same kernel as a user-defined function inside Polars and DuckDB),
  reported beside every benchmark and never a gate.

Every gate runs in a container with `--memory` and `--cpus` set and on the bare
host; results name the machine. Generated data goes under `bench/data/`, which
is ignored by git. `AMORU_BENCH_MORSEL_BYTES` (preamble section 5) is read by
the runner alone.
