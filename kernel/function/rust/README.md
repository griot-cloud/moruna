# moruna-function: a native function in Rust

A function is a program the data plane feeds Arrow batches (system design 5A). It runs on the
core as a native task: one static ELF, no Linux layer. `moruna-function` is the whole interface:

```rust
moruna_function::serve(|batch: RecordBatch, out: &mut Emit| -> Result<(), FunctionError> { … })
```

- `serve` takes each input batch, read in place from the record the engine posted (no copy), and
  calls the function with it;
- `Emit::batch` writes an output batch into a window, and may be called any number of times,
  none included;
- a returned `FunctionError` ends the task with its message in the task's log;
- `moruna_function::log` writes a line to the task's log;
- `moruna_function::params` and `moruna_function::args` read the task's params and the run's
  arguments, JSON objects (below).

## Write one

A new crate, `Cargo.toml`:

```toml
[package]
name = "double"
version = "0.1.0"
edition = "2024"

[dependencies]
moruna-function = { path = "<moruna>/kernel/function/rust" }
arrow-array = { version = "=59.3.0", default-features = false }

[profile.release]
panic = "abort"
```

`src/main.rs`, the example in `examples/double`: it doubles every `Int64` column and passes the
others through.

```rust
use std::process::ExitCode;
use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::Int64Type;
use arrow_array::{ArrayRef, RecordBatch};
use moruna_function::{Emit, FunctionError};

fn double(batch: RecordBatch, out: &mut Emit) -> Result<(), FunctionError> {
    let mut columns: Vec<ArrayRef> = Vec::with_capacity(batch.num_columns());
    for (field, column) in batch.schema().fields().iter().zip(batch.columns()) {
        match column.as_primitive_opt::<Int64Type>() {
            Some(values) => {
                let doubled = values.try_unary::<_, Int64Type, _>(|v| {
                    v.checked_mul(2).ok_or_else(|| {
                        FunctionError::new(format!("{v} doubled is past Int64 in {}", field.name()))
                    })
                })?;
                columns.push(Arc::new(doubled));
            }
            None => columns.push(column.clone()),
        }
    }
    out.batch(&RecordBatch::try_new(batch.schema(), columns)?)
}

fn main() -> ExitCode {
    moruna_function::serve(double)
}
```

