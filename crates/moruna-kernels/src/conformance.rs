//! The kernel catalogue's two formats, the specification and the conformance corpus, and the
//! runner that reads and checks them (system design 4A).
//!
//! A worked example of both is `tests/conformance_example/`: `example.add@1`, exact, and
//! `example.scale@1`, whose cases carry a per-element tolerance.
//!
//! # The specification: `spec/<name>.md`
//!
//! One Markdown file per kernel, named by the kernel's name; [`SPEC_TEMPLATE`] is the template
//! (`spec/_template.md`). A file whose name starts with `_` is not a specification.
//!
//! - The file has one block per published version, in order from 1. A block starts with a
//!   level-1 heading, the identity: `# moruna.filter@1`. Nothing but an HTML comment comes before
//!   the first block.
//! - A block has the eleven level-2 headings of [`SPEC_HEADINGS`], every one, in that order, each
//!   once, and no other; deeper headings may divide a section. No section is empty.
//! - Three sections carry a line the runner reads (`- key: value`, backticks optional):
//!   - **Identity:** `- namespace: moruna`, `- name: filter` and `- version: 1`, which must be
//!     the block's heading, and the name the file's.
//!   - **State shape:** `- shape: none`, one of [`StateShape`]'s four (system design 4C):
//!     `none`, `accumulate-merge-finish`, `build-once-read-many`, `per-partition-ordered`.
//!   - **Tolerance:** the [`Precision`] the version declares, one of:
//!     - `- declared: none`: every output is compared bit for bit;
//!     - `- declared: {"y": {"abs": 1e-6, "rel": 0.0}}`: by output column, the [`Bound`] within
//!       which an implementation's answer may differ from the expected one;
//!     - `- declared: per-element`: a case may give, for an output column, the absolute error
//!       allowed at each element (`"allowed"`, below); every other column is bit for bit.
//! - HTML comments and fenced code blocks are not read for headings.
//!
//! # The conformance corpus: `conformance/<name>/`
//!
//! One directory per kernel, named as its specification, holding `index.json` and the case
//! files, Arrow IPC files (the file format, `.arrow`). Any other file there (the script that
//! wrote the cases) is not read, and every `.arrow` file must be named by a case.
//!
//! `index.json` is one object; every member not listed here is refused:
//!
//! - `"format"`: `"moruna-kernel-corpus/1"` ([`CORPUS_FORMAT`]).
//! - `"kernel"`: the kernel without its version, `"moruna.filter"`.
//! - `"tolerance"` (optional): by version, as a string, the same object as the specification's
//!   `declared` line for that version: `{"1": {"y": {"abs": 1e-6, "rel": 0.0}}}`. A version
//!   that is absent declares `none` or `per-element`.
//! - `"cases"`: a non-empty list of cases, each an object:
//!   - `"name"`: unique in the corpus, `[a-z][a-z0-9_]*`.
//!   - `"version"`: the kernel version it belongs to. An implementation claims a version only
//!     when it passes every case of that version.
//!   - `"tests"`: what it tests, a non-empty list of tags `[a-z][a-z0-9_]*`, the common ones
//!     being `nulls`, `empty`, `sliced`, `all_true`, `all_false`, `dictionary`, `nested`,
//!     `overflow`, `rounding`, `ordering`, `params`, `multi_batch` and `error`. Every version's
//!     cases together must include [`REQUIRED_TAGS`].
//!   - `"about"`: one sentence saying what the case checks.
//!   - `"params"` (optional, default `{}`): the JSON parameters given at configure.
//!   - `"arrays"` (optional, default `{}`): the parameters that are Arrow data, by name, each a
//!     source (below) of exactly one batch.
//!   - `"inputs"`: one source per input port, port 0 first. Each batch of a source's file is one
//!     call of the kernel on that port; a file of no batches makes no call.
//!   - Exactly one of:
//!     - `"expected"`: the file of the expected output. Its schema is the output schema the
//!       kernel must declare and give (names, types and nullability; metadata is not compared),
//!       and its batches, concatenated, are the rows every call's output gives, concatenated.
//!       Where the batch boundaries fall is not compared.
//!     - `"error"`: `{"at": "configure" | "schema" | "apply", "contains": "..."}`: the kernel
//!       must refuse the case at that step (`configure` the parameters; `schema` the inputs'
//!       schemas at plan; `apply` the batches, `finish` included). `contains`, optional, is text
//!       the error must contain.
//!   - `"allowed"` (optional, with `"expected"`, in a version that declares `per-element`): by
//!     output column, a source of one column holding the absolute error allowed at each element
//!     of that column: `{"y": {"file": "x.allowed.arrow"}}`. Its batches, concatenated, are as
//!     long as the expected output, and its type is the output column's with every float
//!     replaced by `Float64` (a `FixedSizeList<Float32>` column's is `FixedSizeList<Float64>` of
//!     the same size, one value per element). Each value is a number from 0, or null. An
//!     element passes when `|got - expected| <= allowed`, NaN only matching NaN and an infinity
//!     only itself; where the allowed value is null, and in every column it does not name, the
//!     comparison is bit for bit. Nulls in the output must be where the expected nulls are.
//!
//! A source is `{"file": "x.arrow"}` or `{"file": "x.arrow", "slice": [offset, length]}`: a file
//! of the corpus directory, by its plain name, and optionally a slice taken of every batch of it
//! before it is given, so a case can give arrays whose offset is not zero (IPC writes none).
//!
//! ```json
//! {
//!   "format": "moruna-kernel-corpus/1",
//!   "kernel": "example.add",
//!   "cases": [
//!     {
//!       "name": "nulls_stay_null",
//!       "version": 1,
//!       "tests": ["nulls"],
//!       "about": "A null x gives a null, and the others x + addend.",
//!       "params": {"addend": 10},
//!       "inputs": [{"file": "nulls.in.arrow"}],
//!       "expected": "nulls.expected.arrow"
//!     }
//!   ]
//! }
//! ```
//!
//! # The runner
//!
//! [`check`] reads every specification and corpus under a root and checks the formats, that
//! each specification has a corpus and each corpus a specification, that they agree on the
//! versions and tolerances (a case gives `allowed` only in a version that declares
//! `per-element`), and that every version has a CPU reference
//! ([`crate::catalogue::CPU_REFERENCE`]) among the implementations and no implementation claims
//! a version with no specification.
//!
//! [`run`] then runs every case of every version each implementation claims, as the engine
//! would drive the kernel: configure it with the case's parameters; check the output schema it
//! declares for the inputs' schemas; one instance (`init`); every batch to `apply` (one input),
//! to `apply_aligned` by position (several inputs joined by morsel) or to `apply_port`, port 0's
//! first (several inputs); then `finish` for a stateful kernel. Its outputs, concatenated, are
//! compared with the expected; a refusal must come at the step the case names. An
//! implementation that fails one case of a version cannot claim that version:
//! [`Report::verdict`] names every such case and why.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::{Path, PathBuf};

use std::sync::Arc;

use moruna_kernel::arrow::array::{Array, ArrayRef, AsArray, new_empty_array};
use moruna_kernel::arrow::compute::{cast, concat, concat_batches};
use moruna_kernel::arrow::datatypes::{DataType, Field, FieldRef, Float64Type, Schema, SchemaRef};
use moruna_kernel::arrow::ipc::reader::FileReader;
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::arrow::util::display::array_value_to_string;
use moruna_kernel::{
    Allocator, InitCtx, Kernel, KernelKind, KernelState, MorunaError, Payload, SourceSchema,
};
use serde_json::{Map, Value};

use crate::catalogue::{CPU_REFERENCE, Implementation, KernelId, Params, is_word, parse_version};

/// The specification template, `spec/_template.md`.
pub const SPEC_TEMPLATE: &str = include_str!("../spec/_template.md");

/// The level-2 headings of every version block of a specification, in order.
pub const SPEC_HEADINGS: [&str; 11] = [
    "Identity",
    "Signature",
    "Nulls",
    "Overflow",
    "Rounding",
    "Ordering",
    "Empty input",
    "State shape",
    "Most output",
    "State kept",
    "Tolerance",
];

/// The `format` of a corpus index.
pub const CORPUS_FORMAT: &str = "moruna-kernel-corpus/1";

/// The tags every version's cases must include between them.
pub const REQUIRED_TAGS: [&str; 3] = ["nulls", "empty", "sliced"];

/// A kernel's state shape (system design 4C).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum StateShape {
    /// No state: `KernelKind::Stateless`.
    None,
    /// Accumulate, merge and finish: `KernelKind::Stateful`, the result given by `finish`.
    AccumulateMergeFinish,
    /// Build once and read many (joins, later).
    BuildOnceReadMany,
    /// Per-partition ordered state (windows, later).
    PerPartitionOrdered,
}

impl StateShape {
    /// The shape as a specification writes it.
    pub fn parse(text: &str) -> Option<StateShape> {
        Some(match text {
            "none" => StateShape::None,
            "accumulate-merge-finish" => StateShape::AccumulateMergeFinish,
            "build-once-read-many" => StateShape::BuildOnceReadMany,
            "per-partition-ordered" => StateShape::PerPartitionOrdered,
            _ => return None,
        })
    }
}

