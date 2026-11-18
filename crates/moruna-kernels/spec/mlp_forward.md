<!--
mlp_forward's specification (system design 4A, 4B; work item 4.6), in the catalogue's format
(crates/moruna-kernels/src/conformance.rs). Its corpus is conformance/mlp_forward/, written by
bench/vm/kernel_mlp.py; its CPU reference is src/mlp.rs.
-->

# moruna.mlp_forward@1

Dense layers over a fixed-width feature vector per row: for each row,
`y = f_L(… f_2(f_1(x)))`, every layer `f(x) = activation((x · W) + b)`. The model (its layers'
weights, biases and activations) is fixed at configure; batches are then processed with nothing
kept between them.

## Identity

- namespace: `moruna`
- name: `mlp_forward`
- version: `1`

## Signature

- **Input, port 0:** a batch holding the feature columns the parameters name, in any position.
  Either `k` columns named by `inputs`, each `Float16`, `Float32` or `Float64` (each its own),
  nullable; or one column named by `input`, `FixedSizeList<Float, k>` (values `Float16`,
  `Float32` or `Float64`), nullable, its values nullable. `k` is the first layer's input count.
  Every other column of the batch is ignored. A named column that is missing, of another type, or
  a list of another size is refused at plan.
- **Parameters, JSON:**
  - `precision`, required: `"float32"` or `"float64"`, the declared precision (Tolerance).
  - `activations`, required: a list of one name per layer, in the network's order, each
    `"identity"`, `"relu"`, `"sigmoid"` or `"tanh"`.
  - Exactly one of `inputs`, a non-empty list of distinct column names, as many as the first
    layer takes, in the model's input order; or `input`, the name of one list column.
  - `output`, optional, default `"y"`: the output column's name, not empty.
- **Parameters, Arrow data (the model handle):** `layer_1` … `layer_L`, one batch per layer, in
  the network's order, `L` the length of `activations`, and no other. A layer's batch has one row
  per output unit and these columns, and no other:
  - `weights`: `FixedSizeList<Float64, inputs>` with no null list and no null value; row `j` holds
    output `j`'s weight from each input, which is column `j` of NumPy's `W` in `x @ W`;
  - `bias`, optional: `Float64` with no null, output `j`'s bias in row `j`. A layer whose batch has
    no `bias` column adds nothing (no zeros are added).

  The weights and biases are `Float64` in the handle whatever the declared precision; a `float32`
  kernel converts them once, at configure (Rounding). Each layer's input count (its list size)
  must equal the previous layer's output count (its row count), and with `inputs`, the first
  layer's must equal the number of columns named. Every weight and bias is finite. A layer has at
  least one input and one output.
- **Refused at configure:** a missing or unknown parameter, a parameter of the wrong JSON type, an
  unknown precision or activation, both or neither of `inputs` and `input`, a column named twice,
  a missing or extra `layer_<l>`, and a layer batch that breaks a rule above; each refusal names
  the parameter or the layer.
- **Output:** a batch of one column named by `output`, nullable. When the last layer has one
  output, the declared precision's Float (`Float32` or `Float64`); when it has `width > 1`,
  `FixedSizeList<Float, width>` of that Float, its values (field `item`) not nullable.

## Nulls

