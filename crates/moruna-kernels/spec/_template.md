<!--
The template of a kernel specification (system design 4A; the format is defined in
crates/moruna-kernels/src/conformance.rs). Copy it to spec/<name>.md and replace every <...>.

- A specification has one block per published version. A block starts with the heading
  `# moruna.<name>@<version>` and has the eleven `##` headings below, every one, in this order,
  each once. Versions are 1, 2, ... in order; a published block never changes.
- Each section states the behaviour in full, so no case is left to the implementation. "Not
  applicable" is a statement: say why in one sentence.
- Three sections carry a line the conformance runner reads, written exactly as shown:
  Identity's three lines, State shape's `- shape:` line and Tolerance's `- declared:` line.
- Files whose name starts with `_` are not specifications.
-->

# moruna.<name>@1

<One paragraph: what the kernel computes.>

## Identity

- namespace: `moruna`
- name: `<name>`
- version: `1`

## Signature

<Every input, in port order (port 0 first), as an Arrow schema: each column's name or role,
its Arrow types and nullability. Every parameter: its name, whether it is JSON (its JSON type,
whether required, its default) or Arrow data fixed at configure (its schema). The output as an
Arrow schema: column names, types and nullability. Which inputs or parameters are refused, and
whether at configure (the parameters) or at plan (the input schemas).>

## Nulls

<What a null in each input and parameter gives.>

## Overflow

<What happens when a result does not fit its type: wraps, saturates, is null, or is an error
(at which call). "Not applicable" when no arithmetic is done, with why.>

## Rounding

<How results that are not exact are rounded, the mode named. "Not applicable" when none are.>

## Ordering

<The order of the output's rows, and of the output across batches. Never "unspecified".>

## Empty input

<What a batch of zero rows gives, and what a run with no batches gives.>

## State shape

- shape: `none`

<One of the four shapes of system design 4C, on the line above: `none`,
`accumulate-merge-finish`, `build-once-read-many` or `per-partition-ordered`. Then how it maps
onto `KernelKind` and `KernelState`: `none` is `KernelKind::Stateless`, the others
`KernelKind::Stateful`, and what `finish` gives.>

## Most output

<The most output one input can produce, in rows and bytes, as a function of the input and the
parameters.>

## State kept

<What the kernel keeps between batches and after configure, and its size as a function of the
parameters (a model's weights, the values of `take`, an accumulator's groups). "Nothing" for a
kernel that keeps nothing.>

## Tolerance

- declared: none

<The line is one of:
- `- declared: none`: every implementation's answer must equal the expected bit for bit.
- `- declared: ` then a JSON object in backticks, by output column, each `{"abs": a, "rel": r}`:
  a value v is within when |v - e| <= a + r * max(|v|, |e|), NaN only matches NaN, an infinity
  only itself, and a null only a null. The corpus index declares the same object for this
  version.
- `- declared: per-element`: a case may give, for an output column, the absolute error allowed
  at each element (the case's `allowed` column); every other column is bit for bit.
Then the declared precision and why the tolerance is what it is (for `per-element`, how the
allowed error of an element follows from its inputs).>
