<!--
The specification of moruna.interval_coverage_count (system design 4B; work item 4.5). The CPU
reference is crates/moruna-kernels/src/interval_coverage_count.rs, the corpus
conformance/interval_coverage_count/.
-->

# moruna.interval_coverage_count@1

For each group and each bucket of a grid, the number of intervals that cover it. The grid is a
ring of `size` buckets, each `step` units of `start` wide, from 0: a position `p` is in bucket
`floor(p / step) mod size`. An interval covers `length` consecutive buckets from the bucket of
its `start`, wrapping from the last bucket to the first, and counts once in a bucket each time it
passes it. Occupancy's minute of the day is the grid `step = 60_000_000` (a minute in
microseconds) and `size = 1440`, which wraps at midnight. The output is what expanding every
interval into one row per bucket it passes and then counting rows per (group, bucket) gives,
without the expansion.

## Identity

- namespace: `moruna`
- name: `interval_coverage_count`
- version: `1`

## Signature

- **Input, port 0:** a batch with three columns, named by the parameters `group`, `start` and
  `length`: the group, `Int32`; the interval's start, `Int64`; its length in buckets, `Int64`.
  Each may be nullable. Any other column is ignored. A batch without one of the three, or with
  one of another type, is refused at plan (`output_schema`) and at `apply`.
- **Parameters,** JSON, an object; any other member is refused at configure:
  - `group`, `start`, `length`: strings, required, non-empty: the names of the three columns.
  - `step`: an integer, required, at least 1: how many units of `start` one bucket is wide.
  - `size`: an integer, required, from 1 to 2^31 - 1: how many buckets the ring has.
  - `groups`: `[low, high]`, two `Int32` integers with `low <= high`, optional (absent or null
    for none): the declared group domain. Its dense table, `(high - low + 1) * (size + 1)` cells,
    must be at most 2^24 cells; a larger domain is refused at configure.
  - No Arrow parameters: one is refused at configure.
- **Output:** a batch of three columns, none nullable: `group`, `Int32`; `bucket`, `Int32`, from
  0 to `size - 1`; `count`, `Int64`, at least 1. One row for every (group, bucket) whose count
  is not zero, and no other.
- **The count:** `count(g, b)` is the number of pairs (row `i`, `k`) with `0 <= k < length_i`,
  `group_i = g` and `(floor(start_i / step) + k) mod size = b`, over the rows that are counted
  (Nulls).
- **The declared domain** chooses the table, never the output: with `groups`, a batch whose
  counted groups all lie in `[low, high]` is counted on a dense table over the domain. A batch
  with a counted group outside it is counted on the sparse (hashed) table instead, with the same
  output; the call's note says so (below). It is never an error. Without `groups`, every batch
  is counted on the sparse table.
- **Notes:** a call with something to say (rows skipped for a null; a batch counted on the
  sparse table, how many of its rows are outside the domain, and the first of them) logs one
  record at level Warning, attributed to the stage (`moruna_kernel::output::log`), beginning
  `moruna.interval_coverage_count@1:`. The record is not part of the output.

## Nulls

A row with a null in `group`, `start` or `length` is skipped: it counts nowhere. The number of
rows skipped is in the call's note. The parameters are never null, apart from `groups`, whose
null is its absence.

A row whose `length` is zero or less covers nothing, and is not noted.

## Overflow

- A count that does not fit `Int64` refuses the batch at `apply`, with an error whose text
  contains `overflows Int64`. Nothing of that batch is given. A count is at most the sum of
  `ceil(length / size)` over its group's counted rows, so only such a sum past 2^63 - 1 can
  overflow.
- The bucket of a start cannot overflow: `floor(start / step)` with `step >= 1` is in `Int64`'s
  range, and its remainder modulo `size` is in `0..size`.

## Rounding

The bucket of a start is `floor(start / step)`, rounded toward negative infinity, then reduced
modulo `size` into `0..size` (Euclidean division): with `step = 10` and `size = 6`, position
`-1` is in bucket 5 and position `-60` in bucket 0. Everything else is exact integer
arithmetic.

## Ordering

Each call's output is in key order: by `group` ascending (as a signed integer), then by `bucket`
ascending. The order of the input's rows does not change the output. Each call gives its own
batch's cells; the output of a run is each call's output in the order of the calls. Partial
outputs merge by addition (State shape), in key order.

## Empty input

A batch of zero rows, or of rows that are all skipped or of non-positive length, gives a batch
of zero rows with the output's schema. No batches give nothing.

## State shape

- shape: `none`

`KernelKind::Stateless`: each `apply` counts its own batch and keeps nothing; `finish` gives
nothing. Partial outputs, of several batches or several instances, merge by addition: every
(group, bucket) once, its counts summed (refused when a sum does not fit `Int64`), in key order.
The CPU reference gives this merge as `interval_coverage_count::merge`.

## Most output

For a batch of `n` rows, at most `min(L, G * size)` rows, where `L` is the sum over counted rows
of `min(length_i, size)` for positive lengths, and `G` the number of distinct groups among
counted rows; so at most `min(n, G) * size` rows. Each row is 16 bytes (4, 4 and 8), with no
validity bitmaps.

## State kept

Nothing between batches. After configure, the three column names and the grid (`step`, `size`,
`groups`). Within one call, the table, which depends on the batch alone:

- **dense** (a declared domain holding every counted group): `(high - low + 1) * (size + 2)`
  `Int64`s, a difference row of `size + 1` and a count of full turns per group of the domain, at
  most 2^24 + 2^24 / (size + 1);
- **sparse**, for `G` groups present: a hash index of `G` entries and a 4-byte slot per counted
  row, then difference rows, `G * (size + 2)` `Int64`s, when `G * (size + 1)` is at most 2^24;
  otherwise a sweep of each interval's changes, at most three 16-byte events per counted row and
  `G` counts of full turns, sorted. None of them holds one entry per bucket an interval covers.

## Tolerance

- declared: none

Every count is an exact integer, so every implementation's output equals the expected bit for
bit.
