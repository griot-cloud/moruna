<!--
The specification of moruna.group_aggregate (system design 4B, 4C; work item 4.4). The format is
crates/moruna-kernels/src/conformance.rs's; the CPU reference is src/group_aggregate/.
-->

# moruna.group_aggregate@1

Count, sum, min, max and mean per group of keys. One signature with two paths: a dense path,
for a small key domain the job declares, and a hash path for any keys; both give the same rows.
The kernel accumulates the batches it is given and gives its result at `finish`, so it can run
as one instance per CPU of the job, each giving a partial result, and one instance merging them.

## Identity

- namespace: `moruna`
- name: `group_aggregate`
- version: `1`

## Signature

Input, port 0: a batch holding the key columns and the value columns the parameters name, by
name; any other column is ignored. In `final` mode the input is partial results (the partial
form, below). Every batch an instance is given has the key and value columns of its first, by
name, position and type; a batch that differs is refused at `apply`.

Parameters, JSON, given at configure. Any other member, a misspelt one, or one of the wrong
shape is refused at configure, naming it.

| Parameter | JSON | Meaning |
|---|---|---|
| `keys` | list of column names, required, at least one | The key columns, in order. No keys is refused: a global aggregate is not this kernel. |
| `aggregates` | list of objects, optional, default `[]` | Each `{"op": ..., "column": ..., "as": ...}`. `op` is `count`, `sum`, `min`, `max` or `mean`. `column` is the value column: optional for `count`, which then counts rows, and required for every other op. `as` is the output column's name, required. With no aggregates the output is the distinct keys. |
| `domain` | object, optional | `{"<key>": [lo, hi], ...}` for every key and no other name: each key's inclusive range of integers, `lo <= hi`. Selects the dense path. A key that is not an integer type is refused at plan. |
| `mode` | `"single"` (default), `"partial"` or `"final"` | `single`: rows in, the final result out, one instance. `partial`: rows in, a partial result out. `final`: partial results in, the final result out, one instance. |
| `instances` | integer from 1 to 64, optional, default 1 | `partial` only: how many instances run (`KernelKind::Stateful { max_instances }`), one per CPU of the job. Any other value, or more than 1 in `single` or `final`, is refused at configure. |

