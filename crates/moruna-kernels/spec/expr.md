<!--
moruna.expr, work item 4.2, system design 4B. The JSON Schema of the parameters is
spec/expr.schema.json. The corpus is conformance/expr/, written by conformance/expr/make.py
from pyarrow 24 and Python 3.14, which are the reference for every rule below except where a rule
says it differs from Arrow C++.
-->

# moruna.expr@1

Evaluates a tree of elementwise operations over a batch and gives its named outputs as columns of
the same length. The tree has Substrait's shape (column, literal, cast, if-then, switch,
function call) and Arrow compute's vocabulary of elementwise functions.

## Identity

- namespace: `moruna`
- name: `expr`
- version: `1`

## Signature

**Input, port 0:** one batch of any columns. The tree reads the columns it names, by name; the
others are not read. A column of a type outside the table of types below can be passed through,
compared and tested for null, and nothing else.

**Parameter `outputs`:** JSON, required, the only parameter: a non-empty list of
`{"name": <string>, "expr": <node>}`, names non-empty and unique. A node is an object with
exactly one of these keys:

| Node | Form | Value |
|---|---|---|
| column | `{"column": "c"}` | the input column `c` |
| literal | `{"literal": v, "type": "t"}` | `v`, a JSON number, string, boolean or null, of type `t`; without `type`, a boolean is `bool`, an integer `int64`, a number with a point or exponent `double`, a string `string`. A null needs a `type`. A float type also takes `"NaN"`, `"Infinity"` and `"-Infinity"`. |
| cast | `{"cast": n, "to": "t", "safe": true}` | `n` converted to `t`; `safe` defaults to true |
| if | `{"if": [{"when": c, "then": v}, ...], "else": e}` | Arrow's `case_when`: the `then` of the first clause whose `when` is true; else `e`; else null |
| switch | `{"switch": a, "cases": [{"match": l, "then": v}, ...], "else": e}` | the `then` of the first case whose literal `l` equals `a` (`equal`, IEEE order); else `e`; else null |
| call | `{"call": "f", "args": [...], "options": {...}}` | function `f` of the table below |

Type spellings are `moruna check`'s: `bool`, `int8` to `int64`, `uint8` to `uint64`, `float`,
`double`, `string`, `large_string`, `date32`, `timestamp[s|ms|us|ns]`,
`timestamp[us, tz=ZONE]`, and `duration[s|ms|us|ns]`.

Refused at configure: arguments that are not this shape (an unknown key anywhere, a node of no
kind or of two, an empty list, a duplicate name, a null literal with no type, a type that does not
parse). Refused at plan, against the input schema, naming the output and the node's path: an
unknown column or function, a wrong number of arguments, an unknown option or an option of the
wrong kind, argument types the function has no kernel for, a literal whose value is not of its
type, a cast not in the table, an unknown time zone, a condition that is not `bool`, and values
of a selection with no common type.

**Output:** one column per output, in the listed order, named by it. A column node passes the
input column through unchanged and zero copy, its field renamed (its type and nullability kept).
Every other output is nullable, of the type the rules below give it.

### Types