/// How far a value of an output column may differ from the expected one: `|v - e| <= abs +
/// rel * max(|v|, |e|)`. NaN matches only NaN, an infinity only itself, a null only a null.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Bound {
    /// The absolute part.
    pub abs: f64,
    /// The relative part.
    pub rel: f64,
}

impl Bound {
    /// Whether `value` is within this bound of `expected`.
    pub fn within(&self, value: f64, expected: f64) -> bool {
        if value.is_nan() || expected.is_nan() {
            return value.is_nan() && expected.is_nan();
        }
        if value.is_infinite() || expected.is_infinite() {
            return value == expected;
        }
        (value - expected).abs() <= self.abs + self.rel * value.abs().max(expected.abs())
    }
}

/// A declared tolerance: a bound by output column.
pub type Tolerance = BTreeMap<String, Bound>;

/// Read a tolerance object, `{"col": {"abs": a, "rel": r}}`.
fn tolerance_of(value: &Value) -> Result<Tolerance, String> {
    let Value::Object(columns) = value else {
        return Err(format!("a tolerance is an object by column, not {value}"));
    };
    if columns.is_empty() {
        return Err("a tolerance names at least one column (write `none` for none)".into());
    }
    let mut out = Tolerance::new();
    for (column, bound) in columns {
        let Value::Object(parts) = bound else {
            return Err(format!(
                "the tolerance of `{column}` is {{\"abs\": a, \"rel\": r}}"
            ));
        };
        let get = |key: &str| {
            parts
                .get(key)
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite() && *v >= 0.0)
                .ok_or_else(|| {
                    format!("the tolerance of `{column}` needs `{key}`, a number from 0")
                })
        };
        if let Some(other) = parts.keys().find(|k| *k != "abs" && *k != "rel") {
            return Err(format!(
                "the tolerance of `{column}` has `{other}`; it has `abs` and `rel` only"
            ));
        }
        out.insert(
            column.clone(),
            Bound {
                abs: get("abs")?,
                rel: get("rel")?,
            },
        );
    }
    Ok(out)
}

/// The precision a version declares: how far an implementation's answer may differ from the
/// expected one.
#[derive(Clone, Debug, PartialEq)]
pub enum Precision {
    /// `none`: bit for bit.
    Exact,
    /// A [`Bound`] by output column; every other column bit for bit.
    Bounds(Tolerance),
    /// `per-element`: a case may give the absolute error allowed at each element of an output
    /// column (`"allowed"`); every other column, and every element whose allowed value is null,
    /// bit for bit.
    PerElement,
}

impl Precision {
    /// The bounds by column, for [`Precision::Bounds`].
    pub fn bounds(&self) -> Option<&Tolerance> {
        match self {
            Precision::Bounds(t) => Some(t),
            Precision::Exact | Precision::PerElement => None,
        }
    }
}

/// One version of a kernel as its specification states it.
#[derive(Clone, Debug, PartialEq)]
pub struct SpecVersion {
    /// The block's identity.
    pub id: KernelId,
    /// Its state shape.
    pub shape: StateShape,
    /// Its declared precision.
    pub precision: Precision,
}

/// A kernel's specification: every version it publishes.
#[derive(Clone, Debug)]
pub struct Spec {
    /// The file.
    pub path: PathBuf,
    /// Its versions, from 1, in order.
    pub versions: Vec<SpecVersion>,
}

impl Spec {
    /// The version `version`, when published.
    pub fn version(&self, version: u32) -> Option<&SpecVersion> {
        self.versions.iter().find(|v| v.id.version == version)
    }
}

/// A version block of a specification: its heading, and its sections in order.
struct Block {
    title: String,
    line: usize,
    sections: Vec<(String, Vec<String>)>,
}

/// The version blocks of a specification's text, without HTML comments and fenced code; the
/// problems with its outline before any block is read.
fn blocks(text: &str) -> (Vec<Block>, Vec<String>) {
    let mut out: Vec<Block> = Vec::new();
    let mut problems = Vec::new();
    let mut in_comment = false;
    let mut in_fence = false;
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim_end();
        if in_comment {
            in_comment = !line.contains("-->");
            continue;
        }
        if line.trim_start().starts_with("<!--") {
            in_comment = !line.contains("-->");
            continue;
        }
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        if let Some(title) = line.strip_prefix("# ") {
            if in_fence {
                continue;
            }
            out.push(Block {
                title: title.trim().to_string(),
                line: n + 1,
                sections: Vec::new(),
            });
            continue;
        }
        let Some(block) = out.last_mut() else {
            if !line.trim().is_empty() {
                problems.push(format!(
                    "line {}: text before the first `# <namespace>.<name>@<version>` heading",
                    n + 1
                ));
            }
            continue;
        };
        if !in_fence && let Some(heading) = line.strip_prefix("## ") {
            block
                .sections
                .push((heading.trim().to_string(), Vec::new()));
            continue;
        }
        // Text before a block's first section is its summary, which is not read.
        if let Some((_, body)) = block.sections.last_mut() {
            body.push(line.to_string());
        }
    }
    (out, problems)
}

/// The problems with a block's headings: every one of [`SPEC_HEADINGS`], in order, once each,
/// and no other; no empty section.
fn heading_problems(block: &Block) -> Vec<String> {
    let mut problems = Vec::new();
    let got: Vec<&str> = block.sections.iter().map(|(h, _)| h.as_str()).collect();
    if got != SPEC_HEADINGS {
        problems.push(format!(
            "`# {}` (line {}) has the sections {got:?}; a version block has exactly {:?}, in \
             that order",
            block.title, block.line, SPEC_HEADINGS
        ));
    }
    for (heading, body) in &block.sections {
        if body.iter().all(|l| l.trim().is_empty()) {
            problems.push(format!("`# {}`: `## {heading}` is empty", block.title));
        }
    }
    problems
}

/// The value of the line `- key: value` in a section, backticks removed.
fn keyed(body: &[String], key: &str) -> Option<String> {
    body.iter().find_map(|line| {
        let rest = line.trim().strip_prefix("- ")?.trim_start();
        let value = rest.strip_prefix(key)?.strip_prefix(':')?;
        Some(value.trim().trim_matches('`').trim().to_string())
    })
}

/// The problems with a specification template's outline (the headings of each block).
pub fn check_template(text: &str) -> Vec<String> {
    let (blocks, mut problems) = blocks(text);
    if blocks.is_empty() {
        problems.push("no `# <namespace>.<name>@<version>` block".into());
    }
    for block in &blocks {
        problems.extend(heading_problems(block));
    }
    problems
}

/// Read the specification at `path`; the problems with it when it does not follow the format.
pub fn read_spec(path: &Path) -> Result<Spec, Vec<String>> {
    let text =
        std::fs::read_to_string(path).map_err(|e| vec![format!("{}: {e}", path.display())])?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let (blocks, mut problems) = blocks(&text);
    if blocks.is_empty() {
        problems.push("no `# <namespace>.<name>@<version>` block".into());
    }
    let mut versions = Vec::new();
    for (at, block) in blocks.iter().enumerate() {
        let before = problems.len();
        problems.extend(heading_problems(block));
        let id = match KernelId::parse(&block.title) {
            Ok(id) => id,
            Err(e) => {
                problems.push(format!("line {}: {e}", block.line));
                continue;
            }
        };
        if id.name != stem {
            problems.push(format!(
                "`# {id}` is in {stem}.md; a kernel's file is its name"
            ));
        }
        if id.version as usize != at + 1 {
            problems.push(format!(
                "`# {id}` is block {}; versions are 1, 2, ... in order",
                at + 1
            ));
        }
        if let Some(first) = versions.first().map(|v: &SpecVersion| &v.id)
            && first.namespace != id.namespace
        {
            problems.push(format!("`# {id}` is in another namespace than `# {first}`"));
        }
        if problems.len() != before {
            continue;
        }
        let section = |name: &str| {
            block
                .sections
                .iter()
                .find(|(h, _)| h == name)
                .map(|(_, b)| b.as_slice())
                .unwrap_or_default()
        };
        let identity = section("Identity");
        for (key, want) in [
            ("namespace", id.namespace.clone()),
            ("name", id.name.clone()),
            ("version", id.version.to_string()),
        ] {
            match keyed(identity, key) {
                Some(got) if got == want => {}
                Some(got) => problems.push(format!(
                    "`# {id}`: Identity says {key} `{got}`, the heading `{want}`"
                )),
                None => problems.push(format!("`# {id}`: Identity has no `- {key}:` line")),
            }
        }
        let shape = match keyed(section("State shape"), "shape") {
            Some(text) => match StateShape::parse(&text) {
                Some(shape) => shape,
                None => {
                    problems.push(format!(
                        "`# {id}`: State shape `{text}` is not none, accumulate-merge-finish, \
                         build-once-read-many or per-partition-ordered"
                    ));
                    continue;
                }
            },
            None => {
                problems.push(format!("`# {id}`: State shape has no `- shape:` line"));
                continue;
            }
        };
        let precision = match keyed(section("Tolerance"), "declared").as_deref() {
            Some("none") => Precision::Exact,
            Some("per-element") => Precision::PerElement,
            Some(json) => match serde_json::from_str::<Value>(json)
                .map_err(|e| format!("not none, per-element or a JSON object: {e}"))
                .and_then(|v| tolerance_of(&v))
            {
                Ok(t) => Precision::Bounds(t),
                Err(e) => {
                    problems.push(format!("`# {id}`: Tolerance: {e}"));
                    continue;
                }
            },
            None => {
                problems.push(format!("`# {id}`: Tolerance has no `- declared:` line"));
                continue;
            }
        };
        versions.push(SpecVersion {
            id,
            shape,
            precision,
        });
    }
    if problems.is_empty() {
        Ok(Spec {
            path: path.to_path_buf(),
            versions,
        })
    } else {
        Err(problems
            .into_iter()
            .map(|p| format!("{}: {p}", path.display()))
            .collect())
    }
}