A row with a null input gives a null output row (a null Float, or a null list), whose values are
zero: a null value in any input column, a null list, or a null value inside a non-null list. Every
other row is computed as if the null row were not there. The parameters have no nulls (a null in a
layer's batch is refused at configure).

## Overflow

IEEE 754 in the declared precision: a sum or product past the type's range is an infinity of its
sign, never an error, and a NaN input makes the outputs of its own row NaN (an infinity times a
zero weight is also NaN). No other row is affected. `sigmoid` of a large negative pre-activation
gives `+0.0`, of a large positive one `1.0`.

## Rounding

Every step rounds to nearest, ties to even, in the declared precision:

- inputs are converted to it, exactly when they widen (`Float16` and `Float32` to `float64`), to
  nearest when they narrow (`Float64` to `float32`); weights and biases are converted the same way,
  once, at configure;
- each layer computes the product `x · W` summed in the precision, then adds the bias, one
  addition per output (`(x · W) + b`, NumPy's order in `x @ W + b`), then applies the activation:
  `identity(x) = x`; `relu(x)` is `x` when `x > 0` or `x` is NaN, else `+0.0` (so
  `relu(-0.0)` is `+0.0`); `sigmoid(x) = 1 / (1 + exp(-x))`; `tanh(x)`.

The order in which a product's terms are summed, and whether a multiply and an add are fused, are
not fixed. The CPU reference sums each output's terms in input order with `matrixmultiply`'s
kernels, fused where the CPU has fused multiply-add; `exp` and `tanh` are the platform's. A device
may sum in any order, within the Tolerance. The kernel does not round its output further: Score's
`np.round(y, 6)` is Score's own step after it.

## Ordering

Row `i` of the output is row `i` of the input, and the output of each batch follows the batch's
order. No row depends on another, so how the platform sizes or splits the batches never changes
an answer.

## Empty input

A batch of zero rows gives a batch of zero rows of the output schema. No batches give nothing.

## State shape

- shape: `none`

`KernelKind::Stateless`: every batch is independent. An instance's `KernelState` holds only
scratch for one block of rows, reused across batches, never a result. `finish` gives nothing.

## Most output

One row per input row: `rows × width` values of 4 bytes (`float32`) or 8 (`float64`), and a
validity bitmap of `rows` bits; `width` is the last layer's output count. Score's network
(23-128-128-1) in float64 gives 8 bytes and a validity bit per row.

## State kept

After configure, the model converted to the declared precision: `parameters × 4` bytes in
`float32`, `parameters × 8` in `float64`, where `parameters` is the count of weights and biases
(Score's network: 19,712, so 78,848 or 157,696 bytes). Between batches, nothing but one block's
scratch: in the CPU reference, 256 rows × (`k` + 2 × the widest hidden layer) values.

## Tolerance

- declared: per-element

An implementation's value `y` of an output element is right when it is within the element's
allowed error of the float64 answer `y64` of the same model on the same input:

```text
|y - y64| <= u * (s(x) + 4 * |y64|),        u = 2^-24 (float32), 2^-53 (float64)
```

Each term covers one source of difference between two correct implementations:

- **`s(x)`, the sums.** The element's scale: the network run in float64 on `|x|` (the inputs
  widened exactly to float64), with every weight and bias replaced by its absolute value and
  every activation left out: `s_0 = |x|`, `s_l = s_(l-1) · |W_l| + |b_l|`, `s(x) = s_L`, one value
  per output element. It bounds the rounding the products' sums accumulate, in whatever order
  and with or without fused multiply-adds, as the network carries it to the output.
- **`4 * |y64|`, the last steps.** A few units in the last place of the output itself: its own
  rounding, the bias's addition, and the error of `exp` and `tanh` (one or two units in the last
  place on every platform), which do not scale with `s`. In a small network `s(x)` is about
  `2 * |y|`, so without this term one unit in the last place of difference already fails; in
  Score's, `s(x)` is about 500 times `|y|`, so this term barely widens it.

The corpus gives `y64` as the expected value, computed by NumPy in float64 and written in the
declared precision's type for the output's schema, and this bound as each element's allowed
error; in float32 the allowed error adds the distance from `y64` to the float32 value written, so
the comparison stays against `y64`. The runner compares element by element. A NaN `y64` must
give NaN, an infinite one the same infinity (their allowed error is null: bit for bit); null rows
must be null.

The declared precision is the arithmetic every step is computed in, and the output's type.

**This tolerance is measured, not proven.** A proven bound for every summation order is
`(sum over layers of (inputs + 2)) * u * s(x)` (285 times the first term for Score's network),
which for Score in float32 would allow errors about 4,000 times those seen. Measured against it:

| Case | Worst `|y - y64|` / allowed |
|---|---:|
| Score, January 2024's 2,964,624 trips, NumPy's float32 network against its float64 one | 0.064 |
| Score, the first 10^6 of them, the CPU reference in float32 / float64 (4G.1, the Mac) | 0.089 / 0.054 |
| Corpus `score_float64`, `score_float32`, `score_sliced` | 0.020, 0.032, 0.019 |
| Corpus `activation_identity`, `_relu`, `_sigmoid`, `_tanh` | 0.35, 0.39, 0.15, 0.45 |
| Corpus `list_input`, `two_batches` | 0.39, 0.39 |
| Corpus `float32_columns`, `float16_columns` | 0.48, 0.36 |
| Corpus `float32_list`, `narrowed_to_float32` | 0.61, 0.57 |
| Corpus `null_column_value`, `null_list_and_value`, `nan_row` | 0.12, 0.054, 0.25 |

(`overflow`'s one finite element is exact, and `empty` has none.) A summation order chosen to cancel
badly, or an `exp` or `tanh` many units in the last place off, could pass it on some element.

The tolerance belongs to the kernel. A job's check stays separate: Score's rows computed in
float64 must match 0.6.6's bit for bit after Score's `np.round(y, 6)`.
