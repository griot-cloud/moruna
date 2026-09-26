# Moruna SDD 15: The kernel is checkable (`moruna check`, `moruna-kernels`)

**Document type:** software design document, component 15 (hosted engine, E8)
**Status:** DRAFT · 2026-09-26 (built by F8.3; becomes HANDOFF-READY when section m is empty and the human flips it)
**Parent:** `architecture/moruna-hosted-engine.md` (MH) section 1 P9, section 3 H13, section 4.1 (`kernels[]`, `fingerprint`, kind `std`), section 4.9, section 5 rows 05, 12 and 15, section 8 E8.8
**Preamble:** `00-preamble.md`; **Contracts:** `01-contracts.md` d.7 (`Kernel`, `KernelHints`, `Fingerprint`), and the declaration types of 05 e.5
**Component location:** the harness in `crates/moruna-runtime/src/check/` (`mod.rs`, `synth.rs`); the standard kernels in `crates/moruna-kernels`; the Python command in `python/moruna/_check.py` and `python/moruna/__main__.py`; the Python bindings in `crates/moruna-py/src/check.rs` and `std_kernel.rs`
**Consumes:** contracts (1), arena (2), discovery (3, the sampler), trace (4), adapters (5). **Consumed by:** authors (`python -m moruna check`), the `moruna` binary of F8.1 (`moruna_runtime::check::check`), a host's registration path, the facade's profile store reader (11 e.3)

**Decisions worth your eye:** (1) a kernel with no declarations is still a kernel on the library path, and `moruna check` exits 2 on it, because "is this checkable" is the question the command answers and a hosted spec requires the declarations (MH H-Q8); (2) the profile row is written in the controller's own format and at the controller's own key, so a later run finds it with no change to the controller, and it is never written over an existing file, because a run's evidence outranks a check's; (3) the synthetic generator is SplitMix64 over a documented seed derivation, so the bytes a kernel is checked on are reproducible on any host from the seed alone; (4) standard kernels are one Rust type constructed from a name and JSON arguments, so a job document can name one without code and its fingerprint is a function of what the document says.

---

## a. Purpose and boundary

MH P9: an author learns their function is wrong from a failed run, and a platform that wants to register functions has to invent its own check. This component is that check, and the standard kernels that make a large share of user code unnecessary.

It owns: generating synthetic batches from a declared input schema; running a kernel on them in process with an arena and a trace; comparing produced schemas with declared ones; the first profile row; the check report and its exit codes; the twelve standard kernels, their declarations, their fingerprints and their fusion.