Values are held by kind: `bool`; the signed integers, `date32`, timestamps and durations (as
int64 with the type's width); the unsigned integers; `float`; `double`; and `string` /
`large_string`. Any other type (decimals, dictionaries, binaries, views, `halffloat`,
intervals, nested types) is opaque: it can be output by a column node, compared with a value of
the same type, and tested with `is_null` or `is_valid`.

- **Common numeric type** (Arrow's): any `double` gives `double`; else any `float` gives
  `float`; else the widest integer, signed if any argument is signed, at least twice as wide as
  any unsigned argument, at most 64 bits (`uint8` with `int8` is `int16`; `uint64` with `int64`
  is `int64`).
- **Implicit casts** are safe casts to the common type: an `int64` beyond ±2^53 does not become
  a `double` (an error at that row), nor a `uint64` above 2^63-1 an `int64`.
- **Common value type** of a selection (`if_else`, `if`, `switch`, `coalesce`, `fill_null`, the
  element-wise extremes): the common numeric type; else `bool`; else `string` if all are
  `string` and `large_string` if any is; else timestamps of one zone at the finest unit; else
  durations at the finest unit; else `date32`. Opaque values are never selected.
- Timestamps compared or subtracted are brought to the finer unit. A timestamp with a zone is
  compared only with one with a zone (any zone: instants are compared); subtracting one with a
  zone from one without is refused.

### Functions

| Function | Arguments | Result | Options |
|---|---|---|---|
| `add`, `subtract`, `multiply`, `divide`, and `_checked` forms | two numbers | common numeric type | |
| `add`, `add_checked` | timestamp and duration (either order); two durations | timestamp; duration | |
| `subtract`, `subtract_checked` | timestamp − timestamp; timestamp − duration; duration − duration; date32 − date32 | duration; timestamp; duration; `duration[s]` | |
| `multiply`, `multiply_checked` | duration and integer (either order) | duration | |
| `power`, `power_checked` | two numbers | common numeric type | |
| `negate`, `abs`, and `_checked` forms | a number or a duration (`negate_checked`: not unsigned) | its type | |
| `sign` | a number | `int8` for an integer; else its float type | |
| `sqrt`, `exp`, `ln`, `log10`, `log2`, `log1p`, `sin`, `cos`, `tan`, `asin`, `acos`, `atan`; `_checked` forms of `sqrt`, `ln`, `log10`, `log2`, `log1p`, `sin`, `cos`, `tan`, `asin`, `acos` | a number (an integer as `double`) | its float type | |
| `atan2` | y, x numbers (integers as `double`) | common float type | |
| `round` | a number | its type | `ndigits` (integer, 0), `round_mode` (`half_to_even`) |
| `floor`, `ceil`, `trunc` | a number (an integer as `double`) | its float type | |
| `equal`, `not_equal`, `less`, `less_equal`, `greater`, `greater_equal` | two numbers, two strings, two `bool`, two timestamps, two durations, two `date32`, or two opaque values of one type | `bool` | `order`: `"ieee"` (default) or `"total"` |
| `and`, `or`, `xor`, `and_not`, `and_kleene`, `or_kleene`, `and_not_kleene` | two `bool` | `bool` | |
| `invert` | `bool` | `bool` | |
| `is_null` | any | `bool` | `nan_is_null` (false) |
| `is_valid` | any | `bool` | |
| `is_nan`, `is_finite`, `is_inf` | a number | `bool` | |
| `if_else` | `bool`, then two values | common value type | |
| `coalesce` | one or more values | common value type | |
| `fill_null` | a value and its fill | common value type | |
| `min_element_wise`, `max_element_wise` | one or more numbers, timestamps, durations or dates | common value type | `skip_nulls` (true) |
| `year`, `quarter`, `month`, `day`, `day_of_week`, `day_of_year`, `iso_year`, `iso_week` | a timestamp or a `date32` | `int64` | `day_of_week`: `count_from_zero` (true), `week_start` (1 Monday to 7 Sunday, 1) |
| `hour`, `minute`, `second`, `millisecond`, `microsecond`, `nanosecond` | a timestamp | `int64` | |

`case_when` is the `if` node. Every argument of every call is evaluated on every row, the branch
a row does not take included: an error in it is still an error (as Arrow). Identical subtrees are
one computation.

- **Integer arithmetic:** `divide` truncates toward zero. `power` of integers is repeated
  multiplication in the type; a negative exponent is an error (both forms); `0^0` is 1.
- **Temporal parts** are of the local time: the timestamp's zone (a named zone or a fixed
  offset; none and `UTC` are the stored time), the proleptic Gregorian calendar, with floor
  division before 1970 (the instant one microsecond before the epoch is 1969-12-31 23:59:59.999999,
  a Wednesday). `day_of_week` counts from `week_start`, as 0 or, with `count_from_zero` false,
  as 1. `day_of_year` is 1 to 366. `iso_year` and `iso_week` are ISO 8601's (the week of the
  year's first Thursday is week 1). `millisecond` is 0 to 999 within the second, `microsecond`
  0 to 999 within the millisecond, `nanosecond` 0 to 999 within the microsecond (Arrow C++'s;
  arrow-rs counts differently).

### Casts

| From | To |
|---|---|
| any number or `bool` | any number or `bool` |
| `int64` | a timestamp or a duration, and back (the stored integer) |
| `int32` | `date32`, and back (the stored integer) |
| a timestamp | a timestamp of another unit or zone (a zone change keeps the instant) |
| a duration | a duration of another unit |
| a timestamp | `date32`: the date of its local time |
| `date32` | a timestamp without a zone: midnight of that day |
| `string` | `large_string`, and back |

A cast to the same type is no cast. Numbers to `bool`: non-zero (NaN included) is true. `bool` to
a number: 1 and 0. `double` to `float`: the nearest `float`, an infinity past its range.

### The old filter language

`moruna.std.filter`'s expression text (`crates/moruna-kernels/src/expr.rs`) is translated into
this tree and evaluated by it, keeping every row an existing document gave: a comparison of a
column with a literal converts the literal to the column's type as before (Arrow's cast, an error
where it does not convert); two columns compared must have one type; floats are compared with
`"order": "total"`, the order the old evaluator used; `and`, `or`, `not`, `is_null(c)` and
`is_not_null(c)` are `and_kleene`, `or_kleene`, `invert`, `is_null` and `is_valid`.

