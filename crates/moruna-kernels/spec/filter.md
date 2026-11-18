# moruna.filter@1

The rows of a batch that a Boolean column of it selects, given either as the compacted batch or
as the positions of the selected rows.

## Identity

- namespace: `moruna`
- name: `filter`
- version: `1`

## Signature

- Input, port 0: a batch of any columns of any Arrow types, one of which is the predicate: a
  column named by `predicate`, of type `Boolean`, nullable or not.
- Parameter `predicate`: JSON, a non-empty string, required: the predicate column's name.
- Parameter `output`: JSON, `"batch"` or `"selection"`, optional, default `"batch"`. Which form
  the kernel gives. The platform will choose the form by the predicate's density once kernel
  calls are placed by the engine (work items 4.7 and 4.9); this parameter is how it chooses.
- No Arrow parameters.
- Output with `"output": "batch"`: the input's schema less the predicate column, every other
  field (name, type, nullability and metadata) and the schema's metadata as given, in the
  input's order.
- Output with `"output": "selection"`: one column, `selection`, `UInt32`, not nullable.
- Refused at configure: a missing or empty `predicate`, an `output` other than the two, any
  other parameter, any Arrow parameter.
- Refused at plan: an input with no column named `predicate`, a predicate column that is not
  `Boolean`, a tensor input, more than one input.

## Nulls

A null in the predicate does not select its row: only `true` selects (SQL's `WHERE`, Arrow's
`filter` with nulls dropped). Nulls in the other columns are kept as they are in the rows
selected.

## Overflow

The selection's positions are `UInt32`, so the selection form refuses, at `apply`, a batch of
more than 4294967296 rows; nothing of it is given. The batch form has no arithmetic.

## Rounding

Not applicable: no value is computed.

## Ordering

The selected rows keep the input's order. The selection's positions are ascending, each the
position of a selected row in the batch as given: a batch that is a slice of a larger one is
numbered from 0 at its first row.

## Empty input

A batch of zero rows gives a batch of zero rows of the output's schema, in either form. A
predicate that selects nothing (all false, or all null) gives a batch of zero rows. No batches
give nothing.

## State shape

- shape: `none`

`KernelKind::Stateless`: each batch is filtered on its own. `finish` gives nothing.

## Most output

Batch form: at most the input's rows, and at most the input's bytes less the predicate column.
Selection form: at most one `UInt32` per input row, 4 bytes a row.

## State kept

The predicate's name and the form, from configure. Nothing between batches.

## Tolerance

- declared: none

The rows selected and their values are exact.