Every column name the output has (the keys, each `as`, and in `partial` and `final` modes each
mean's `<as>.sum` and `<as>.count`) is distinct; a name given twice is refused at configure.

Key types: `Boolean`; `Int8`, `Int16`, `Int32`, `Int64`; `UInt8`, `UInt16`, `UInt32`, `UInt64`;
`Float16`, `Float32`, `Float64`; `Date32`, `Date64`; `Time32`, `Time64`; `Timestamp` (any unit
and zone); `Duration`; `Decimal32`, `Decimal64`, `Decimal128`, `Decimal256`; `FixedSizeBinary`;
`Utf8`, `LargeUtf8`, `Utf8View`; `Binary`, `LargeBinary`, `BinaryView`. Any other key type
(dictionary, nested, interval) is refused at plan, as is a key or value column the input lacks.

Value types: `count` takes a column of any type. `sum` and `mean` take the integer and float
types. `min` and `max` take those and `Date32`, `Date64`, `Time32`, `Time64`, `Timestamp` and
`Duration`. Any other is refused at plan.

Output in `single` and `final` modes: the key columns, with their input names, types and
nullability, then one column per aggregate, in order, named by its `as`:

| op | Type | Nullable |
|---|---|---|
| `count` | `Int64` | no |
| `sum` | `Int64` of signed integers, `UInt64` of unsigned, `Float64` of floats | yes |
| `min`, `max` | the value column's type | yes |
| `mean` | `Float64` | yes |

Output in `partial` mode, the partial form: the key columns, then each aggregate's state:

| op | Columns |
|---|---|
| `count` | `<as>`, `Int64`, not null |
| `sum` of integers | `<as>`, `Decimal128(38, 0)`, the exact total; null for a group with no value |
| `sum` of floats | `<as>`, `Binary`, the exact sum's encoding (below); null for a group with no value |
| `min`, `max` | `<as>`, the value column's type; null for a group with no value |
| `mean` | `<as>.sum` (`Decimal128(38, 0)` of integers, `Binary` of floats) and `<as>.count` (`Int64`), both not null |

A partial `sum` column carries the field metadata `moruna.group_aggregate.sum`, whose value is
its final type: `int64`, `uint64` or `float64`. `final` mode reads the partial columns by these
names, and refuses at plan a partial column that is missing or of another type.

The exact sum's encoding: one byte of flags (1: a NaN was added; 2: +infinity; 4: -infinity),
one byte of tag, then for tag 0 an `i32` `e` and an `i128` `s`, little-endian, the value
`s × 2^(e − 1074)` with `s` odd (or `s` and `e` both 0 for zero), used whenever `|s| < 2^126`;
for tag 1, otherwise, 72 `i64` digits, little-endian, digit `k` weighing `2^(32k − 1074)`, digits
0 to 70 in `[0, 2^32)` and the last signed. Each value has exactly one encoding; any other bytes
are refused at `apply`.

Every `apply` gives an empty batch of the output schema. `finish` gives the instance's result,
or nothing when the instance was given no row.

## Nulls

- A null key is a group of its own: in each key column null is one value, and two rows are in
  one group when every key is equal or both null.
- `count` with no column counts the group's rows; `count` of a column counts its non-null values.
- `sum`, `min`, `max` and `mean` skip null values. A group with no non-null value has a null
  `sum`, `min`, `max` and `mean`, and a `count` of that column of 0.
- No parameter is null.

## Overflow

- `count` is `Int64`; it cannot overflow (2^63 rows).
- `sum` and `mean` of integers accumulate the exact total in 128 bits; the partial form carries
  it as `Decimal128(38, 0)`. Only the final total is checked: a final `sum` outside `Int64` (or
  `UInt64`, of unsigned values) refuses the run at `finish`, with an error naming the aggregate,
  the group's keys and the total (`the sum `s` of the group (1) is ..., past Int64`). A total
  that leaves `Int64` within one instance and comes back into range in the merge is no overflow.
  A merged total past 128 bits (more than 2^64 rows of `Int64`) refuses the merge at `apply`.
- `sum` of floats is exact and cannot overflow before rounding; an exact total beyond the
  largest double rounds to the infinity of its sign.

## Rounding

- `sum` of floats is the exact sum of the values, rounded once to the nearest `Float64`, ties to
  even, when the result is given. Its bits are the same for any order of the rows and any split
  of them into batches, instances, partial results or devices. It is NaN when a NaN was added or
  both infinities were, and an infinity when one was. An exact zero is `+0.0` (so the sum of
  `-0.0` and `-0.0` is `+0.0`).
- `mean` of integers is the exact total converted to `Float64` (to nearest, ties to even) divided
  by the count in `Float64`. `mean` of floats is the float `sum` above divided by the count in
  `Float64`. Both are bit-reproducible.
- `min` and `max` do not round. Of floats they compare by IEEE 754 total order with every NaN
  taken as the positive quiet NaN: `-0.0` is less than `+0.0`, and NaN is greater than
  +infinity, so `max` is NaN when a NaN is present and `min` is NaN only when every value is. A
  NaN result is the positive quiet NaN.
- `count` and `sum` of integers are exact.

## Ordering

One instance gives its result in one batch at `finish`, its rows in ascending order of their
keys, compared column by column in the order of `keys`. Within a column: integers, decimals and
temporal values by value; floats after canonicalisation (`-0.0` as `+0.0`, every NaN as the
positive quiet NaN) in total order, so NaN follows +infinity; `false` before `true`; strings and
binaries by their bytes, unsigned, lexicographically; null last. The dense path and the hash
path give the same order, as do the partial form and the final result. `apply` gives no rows.

## Empty input

A batch of no rows adds no group, and its `apply` gives an empty batch. An instance given no
rows (no batches, or only empty ones) gives nothing at `finish`, in every mode.

## State shape

- shape: `accumulate-merge-finish`

`KernelKind::Stateful { max_instances }`: `instances` in `partial` mode, 1 otherwise. Each
instance's `KernelState` accumulates every batch it is given; `finish` gives its result
(`single`, `final`) or its partial result (`partial`).

The merge rule, which `final` applies to partial results: counts add; integer totals add,
exactly; float sums add, exactly; `min` is the least of the minima and `max` the greatest of the
maxima (floats in the total order above); a mean's sums add and its counts add, and the mean is
their quotient. A null partial value adds nothing. The rule is associative and commutative, so
the final result is the same for any split of the rows across instances and any order of the
partial results. For `count` alone the partial form is the final form, and the merge is the sum
of the counts per group.

`ResumePolicy::Checkpoint`: an instance's checkpoint is its state in the partial form after the
schema of its input, as two Arrow IPC streams, each after its length as a little-endian `u64`;
it is empty before the first batch, and a restore merges it into a new instance.

## Most output

At most one row per distinct combination of keys in the input: no more rows than the input
(`single`, `partial`) or than the partial rows it is given (`final`), and on the dense path no
more than its cells. Each row holds the keys and one value per aggregate (two per `mean` in the
partial form); every `apply` gives no rows.

## State kept

Configure keeps the parameters. Between batches an instance keeps its groups.

The dense path is taken when `domain` declares every key, every key is an integer type, and the
instance's dense state is at most 64 MiB: its cells, the product over the keys of `hi − lo + 2`
(each key's values and one slot for null), times the bytes of a cell, 8 plus per aggregate 8
for `count`, 33 for `sum`, 9 for `min` and `max` and 40 for `mean`. That size is declared at
configure as `KernelHints::state_bytes` and allocated at the first batch. Otherwise the instance
is on the hash path, which keeps per group its key (40 bytes when there are at most four keys of
at most 8 bytes each; otherwise the key in Arrow's row format and a pointer), 8 bytes of row
count, and per aggregate 8 for `count`, 17 for `sum` of integers, 33 for `sum` of floats (and 584
more while a sum spans more than 126 bits), 9 for `min` and `max`, 24 for `mean` of integers and
40 for `mean` of floats, and the hash table's slots. `KernelState::footprint` reports the bytes
held after every `apply`.

A value outside its declared range does not fail the run: the instance moves its groups to the
hash path and continues there, giving the same rows, and logs a warning record
(`moruna_kernel::output::log`, `Level::Warning`) that says `dense path declined: value V outside
[lo, hi]` and names the key.

## Tolerance

- declared: none

Every output is exact, or rounded once from an exact value, so every implementation gives the
same bits; the float sums compare bit for bit like everything else.
