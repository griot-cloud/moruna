# Amoru benchmark suite

This directory is owned by the `bench` agent (preamble section 6.5) and is filled
in two parts. This document covers the part that exists today.

- Wave 1 (here now): the data generator, `amoru-bench`. It writes Parquet with a
  controllable row count, column mix, null ratio and row group size, and
  safetensors and `AMB1` aligned binary tensors of controllable shape and dtype,
  to a local directory and to an S3 compatible store.
- Wave 1 (next): the kernels (identity, normalise, tokenise-explode,
  adversarial, wide-intermediate, embed-score; torch-score once a reference GPU
  host exists, E1).
- Wave 5: the hand tuned baselines (what S3 is measured against) and the engine
  baseline (the same kernel as a user defined function inside Polars and DuckDB),
  reported beside every benchmark and never a gate.

Every gate runs in a container with `--memory` and `--cpus` set and on the bare
host; results name the machine. Generated data goes under `bench/data/`, which is
ignored by git. `AMORU_BENCH_MORSEL_BYTES` (preamble section 5) is read by the
wave 5 runner alone, not by the generator.

## The generator in one line

```
cargo run --release -p amoru-bench -- suite --out bench/data
```

That writes the whole suite, prints one line per file and leaves a
`manifest.json` beside the data. Every run opens with the generator version and
the machine, for example:

```
amoru-bench 0.1.0 (generator format 1) on some-host (linux/x86_64, 16 logical cpus)
```

The version is the crate version; the **generator format** is a separate number
that changes only when the bytes a given seed produces change. A dataset
regenerated after a format bump is expected to differ; a dataset regenerated
without one is not.

## Determinism

Every dataset is a pure function of `--seed` and the shape arguments. Two runs of
the same command produce byte identical files, on the same host and on any other
little endian host with the pinned toolchain and dependency versions. Nothing
time based, host based or iteration order based reaches a data byte:

- The pseudo random stream is SplitMix64, written out in `src/rng.rs`, seeded per
  column and per tensor from the seed and the column or tensor name. Adding a
  column does not move the values of the others.
- Normal draws use the Irwin Hall sum of twelve uniforms, not Box Muller, so no
  value depends on the host's `ln`, `sqrt` or `cos`.
- The Parquet footer's `created_by` is set to the generator and its format, not
  to the Parquet library's version, and the key value metadata is written in a
  fixed order.
- The safetensors `__metadata__` map carries exactly one key, because a map with
  more keys is serialised in hash order.

`manifest.json` is the one file that is not reproducible: it records the machine.
It also records a BLAKE3 digest per file, so two runs can be compared without
keeping both copies:

```
cargo run --release -p amoru-bench -- suite --seed 20260922 --out /tmp/a
cargo run --release -p amoru-bench -- suite --seed 20260922 --out /tmp/b
diff <(jq -S '.files' /tmp/a/manifest.json) <(jq -S '.files' /tmp/b/manifest.json)
```

The test `the_whole_suite_is_byte_identical_between_two_runs` in
`bench/tests/generator.rs` proves this on every run of the quality gate.

## Regenerating every dataset

`cargo run -p amoru-bench -- list` prints the suite. `--scale small` writes the
same shapes at a few thousand rows, which is what the tests use and what a smoke
run wants; `--scale full` (the default) writes the benchmark sizes, about 1.3 GB
in total.

| Dataset | Shape | The kernel it feeds | One command |
|---|---|---|---|
| `identity-mixed` | 2,000,000 rows, 4 i64, 4 f64, 2 short string, no nulls, 65536 rows per row group | identity (amplification about 1) | `amoru-bench dataset identity-mixed --out bench/data` |
| `text-normalise` | 400,000 rows, 2 text columns of mean 512 bytes, stddev 192, 5 percent nulls | normalise (a regex over text, about 1.5) | `amoru-bench dataset text-normalise --out bench/data` |
| `text-explode` | 200,000 rows, 1 text column of mean 2048 bytes, stddev 1024 | tokenise-explode (5 to 10) | `amoru-bench dataset text-explode --out bench/data` |
| `numeric-embed` | 1,000,000 rows, 2 i64 and 16 f64, no nulls | embed-score | `amoru-bench dataset numeric-embed --out bench/data` |
| `nulls-heavy` | 500,000 rows, every column type, 35 percent nulls | the null path of every kernel | `amoru-bench dataset nulls-heavy --out bench/data` |
| `wide-mixed` | 200,000 rows, 74 columns, snappy | wide-intermediate (about 20) | `amoru-bench dataset wide-mixed --out bench/data` |
| `small-row-groups` | 100,000 rows, 1024 rows per row group (98 row groups) | adversarial | `amoru-bench dataset small-row-groups --out bench/data` |
| `embed-weights` | safetensors: `weight` f32 [128,64], `bias` f32 [64] | embed-score weights | `amoru-bench dataset embed-weights --out bench/data` |
| `embed-weights-half` | safetensors: `weight` f16 [256,128], `bias` bf16 [128] | half precision weights | `amoru-bench dataset embed-weights-half --out bench/data` |
| `embed-weights-amb1` | `AMB1` f32 [128,64] | `TensorSource` | `amoru-bench dataset embed-weights-amb1 --out bench/data` |
| `score-bias-amb1` | `AMB1` f64 [64] | `TensorSource` | `amoru-bench dataset score-bias-amb1 --out bench/data` |
| `token-blocks-amb1` | `AMB1` i64 [32,16,8] | rank 3 tensors | `amoru-bench dataset token-blocks-amb1 --out bench/data` |

