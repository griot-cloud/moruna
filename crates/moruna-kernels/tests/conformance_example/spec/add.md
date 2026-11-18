<!--
The worked example of a kernel specification (crates/moruna-kernels/src/conformance.rs). The
kernel, example.add@1, exists only in the runner's tests (tests/conformance.rs).
-->

# example.add@1

Adds a constant, fixed at configure, to every value of an Int64 column.

## Identity

- namespace: `example`
- name: `add`
- version: `1`

## Signature

- Input, port 0: a batch with a column `x`, `Int64`, nullable. Any other column is refused at
  plan, as is an `x` of another type.
- Parameter `addend`: JSON, an integer in the range of `Int64`, required. Any other parameter,
  a missing `addend` or one that is not such an integer is refused at configure.
- Output: a batch with one column `x`, `Int64`, nullable.

## Nulls

A null `x` gives a null. `addend` is never null.

## Overflow

A sum outside `Int64` refuses the batch with an error whose text contains `overflow`, at
`apply`. Nothing of that batch is given.

## Rounding

Not applicable: integer addition is exact.

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

The addend, 8 bytes, from configure. Nothing between batches.

## Tolerance

- declared: none

Every implementation gives the same integers.
