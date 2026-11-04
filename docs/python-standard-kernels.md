# Standard functions

Each `moruna.std` function returns a [`moruna.Function`](#standard-functions-as-tasks) a job takes as a task. `None` for an optional option delegates to the standard function's default.

| Function signature | Result |
| --- | --- |
| `cast(columns: Mapping[str, type], strict: bool \| None = None)` | Cast columns to Arrow-compatible types. Failed conversions become null unless `strict=True`. |
| `rename(columns: Mapping[str, str])` | Rename old column names to new names. |
| `select(columns: str \| Sequence[str])` | Keep columns in supplied order. |
| `drop(columns: str \| Sequence[str])` | Remove columns. |
| `filter(expr: str)` | Keep rows for which the expression is true. |
| `fill_null(values: Mapping[str, value])` | Replace nulls per column with a number, string or Boolean. |
| `dedupe(keys: str \| Sequence[str])` | Keep the first row per key across the run. |
| `hash(columns: str \| Sequence[str], algo: str \| None = None, output: str \| None = None)` | Append a hex digest. `algo`: `"sha256"` (default) or `"blake3"`; `output` defaults to `"hash"`. |
| `mask(columns: str \| Sequence[str], mode: str \| None = None, keep: int \| None = None)` | Mask in place. Modes: `"redact"` (default), `"partial"`, `"hash"`, `"null"`; `keep` defaults to 4 for partial. |
| `explode(column: str)` | One row per list element; null/empty lists give one null element. |
| `concat_str(columns: Sequence[str], separator: str \| None = None, output: str \| None = None)` | Append joined text; separator defaults to empty, output name to `"concat"`; null if any input is null. |
| `date_trunc(column: str, unit: str, output: str \| None = None)` | Truncate a date/timestamp to `"year"`, `"month"`, `"day"`, `"hour"`, `"minute"`, or `"second"`; replace input unless `output` names a new column. |
| `join()` | For each morsel, the columns of every input side by side, in input order. Several inputs, each a branch of one upstream through functions that keep its rows and their order ([`moruna.Job`](python-run.md#morunajob)). A row count that differs between inputs, or a column two inputs give, raises an error naming the inputs. |
| `union()` | Pass on every batch of every input as it came; every input has one schema. Several inputs ([`moruna.Job`](python-run.md#morunajob)). |

`filter` supports comparisons (`==`, `!=`, `<`, `<=`, `>`, `>=`), `is_null(c)`, `is_not_null(c)`, `and`, `or`, `not`, and parentheses. Put names with spaces in backquotes, for example `` `order amount` > 0 ``. Invalid standard-function arguments raise a planning error at construction.

## Standard functions as tasks

The `moruna.Function` a `moruna.std` function returns has no public constructor; a spec names it by the kind `std`, its name and its arguments. Its read-only attributes are `name: str` (for example `"filter"`), `args: str` (canonical JSON), and `fingerprint: str` (64 lowercase hexadecimal characters).