## Nulls

A function's result is null where any argument is null, except:

- `and_kleene`, `or_kleene`, `and_not_kleene`: Kleene logic. A false operand of `and_kleene` is
  false whatever the other; a true operand of `or_kleene` is true whatever the other;
  `and_not_kleene(a, b)` is `and_kleene(a, invert(b))`. `and`, `or`, `xor`, `and_not` are null
  where either operand is.
- `is_null` and `is_valid` are never null. With `nan_is_null`, `is_null` is true for NaN too.
  `is_nan`, `is_finite`, `is_inf` of an integer are false, true and false (null where it is
  null).
- `if_else`: a null condition gives null; else the chosen value, null or not.
- `if`: a null condition counts as false. `switch`: a null argument matches no case.
- `coalesce` and `fill_null`: the first argument that is not null; null if all are.
- `min_element_wise`, `max_element_wise`: with `skip_nulls` (the default) the extreme of the
  arguments that are not null, null if all are; without it, null if any is.
- A literal null of a type is null in every row.

A null row never raises an error: a checked overflow, a zero divisor, a cast that does not
convert, a domain error and an instant out of the calendar's range are errors only on rows where
the value is valid. NaN is a value, not a null.

## Overflow

- **Integers** (and timestamps, durations and dates, as 64- or 32-bit integers): `add`,
  `subtract`, `multiply`, `negate`, `abs` and `power` wrap in two's complement at the result
  type's width; their `_checked` forms refuse the batch with an error naming the row,
  `<function>: overflow`. `abs` and `negate` of the type's minimum are the minimum. `negate` of
  an unsigned integer wraps (`negate(1)` of `uint8` is 255).
- **Integer division:** a zero divisor is an error, `divide: divide by zero`, in both forms. The
  minimum divided by −1 is 0 in `divide` and an overflow error in `divide_checked` (Arrow's).
- **Floats:** IEEE 754: a result past the range is an infinity; `x / 0.0` is ±infinity or NaN.
  The `_checked` forms of `add`, `subtract`, `multiply` and `power` do not refuse infinities.
  `divide_checked` refuses a zero divisor. The checked maths refuse: `sqrt_checked` of a
  negative number, `ln_checked`, `log10_checked`, `log2_checked` of zero ("logarithm of zero")
  or a negative number, `log1p_checked` of −1 or less, `sin_checked`, `cos_checked`,
  `tan_checked` of an infinity, `asin_checked` and `acos_checked` outside [−1, 1] ("domain
  error"). The unchecked forms give NaN or an infinity there.
- **Safe casts** (the default, and every implicit cast) refuse, naming the row: an integer
  outside the target type (`integer value X not in range: lo to hi`); a float that is NaN,
  infinite, not whole, or outside the target integer type (`was truncated`); an integer beyond
  ±2^53 to `double` or ±2^24 to `float`; a timestamp or duration to a finer unit past int64
  (`out of bounds timestamp`), or to a coarser unit that is not a whole number of it (`would
  lose data`); a date outside `date32`.
- **Unsafe casts** (`"safe": false`): integers wrap; a float to an integer is truncated toward
  zero and held at the target type's ends, NaN being 0 (Arrow C++ leaves an out-of-range
  conversion undefined; pyarrow 24 on arm64 holds 32- and 64-bit targets at their ends and
  wraps 8- and 16-bit ones); a time to a coarser unit is truncated toward zero, to a finer unit
  wraps.
- `round` of an integer to a multiple of 10^−ndigits past its type is an error (`would
  overflow`), as in Arrow.
- A temporal part or a date of an instant outside the calendar's range (beyond ±262,000 years in
  a named zone) is an error.

## Rounding