/// A file of the corpus and the slice taken of each of its batches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    /// The file's plain name in the corpus directory.
    pub file: String,
    /// `(offset, length)`, taken of every batch before it is given.
    pub slice: Option<(usize, usize)>,
}

/// The step of a case at which the kernel must refuse it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Configure: the parameters.
    Configure,
    /// Plan: the inputs' schemas (`output_schema_of`).
    Schema,
    /// The batches: `apply`, `apply_port`, `apply_aligned` or `finish`.
    Apply,
}

/// What a case expects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expect {
    /// The file of the expected output.
    Output(String),
    /// A refusal at `at`, whose text contains `contains`.
    Error {
        /// The step.
        at: Step,
        /// Text the error must contain.
        contains: Option<String>,
    },
}

/// One case of a corpus.
#[derive(Clone, Debug, PartialEq)]
pub struct Case {
    /// Its name, unique in the corpus.
    pub name: String,
    /// The kernel version it belongs to.
    pub version: u32,
    /// What it tests.
    pub tests: Vec<String>,
    /// One sentence on what it checks.
    pub about: String,
    /// The JSON parameters.
    pub params: Value,
    /// The Arrow parameters, by name.
    pub arrays: BTreeMap<String, Source>,
    /// One source per input port.
    pub inputs: Vec<Source>,
    /// The expected output, or refusal.
    pub expect: Expect,
    /// The absolute error allowed at each element, by output column.
    pub allowed: BTreeMap<String, Source>,
}

/// The type of an `allowed` column for an output column of type `output`: every float replaced
/// by `Float64`, through lists. `None` for a type that has no float to bound.
pub fn allowed_type(output: &DataType) -> Option<DataType> {
    let item = |field: &FieldRef| -> Option<FieldRef> {
        Some(Arc::new(Field::new(
            field.name(),
            allowed_type(field.data_type())?,
            true,
        )))
    };
    Some(match output {
        DataType::Float16 | DataType::Float32 | DataType::Float64 => DataType::Float64,
        DataType::FixedSizeList(field, size) => DataType::FixedSizeList(item(field)?, *size),
        DataType::List(field) => DataType::List(item(field)?),
        DataType::LargeList(field) => DataType::LargeList(item(field)?),
        _ => return None,
    })
}

/// Whether `got` has the shape of `want`, an [`allowed_type`]: the same kinds and list sizes,
/// leaves `Float64`; field names and nullability are not compared.
fn same_shape(got: &DataType, want: &DataType) -> bool {
    match (got, want) {
        (DataType::Float64, DataType::Float64) => true,
        (DataType::FixedSizeList(a, m), DataType::FixedSizeList(b, n)) => {
            m == n && same_shape(a.data_type(), b.data_type())
        }
        (DataType::List(a), DataType::List(b))
        | (DataType::LargeList(a), DataType::LargeList(b)) => {
            same_shape(a.data_type(), b.data_type())
        }
        _ => false,
    }
}

/// The problem with an `allowed` source for `column` of an expected output: one column of the
/// shape [`allowed_type`] gives, as many rows as the expected, every value a number from 0 or
/// null.
fn allowed_problem(
    dir: &Path,
    column: &str,
    source: &Source,
    expected: &(SchemaRef, Vec<RecordBatch>),
) -> Result<(), String> {
    let (want_schema, want) = expected;
    let field = want_schema
        .field_with_name(column)
        .map_err(|_| format!("`allowed` names `{column}`, which the expected output has not"))?;
    let shape = allowed_type(field.data_type()).ok_or_else(|| {
        format!(
            "`allowed` names `{column}`, of type {}, which has no float to bound",
            field.data_type()
        )
    })?;
    let (schema, batches) = read_source(dir, source)?;
    match schema.fields().as_ref() {
        [f] if same_shape(f.data_type(), &shape) => {}
        _ => {
            return Err(format!(
                "the allowed error of `{column}` ({}) is one column of type {shape}",
                source.file
            ));
        }
    }
    let rows: usize = batches.iter().map(RecordBatch::num_rows).sum();
    let want_rows: usize = want.iter().map(RecordBatch::num_rows).sum();
    if rows != want_rows {
        return Err(format!(
            "the allowed error of `{column}` ({}) has {rows} rows, and the expected output \
             {want_rows}",
            source.file
        ));
    }
    for batch in &batches {
        if let Some(bad) = leaves(batch.column(0).as_ref())
            .into_iter()
            .find(|v| !(v.is_finite() && *v >= 0.0))
        {
            return Err(format!(
                "the allowed error of `{column}` ({}) has {bad}; each is a number from 0, or null",
                source.file
            ));
        }
    }
    Ok(())
}

/// Every non-null `Float64` leaf value of an `allowed` array, through lists.
fn leaves(array: &dyn Array) -> Vec<f64> {
    match array.data_type() {
        DataType::Float64 => array
            .as_primitive::<Float64Type>()
            .iter()
            .flatten()
            .collect(),
        DataType::FixedSizeList(..) => {
            let list = array.as_fixed_size_list();
            (0..list.len())
                .filter(|r| list.is_valid(*r))
                .flat_map(|r| leaves(list.value(r).as_ref()))
                .collect()
        }
        DataType::List(_) => {
            let list = array.as_list::<i32>();
            (0..list.len())
                .filter(|r| list.is_valid(*r))
                .flat_map(|r| leaves(list.value(r).as_ref()))
                .collect()
        }
        DataType::LargeList(_) => {
            let list = array.as_list::<i64>();
            (0..list.len())
                .filter(|r| list.is_valid(*r))
                .flat_map(|r| leaves(list.value(r).as_ref()))
                .collect()
        }
        _ => Vec::new(),
    }
}

/// A kernel's conformance corpus.
#[derive(Clone, Debug)]
pub struct Corpus {
    /// The directory.
    pub dir: PathBuf,
    /// The kernel without its version: `moruna.filter`.
    pub kernel: String,
    /// The declared tolerance by version.
    pub tolerance: BTreeMap<u32, Tolerance>,
    /// The cases, in the index's order.
    pub cases: Vec<Case>,
}

/// The batches of an Arrow IPC file, each sliced when `slice` says, and the file's schema.
pub fn read_source(dir: &Path, source: &Source) -> Result<(SchemaRef, Vec<RecordBatch>), String> {
    let path = dir.join(&source.file);
    let file = File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let reader = FileReader::try_new_buffered(file, None)
        .map_err(|e| format!("{}: not an Arrow IPC file: {e}", path.display()))?;
    let schema = reader.schema();
    let mut batches = Vec::new();
    for batch in reader {
        let batch = batch.map_err(|e| format!("{}: {e}", path.display()))?;
        batches.push(match source.slice {
            None => batch,
            Some((offset, length)) => {
                if offset
                    .checked_add(length)
                    .is_none_or(|end| end > batch.num_rows())
                {
                    return Err(format!(
                        "{}: the slice [{offset}, {length}] is outside a batch of {} rows",
                        path.display(),
                        batch.num_rows()
                    ));
                }
                batch.slice(offset, length)
            }
        });
    }
    Ok((schema, batches))
}