Any shape outside the suite is written directly:

```
# Parquet with every knob preamble 6.5 names
amoru-bench parquet --name my-shape --rows 1_000_000 \
  --int-cols 4 --float-cols 4 --string-cols 2 --text-cols 1 \
  --text-mean-len 512 --text-len-stddev 192 --null-ratio 0.1 \
  --row-group-rows 32768 --compression snappy --out bench/data

# the same text length given as a variance instead of a deviation
amoru-bench parquet --text-mean-len 512 --text-len-variance 36864 --out bench/data

# a safetensors file of several tensors
amoru-bench safetensors --name model --tensor weight:f32:512,256 \
  --tensor bias:bf16:256 --out bench/data

# one AMB1 tensor (contracts e.4), any dtype, up to 8 dimensions
amoru-bench amb1 --name weights --dtype f32 --shape 128,64 --out bench/data
```

`--seed <u64>` (default 20260922) selects the corpus, `--out <dir>` the
directory, `--local-only` skips the upload, and `amoru-bench help` prints every
option.

## The S3 compatible half

The same files are written to an S3 compatible store (MinIO in a container, the
`minio` job of `.github/workflows/ci.yml`) when four environment variables are
set:

| Variable | Meaning |
|---|---|
| `AMORU_S3_ENDPOINT` | the base URL, for example `http://127.0.0.1:9000` |
| `AMORU_S3_BUCKET` | the bucket to write into |
| `AWS_ACCESS_KEY_ID` | the access key |
| `AWS_SECRET_ACCESS_KEY` | the secret key |
| `AMORU_S3_PREFIX` | optional key prefix, `bench` by default |
| `AWS_REGION` | optional region, `us-east-1` by default |

With any of the four unset, the upload is skipped with a printed note naming the
ones that are missing, never an error: the quality gate runs `cargo test` on
hosts with no object store at all. Buckets are addressed path style, which is
what MinIO and every other self hosted store expects, and plain HTTP is allowed
for an `http://` endpoint.

```
AMORU_S3_ENDPOINT=http://127.0.0.1:9000 AMORU_S3_BUCKET=amoru-ci \
AWS_ACCESS_KEY_ID=amoru AWS_SECRET_ACCESS_KEY=amoru-ci-secret \
  cargo run --release -p amoru-bench -- suite --out bench/data
```

## What the files are

- **Parquet.** Columns are named after their type: `i64_0`, `f64_0`, `str_0`,
  `text_0`. Short strings are a word from a fixed list plus a two digit suffix;
  long text is words from a fixed mixed case, punctuated list, cut to a length
  drawn from a normal distribution with the mean and deviation given. A null
  ratio of zero makes every column non nullable in the schema as well as in the
  data. The writer is fed batches of exactly `--row-group-rows` rows, so every
  row group but the last holds exactly that many.
- **safetensors.** Written by the `safetensors` crate itself, so what the
  generator writes is what that crate reads back.
- **`AMB1`.** The aligned binary format of `architecture/sdd/01-contracts.md`
  section e.4, written here by a small writer of this crate's own
  (`src/amb1.rs`). It does not depend on `amoru-kernel`: a generator that shared
  the runtime's code could not catch a disagreement between the two, so the
  header test is written against the table in that section rather than against
  another implementation.

## Tests

`cargo test -p amoru-bench` covers determinism by seed, the null ratio and the
text length statistics read back out of the written file, the row group size, the
`AMB1` header byte for byte against contracts e.4, and the safetensors round trip
through the `safetensors` crate. The S3 test is skipped, with its reason printed,
on a host with no store configured; it runs in CI's MinIO job.

`tools/quality/check.sh` is the gate (preamble 6.7): em dashes, `cargo fmt`,
`cargo clippy -D warnings`, the tier wildcard lint, `cargo test` and at least 90
percent line coverage per crate.
