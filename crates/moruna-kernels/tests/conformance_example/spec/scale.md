<!--
The worked example of a kernel whose answer may differ by element (crates/moruna-kernels/src/
conformance.rs, `per-element`). The kernel, example.scale@1, exists only in the runner's tests
(tests/conformance.rs).
-->

# example.scale@1

Multiplies every value of a Float64 column by a constant fixed at configure.

## Identity

- namespace: `example`
- name: `scale`
- version: `1`

## Signature

- Input, port 0: a batch with a column `x`, `Float64`, nullable. Any other column is refused at
  plan, as is an `x` of another type.
- Parameter `factor`: JSON, a finite number, required. Any other parameter, or a missing one, is
  refused at configure.
- Output: a batch with one column `x`, `Float64`, nullable.

## Nulls

A null `x` gives a null.

## Overflow

A product past `Float64`'s range is an infinity of its sign, as IEEE 754 gives it; never an
error.

## Rounding

The CPU reference rounds the product to nearest, ties to even (IEEE 754). Another
implementation may round differently, within the tolerance below.

## Ordering

Row i of the output is row i of the input; the output of each batch follows the batch's order.

## Empty input

A batch of zero rows gives a batch of zero rows. No batches give nothing.

## State shape

- shape: `none`

`KernelKind::Stateless`. `finish` gives nothing.

## Most output

One row per input row: 8 bytes per row and a validity bit.

## State kept

The factor, 8 bytes, from configure. Nothing between batches.

## Tolerance

- declared: per-element

An element of `x` may differ from the expected by at most its case's allowed error, which the
corpus sets to |expected| * 2^-50 (four units in the last place, room for a fused or reordered
multiply), and to null, bit for bit, where the case checks an exact value.