Use the arrow-rs version above (59.3.0, the engine's): `RecordBatch` is arrow-rs's own type, and
another version is another type.

## A function with an end

A function that writes something once the input has ended, an aggregate say, is a type that
implements `moruna_function::Function`: `batch` answers each batch as `serve`'s closure does, and
`end` is called once more after the last batch, with an `Emit` of its own. Its state is its own
fields, which both reach. `main` serves it:

```rust
// Its total is the whole input's only if one instance sees every batch (below).
moruna_function::processes!(1);

#[derive(Default)]
struct Rows(i64);

impl Function for Rows {
    fn batch(&mut self, batch: RecordBatch, _out: &mut Emit) -> Result<(), FunctionError> {
        self.0 += batch.num_rows() as i64;
        Ok(())
    }

    fn end(&mut self, out: &mut Emit) -> Result<(), FunctionError> {
        let rows: ArrayRef = Arc::new(Int64Array::from(vec![self.0]));
        out.batch(&RecordBatch::try_from_iter([("rows", rows)])?)
    }
}

fn main() -> ExitCode {
    Rows::default().serve()
}
```

What `end` writes answers no input batch. `end` is called after every batch was answered and
released and the engine posts no more, with no input too; an error from it ends the task as one
from `batch` does. Unless a function implements it, `end` writes nothing.

## How many instances run

A job runs a function one instance per CPU of the job (its `budget.cpu`, or the node's CPUs), each
a process of its own, and deals the batches among them. A function that keeps state across
batches, as `Rows` does, declares the most instances it can run, 1 to 64, once at the top level of
its program:

```rust
moruna_function::processes!(1);
```

It is a ceiling, never a count: a job of fewer CPUs runs fewer. With more than one instance, each
`end` writes what its own instance saw. The declaration is the program's ELF note
(`.note.moruna`, owner `Moruna`, `MORUNA_PROCESSES=N`), which the engine reads before it starts
the function; on a host it declares nothing. A function that declares nothing runs on every CPU.

## Params and arguments

A job gives a function task its `params`, a JSON object (`{"function": "taxi.score", "params":
{"threshold": 0.5}}`), and a run its arguments. A function reads them with
`moruna_function::params()` and `moruna_function::args()`, each a
`Result<serde_json::Map<String, serde_json::Value>, FunctionError>`, so `?` takes them in
`serve`'s closure:

```rust
fn score(batch: RecordBatch, out: &mut Emit) -> Result<(), FunctionError> {
    let threshold = moruna_function::params()?
        .get("threshold")
        .and_then(|t| t.as_f64())
        .ok_or("the params name no threshold")?;
    …
}
```

They are the process's `MORUNA_PARAMS` and `MORUNA_ARGS` (`docs/images.md`), which every function
is given, native or image: on the core std reads them from the process's start slot `env`. Each is
`{}` when the task was given none, or when the program runs outside a job; one that is not a JSON
object is an error naming the variable. On a host, `host::Plane` runs the function in the test's
own process, which sets the variables (`tests/params.rs`).

## What `serve` promises, and what it asks

- **A batch is the function's only during its call.** Its buffers are the engine's memory, which
  the engine writes the next batches into once the task releases the batch. If the function keeps
  any part of it after its call returns (the batch, a column, a slice or a buffer of it), the task
  ends with `<task>: the function kept its input batch …`, exit code 7, and the batch is never
  released. Copy what you keep (`Int64Array::from(a.values().to_vec())`, say).
- **Output.** Each `Emit::batch` copies the batch once, into a window, and returns; the function
  keeps its batch. A batch whose record is bigger than a window is cut by rows into several
  records, in order. One row's record must fit a window (exit code 8 otherwise). A dictionary
  column is refused: a data-plane record carries no dictionaries, so cast it to its values first.
  An error from `Emit::batch` ends the task even if the function ignores it.
- **The log is std's.** On the core std owns the task's log (5A0) and sends standard error to it,
  so `moruna_function::log` writes its line to standard error, as `eprintln!` would, and the SDK
  never takes the log itself. Every line `serve` writes starts with the task's name; a
  function's own lines are written as given.
- **Exit codes,** said on the task's rings and returned for `main` to return
  (`moruna_function::code`):

  | Code | Meaning |
  |---|---|
  | 0 | every batch answered, and the engine posts no more |
  | 2 | not started on the data plane |
  | 3 | the plane holds no rings |
  | 5 | the rings broke |
  | 7 | the function refused a batch: it returned an error, kept its batch, or emitted a batch no record can carry; or the input record failed the engine's record check |
  | 8 | one row of an output batch takes more than a window |

## Test it on your machine

On a host `serve` runs on the data plane in memory: `moruna_function::host::Plane` plays the
engine, posting batches as records, taking back what the task writes through the engine's own
record check, and reading the exit code it says. The task is `main` itself:

```rust
#[test]
fn doubles() {
    let outcome = moruna_function::host::Plane::new().named("double").run([input], main).unwrap();
    assert_eq!(outcome.said, Some(moruna_function::code::DONE));
    assert_eq!(outcome.batches(), [expected]);
}
```

`examples/double/src/main.rs` has the whole test. `cargo test` runs it. `outcome.answers` holds
each input's answer and whether it was released; what a `Function`'s `end` wrote is
`outcome.unheld`, and `outcome.batches()` lists it after the answers.

## Build it

For the core, with Rust's std on Moruna (system design 5A0, item 5.0): the kernel's pinned
nightly, the target `kernel/targets/x86_64-moruna.json`, and std built from Moruna's copy of
Rust's library:

```sh
cd <your crate>
__CARGO_TESTS_ONLY_SRC_ROOT=<moruna>/kernel/vendor/rust-std \
CARGO_TARGET_X86_64_MORUNA_RUSTFLAGS="-C relocation-model=static -C link-arg=-T<moruna>/kernel/sys/user.ld -C link-arg=-static" \
  rustup run nightly-2026-07-21 cargo build --release \
  --target <moruna>/kernel/targets/x86_64-moruna.json -Zjson-target-spec \
  -Zbuild-std=std,panic_abort -Zbuild-std-features=compiler-builtins-mem
```

The function is the one file `target/x86_64-moruna/release/double`, a static ELF. Nothing else is
packaged. (Item 5.0's `kernel/tools/build-std.sh` is the reference for these flags once it lands;
`images/gate/tasks/build.sh` builds the gate's tasks the same way.)

## Register it

Registration (system design 3E, item 3.6) takes that file:

1. Put it where the node can fetch it: an `https://` URL or an object-store URL, its `address`.
2. Its `sha256` is the file's: `shasum -a 256 target/x86_64-moruna/release/double`.
3. Its `signature` is an Ed25519 signature over the 32 bytes of that SHA-256, by a key whose public
   half is one of the node config's `function_keys`.
4. `POST /v1/functions` (admin scope) with `{"name", "address", "sha256", "signature"}`. The node
   checks the signature and stores the entry; nothing is fetched yet.

A job names the function by its registered name. At the job's start the node fetches the file
through the job's network, refuses it if its SHA-256 differs, and keeps it in its function cache.

## Working on the SDK

- `cargo test` in this directory runs the SDK's tests (the logic over a stand-in lane, and `serve`
  end to end on the plane in memory) and the example's.
- `src/sys.rs`, the core's lane over `moruna_sys::plane`, builds only for the core. It type-checks
  on an x86-64 host with
  `RUSTFLAGS="--cfg moruna_function_sys" cargo clippy --lib --target x86_64-apple-darwin`
  (or `x86_64-unknown-linux-gnu`), which builds the crate as the core would and runs nothing.
- `images/gate/tasks` is the SDK's conformance suite: the gate's seven functions on it, compared
  bit for bit with the native path they ran on before.