- **Float arithmetic** is IEEE 754, round to nearest, ties to even, each operation rounded once,
  in the tree's order: no operation is fused (no multiply-add), reordered or reassociated. A
  `float` is computed in `float`. `sqrt` is correctly rounded. An integer becomes a float by
  rounding to nearest, ties to even.
- **`round(x, ndigits, round_mode)`** is defined on the exact binary value of `x`: of the two
  multiples of 10^−ndigits that bracket it exactly, the one the mode picks, then the float
  nearest that decimal (ties to even). The modes are Arrow's ten: `down` (toward −∞), `up`
  (toward +∞), `towards_zero`, `towards_infinity` (away from zero), and `half_down`, `half_up`,
  `half_towards_zero`, `half_towards_infinity`, `half_to_even`, `half_to_odd`, which go to the
  nearer multiple and settle an exact tie as named. A zero result keeps the sign of `x`
  (`round(-0.001, 2)` is −0.0); NaN and the infinities are unchanged; `ndigits` may be any
  integer. With `half_to_even` this is Python's `round(x, ndigits)`.
  **This differs from Arrow C++**, which scales by 10^ndigits in floating point, rounds and
  scales back: where the scaling rounds onto or across a tie the two differ. 7.095 at two digits
  is 7.09 here (its binary value is 7.09499999...) and 7.1 in Arrow; 86.64999999999999 is 86.65
  here and unchanged in Arrow. Of the 2,964,624 fares Audit re-prices in January 2024, 53,698
  round differently in pyarrow 24; the corpus holds them (`round_python_cases`).
- `round` of an integer with `ndigits` ≥ 0 is the integer; with `ndigits` < 0, the multiple of
  10^−ndigits the mode picks.
- `floor`, `ceil`, `trunc`: toward −∞, +∞ and zero; exact.
- Integer `divide` truncates toward zero. A timestamp cast to a coarser unit truncates toward
  zero; the date of a timestamp is the floor of its local time to the day.
- **Transcendental functions** (`exp`, `ln`, `log10`, `log2`, `log1p`, `sin`, `cos`, `tan`,
  `asin`, `acos`, `atan`, `atan2`, and `power` of floats) are the platform's (the CPU reference
  calls the C library's, as Arrow C++ does); they are within the tolerance below.

## Ordering

Row i of every output is computed from row i of the input; each batch's output keeps the batch's
order, and batches are output in the order they come.

Comparisons order values as follows. Integers, dates, timestamps (as instants) and durations by
value; `false` before `true`; strings by their UTF-8 bytes. Floats in one of two orders, the
`order` option:

- `"ieee"` (the default, Arrow's): IEEE 754. Every comparison involving NaN is false except
  `not_equal`, which is true; −0.0 equals 0.0. NaN = NaN is false.
- `"total"`: IEEE 754's totalOrder. NaN equals NaN (bit for bit) and is above every number
  (above +∞); −0.0 is below 0.0 and not equal to it. So `less(-0.0, 0.0)` is true,
  `greater(NaN, 1.0)` is true, and `equal(NaN, NaN)` is true. The old filter language's
  translation uses it. A `halffloat` is compared in total order only.

`min_element_wise` and `max_element_wise` ignore NaN unless every value is NaN (then NaN), and
order −0.0 below 0.0.

## Empty input

A batch of zero rows gives a batch of zero rows with the output schema; nothing is evaluated, so
no constant subtree raises an error. No batches give no output.

## State shape

- shape: `none`

`KernelKind::Stateless`; `init` gives `NoState`, and nothing is checkpointed. `finish` gives
nothing.

## Most output

One row per input row. Per output: nothing new for a column passed through; else its type's
width per row and a validity bit (a `bool` a bit), and for a string output its offsets and at
most the longest string among its possible values per row.

## State kept

Nothing between batches. From configure, the tree; per input schema, the tree checked and
compiled (its instructions, linear in the tree's size), cached for the batches of that schema.
While a batch is evaluated, a fixed working set of registers of 1,024 rows each, at most one per
distinct node.

## Tolerance

- declared: per-element

Every output is bit for bit, except the transcendental functions' (`exp`, `ln`, `log10`,
`log2`, `log1p`, `sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `atan2`, `power` of floats): an
implementation may differ from the expected by at most two units in the last place of the
expected value, the room two C libraries that each round within one unit leave between them. A
case gives these columns their allowed error as `2 × ulp(expected)`; NaN and the infinities are
exact. Arithmetic, comparisons, logic, selection, casts, rounding, `sqrt` and the temporal parts
are exact.
