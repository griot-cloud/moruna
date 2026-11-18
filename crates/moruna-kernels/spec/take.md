# moruna.take@1

Values gathered by position: for each entry of an index, the value at that position of a values
array. The values are fixed at configure, or come with each index batch.

## Identity

- namespace: `moruna`
- name: `take`
- version: `1`

## Signature

- With the Arrow parameter `values`, the values are fixed at configure:
  - Parameter `values`: Arrow, a batch of exactly one column, of any Arrow type, dictionary
    and nested types (lists, structs) included.
  - Input, port 0: the index, a batch of exactly one column of any integer type (`Int8`,
    `Int16`, `Int32`, `Int64`, `UInt8`, `UInt16`, `UInt32`, `UInt64`), nullable or not.
- Without it, the values come with each index batch:
  - Input, port 0: the index, as above.
  - Input, port 1: the values, a batch of exactly one column of any Arrow type. The two inputs
    are joined by morsel (`aligns_inputs`): the index batch of one morsel number is taken from
    the values batch of the same number.
- No JSON parameters.
- Output: one column with the values column's name and type, as long as the index; nullable
  when the values column or the index column is.
- Refused at configure: any JSON parameter, an Arrow parameter other than `values`, a `values`
  batch that is not one column.
- Refused at plan: an index that is not one column of an integer type, a values input that is
  not one column, a tensor input, and an input count other than one (values fixed) or two
  (values not fixed).

## Nulls

A null index entry gives a null. An entry that selects a null value gives a null. A null index
entry is never out of range, whatever the values' length, so an index of nulls alone over no
values gives nulls.

## Overflow

An index entry that is negative, or not less than the number of values, refuses the batch at
`apply`, with an error whose text contains `out of range`: such an index is wrong input, with
no correct output. Nothing of that batch is given.

## Rounding

Not applicable: no value is computed.

## Ordering

Row i of the output is the value at the position of row i of the index. Each batch's output
follows its index's order.

## Empty input

An index of zero rows gives a batch of zero rows of the output's schema, whatever the values.
No batches give nothing.

## State shape

- shape: `none`

`KernelKind::Stateless`: each index batch is gathered on its own. `finish` gives nothing. The
values fixed at configure are configuration, not state.

## Most output

As many rows as the index. In bytes: one value of the values' type per row; for a variable
width type (strings, lists) at most the values' largest value per row, so an index can produce
many times its own size.

## State kept

The values fixed at configure, as given, for the kernel's life (their Arrow memory size is the
`state_bytes` hint). Nothing between batches.

## Tolerance

- declared: none

Every value is copied, exactly.