/// Refuse any member of `object` not in `known`.
fn only(what: &str, object: &Map<String, Value>, known: &[&str]) -> Result<(), String> {
    match object.keys().find(|k| !known.contains(&k.as_str())) {
        Some(key) => Err(format!(
            "{what} has `{key}`; it has {}",
            known
                .iter()
                .map(|k| format!("`{k}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )),
        None => Ok(()),
    }
}

fn source_of(what: &str, value: &Value) -> Result<Source, String> {
    let Value::Object(object) = value else {
        return Err(format!(
            "{what} is {{\"file\": ..., \"slice\": [offset, length]}}"
        ));
    };
    only(what, object, &["file", "slice"])?;
    let file = object
        .get("file")
        .and_then(Value::as_str)
        .filter(|f| !f.is_empty() && !f.contains(['/', '\\']) && !f.starts_with('.'))
        .ok_or_else(|| format!("{what} needs `file`, the plain name of a file of the corpus"))?;
    let slice = match object.get("slice") {
        None => None,
        Some(Value::Array(pair)) if pair.len() == 2 => match (pair[0].as_u64(), pair[1].as_u64()) {
            (Some(o), Some(l)) => Some((o as usize, l as usize)),
            _ => return Err(format!("{what}: `slice` is [offset, length], two counts")),
        },
        Some(_) => return Err(format!("{what}: `slice` is [offset, length], two counts")),
    };
    Ok(Source {
        file: file.to_string(),
        slice,
    })
}

fn case_of(at: usize, value: &Value) -> Result<Case, String> {
    let what = format!("case {at}");
    let Value::Object(object) = value else {
        return Err(format!("{what} is an object"));
    };
    only(
        &what,
        object,
        &[
            "name", "version", "tests", "about", "params", "arrays", "inputs", "expected", "error",
            "allowed",
        ],
    )?;
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| is_word(n))
        .ok_or_else(|| format!("{what} needs `name`, [a-z][a-z0-9_]*"))?
        .to_string();
    let what = format!("case `{name}`");
    let version = object
        .get("version")
        .and_then(Value::as_u64)
        .filter(|v| (1..=u64::from(u32::MAX)).contains(v))
        .ok_or_else(|| format!("{what} needs `version`, a kernel version from 1"))?
        as u32;
    let tests: Vec<String> = match object.get("tests") {
        Some(Value::Array(tags)) if !tags.is_empty() => tags
            .iter()
            .map(|t| t.as_str().filter(|t| is_word(t)).map(str::to_string))
            .collect::<Option<_>>()
            .ok_or_else(|| format!("{what}: every tag of `tests` is [a-z][a-z0-9_]*"))?,
        _ => return Err(format!("{what} needs `tests`, a non-empty list of tags")),
    };
    let about = object
        .get("about")
        .and_then(Value::as_str)
        .filter(|a| !a.trim().is_empty())
        .ok_or_else(|| format!("{what} needs `about`, what it checks"))?
        .to_string();
    let params = match object.get("params") {
        None => Value::Object(Map::new()),
        Some(p @ Value::Object(_)) => p.clone(),
        Some(_) => return Err(format!("{what}: `params` is an object")),
    };
    let mut arrays = BTreeMap::new();
    match object.get("arrays") {
        None => {}
        Some(Value::Object(named)) => {
            for (param, source) in named {
                arrays.insert(
                    param.clone(),
                    source_of(&format!("{what}: array `{param}`"), source)?,
                );
            }
        }
        Some(_) => return Err(format!("{what}: `arrays` is an object of sources by name")),
    }
    let inputs = match object.get("inputs") {
        Some(Value::Array(ports)) if !ports.is_empty() => ports
            .iter()
            .enumerate()
            .map(|(port, s)| source_of(&format!("{what}: input {port}"), s))
            .collect::<Result<Vec<_>, _>>()?,
        _ => return Err(format!("{what} needs `inputs`, one source per input port")),
    };
    let expect = match (object.get("expected"), object.get("error")) {
        (Some(Value::String(file)), None)
            if !file.is_empty() && !file.contains(['/', '\\']) && !file.starts_with('.') =>
        {
            Expect::Output(file.clone())
        }
        (None, Some(Value::Object(error))) => {
            only(&format!("{what}: `error`"), error, &["at", "contains"])?;
            let at = match error.get("at").and_then(Value::as_str) {
                Some("configure") => Step::Configure,
                Some("schema") => Step::Schema,
                Some("apply") => Step::Apply,
                _ => {
                    return Err(format!(
                        "{what}: `error.at` is \"configure\", \"schema\" or \"apply\""
                    ));
                }
            };
            let contains = match error.get("contains") {
                None => None,
                Some(Value::String(text)) if !text.is_empty() => Some(text.clone()),
                Some(_) => return Err(format!("{what}: `error.contains` is text")),
            };
            Expect::Error { at, contains }
        }
        _ => {
            return Err(format!(
                "{what} needs one of `expected` (the plain name of a file) and `error`"
            ));
        }
    };
    let mut allowed = BTreeMap::new();
    match (object.get("allowed"), &expect) {
        (None, _) => {}
        (Some(Value::Object(columns)), Expect::Output(_)) if !columns.is_empty() => {
            for (column, source) in columns {
                allowed.insert(
                    column.clone(),
                    source_of(&format!("{what}: the allowed error of `{column}`"), source)?,
                );
            }
        }
        (Some(_), Expect::Output(_)) => {
            return Err(format!(
                "{what}: `allowed` is a non-empty object of sources by output column"
            ));
        }
        (Some(_), Expect::Error { .. }) => {
            return Err(format!(
                "{what}: `allowed` goes with `expected`, not with `error`"
            ));
        }
    }
    Ok(Case {
        name,
        version,
        tests,
        about,
        params,
        arrays,
        inputs,
        expect,
        allowed,
    })
}

/// Read the corpus in `dir`; the problems with it when it does not follow the format. Every
/// file it names is read, so a file that is missing, not Arrow IPC, or shorter than a slice is
/// a problem here.
pub fn read_corpus(dir: &Path) -> Result<Corpus, Vec<String>> {
    match read_corpus_and_problems(dir)? {
        (corpus, problems) if problems.is_empty() => Ok(corpus),
        (_, problems) => Err(problems),
    }
}

/// The corpus in `dir` and every problem with its files; `Err` only when its index cannot be
/// read at all. A corpus whose files have problems is still checked against its specification
/// ([`check`]), so a case's other departures are named beside them.
fn read_corpus_and_problems(dir: &Path) -> Result<(Corpus, Vec<String>), Vec<String>> {
    let at = |p: String| format!("{}: {p}", dir.display());
    let index = dir.join("index.json");
    let text =
        std::fs::read_to_string(&index).map_err(|e| vec![format!("{}: {e}", index.display())])?;
    let value: Value =
        serde_json::from_str(&text).map_err(|e| vec![format!("{}: {e}", index.display())])?;
    let Value::Object(object) = &value else {
        return Err(vec![at("index.json is an object".into())]);
    };
    only(
        "index.json",
        object,
        &["format", "kernel", "tolerance", "cases"],
    )
    .map_err(|p| vec![at(p)])?;
    if object.get("format").and_then(Value::as_str) != Some(CORPUS_FORMAT) {
        return Err(vec![at(format!(
            "index.json needs `\"format\": \"{CORPUS_FORMAT}\"`"
        ))]);
    }
    let kernel = object
        .get("kernel")
        .and_then(Value::as_str)
        .and_then(|k| KernelId::parse(&format!("{k}@1")).ok())
        .map(|id| id.kernel())
        .ok_or_else(|| vec![at("index.json needs `kernel`, <namespace>.<name>".into())])?;
    let mut problems = Vec::new();
    let mut tolerance = BTreeMap::new();
    match object.get("tolerance") {
        None => {}
        Some(Value::Object(versions)) => {
            for (version, t) in versions {
                match (parse_version(version), tolerance_of(t)) {
                    (Some(v), Ok(t)) => {
                        tolerance.insert(v, t);
                    }
                    (None, _) => problems.push(at(format!(
                        "`tolerance` is by version; `{version}` is not one"
                    ))),
                    (_, Err(e)) => {
                        problems.push(at(format!("tolerance of version {version}: {e}")))
                    }
                }
            }
        }
        Some(_) => problems.push(at("`tolerance` is an object by version".into())),
    }
    let mut cases = Vec::new();
    match object.get("cases") {
        Some(Value::Array(list)) if !list.is_empty() => {
            for (n, case) in list.iter().enumerate() {
                match case_of(n, case) {
                    Ok(case) => cases.push(case),
                    Err(e) => problems.push(at(e)),
                }
            }
        }
        _ => problems.push(at("index.json needs `cases`, a non-empty list".into())),
    }
    let mut names = BTreeSet::new();
    let mut named = BTreeSet::new();
    for case in &cases {
        if !names.insert(case.name.as_str()) {
            problems.push(at(format!("two cases are named `{}`", case.name)));
        }
        let mut sources: Vec<(&Source, bool)> = case.inputs.iter().map(|s| (s, false)).collect();
        sources.extend(case.arrays.values().map(|s| (s, true)));
        for (source, one_batch) in sources {
            named.insert(source.file.clone());
            match read_source(dir, source) {
                Ok((_, batches)) if one_batch && batches.len() != 1 => problems.push(at(format!(
                    "case `{}`: the array file {} holds {} batches; an array is one",
                    case.name,
                    source.file,
                    batches.len()
                ))),
                Ok(_) => {}
                Err(e) => problems.push(format!("case `{}`: {e}", case.name)),
            }
        }
        if let Expect::Output(file) = &case.expect {
            named.insert(file.clone());
            let expected = read_source(
                dir,
                &Source {
                    file: file.clone(),
                    slice: None,
                },
            );
            match &expected {
                Err(e) => problems.push(format!("case `{}`: {e}", case.name)),
                Ok(expected) => {
                    for (column, source) in &case.allowed {
                        named.insert(source.file.clone());
                        if let Err(e) = allowed_problem(dir, column, source, expected) {
                            problems.push(at(format!("case `{}`: {e}", case.name)));
                        }
                    }
                }
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        let mut unnamed: Vec<String> = entries
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|f| f.ends_with(".arrow") && !named.contains(f))
            .collect();
        unnamed.sort();
        for file in unnamed {
            problems.push(at(format!("{file} is named by no case")));
        }
    }
    Ok((
        Corpus {
            dir: dir.to_path_buf(),
            kernel,
            tolerance,
            cases,
        },
        problems,
    ))
}

/// A catalogue as read from its root: its specifications and corpora by kernel name.
#[derive(Debug, Default)]
pub struct Catalogue {
    /// The specifications, by file name (the kernel's name).
    pub specs: BTreeMap<String, Spec>,
    /// The corpora, by directory name (the kernel's name).
    pub corpora: BTreeMap<String, Corpus>,
    /// Every problem found with the formats and their agreement.
    pub problems: Vec<String>,
}

/// The names in `dir` that are not `_`-prefixed, by `keep`; none when `dir` does not exist.
fn listed(
    dir: &Path,
    keep: impl Fn(&Path) -> Option<String>,
) -> Result<Vec<(String, PathBuf)>, String> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut out: Vec<(String, PathBuf)> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter_map(|p| keep(&p).map(|name| (name, p)))
        .filter(|(name, _)| !name.starts_with('_'))
        .collect();
    out.sort();
    Ok(out)
}

/// Read and check the catalogue under `root` (its `spec/` and `conformance/`) in `namespace`,
/// against the implementations the build has.
pub fn check(root: &Path, namespace: &str, implementations: &[Implementation]) -> Catalogue {
    let mut catalogue = Catalogue::default();
    let problems = &mut catalogue.problems;
    let specs = listed(&root.join("spec"), |p| {
        (p.extension()? == "md").then(|| p.file_stem()?.to_str().map(str::to_string))?
    });
    let corpora = listed(&root.join("conformance"), |p| {
        p.is_dir()
            .then(|| p.file_name()?.to_str().map(str::to_string))?
    });
    for (name, path) in specs.unwrap_or_else(|e| {
        problems.push(e);
        Vec::new()
    }) {
        match read_spec(&path) {
            Ok(spec) => {
                if let Some(v) = spec.versions.iter().find(|v| v.id.namespace != namespace) {
                    problems.push(format!(
                        "{}: `# {}` is not in the namespace `{namespace}`",
                        path.display(),
                        v.id
                    ));
                }
                catalogue.specs.insert(name, spec);
            }
            Err(p) => problems.extend(p),
        }
    }
    for (name, dir) in corpora.unwrap_or_else(|e| {
        problems.push(e);
        Vec::new()
    }) {
        match read_corpus_and_problems(&dir) {
            Ok((corpus, found)) => {
                problems.extend(found);
                let want = format!("{namespace}.{name}");
                if corpus.kernel != want {
                    problems.push(format!(
                        "{}: the corpus of `{want}` says it is of `{}`",
                        dir.display(),
                        corpus.kernel
                    ));
                }
                catalogue.corpora.insert(name, corpus);
            }
            Err(p) => problems.extend(p),
        }
    }
    agreement(&mut catalogue, namespace, implementations);
    catalogue
}

/// The specifications, corpora and implementations agree: each specification has a corpus and
/// each corpus a specification; every case's version is published; every version has cases,
/// covering [`REQUIRED_TAGS`], and the tolerance its specification declares; a case gives
/// `allowed` only in a version that declares `per-element`; every version has a CPU reference;
/// no implementation claims a version with no specification.
fn agreement(catalogue: &mut Catalogue, namespace: &str, implementations: &[Implementation]) {
    let Catalogue {
        specs,
        corpora,
        problems,
    } = catalogue;
    for name in specs.keys() {
        if !corpora.contains_key(name) {
            problems.push(format!(
                "{namespace}.{name} has a specification and no corpus"
            ));
        }
    }
    for (name, corpus) in corpora.iter() {
        let Some(spec) = specs.get(name) else {
            problems.push(format!(
                "{namespace}.{name} has a corpus and no specification"
            ));
            continue;
        };
        for case in &corpus.cases {
            match spec.version(case.version) {
                None => problems.push(format!(
                    "{}: case `{}` is of version {}, which the specification does not publish",
                    corpus.dir.display(),
                    case.name,
                    case.version
                )),
                Some(v) if !case.allowed.is_empty() && v.precision != Precision::PerElement => {
                    problems.push(format!(
                        "{}: case `{}` gives `allowed`, and {} declares {:?}, not per-element",
                        corpus.dir.display(),
                        case.name,
                        v.id,
                        v.precision
                    ));
                }
                Some(_) => {}
            }
        }
        for version in corpus.tolerance.keys() {
            if spec.version(*version).is_none() {
                problems.push(format!(
                    "{}: a tolerance for version {version}, which is not published",
                    corpus.dir.display()
                ));
            }
        }
        for v in &spec.versions {
            let tags: BTreeSet<&str> = corpus
                .cases
                .iter()
                .filter(|c| c.version == v.id.version)
                .flat_map(|c| c.tests.iter().map(String::as_str))
                .collect();
            if !corpus.cases.iter().any(|c| c.version == v.id.version) {
                problems.push(format!("{}: no case of {}", corpus.dir.display(), v.id));
            } else {
                for tag in REQUIRED_TAGS {
                    if !tags.contains(tag) {
                        problems.push(format!(
                            "{}: no case of {} tests `{tag}`",
                            corpus.dir.display(),
                            v.id
                        ));
                    }
                }
            }
            if corpus.tolerance.get(&v.id.version) != v.precision.bounds() {
                problems.push(format!(
                    "{}: the tolerance of {} is {:?} in the corpus and {:?} in the specification",
                    corpus.dir.display(),
                    v.id,
                    corpus.tolerance.get(&v.id.version),
                    v.precision
                ));
            }
        }
    }
    for spec in specs.values() {
        for v in &spec.versions {
            let kernel = v.id.kernel();
            if !implementations.iter().any(|i| {
                i.kernel == kernel
                    && i.implementation == CPU_REFERENCE
                    && i.versions.contains(&v.id.version)
            }) {
                problems.push(format!("{} has no CPU reference", v.id));
            }
        }
    }
    for i in implementations {
        let name = i
            .kernel
            .rsplit_once('.')
            .map(|(_, n)| n)
            .unwrap_or(i.kernel);
        for version in i.versions {
            let published = specs
                .get(name)
                .and_then(|s| s.version(*version))
                .is_some_and(|v| v.id.kernel() == i.kernel);
            if !published {
                problems.push(format!(
                    "the implementation {} of {}@{version} claims a version with no \
                     specification",
                    i.implementation, i.kernel
                ));
            }
        }
    }
}

/// What one implementation did with the cases of one version it claims.
#[derive(Clone, Debug)]
pub struct Outcome {
    /// The implementation's name.
    pub implementation: String,
    /// The version.
    pub id: KernelId,
    /// The cases it passed.
    pub passed: usize,
    /// The cases it failed: the case's name and what went wrong.
    pub failures: Vec<(String, String)>,
}

impl Outcome {
    /// Whether the implementation may claim the version: it passed every case, of which there
    /// is at least one.
    pub fn holds(&self) -> bool {
        self.failures.is_empty() && self.passed > 0
    }
}

/// The runner's report: the catalogue's problems, and every claim's outcome.
#[derive(Debug)]
pub struct Report {
    /// The catalogue as read, with its problems.
    pub catalogue: Catalogue,
    /// One outcome per implementation and version it claims, in the implementations' order.
    pub outcomes: Vec<Outcome>,
}

impl Report {
    /// `Ok` when the catalogue has no problem and every claim holds; otherwise every problem and
    /// every failed case, one per line.
    pub fn verdict(&self) -> Result<(), String> {
        let mut lines = self.catalogue.problems.clone();
        for outcome in &self.outcomes {
            for (case, why) in &outcome.failures {
                lines.push(format!(
                    "{} cannot claim {}: case `{case}`: {why}",
                    outcome.implementation, outcome.id
                ));
            }
        }
        if lines.is_empty() {
            Ok(())
        } else {
            Err(lines.join("\n"))
        }
    }
}

/// Read and check the catalogue under `root` ([`check`]), then run every case of every version
/// each implementation claims against it. An implementation that fails one case of a version
/// cannot claim that version, and [`Report::verdict`] says so.
pub fn run(
    root: &Path,
    namespace: &str,
    implementations: &[Implementation],
    alloc: Arc<dyn Allocator>,
) -> Report {
    let catalogue = check(root, namespace, implementations);
    let mut outcomes = Vec::new();
    for implementation in implementations {
        let name = implementation
            .kernel
            .rsplit_once('.')
            .map_or(implementation.kernel, |(_, n)| n);
        let (Some(spec), Some(corpus)) = (catalogue.specs.get(name), catalogue.corpora.get(name))
        else {
            continue;
        };
        for version in implementation.versions {
            let Some(spec_version) = spec.version(*version) else {
                continue;
            };
            let mut outcome = Outcome {
                implementation: implementation.implementation.to_string(),
                id: spec_version.id.clone(),
                passed: 0,
                failures: Vec::new(),
            };
            for case in corpus.cases.iter().filter(|c| c.version == *version) {
                match run_case(&corpus.dir, case, spec_version, implementation, &alloc) {
                    Ok(()) => outcome.passed += 1,
                    Err(why) => outcome.failures.push((case.name.clone(), why)),
                }
            }
            outcomes.push(outcome);
        }
    }
    Report {
        catalogue,
        outcomes,
    }
}

/// The step's name as a case writes it.
fn step_name(step: Step) -> &'static str {
    match step {
        Step::Configure => "configure",
        Step::Schema => "schema",
        Step::Apply => "apply",
    }
}

/// Whether a refusal at `step` is what `expect` asks for.
fn refusal(expect: &Expect, step: Step, err: &MorunaError) -> Result<(), String> {
    let at_step = step_name(step);
    match expect {
        Expect::Error { at, contains } if *at == step => match contains {
            Some(text) if !err.to_string().contains(text.as_str()) => Err(format!(
                "refused at {at_step} with `{err}`, which does not contain `{text}`"
            )),
            _ => Ok(()),
        },
        Expect::Error { at, .. } => Err(format!(
            "refused at {at_step} with `{err}`; the case expects a refusal at {}",
            step_name(*at)
        )),
        Expect::Output(_) => Err(format!("refused at {at_step}: {err}")),
    }
}

/// Two schemas agree when they have the same columns, by name, type and nullability, in order.
/// Metadata is not compared.
fn same_schema(what: &str, got: &Schema, want: &Schema) -> Result<(), String> {
    let shape = |s: &Schema| -> Vec<(String, DataType, bool)> {
        s.fields()
            .iter()
            .map(|f| (f.name().clone(), f.data_type().clone(), f.is_nullable()))
            .collect()
    };
    if shape(got) == shape(want) {
        Ok(())
    } else {
        Err(format!(
            "{what} is {:?}; the expected output's is {:?}",
            shape(got),
            shape(want)
        ))
    }
}

/// Run one case against one implementation.
fn run_case(
    dir: &Path,
    case: &Case,
    spec: &SpecVersion,
    implementation: &Implementation,
    alloc: &Arc<dyn Allocator>,
) -> Result<(), String> {
    let mut arrays = BTreeMap::new();
    for (name, source) in &case.arrays {
        let (_, batches) = read_source(dir, source)?;
        let batch = batches
            .into_iter()
            .next()
            .ok_or_else(|| format!("the array `{name}` has no batch"))?;
        arrays.insert(name.clone(), batch);
    }
    let inputs = case
        .inputs
        .iter()
        .map(|s| read_source(dir, s))
        .collect::<Result<Vec<_>, _>>()?;
    let expected = match &case.expect {
        Expect::Output(file) => Some(read_source(
            dir,
            &Source {
                file: file.clone(),
                slice: None,
            },
        )?),
        Expect::Error { .. } => None,
    };
    let params = Params {
        json: case.params.clone(),
        arrays,
    };
    let kernel = match (implementation.configure)(case.version, &params) {
        Ok(kernel) => kernel,
        Err(e) => return refusal(&case.expect, Step::Configure, &e),
    };
    let stateful = matches!(kernel.kind(), KernelKind::Stateful { .. });
    if stateful == (spec.shape == StateShape::None) {
        return Err(format!(
            "the specification's state shape is {:?} and the kernel is {}",
            spec.shape,
            if stateful { "stateful" } else { "stateless" }
        ));
    }
    if inputs.len() > usize::from(kernel.max_inputs()) {
        return Err(format!(
            "the case gives {} inputs and the kernel takes at most {}",
            inputs.len(),
            kernel.max_inputs()
        ));
    }
    let schemas: Vec<SourceSchema> = inputs
        .iter()
        .map(|(schema, _)| SourceSchema::Table(Arc::clone(schema)))
        .collect();
    let declared = match kernel.output_schema_of(&schemas) {
        Ok(SourceSchema::Table(schema)) => schema,
        Ok(SourceSchema::Tensor { .. }) => return Err("it declares a tensor output".into()),
        Err(e) => return refusal(&case.expect, Step::Schema, &e),
    };
    if let Some((want, _)) = &expected {
        same_schema("the declared output schema", &declared, want)?;
    }
    let mut state = kernel
        .init(&InitCtx {
            instance: 0,
            device: None,
            alloc: Arc::clone(alloc),
        })
        .map_err(|e| format!("init: {e}"))?;
    let mut outputs = Vec::new();
    let applied = feed(
        kernel.as_ref(),
        state.as_mut(),
        &inputs,
        stateful,
        &mut outputs,
    );
    if let Err(e) = applied {
        return refusal(&case.expect, Step::Apply, &e);
    }
    let Some((want_schema, want)) = expected else {
        return Err("it gave its output; the case expects a refusal".into());
    };
    let mut got = Vec::with_capacity(outputs.len());
    for output in outputs {
        match output {
            Payload::Table(batch, _) => {
                same_schema("an output batch's schema", &batch.schema(), &want_schema)?;
                got.push(batch);
            }
            Payload::Tensor(..) => return Err("it gave a tensor".into()),
        }
    }
    let got = concat_batches(&want_schema, &got).map_err(|e| format!("its output: {e}"))?;
    let want = concat_batches(&want_schema, &want).map_err(|e| format!("the expected: {e}"))?;
    let mut allowed = BTreeMap::new();
    for (column, source) in &case.allowed {
        let (schema, batches) = read_source(dir, source)?;
        let arrays: Vec<&dyn Array> = batches.iter().map(|b| b.column(0).as_ref()).collect();
        let array = match arrays.as_slice() {
            [] => new_empty_array(schema.field(0).data_type()),
            some => concat(some).map_err(|e| format!("the allowed error of `{column}`: {e}"))?,
        };
        allowed.insert(column.clone(), array);
    }
    compare(&got, &want, &spec.precision, &allowed)
}

/// Give a case's batches to an instance, as the engine would: one input, each batch to
/// `apply`; several inputs joined by morsel, the batches of one position together to
/// `apply_aligned`; otherwise each port's batches in turn to `apply_port`, port 0 first. Then,
/// for a stateful kernel, `finish`.
fn feed(
    kernel: &dyn Kernel,
    state: &mut dyn KernelState,
    inputs: &[(SchemaRef, Vec<RecordBatch>)],
    stateful: bool,
    outputs: &mut Vec<Payload>,
) -> moruna_kernel::Result<()> {
    match inputs {
        [(_, batches)] => {
            for batch in batches {
                outputs.push(kernel.apply(state, Payload::table(batch.clone())?)?);
            }
        }
        many if kernel.aligns_inputs() => {
            let count = many.first().map_or(0, |(_, b)| b.len());
            if many.iter().any(|(_, b)| b.len() != count) {
                return Err(MorunaError::Plan(
                    "a kernel that joins its inputs by morsel is given ports of different batch \
                     counts"
                        .into(),
                ));
            }
            for at in 0..count {
                let joined = many
                    .iter()
                    .map(|(_, b)| Payload::table(b[at].clone()))
                    .collect::<moruna_kernel::Result<Vec<_>>>()?;
                outputs.push(kernel.apply_aligned(state, joined)?);
            }
        }
        many => {
            for (port, (_, batches)) in many.iter().enumerate() {
                let port = u16::try_from(port)
                    .map_err(|_| MorunaError::Plan("more than 65535 inputs".into()))?;
                for batch in batches {
                    outputs.push(kernel.apply_port(state, port, Payload::table(batch.clone())?)?);
                }
            }
        }
    }
    if stateful && let Some(last) = kernel.finish(state)? {
        outputs.push(last);
    }
    Ok(())
}

/// The output equals the expected: as many rows, and every column bit for bit, but for the
/// columns the version bounds ([`Precision::Bounds`]) or the case gives an allowed error for
/// (`allowed`), which are compared element by element.
fn compare(
    got: &RecordBatch,
    want: &RecordBatch,
    precision: &Precision,
    allowed: &BTreeMap<String, ArrayRef>,
) -> Result<(), String> {
    if got.num_rows() != want.num_rows() {
        return Err(format!(
            "it gave {} rows; the expected output has {}",
            got.num_rows(),
            want.num_rows()
        ));
    }
    let schema = want.schema();
    if let Some(column) = precision
        .bounds()
        .into_iter()
        .flat_map(|t| t.keys())
        .find(|c| schema.index_of(c).is_err())
    {
        return Err(format!(
            "the tolerance names `{column}`, which the output does not have"
        ));
    }
    for (at, field) in schema.fields().iter().enumerate() {
        let (g, w) = (got.column(at).as_ref(), want.column(at).as_ref());
        let rule = match (
            allowed.get(field.name()),
            precision.bounds().and_then(|t| t.get(field.name())),
        ) {
            (Some(allowed), _) => Some(Rule::Allowed(allowed.as_ref())),
            (None, Some(bound)) => Some(Rule::Bound(bound)),
            (None, None) => None,
        };
        match rule {
            Some(rule) => close(g, w, rule),
            None => exact(g, w),
        }
        .map_err(|why| format!("column `{}`: {why}", field.name()))?;
    }
    Ok(())
}

/// A value of an array, for a message.
fn shown(array: &dyn Array, row: usize) -> String {
    if array.is_null(row) {
        return "null".into();
    }
    array_value_to_string(array, row).unwrap_or_else(|e| format!("<{e}>"))
}

/// Logical equality, bit for bit for floats; the first row that differs when not.
fn exact(got: &dyn Array, want: &dyn Array) -> Result<(), String> {
    if got.data_type() == want.data_type() && got.to_data() == want.to_data() {
        return Ok(());
    }
    if got.data_type() != want.data_type() || got.len() != want.len() {
        return Err(format!(
            "it is {} of {} values; expected {} of {}",
            got.data_type(),
            got.len(),
            want.data_type(),
            want.len()
        ));
    }
    let row = (0..want.len())
        .find(|r| got.slice(*r, 1).to_data() != want.slice(*r, 1).to_data())
        .unwrap_or(0);
    Err(format!(
        "row {row} is {}; expected {}",
        shown(got, row),
        shown(want, row)
    ))
}

/// How a float element may differ from the expected one.
#[derive(Copy, Clone)]
enum Rule<'a> {
    /// The version's bound for the column.
    Bound(&'a Bound),
    /// The case's allowed error, an array of the column's shape with `Float64` leaves.
    Allowed(&'a dyn Array),
}

/// Equal but for float elements, which are within `rule`: floats, and lists of them.
fn close(got: &dyn Array, want: &dyn Array, rule: Rule<'_>) -> Result<(), String> {
    if got.data_type() != want.data_type() || got.len() != want.len() {
        return exact(got, want);
    }
    if let Some(row) = (0..want.len()).find(|r| got.is_null(*r) != want.is_null(*r)) {
        return Err(format!(
            "row {row} is {}; expected {}",
            shown(got, row),
            shown(want, row)
        ));
    }
    if let Rule::Allowed(allowed) = rule
        && allowed.len() != want.len()
    {
        return Err(format!(
            "the allowed error has {} values for {}",
            allowed.len(),
            want.len()
        ));
    }
    let rows = || (0..want.len()).filter(|r| want.is_valid(*r));
    // The rule for row `row`'s children: the same bound, or the allowed list's row; a null
    // allowed row is bit for bit.
    let child = |row: usize, f: &dyn Fn(&dyn Array) -> ArrayRef| -> Option<Option<ArrayRef>> {
        match rule {
            Rule::Bound(_) => Some(None),
            Rule::Allowed(allowed) if allowed.is_null(row) => None,
            Rule::Allowed(allowed) => Some(Some(f(allowed))),
        }
    };
    let recurse = |row: usize, g: ArrayRef, w: ArrayRef, a: Option<Option<ArrayRef>>| {
        match a {
            None => exact(g.as_ref(), w.as_ref()),
            Some(None) => close(g.as_ref(), w.as_ref(), rule),
            Some(Some(a)) => close(g.as_ref(), w.as_ref(), Rule::Allowed(a.as_ref())),
        }
        .map_err(|why| format!("row {row}: {why}"))
    };
    match want.data_type() {
        DataType::Float16 | DataType::Float32 | DataType::Float64 => {
            let as_f64 = |a: &dyn Array| cast(a, &DataType::Float64).map_err(|e| e.to_string());
            let (g, w) = (as_f64(got)?, as_f64(want)?);
            let (g, w) = (
                g.as_primitive::<Float64Type>(),
                w.as_primitive::<Float64Type>(),
            );
            for row in rows() {
                let (v, e) = (g.value(row), w.value(row));
                let ok = match rule {
                    Rule::Bound(bound) => bound.within(v, e),
                    Rule::Allowed(allowed) if allowed.is_null(row) => v.to_bits() == e.to_bits(),
                    Rule::Allowed(allowed) => Bound {
                        abs: allowed.as_primitive::<Float64Type>().value(row),
                        rel: 0.0,
                    }
                    .within(v, e),
                };
                if !ok {
                    let by = match rule {
                        Rule::Bound(bound) => format!("within {bound:?}"),
                        Rule::Allowed(allowed) if allowed.is_null(row) => "bit for bit".into(),
                        Rule::Allowed(allowed) => format!(
                            "within {}",
                            allowed.as_primitive::<Float64Type>().value(row)
                        ),
                    };
                    return Err(format!("row {row} is {v}; expected {e} {by}"));
                }
            }
            Ok(())
        }
        DataType::FixedSizeList(..) => {
            let (g, w) = (got.as_fixed_size_list(), want.as_fixed_size_list());
            for row in rows() {
                let a = child(row, &|a| a.as_fixed_size_list().value(row));
                recurse(row, g.value(row), w.value(row), a)?;
            }
            Ok(())
        }
        DataType::List(_) => {
            let (g, w) = (got.as_list::<i32>(), want.as_list::<i32>());
            for row in rows() {
                let a = child(row, &|a| a.as_list::<i32>().value(row));
                recurse(row, g.value(row), w.value(row), a)?;
            }
            Ok(())
        }
        DataType::LargeList(_) => {
            let (g, w) = (got.as_list::<i64>(), want.as_list::<i64>());
            for row in rows() {
                let a = child(row, &|a| a.as_list::<i64>().value(row));
                recurse(row, g.value(row), w.value(row), a)?;
            }
            Ok(())
        }
        other => Err(format!(
            "a tolerance applies to floats and lists of them, not {other}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use moruna_kernel::arrow::array::{
        FixedSizeListArray, Float32Array, Float64Array, Int64Array, LargeListArray, ListArray,
    };
    use moruna_kernel::arrow::buffer::{NullBuffer, OffsetBuffer};

    use super::*;

    #[test]
    fn the_template_has_every_heading() {
        assert_eq!(check_template(SPEC_TEMPLATE), Vec::<String>::new());
    }

    #[test]
    fn a_bound_is_canons() {
        let b = Bound { abs: 0.5, rel: 0.0 };
        assert!(b.within(1.0, 1.5));
        assert!(!b.within(1.0, 1.6));
        assert!(b.within(f64::NAN, f64::NAN));
        assert!(!b.within(f64::NAN, 1.0));
        assert!(b.within(f64::INFINITY, f64::INFINITY));
        assert!(!b.within(f64::INFINITY, f64::MAX));
        let r = Bound { abs: 0.0, rel: 0.1 };
        assert!(r.within(100.0, 110.0));
        assert!(!r.within(100.0, 112.0));
    }

    #[test]
    fn tolerances_are_read_strictly() {
        let ok = serde_json::json!({"y": {"abs": 1e-6, "rel": 0}});
        assert_eq!(
            tolerance_of(&ok).expect("ok")["y"],
            Bound {
                abs: 1e-6,
                rel: 0.0
            }
        );
        for bad in [
            serde_json::json!({}),
            serde_json::json!([]),
            serde_json::json!({"y": 1}),
            serde_json::json!({"y": {"abs": 1}}),
            serde_json::json!({"y": {"abs": -1, "rel": 0}}),
            serde_json::json!({"y": {"abs": 1, "rel": 0, "ulp": 2}}),
        ] {
            assert!(tolerance_of(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn shapes_parse() {
        for (text, shape) in [
            ("none", StateShape::None),
            ("accumulate-merge-finish", StateShape::AccumulateMergeFinish),
            ("build-once-read-many", StateShape::BuildOnceReadMany),
            ("per-partition-ordered", StateShape::PerPartitionOrdered),
        ] {
            assert_eq!(StateShape::parse(text), Some(shape));
        }
        assert_eq!(StateShape::parse("stateless"), None);
    }

    #[test]
    fn keyed_lines_drop_backticks() {
        let body = vec!["".to_string(), "- shape: `none`".to_string()];
        assert_eq!(keyed(&body, "shape").as_deref(), Some("none"));
        assert_eq!(keyed(&body, "name"), None);
    }

    #[test]
    fn an_outline_out_of_order_is_named() {
        let mut text = String::from("# moruna.x@1\n\n");
        for h in SPEC_HEADINGS.iter().rev() {
            text.push_str(&format!("## {h}\n\ntext\n\n"));
        }
        let problems = check_template(&text);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("in that order"));
        assert!(!check_template("prose\n").is_empty());
    }

    fn batch_of(name: &str, array: ArrayRef) -> RecordBatch {
        RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new(
                name,
                array.data_type().clone(),
                true,
            )])),
            vec![array],
        )
        .expect("batch")
    }

    fn none() -> BTreeMap<String, ArrayRef> {
        BTreeMap::new()
    }

    fn bounds(abs: f64) -> Precision {
        Precision::Bounds(Tolerance::from([(
            "f".to_string(),
            Bound { abs, rel: 0.0 },
        )]))
    }

    #[test]
    fn exact_comparison_names_the_first_row_that_differs() {
        let want = batch_of(
            "v",
            Arc::new(Int64Array::from(vec![Some(1), None, Some(3)])),
        );
        assert_eq!(compare(&want, &want, &Precision::Exact, &none()), Ok(()));
        let got = batch_of(
            "v",
            Arc::new(Int64Array::from(vec![Some(1), Some(2), Some(3)])),
        );
        let err = compare(&got, &want, &Precision::Exact, &none()).expect_err("differs");
        assert!(
            err.contains("column `v`: row 1 is 2; expected null"),
            "{err}"
        );
        let short = batch_of("v", Arc::new(Int64Array::from(vec![1])));
        let err = compare(&short, &want, &Precision::Exact, &none()).expect_err("rows");
        assert!(err.contains("gave 1 rows"), "{err}");
        // Floats are compared bit for bit: -0.0 is not 0.0, and NaN is NaN.
        let a = batch_of("f", Arc::new(Float64Array::from(vec![0.0, f64::NAN])));
        let b = batch_of("f", Arc::new(Float64Array::from(vec![-0.0, f64::NAN])));
        let err = compare(&a, &b, &Precision::Exact, &none()).expect_err("signed zero");
        assert!(err.contains("row 0"), "{err}");
        assert_eq!(compare(&b, &b, &Precision::Exact, &none()), Ok(()));
        let err = exact(a.column(0).as_ref(), want.column(0).as_ref()).expect_err("types");
        assert!(err.contains("expected Int64"), "{err}");
    }

    #[test]
    fn a_bound_applies_to_floats_and_lists_of_them() {
        let want = batch_of(
            "f",
            Arc::new(Float32Array::from(vec![Some(1.0), None, Some(3.0)])),
        );
        let got = batch_of(
            "f",
            Arc::new(Float32Array::from(vec![Some(1.25), None, Some(3.0)])),
        );
        assert_eq!(compare(&got, &want, &bounds(0.5), &none()), Ok(()));
        let err = compare(&got, &want, &bounds(0.1), &none()).expect_err("outside");
        assert!(err.contains("row 0 is 1.25; expected 1 within"), "{err}");
        let nulls = batch_of(
            "f",
            Arc::new(Float32Array::from(vec![Some(1.0), Some(2.0), Some(3.0)])),
        );
        let err = compare(&nulls, &want, &bounds(9.0), &none()).expect_err("null");
        assert!(err.contains("row 1"), "{err}");
        let other = Precision::Bounds(Tolerance::from([(
            "g".to_string(),
            Bound { abs: 0.0, rel: 0.0 },
        )]));
        let err = compare(&want, &want, &other, &none()).expect_err("names g");
        assert!(err.contains("`g`"), "{err}");
        let ints = batch_of("f", Arc::new(Int64Array::from(vec![1, 2, 3])));
        let err = compare(&ints, &ints, &bounds(1.0), &none()).expect_err("ints");
        assert!(err.contains("floats"), "{err}");
        let wider = batch_of(
            "f",
            Arc::new(Float64Array::from(vec![Some(1.0), None, Some(3.0)])),
        );
        assert!(compare(&wider, &want, &bounds(1.0), &none()).is_err());

        let item = Arc::new(Field::new("item", DataType::Float64, true));
        let fixed = |v: Vec<f64>| -> ArrayRef {
            Arc::new(
                FixedSizeListArray::try_new(
                    Arc::clone(&item),
                    2,
                    Arc::new(Float64Array::from(v)),
                    Some(NullBuffer::from(vec![true, false])),
                )
                .expect("fixed"),
            )
        };
        let want = batch_of("f", fixed(vec![1.0, 2.0, 0.0, 0.0]));
        let close_by = batch_of("f", fixed(vec![1.0, 2.05, 9.0, 9.0]));
        assert_eq!(compare(&close_by, &want, &bounds(0.1), &none()), Ok(()));
        let err = compare(&close_by, &want, &bounds(0.01), &none()).expect_err("far");
        assert!(err.contains("row 0: row 1 is 2.05"), "{err}");

        let list = |v: Vec<f64>| -> ArrayRef {
            Arc::new(ListArray::new(
                Arc::clone(&item),
                OffsetBuffer::from_lengths([1, 2]),
                Arc::new(Float64Array::from(v)),
                None,
            ))
        };
        let want = batch_of("f", list(vec![1.0, 2.0, 3.0]));
        let close_by = batch_of("f", list(vec![1.0, 2.0, 3.05]));
        assert_eq!(compare(&close_by, &want, &bounds(0.1), &none()), Ok(()));
        assert!(compare(&close_by, &want, &bounds(0.01), &none()).is_err());
        let large = |v: Vec<f64>| -> ArrayRef {
            Arc::new(LargeListArray::new(
                Arc::clone(&item),
                OffsetBuffer::from_lengths([3]),
                Arc::new(Float64Array::from(v)),
                None,
            ))
        };
        let want = batch_of("f", large(vec![1.0, 2.0, 3.0]));
        let close_by = batch_of("f", large(vec![1.0, 2.0, 3.05]));
        assert_eq!(compare(&close_by, &want, &bounds(0.1), &none()), Ok(()));
        assert!(compare(&close_by, &want, &bounds(0.01), &none()).is_err());
    }

    #[test]
    fn an_allowed_error_applies_element_by_element() {
        let want = batch_of(
            "f",
            Arc::new(Float64Array::from(vec![
                Some(1.0),
                None,
                Some(100.0),
                Some(5.0),
            ])),
        );
        let got = batch_of(
            "f",
            Arc::new(Float64Array::from(vec![
                Some(1.01),
                None,
                Some(101.0),
                Some(5.0),
            ])),
        );
        let allowed = |v: Vec<Option<f64>>| -> BTreeMap<String, ArrayRef> {
            BTreeMap::from([("f".to_string(), Arc::new(Float64Array::from(v)) as ArrayRef)])
        };
        let ok = allowed(vec![Some(0.02), None, Some(1.0), None]);
        assert_eq!(compare(&got, &want, &Precision::PerElement, &ok), Ok(()));
        let tight = allowed(vec![Some(0.02), None, Some(0.5), None]);
        let err = compare(&got, &want, &Precision::PerElement, &tight).expect_err("row 2");
        assert!(
            err.contains("row 2 is 101; expected 100 within 0.5"),
            "{err}"
        );
        let exact_row = allowed(vec![None, None, Some(1.0), None]);
        let err = compare(&got, &want, &Precision::PerElement, &exact_row).expect_err("row 0");
        assert!(
            err.contains("row 0 is 1.01; expected 1 bit for bit"),
            "{err}"
        );
        let short = allowed(vec![Some(1.0)]);
        let err = compare(&got, &want, &Precision::PerElement, &short).expect_err("short");
        assert!(
            err.contains("the allowed error has 1 values for 4"),
            "{err}"
        );
        // Without `allowed`, a per-element version compares bit for bit.
        assert!(compare(&got, &want, &Precision::PerElement, &none()).is_err());

        // A FixedSizeList<Float32> column, one allowed value per element; a null allowed row
        // is bit for bit.
        let item32 = Arc::new(Field::new("item", DataType::Float32, true));
        let item64 = Arc::new(Field::new("item", DataType::Float64, true));
        let fixed = |v: Vec<f32>| -> ArrayRef {
            Arc::new(
                FixedSizeListArray::try_new(
                    Arc::clone(&item32),
                    2,
                    Arc::new(Float32Array::from(v)),
                    None,
                )
                .expect("fixed"),
            )
        };
        let want = batch_of("f", fixed(vec![1.0, 2.0, 3.0, 4.0]));
        let got = batch_of("f", fixed(vec![1.0, 2.5, 3.0, 4.0]));
        let per = |v: Vec<f64>, nulls: Option<Vec<bool>>| -> BTreeMap<String, ArrayRef> {
            let array = FixedSizeListArray::try_new(
                Arc::clone(&item64),
                2,
                Arc::new(Float64Array::from(v)),
                nulls.map(NullBuffer::from),
            )
            .expect("allowed");
            BTreeMap::from([("f".to_string(), Arc::new(array) as ArrayRef)])
        };
        let ok = per(vec![0.0, 0.5, 0.0, 0.0], None);
        assert_eq!(compare(&got, &want, &Precision::PerElement, &ok), Ok(()));
        let tight = per(vec![0.5, 0.1, 0.0, 0.0], None);
        let err = compare(&got, &want, &Precision::PerElement, &tight).expect_err("tight");
        assert!(
            err.contains("row 0: row 1 is 2.5; expected 2 within 0.1"),
            "{err}"
        );
        let null_row = per(vec![0.5, 0.5, 0.0, 0.0], Some(vec![false, true]));
        let err = compare(&got, &want, &Precision::PerElement, &null_row).expect_err("exact");
        assert!(err.contains("row 0: row 1 is 2.5; expected 2"), "{err}");
    }

    #[test]
    fn allowed_types_mirror_the_output() {
        let f32_list =
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float32, false)), 4);
        let want =
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float64, true)), 4);
        assert_eq!(allowed_type(&f32_list), Some(want.clone()));
        assert!(same_shape(&want, &allowed_type(&f32_list).expect("shape")));
        assert!(!same_shape(&DataType::Float32, &DataType::Float64));
        let list = DataType::List(Arc::new(Field::new("x", DataType::Float16, true)));
        let large = DataType::LargeList(Arc::new(Field::new("x", DataType::Float16, true)));
        assert!(same_shape(
            &allowed_type(&list).expect("list"),
            &allowed_type(&list).expect("list")
        ));
        assert!(!same_shape(
            &allowed_type(&list).expect("list"),
            &allowed_type(&large).expect("large")
        ));
        assert_eq!(allowed_type(&DataType::Int64), None);
        let nested = DataType::List(Arc::new(Field::new("x", DataType::Utf8, true)));
        assert_eq!(allowed_type(&nested), None);
    }

    #[test]
    fn schemas_agree_by_name_type_and_nullability() {
        let a = Schema::new(vec![Field::new("x", DataType::Int64, true)]);
        let b = Schema::new(vec![Field::new("x", DataType::Int64, false)]);
        assert_eq!(same_schema("s", &a, &a), Ok(()));
        assert!(same_schema("s", &a, &b).is_err());
        let meta = a
            .clone()
            .with_metadata([("k".to_string(), "v".to_string())].into());
        assert_eq!(same_schema("s", &meta, &a), Ok(()));
    }

    #[test]
    fn a_refusal_matches_its_step_and_words() {
        let err = MorunaError::Plan("it overflows".into());
        let expect = Expect::Error {
            at: Step::Apply,
            contains: Some("overflow".into()),
        };
        assert_eq!(refusal(&expect, Step::Apply, &err), Ok(()));
        assert!(refusal(&expect, Step::Schema, &err).is_err());
        let other = Expect::Error {
            at: Step::Apply,
            contains: Some("range".into()),
        };
        assert!(refusal(&other, Step::Apply, &err).is_err());
        assert!(refusal(&Expect::Output("x".into()), Step::Configure, &err).is_err());
    }
}