It refuses to know: the job document's serialisation (F8.1 owns the serde of `RunSpec`, and this component adds only the Rust variant, e.8); anything about real data (a check is not a guarantee about behaviour on real data, MH 4.9); the controller's decisions (the row it writes is read by the controller exactly as a run's would be).

## b. Vocabulary

**Declaration.** A kernel's `input_schema` and `output_schema` (05 e.5): each absolute (exact), a subset, or, for an output, relative to the input (`adds`, `drops`, `changes`).

**Checkable.** Declares both halves, takes tables, and every declared input type is one the generator of f.1 knows.

**Synthetic batch.** One of the five batches of f.1.

**Refusal.** A batch on which the kernel failed, or whose produced schema disagrees with the declaration (f.3).

**Profile row.** The file of e.4.

**Standard kernel.** A `moruna_kernels::StdKernel` built from a name of e.6 and a JSON object of arguments.

## c. Invariants

**CK-I1. Byte-exact generation.** For a given declared input schema, seed and `preferred_rows` hint, every synthetic batch is identical, value for value and null for null, on every host and every run (f.1).

**CK-I2. A disagreement names the column.** Every refusal of a produced schema names the column and, where both exist, the declared and produced types (f.3, e.3).

**CK-I3. The row is found.** The profile row a check writes is at the path the controller reads for the same fingerprint and the same stage input schema (11 e.3), and a later run with that kernel on that schema loads it (the merged row it writes has `runs` one higher).

**CK-I4. A check never destroys evidence.** An existing profile file is never overwritten (e.4).

**CK-I5. Exit codes are the verdict.** 0 if and only if every kernel checked agreed (e.5).

**CK-I6. Standard kernels declare what they do.** For every standard kernel and every input on which `output_schema` succeeds, the schema `apply` produces equals `output_schema`, and every standard kernel with valid arguments passes `moruna check`.

**CK-I7. Fusion is invisible.** A fused chain produces exactly the batch the unfused chain produces, on every input (f.6).

## d. Interfaces

### d.1 Exposed

```rust
// crate moruna-runtime, module check
pub struct CheckOptions {
    pub name: String, pub kind: &'static str, pub fingerprint_scheme: &'static str,
    pub seed: u64, pub profiles_dir: Option<PathBuf>, pub gil: Option<GilState>,
    pub bind: Option<Box<dyn FnOnce(Arc<dyn Allocator>) + Send>>,
}
impl CheckOptions { pub fn new(name: impl Into<String>) -> CheckOptions; }
pub enum Verdict { Agreed, Refused, NotCheckable }
pub struct CheckReport { /* e.3 */ }
impl CheckReport { pub fn exit_code(&self) -> i32; pub fn to_json(&self) -> serde_json::Value; pub fn summary(&self) -> String; }
/// The harness. Err only when the arena or the trace cannot start; everything the kernel does wrong is in the report.
pub fn check(kernel: Arc<dyn Kernel>, opts: CheckOptions) -> Result<CheckReport>;

// crate moruna-runtime, module spec (e.8)
pub enum KernelEntry { Std(StdKernel) }
impl KernelEntry { pub fn std(name: &str, args: serde_json::Value) -> Result<KernelEntry>; pub fn fingerprint(&self) -> Fingerprint; }
pub fn build_kernels(entries: Vec<KernelEntry>) -> Vec<Arc<dyn Kernel>>;   // fuses (f.6)

// crate moruna-kernels
pub struct StdKernel;   // implements Kernel
impl StdKernel { pub fn new(name: &str, args: &serde_json::Value) -> Result<StdKernel>; pub fn name(&self) -> &str; pub fn args(&self) -> &Value; pub fn is_fused(&self) -> bool; }
pub fn std_fingerprint(name: &str, args: &Value) -> Fingerprint;
pub fn fuse(first: &StdKernel, second: &StdKernel) -> Option<StdKernel>;
pub fn fuse_chain(chain: Vec<StdKernel>) -> Vec<StdKernel>;
pub const NAMES: [&str; 12];
pub const CRATE_VERSION: &str;
```

```python
python -m moruna check <module-or-file> [--kernel NAME] [--json] [--seed N] [--profiles-dir DIR] [--no-profile]
moruna.std.cast(columns, strict=None) ... moruna.std.date_trunc(column, unit, output=None)   # e.6
moruna._core.check_kernel(kernel, *, name=None, seed=0, profiles_dir=None) -> str            # the JSON of e.3 plus "summary"
moruna._core.std_kernel(name, args_json) -> StdKernel
moruna._core.default_profiles_dir() -> str | None
```

### d.2 Consumed

`moruna_kernel::{Kernel, KernelHints, KernelKind, KernelState, Fingerprint, Payload, SourceSchema, declare::*}`; `moruna_arena::{Arena, ArenaConfig}`; `moruna_discovery::{discover, Sampler}`; `moruna_trace::{TraceWriter, TraceConfig}`; `moruna_adapters::PyKernel` (05 d.1) through `moruna-py`; `serde_json`; `sha2` (the standard kernels' fingerprints and `hash`; E2 table addition, reported); `blake3` (`hash(algo="blake3")`).

## e. Data model, formats and state machines

### e.1 The type grammar

A declared type is a string. The canonical spellings are pyarrow's `str(type)`: `null`, `bool`, `int8` to `int64`, `uint8` to `uint64`, `halffloat`, `float`, `double`, `string`, `large_string`, `string_view`, `binary`, `large_binary`, `binary_view`, `date32[day]`, `date64[ms]`, `timestamp[<s|ms|us|ns>]`, `timestamp[<unit>, tz=<zone>]`, `decimal128(<p>, <s>)`, `list<item: <type>>`, `large_list<item: <type>>`. Accepted aliases: `boolean`, `float16`, `float32`, `float64`, `utf8`, `str`, `large_utf8`, `date32`, `date64`, `list<<type>>`. `any` matches every type and generates as `int64`. The parser is `moruna_kernel::declare::parse_type`; its printer, `type_name`, prints the canonical spelling, which is what every report shows.

### e.2 Declarations

`SchemaDecl::Exact(columns)` (a `pyarrow.Schema`: order, names and types, nothing else), `SchemaDecl::Subset(columns)` (a mapping `{column: type}`: these columns with these types, others allowed), `SchemaDecl::Relative { adds, drops, changes }` (an output mapping whose keys are only `adds`, `drops` and `changes`). A column is `(name, TypeDecl, nullable)`; a mapping's columns are nullable. The canonical JSON a fingerprint hashes is `Declared::canonical_json`: `{"input":<d>,"output":<d>}` with `<d>` one of `null`, `{"exact":[[name,type,nullable],...]}`, `{"subset":[...]}`, `{"adds":[...],"changes":[...],"drops":[name,...]}`.

### e.3 The JSON report

One object per kernel, keys in this order when printed by the harness (`serde_json` sorts them in the Python binding, which is the byte-exact form):

| key | value |
|---|---|
| `moruna_check` | `1` |
| `kernel` | the name |
| `kind` | `"python"`, `"std"` or `"rust"` |
| `fingerprint` | `"<scheme>:<64 hex>"`; `sha256` for Python and standard kernels, `blake3` for a Rust kernel's contracts e.6 fingerprint |
| `verdict` | `"agreed"`, `"refused"`, `"not_checkable"` |
| `exit` | 0 or 2 |
| `reason` | `null`, or why the kernel is not checkable or was refused before any batch |
| `seed` | the seed |
| `batches` | per batch run: `name`, `rows_in`, `rows_out`, `bytes_in`, `bytes_out`, `wall_ns`, `amplification` (null for an empty or failed batch), `error` (null, or the kernel's error text) |
| `refusals` | per disagreement: `batch`, `column`, `reason` (`type`, `missing`, `undeclared`, `position`), `declared`, `produced`, `message` |
| `profile` | null unless agreed; else `path`, `written`, `schema_hash`, `a_k_p50`, `a_k_p95`, `a_k_var`, `samples`, `state_bytes_max`, `wall_ns_per_row`, `gil` |
| `trace_records` | records the trace held at the end, one per batch run |
| `notes` | strings |
| `summary` | (Python binding only) the human summary |

`python -m moruna check --json` prints `{"moruna_check": 1, "target", "exit", "error", "kernels": [report, ...]}` with a two-space indent. The human form prints each report's summary: `"<name> (<kind>): <verdict>"`, the fingerprint, one line per failed batch and per refusal (`"batch <b>: column `<c>`: declared <t>, produced <u>"`), then the profile line and the path written.

### e.4 The profile row

Path: `<profiles_dir>/<fingerprint hex>-<schema hash hex>.json`, where the schema hash is `SourceSchema::hash()` (contracts d.4) of the synthetic input schema of f.1, which is the declared input with `any` as `int64`: the controller's key (11 e.3) for a run whose stage input schema is that schema. The body is the controller's format, version 1, with `a_k_p50`, `a_k_p95` (nearest rank over the amplification samples of f.4), `a_k_dev_p95 = 0`, `a_k_samples` (the sample count), `a_k_var` (sample variance), `state_bytes_max`, `final_target = 0`, `final_workers = 1`, `final_safety = 1.5` (the preamble's initial safety), `runs = 1`, `prediction_error_p95 = 0`, and five fields the controller ignores: `source: "moruna check"`, `check_format: 1`, `check_fingerprint`, `wall_ns_per_row`, `gil` (`"free_threaded"`, `"serialised"` or `"none"`). Written through a temporary file and a rename; not written when the file exists (CK-I4), with a note; not written for a refused or uncheckable kernel. The default directory is the run's default, `~/.moruna/profiles` (12 f.1).

Because the key is the exact input schema, a row written from a subset declaration is found by a run whose source schema is exactly the synthetic one; a run on a wider schema starts from the kernel's hints as before. A fingerprint-only fallback in the controller would lift that and is 11's to add (CK-O1).

### e.5 Exit codes

0: every kernel found agreed. 2: any kernel refused or not checkable, no kernel found, or the module failed to load (its exception in `error`). `argparse` usage errors are its own exit 2.

### e.6 Standard kernels

| name | arguments | declared input | declared output | amplification hint |
|---|---|---|---|---|
| `cast` | `columns: {col: type}`, `strict: bool = false` (strict fails a value that does not convert; otherwise it becomes null) | subset, each `any` | relative, `changes` = the casts | 1.0 |
| `rename` | `columns: {old: new}` | subset, the old names `any` | relative, `drops` old, `adds` new as `any` | 0.0 |
| `select` | `columns: [col]` | subset, `any` | exact, the columns in order, `any` | 0.0 |
| `drop` | `columns: [col]` | subset, `any` | relative, `drops` | 0.0 |
| `filter` | `expr: str` (grammar in `moruna-kernels/src/expr.rs`) | subset, a column compared with a literal has the literal's type, others `any` | relative, unchanged | 1.0 |
| `fill_null` | `values: {col: number or string or bool}` | subset, each the value's type | relative, unchanged | 1.0 |
| `dedupe` | `keys: [col]`; stateful, one instance, first seen kept across the run, `ResumePolicy::Checkpoint` | subset, `any` | relative, unchanged | 1.5 |
| `hash` | `columns: [col]`, `algo: "sha256" | "blake3"`, `output: str = "hash"` | subset, `any` | relative, `adds` output `string` | 1.5 |
| `mask` | `columns: [col]`, `mode: "redact" | "partial" | "null" | "hash"`, `keep: int = 4` (partial only) | subset, `string` (`any` for `null`) | relative, unchanged | 1.0 |
| `explode` | `column: str` (a `list`) | subset, `list<item: int64>` | relative, `changes` the column to `any` | 2.0 |
| `concat_str` | `columns: [col]`, `separator: str = ""`, `output: str = "concat"` | subset, `any` | relative, `adds` output `string` | 1.0 |
| `date_trunc` | `column: str`, `unit: year | month | day | hour | minute | second`, `output: str` (default in place) | subset, `timestamp[us]` | relative, unchanged, or `adds` output | 1.0 |

A concrete type in a standard kernel's input declaration is the type the check generates; at run time the kernel accepts every type its `output_schema` accepts (a `mask` of a `large_string`, a `date_trunc` of a `date32`). Every standard kernel reports `releases_gil = Some(true)`. `hash` encodes each row as, per column in order, `0x00` for null, else `0x01`, the value's byte length as a little-endian u64 and its bytes (binary columns as they are, everything else cast to `string`), and hashes that. `explode` gives a null or empty list one row with a null element. `date_trunc` truncates in UTC; a result that does not fit its type is null.

**Fingerprint:** `sha256("moruna-std\0" ‖ name ‖ "\0" ‖ canonical args ‖ "\0" ‖ CRATE_VERSION)`, where canonical args is the JSON object with keys sorted by UTF-8 bytes at every depth and no whitespace; arguments left out are not written, so `moruna.std.hash(columns=["a"])` and `{"kind":"std","name":"hash","args":{"columns":["a"]}}` share a fingerprint. A fused kernel's is `sha256("moruna-std-fused\0" ‖ fp(first) ‖ fp(second))`, its name is `first+second`, its args `{"stages": [{"name","args"}, ...]}`.

### e.7 The Python kernel fingerprint (amends 05 e.4)

`sha256("moruna-kernel\0" ‖ canonical source ‖ "\0" ‖ lockfile bytes, or nothing when none is given ‖ "\0abi:" ‖ ABI_VERSION as decimal)`, where canonical source is `"py:" ‖ qualname ‖ "\n" ‖ source ‖ "\n" ‖ config`: the qualified name of the function (or of the class of a class kernel; of the undecorated function for a Polars kernel), its source text from `inspect.getsource` dedented, with `\r\n` as `\n`, trailing spaces stripped from every line and leading and trailing blank lines removed (or the code object stand-in of 05 e.4), and config the decorator arguments as canonical JSON followed by `Declared::canonical_json`. The module name is not part of it: `moruna check file.py` and a program that imports the same file under another name must agree. `moruna_kernel::ABI_VERSION` is 1.

### e.8 `RunSpec.kernels[]` kind `std`

`moruna_runtime::spec::KernelEntry::Std(StdKernel)` is the Rust variant of MH 4.1's `{ "kind": "std", "name": ..., "args": {...} }`; `KernelEntry::std(name, args)` builds it and `build_kernels` turns a list of entries into the `Vec<Arc<dyn Kernel>>` `RunSpec::kernels` holds, fusing adjacent standard kernels. The JSON field, its serde and the enforcement of `kernels[].fingerprint` at load are F8.1's; the seam is `KernelEntry::fingerprint`, which returns what a document pins.

## f. Algorithms and policies

**f.1 Synthetic batches.** The input schema is the declared input's columns in declaration order, `any` as `int64`, each nullable as declared. Five batches, in order, index 0 to 4: `empty` (0 rows), `one_row` (1 row, random, no nulls), `preferred` (`preferred_rows` rows, the kernel's hint or 1024, clamped to 1..65536; random, a nullable column null where a draw is a multiple of 8), `all_null` (16 rows; every nullable column null, the rest random), `edges` (as many rows as the longest edge list among the columns; each column cycles its type's edge list). Each column of each batch has its own generator, SplitMix64 with initial state `seed XOR (((batch << 32) | column) * 0x9E3779B97F4A7C15)` (wrapping), each output `z = state += 0x9E3779B97F4A7C15; z = (z ^ z>>30) * 0xBF58476D1CE4E5B9; z = (z ^ z>>27) * 0x94D049BB133111EB; z ^ z>>31`. Per row, a column that can be null at random draws once for the null test, then draws its value. Values: bool `draw & 1`; integers the draw truncated; floats `(draw >> 11) / 2^53 * 2e6 - 1e6`; strings a length `draw % 17` then each character `ALNUM[draw % 62]` over `A-Za-z0-9`; binary a length `draw % 17` then each byte the low byte of a draw; `date32` `draw % 47482` days; `date64` that in milliseconds; timestamps `draw % 4102444800` seconds scaled to the unit; `decimal128(p, s)` `draw % 10^min(p, 18)`; a list a length `draw % 4` (edges: row mod 3) whose items come from the item type's generator, which continues the same stream. Edge lists: bool `false, true`; signed `MIN, MAX, 0, -1, 1`; unsigned `0, MAX, 1`; floats `0, -0, NaN, +inf, -inf, MIN_POSITIVE, MAX, MIN`; strings `"", "a", "x" * 1024, "é中😀", " "`; binary `[], [0], [0xff] * 1024`; dates and timestamps the epoch, one unit either side of it, 0001-01-01 and 9999-12-31T23:59:59 (for nanoseconds, which cannot hold those, `i64::MIN + 1` and `i64::MAX`); decimals `0, 10^p - 1, -(10^p - 1)`. Any other type makes the kernel not checkable, naming the column and the type.

**f.2 Load and run.** Python: import the file under its stem (its directory on `sys.path`) or the dotted module; the kernels are the module's public `KernelSpec` and `StdKernel` values and its public functions defined in that module whose signature is Polars frame to frame (wrapped as the decorator wraps them), in definition order, or the one `--kernel` names. Each is handed to the harness: discovery for the page size and the sampler; an arena of twice the synthetic inputs plus 32 MiB, rounded to the page granule; a trace writer in memory. A stateful kernel gets one instance, `init` with instance 0. Each batch is copied into arena buffers (as a source's decoder would have written it), wrapped with `Payload::table_with`, and `apply`d; a trace record per batch carries the rows, bytes, wall time, anonymous memory before and after, and the outcome (`Probe`, or `Error` with the message).

**f.3 Comparison.** The output declaration is resolved against the synthetic input schema: exact stands, and its columns' positions are compared; subset stands, positions are not compared and other columns are allowed; relative becomes the input's columns less `drops`, `changes` in place, `adds` after, positions not compared and other columns disagree. A relative declaration that names a column the input lacks refuses the kernel before any batch. Per expected column: absent is `missing`; present with a different type (an `any` accepts every type) is `type`; at a different position under exact is `position`. Under exact or relative, a produced column not expected is `undeclared`. Nullability is not compared: Arrow producers disagree about it more than kernels do. A batch whose kernel failed is a refusal carrying the error; a tensor returned for a table declaration is a failure.

**f.4 Profile.** Per batch of at least 4096 input bytes that ran (`MIN_SAMPLE_BYTES`; below it a batch's fixed costs, alignment and validity bitmaps, are its whole output): `amplification = bytes of output buffers not in the arena / bytes_in`, the bytes the kernel allocated for its output per input byte. The process's anonymous memory before and after is recorded in the trace record and is not in the figure: at a check's batch sizes it is allocator noise (a first draft that used it recorded a p95 of 10^5 for a kernel that doubles a column), and a seed that is too low is corrected by the run's first probe while one that is absurdly high refuses the run. A check in which no batch reaches the threshold writes no row and says so. `wall_ns_per_row` is the wall time of the non-empty batches over their rows. `state_bytes_max` is the largest `footprint()` after any batch, or the declared `state_bytes`, whichever is larger. `gil` is the Python kernel's `gil_state()`, `"none"` for a kernel that never enters the interpreter.

**f.5 Fingerprint.** Printed in every report and recorded in the row (e.4, e.6, e.7). The profile file is keyed by the same 32 bytes, which is `Kernel::fingerprint()`, so the row and a run agree by construction.

**f.6 Fusion.** Two adjacent standard kernels fuse when both are projections (`cast`, `rename`, `select`, `drop`, or a fused projection), which become one projection whose plan is computed per input schema so a column a later step drops is never cast; and when `filter` follows `fill_null`, which becomes one pass that fills only the columns the predicate reads, evaluates it, filters, then fills the surviving rows. `fuse_chain` applies this left to right. A fused kernel declares the union of its parts' inputs (less the columns an earlier part adds) and, as its output, exactly what it produces from that input. The Python surface fuses adjacent standard kernels in `moruna.run`; `build_kernels` does the same for a spec.

## g. Concurrency within the component

The harness is single threaded and runs one kernel at a time; `check_kernel` in `moruna-py` releases the interpreter around it, so a Python kernel attaches in `apply` as it would on a worker. Checks in one process do not share an arena or a trace; scratch directories are per process and per check.

## h. Behaviour

**Normal path.** Declare, check, read the fingerprint, run: the first run seeds its amplification from the row and merges its own figures into it.

**Edge cases.** A kernel with no declarations: `not_checkable`, exit 2, the reason names what is missing. A tensor kernel: `not_checkable`. A kernel whose `output_schema` rejects its own declared input: refused before any batch. A kernel that raises on the edges batch: refused, the error in that batch. An existing profile file: kept, noted.

**Failures.** The module does not import: exit 2 with the exception. The arena or the trace cannot start: the harness returns `Err`, which the binding raises as the mapped `MorunaError` subclass.

## i. Configuration

`--seed` (default 0), `--profiles-dir` (default `~/.moruna/profiles`), `--no-profile`. No environment variable.

## j. Observability

The report (e.3); one trace record per batch (f.2).

## k. Tests

**CK-T1 h13_refused_names_the_column** (`crates/moruna-runtime/tests/check.rs`). A Rust kernel declaring `adds: doubled int64` that produces `double` is refused; the refusal names `doubled`, `int64` and `double`; exit 2. CK-I2, H13.

**CK-T2 h13_profile_row_read_by_the_sizer** (`crates/moruna-runtime/tests/check.rs`). A kernel declaring its input exactly as a Parquet file's schema is checked (row written, `runs` 1), then run over that file with the same profile directory; the row afterwards has `runs` 2 and more samples. CK-I3, H13.

**CK-T3 generation_is_byte_exact** (`check/synth.rs` unit tests). CK-I1.

**CK-T4 profile_never_overwritten** (`crates/moruna-runtime/tests/check.rs`). CK-I4.

**CK-T5 every_std_kernel_passes_check** (`crates/moruna-runtime/tests/check.rs`, and `python/tests/test_check.py` through the command). CK-I5, CK-I6.

**CK-T6 std_output_schema_is_exact** (`crates/moruna-kernels/tests/std_kernels.rs`). CK-I6.

**CK-T7 fusion_is_invisible** (`crates/moruna-kernels/tests/fusion.rs`). `select` after `cast`, `filter` after `fill_null`, a four-step projection, on synthetic and hand-written batches: fused output equals the sequential output; the fused stage count is lower. CK-I7.

**CK-T8 polars_signature_checks_and_runs** (`python/tests/test_check.py`, on an interpreter that has Polars; skipped with its reason otherwise). A `pl.DataFrame -> pl.DataFrame` function passes `moruna check` and runs through `moruna.run`; a `LazyFrame` one too. The Rust path: `moruna-polars` `frame_kernel` through the harness (`crates/moruna-polars/tests/frame_kernel.rs`). MH 4.9.

**CK-T9 cli_exit_codes** (`python/tests/test_check.py`). Agreeing module 0; a refused kernel 2 with the column in the output; no kernels 2; `--json` parses; `--kernel` selects.

## l. Implementation notes

`unsafe` is not used anywhere in this component. The harness depends on the arena, discovery and trace crates, which `moruna-runtime` already does. `moruna-kernels` depends on `moruna-kernel`, `serde_json`, `sha2` and `blake3`.

## m. Open items

None. (Two seams this component leaves to others are CK-O1 and CK-O2 in section o.)

## n. Traceability

| Parent id | Invariant | Test |
|---|---|---|
| MH H13, P9 | CK-I2, CK-I3 | CK-T1, CK-T2 |
| MH 4.9 synthetic batch | CK-I1 | CK-T3 |
| MH 4.9 profile | CK-I3, CK-I4 | CK-T2, CK-T4 |
| MH 4.9 exit codes | CK-I5 | CK-T5, CK-T9 |
| MH 4.9 standard kernels | CK-I6, CK-I7 | CK-T5, CK-T6, CK-T7 |
| MH 4.9 Polars | 05 f.9 | CK-T8 |

## o. Deferred

**CK-O1. Fingerprint-only profile fallback (11).** A row from a subset declaration is keyed by the synthetic schema (e.4); a controller fallback to a row keyed by fingerprint alone would let it seed runs on wider schemas. Not built here because 11 was F8.2's while this was built.

**CK-O2. `kernels[].fingerprint` enforcement at load (F8.1).** The seams are `KernelEntry::fingerprint` and `KernelSpec.fingerprint`; the comparison belongs to the loader.

A property-based generator (more batches, shrinking) and a `--rows` override are deliberate omissions: a check is a first measurement, not a fuzzer.
