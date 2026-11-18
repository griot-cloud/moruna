//! The conformance runner (system design 4A): every corpus against every implementation the
//! build has, and the runner's own checks on the worked example, `example.add@1`.

#![allow(clippy::result_large_err)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use moruna_kernel::Allocator;
use moruna_kernel::arrow::array::{AsArray, Float64Array, Int64Array, RecordBatch};
use moruna_kernel::arrow::datatypes::{DataType, Field, Float64Type, Int64Type, Schema};
use moruna_kernel::arrow::ipc::writer::FileWriter;
use moruna_kernel::{
    Fingerprint, InitCtx, Kernel, KernelKind, KernelState, MorunaError, NoState, Payload,
    PayloadKind, PayloadSpec, Result, SourceSchema, TierPref,
};
use moruna_kernels::catalogue::{
    CPU_REFERENCE, Configure, Implementation, Params, implementations,
};
use moruna_kernels::conformance::{Outcome, Precision, check, run};
use moruna_testkit::FakeAllocator;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn example_root() -> PathBuf {
    root().join("tests").join("conformance_example")
}

/// `example.add@1`, as its specification (tests/conformance_example/spec/add.md) states it.
struct Add {
    addend: i64,
}

fn x_only(schema: &Schema) -> Result<()> {
    match schema.fields().as_ref() {
        [f] if f.name() == "x" && f.data_type() == &DataType::Int64 => Ok(()),
        _ => Err(MorunaError::Plan(format!(
            "example.add@1 takes one column x, Int64, and is given {schema:?}"
        ))),
    }
}

impl Kernel for Add {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint([0; 32])
    }
    fn kind(&self) -> KernelKind {
        KernelKind::Stateless
    }
    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Table,
            tier: TierPref::Host,
        }
    }
    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        match input {
            SourceSchema::Table(schema) => {
                x_only(schema)?;
                Ok(SourceSchema::Table(Arc::new(Schema::new(vec![
                    Field::new("x", DataType::Int64, true),
                ]))))
            }
            SourceSchema::Tensor { .. } => Err(MorunaError::Plan("a tensor".into())),
        }
    }
    fn init(&self, _ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }
    fn apply(&self, _state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        let Payload::Table(batch, _) = input else {
            return Err(MorunaError::Plan("a tensor".into()));
        };
        x_only(&batch.schema())?;
        let x = batch.column(0).as_primitive::<Int64Type>();
        let mut out = Vec::with_capacity(x.len());
        for v in x.iter() {
            out.push(match v {
                None => None,
                Some(v) => Some(v.checked_add(self.addend).ok_or_else(|| {
                    MorunaError::Plan(format!("example.add@1: {v} + {} overflows", self.addend))
                })?),
            });
        }
        let schema = Arc::new(Schema::new(vec![Field::new("x", DataType::Int64, true)]));
        Payload::table(
            RecordBatch::try_new(schema, vec![Arc::new(Int64Array::from(out))])
                .map_err(|e| MorunaError::Plan(e.to_string()))?,
        )
    }
}

fn configure_add(version: u32, params: &Params) -> Result<Arc<dyn Kernel>> {
    let refuse = |msg: &str| MorunaError::Plan(format!("example.add@{version}: {msg}"));
    let object = params
        .json
        .as_object()
        .ok_or_else(|| refuse("parameters are an object"))?;
    if object.keys().any(|k| k != "addend") || !params.arrays.is_empty() {
        return Err(refuse("the one parameter is `addend`"));
    }
    let addend = object
        .get("addend")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| refuse("`addend` is required, an Int64"))?;
    Ok(Arc::new(Add { addend }))
}

const EXAMPLE_ADD: Implementation = Implementation {
    kernel: "example.add",
    implementation: CPU_REFERENCE,
    versions: &[1],
    configure: configure_add,
};

/// `example.scale@1`, as its specification (tests/conformance_example/spec/scale.md) states it.
struct Scale {
    factor: f64,
}

impl Kernel for Scale {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint([2; 32])
    }
    fn kind(&self) -> KernelKind {
        KernelKind::Stateless
    }
    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Table,
            tier: TierPref::Host,
        }
    }
    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        let SourceSchema::Table(schema) = input else {
            return Err(MorunaError::Plan("a tensor".into()));
        };
        match schema.fields().as_ref() {
            [f] if f.name() == "x" && f.data_type() == &DataType::Float64 => {}
            _ => return Err(MorunaError::Plan("example.scale@1 takes x, Float64".into())),
        }
        Ok(SourceSchema::Table(Arc::new(Schema::new(vec![
            Field::new("x", DataType::Float64, true),
        ]))))
    }
    fn init(&self, _ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }
    fn apply(&self, _state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        let Payload::Table(batch, _) = input else {
            return Err(MorunaError::Plan("a tensor".into()));
        };
        let x = batch.column(0).as_primitive::<Float64Type>();
        let out: Float64Array = x.iter().map(|v| v.map(|v| v * self.factor)).collect();
        let schema = Arc::new(Schema::new(vec![Field::new("x", DataType::Float64, true)]));
        Payload::table(
            RecordBatch::try_new(schema, vec![Arc::new(out)])
                .map_err(|e| MorunaError::Plan(e.to_string()))?,
        )
    }
}

fn configure_scale(version: u32, params: &Params) -> Result<Arc<dyn Kernel>> {
    let object = params.json.as_object();
    match object
        .and_then(|o| o.get("factor"))
        .and_then(serde_json::Value::as_f64)
    {
        Some(factor) if object.is_some_and(|o| o.len() == 1) && factor.is_finite() => {
            Ok(Arc::new(Scale { factor }))
        }
        _ => Err(MorunaError::Plan(format!(
            "example.scale@{version}: the one parameter is `factor`, a finite number"
        ))),
    }
}

const EXAMPLE_SCALE: Implementation = Implementation {
    kernel: "example.scale",
    implementation: CPU_REFERENCE,
    versions: &[1],
    configure: configure_scale,
};

const EXAMPLES: [Implementation; 2] = [EXAMPLE_ADD, EXAMPLE_SCALE];

fn assert_clean(root: &Path, namespace: &str, implementations: &[Implementation]) {
    let catalogue = check(root, namespace, implementations);
    assert!(
        catalogue.problems.is_empty(),
        "{}",
        catalogue.problems.join("\n")
    );
}

#[test]
fn the_catalogue_is_well_formed() {
    assert_clean(&root(), "moruna", implementations());
}

#[test]
fn the_worked_example_is_well_formed() {
    assert_clean(&example_root(), "example", &EXAMPLES);
    let catalogue = check(&example_root(), "example", &EXAMPLES);
    assert_eq!(catalogue.specs.len(), 2);
    assert_eq!(catalogue.corpora["add"].cases.len(), 6);
    let scale = &catalogue.corpora["scale"];
    assert_eq!(scale.cases.len(), 5);
    assert!(scale.cases[0].allowed.contains_key("x"));
    assert_eq!(
        catalogue.specs["scale"].versions[0].precision,
        Precision::PerElement
    );
}

#[test]
fn a_version_with_no_cpu_reference_is_a_problem() {
    let catalogue = check(&example_root(), "example", &[EXAMPLE_SCALE]);
    assert!(
        catalogue
            .problems
            .iter()
            .any(|p| p.contains("example.add@1 has no CPU reference")),
        "{:?}",
        catalogue.problems
    );
}

#[test]
fn a_claim_with_no_specification_is_a_problem() {
    let two = Implementation {
        versions: &[1, 2],
        ..EXAMPLE_ADD
    };
    let catalogue = check(&example_root(), "example", &[two, EXAMPLE_SCALE]);
    assert!(
        catalogue
            .problems
            .iter()
            .any(|p| p.contains("example.add@2 claims a version with no specification")),
        "{:?}",
        catalogue.problems
    );
}

fn alloc() -> Arc<dyn Allocator> {
    Arc::new(FakeAllocator::new())
}

/// Every corpus against every implementation the build has: each claim holds.
#[test]
fn every_implementation_passes_every_case_it_claims() {
    let report = run(&root(), "moruna", implementations(), alloc());
    if let Err(lines) = report.verdict() {
        panic!("{lines}");
    }
    for outcome in &report.outcomes {
        assert!(outcome.holds(), "{outcome:?}");
    }
}

#[test]
fn the_worked_example_passes() {
    let report = run(&example_root(), "example", &EXAMPLES, alloc());
    assert_eq!(report.verdict(), Ok(()));
    assert_eq!(report.outcomes.len(), 2);
    assert_eq!(report.outcomes[0].passed, 6);
    assert_eq!(report.outcomes[1].passed, 5);
    assert!(report.outcomes.iter().all(Outcome::holds));
}

/// An implementation that is wrong in one way, so it fails some case.
struct Wrong {
    how: How,
}

#[derive(Copy, Clone)]
enum How {
    /// Adds one too many.
    OffByOne,
    /// Wraps instead of refusing.
    Wraps,
    /// Declares Int32.
    Int32,
    /// Is stateful.
    Stateful,
    /// Refuses every input schema.
    RefusesSchema,
    /// Gives a tensor.
    Tensor,
    /// Takes no input.
    NoInputs,
    /// Refuses at apply with words the case does not expect.
    OtherWords,
}

impl Kernel for Wrong {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint([1; 32])
    }
    fn kind(&self) -> KernelKind {
        match self.how {
            How::Stateful => KernelKind::Stateful {
                max_instances: core::num::NonZeroUsize::MIN,
            },
            _ => KernelKind::Stateless,
        }
    }
    fn accepts(&self) -> PayloadSpec {
        Add { addend: 0 }.accepts()
    }
    fn max_inputs(&self) -> u16 {
        match self.how {
            How::NoInputs => 0,
            _ => 1,
        }
    }
    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        match self.how {
            How::RefusesSchema => Err(MorunaError::Plan("no".into())),
            How::Int32 => Ok(SourceSchema::Table(Arc::new(Schema::new(vec![
                Field::new("x", DataType::Int32, true),
            ])))),
            _ => Add { addend: 0 }.output_schema(input),
        }
    }
    fn init(&self, _ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }
    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        match self.how {
            How::OffByOne => Add { addend: 11 }.apply(state, input),
            How::Wraps => {
                let Payload::Table(batch, _) = input else {
                    unreachable!()
                };
                let x = batch.column(0).as_primitive::<Int64Type>();
                let out: Int64Array = x.iter().map(|v| v.map(|v| v.wrapping_add(1))).collect();
                Payload::table(
                    RecordBatch::try_new(batch.schema(), vec![Arc::new(out)]).expect("batch"),
                )
            }
            How::Tensor => {
                let Payload::Table(batch, _) = input else {
                    unreachable!()
                };
                let column = Arc::new(Int64Array::from(vec![1i64; batch.num_rows()]));
                let p = Payload::table(
                    RecordBatch::try_new(batch.schema(), vec![column]).expect("batch"),
                )?;
                Payload::tensor(p.as_tensor(Some("x"))?)
            }
            How::OtherWords => Err(MorunaError::Plan("something else".into())),
            _ => Add { addend: 10 }.apply(state, input),
        }
    }
}

fn wrong(how: How) -> Implementation {
    fn configure(how: How) -> Configure {
        match how {
            How::OffByOne => |_, _| Ok(Arc::new(Wrong { how: How::OffByOne })),
            How::Wraps => |_, _| Ok(Arc::new(Wrong { how: How::Wraps })),
            How::Int32 => |_, _| Ok(Arc::new(Wrong { how: How::Int32 })),
            How::Stateful => |_, _| Ok(Arc::new(Wrong { how: How::Stateful })),
            How::RefusesSchema => |_, _| {
                Ok(Arc::new(Wrong {
                    how: How::RefusesSchema,
                }))
            },
            How::Tensor => |_, _| Ok(Arc::new(Wrong { how: How::Tensor })),
            How::NoInputs => |_, _| Ok(Arc::new(Wrong { how: How::NoInputs })),
            How::OtherWords => |_, _| {
                Ok(Arc::new(Wrong {
                    how: How::OtherWords,
                }))
            },
        }
    }
    Implementation {
        kernel: "example.add",
        implementation: "wrong",
        versions: &[1],
        configure: configure(how),
    }
}

/// An implementation that fails one case of a version cannot claim it, and the verdict names
/// the case and what went wrong.
#[test]
fn a_wrong_implementation_cannot_claim_the_version() {
    for (how, case, words) in [
        (How::OffByOne, "adds", "row 0 is 12; expected 11"),
        (How::Wraps, "overflow_refused", "the case expects a refusal"),
        (How::Int32, "adds", "the declared output schema"),
        (How::Stateful, "adds", "the specification's state shape"),
        (How::RefusesSchema, "adds", "refused at schema"),
        (How::Tensor, "nulls_stay_null", "it gave a tensor"),
        (How::NoInputs, "adds", "takes at most 0"),
        (
            How::OtherWords,
            "overflow_refused",
            "does not contain `overflow`",
        ),
    ] {
        let report = run(
            &example_root(),
            "example",
            &[EXAMPLE_ADD, EXAMPLE_SCALE, wrong(how)],
            alloc(),
        );
        let verdict = report.verdict().expect_err("a failed claim");
        let line = verdict
            .lines()
            .find(|l| l.contains(&format!("case `{case}`")))
            .unwrap_or_else(|| panic!("no line for `{case}` in:\n{verdict}"));
        assert!(
            line.starts_with("wrong cannot claim example.add@1"),
            "{line}"
        );
        assert!(line.contains(words), "{line}");
        let outcome = report
            .outcomes
            .iter()
            .find(|o| o.implementation == "wrong")
            .expect("outcome");
        assert!(!outcome.holds());
        assert!(
            report
                .outcomes
                .iter()
                .any(|o| o.implementation == CPU_REFERENCE && o.holds())
        );
    }
    // Refusing at configure what the case expects to run is a failure too.
    let refuses = Implementation {
        implementation: "refuses",
        configure: |_, _| Err(MorunaError::Plan("refused".into())),
        ..EXAMPLE_ADD
    };
    let verdict = run(
        &example_root(),
        "example",
        &[EXAMPLE_ADD, EXAMPLE_SCALE, refuses],
        alloc(),
    )
    .verdict()
    .expect_err("refused");
    assert!(verdict.contains("refused at configure"), "{verdict}");
    // Accepting what the case expects refused at configure.
    let accepts = Implementation {
        implementation: "accepts",
        configure: |_, _| Ok(Arc::new(Add { addend: 10 })),
        ..EXAMPLE_ADD
    };
    let verdict = run(
        &example_root(),
        "example",
        &[EXAMPLE_ADD, EXAMPLE_SCALE, accepts],
        alloc(),
    )
    .verdict()
    .expect_err("accepted");
    assert!(verdict.contains("case `addend_required`"), "{verdict}");
}

/// A copy of the worked example in the target's scratch directory, changed by `change`.
fn broken(name: &str, change: impl FnOnce(&Path)) -> PathBuf {
    let to = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("conformance")
        .join(name);
    let _ = std::fs::remove_dir_all(&to);
    copy_dir(&example_root(), &to);
    change(&to);
    to
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("mkdir");
    for entry in std::fs::read_dir(from).expect("read_dir") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

fn edit(path: &Path, from: &str, to: &str) {
    let text = std::fs::read_to_string(path).expect("read");
    assert!(text.contains(from), "{} has no `{from}`", path.display());
    std::fs::write(path, text.replacen(from, to, 1)).expect("write");
}

fn index(root: &Path) -> PathBuf {
    root.join("conformance").join("add").join("index.json")
}

fn scale_index(root: &Path) -> PathBuf {
    root.join("conformance").join("scale").join("index.json")
}

/// An Arrow IPC file of one Int64 column `x` of `rows` rows.
fn write_ints(path: &Path, rows: i64) {
    let schema = Arc::new(Schema::new(vec![Field::new("x", DataType::Int64, true)]));
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![Arc::new(Int64Array::from_iter_values(0..rows))],
    )
    .expect("batch");
    let mut writer =
        FileWriter::try_new(std::fs::File::create(path).expect("create"), &schema).expect("writer");
    writer.write(&batch).expect("write");
    writer.finish().expect("finish");
}

fn spec(root: &Path) -> PathBuf {
    root.join("spec").join("add.md")
}

fn problems_of(root: &Path) -> String {
    check(root, "example", &EXAMPLES).problems.join("\n")
}

/// An edit that breaks a copy of the worked example in one way.
type Change = Box<dyn FnOnce(&Path)>;

/// Every way a specification or a corpus can depart from its format is named.
#[test]
fn every_departure_from_the_formats_is_named() {
    let cases: Vec<(&str, Change, &str)> = vec![
        (
            "missing_heading",
            Box::new(|r: &Path| edit(&spec(r), "## Rounding", "## Rounded")),
            "in that order",
        ),
        (
            "empty_section",
            Box::new(|r: &Path| edit(&spec(r), "Not applicable: integer addition is exact.", "")),
            "`## Rounding` is empty",
        ),
        (
            "bad_title",
            Box::new(|r: &Path| edit(&spec(r), "# example.add@1", "# example.add")),
            "is not a kernel identity",
        ),
        (
            "other_name",
            Box::new(|r: &Path| edit(&spec(r), "# example.add@1", "# example.sum@1")),
            "a kernel's file is its name",
        ),
        (
            "second_version_gap",
            Box::new(|r: &Path| {
                let text = std::fs::read_to_string(spec(r)).expect("read");
                let block = text
                    .split_once("# example.add@1")
                    .expect("block")
                    .1
                    .to_string();
                std::fs::write(
                    spec(r),
                    format!("{text}\n# example.add@3{}", block.replace("`1`", "`3`")),
                )
                .expect("write");
            }),
            "versions are 1, 2, ... in order",
        ),
        (
            "identity_differs",
            Box::new(|r: &Path| edit(&spec(r), "- version: `1`", "- version: `2`")),
            "Identity says version `2`",
        ),
        (
            "identity_missing",
            Box::new(|r: &Path| edit(&spec(r), "- namespace: `example`", "")),
            "Identity has no `- namespace:` line",
        ),
        (
            "bad_shape",
            Box::new(|r: &Path| edit(&spec(r), "- shape: `none`", "- shape: `stateless`")),
            "State shape `stateless`",
        ),
        (
            "no_shape",
            Box::new(|r: &Path| edit(&spec(r), "- shape: `none`", "none")),
            "State shape has no `- shape:` line",
        ),
        (
            "bad_tolerance",
            Box::new(|r: &Path| edit(&spec(r), "- declared: none", "- declared: {\"x\": 1}")),
            "Tolerance:",
        ),
        (
            "no_tolerance",
            Box::new(|r: &Path| edit(&spec(r), "- declared: none", "none")),
            "Tolerance has no `- declared:` line",
        ),
        (
            "text_first",
            Box::new(|r: &Path| edit(&spec(r), "<!--", "prose\n<!--")),
            "text before the first",
        ),
        (
            "tolerance_differs",
            Box::new(|r: &Path| {
                edit(
                    &spec(r),
                    "- declared: none",
                    "- declared: `{\"x\": {\"abs\": 1, \"rel\": 0}}`",
                )
            }),
            "the tolerance of example.add@1 is None in the corpus",
        ),
        (
            "not_json",
            Box::new(|r: &Path| std::fs::write(index(r), "{").expect("write")),
            "index.json",
        ),
        (
            "not_object",
            Box::new(|r: &Path| std::fs::write(index(r), "[]").expect("write")),
            "index.json is an object",
        ),
        (
            "unknown_member",
            Box::new(|r: &Path| edit(&index(r), "\"format\"", "\"extra\": 1, \"format\"")),
            "index.json has `extra`",
        ),
        (
            "bad_format",
            Box::new(|r: &Path| {
                edit(
                    &index(r),
                    "moruna-kernel-corpus/1",
                    "moruna-kernel-corpus/2",
                )
            }),
            "needs `\"format\"",
        ),
        (
            "bad_kernel",
            Box::new(|r: &Path| {
                edit(
                    &index(r),
                    "\"kernel\": \"example.add\"",
                    "\"kernel\": \"add\"",
                )
            }),
            "needs `kernel`",
        ),
        (
            "other_kernel",
            Box::new(|r: &Path| {
                edit(
                    &index(r),
                    "\"kernel\": \"example.add\"",
                    "\"kernel\": \"example.sum\"",
                )
            }),
            "says it is of `example.sum`",
        ),
        (
            "bad_tolerance_version",
            Box::new(|r: &Path| {
                edit(
                    &index(r),
                    "\"cases\"",
                    "\"tolerance\": {\"one\": {\"x\": {\"abs\": 0, \"rel\": 0}}}, \"cases\"",
                )
            }),
            "`one` is not one",
        ),
        (
            "tolerance_unpublished",
            Box::new(|r: &Path| {
                edit(
                    &index(r),
                    "\"cases\"",
                    "\"tolerance\": {\"2\": {\"x\": {\"abs\": 0, \"rel\": 0}}}, \"cases\"",
                )
            }),
            "a tolerance for version 2",
        ),
        (
            "tolerance_not_object",
            Box::new(|r: &Path| edit(&index(r), "\"cases\"", "\"tolerance\": 1, \"cases\"")),
            "`tolerance` is an object by version",
        ),
        (
            "bad_tolerance_value",
            Box::new(|r: &Path| {
                edit(
                    &index(r),
                    "\"cases\"",
                    "\"tolerance\": {\"1\": {}}, \"cases\"",
                )
            }),
            "tolerance of version 1",
        ),
        (
            "no_cases",
            Box::new(|r: &Path| {
                std::fs::write(index(r), "{\"format\": \"moruna-kernel-corpus/1\", \"kernel\": \"example.add\", \"cases\": []}").expect("write")
            }),
            "a non-empty list",
        ),
        (
            "case_not_object",
            Box::new(|r: &Path| edit(&index(r), "\"cases\": [", "\"cases\": [1, ")),
            "case 0 is an object",
        ),
        (
            "case_member",
            Box::new(|r: &Path| {
                edit(
                    &index(r),
                    "\"name\": \"adds\",",
                    "\"name\": \"adds\", \"why\": 1,",
                )
            }),
            "case 0 has `why`",
        ),
        (
            "case_name",
            Box::new(|r: &Path| edit(&index(r), "\"name\": \"adds\"", "\"name\": \"Adds\"")),
            "needs `name`",
        ),
        (
            "case_twice",
            Box::new(|r: &Path| edit(&index(r), "\"name\": \"sliced\"", "\"name\": \"adds\"")),
            "two cases are named `adds`",
        ),
        (
            "case_version",
            Box::new(|r: &Path| edit(&index(r), "\"version\": 1", "\"version\": 0")),
            "needs `version`",
        ),
        (
            "case_unpublished",
            Box::new(|r: &Path| edit(&index(r), "\"version\": 1", "\"version\": 2")),
            "which the specification does not publish",
        ),
        (
            "case_tests",
            Box::new(|r: &Path| {
                edit(
                    &index(r),
                    "\"tests\": [\"params\", \"multi_batch\"]",
                    "\"tests\": []",
                )
            }),
            "needs `tests`",
        ),
        (
            "case_tag",
            Box::new(|r: &Path| edit(&index(r), "\"multi_batch\"", "\"multi batch\"")),
            "every tag of `tests`",
        ),
        (
            "case_about",
            Box::new(|r: &Path| {
                edit(
                    &index(r),
                    "\"about\": \"Each of two batches gives x + addend, in its rows' order.\"",
                    "\"about\": \" \"",
                )
            }),
            "needs `about`",
        ),
        (
            "case_params",
            Box::new(|r: &Path| edit(&index(r), "\"params\": {\"addend\": 10}", "\"params\": 10")),
            "`params` is an object",
        ),
        (
            "case_arrays",
            Box::new(|r: &Path| {
                edit(
                    &index(r),
                    "\"params\": {\"addend\": 10}",
                    "\"params\": {\"addend\": 10}, \"arrays\": 1",
                )
            }),
            "`arrays` is an object",
        ),
        (
            "case_array",
            Box::new(|r: &Path| {
                edit(
                    &index(r),
                    "\"params\": {\"addend\": 10}",
                    "\"params\": {\"addend\": 10}, \"arrays\": {\"v\": {\"file\": \"adds.in.arrow\"}}",
                )
            }),
            "holds 2 batches",
        ),
        (
            "case_inputs",
            Box::new(|r: &Path| {
                edit(
                    &index(r),
                    "\"inputs\": [{\"file\": \"adds.in.arrow\"}],\n      \"expected\"",
                    "\"inputs\": [],\n      \"expected\"",
                )
            }),
            "needs `inputs`",
        ),
        (
            "source_shape",
            Box::new(|r: &Path| {
                edit(
                    &index(r),
                    "{\"file\": \"adds.in.arrow\"}",
                    "\"adds.in.arrow\"",
                )
            }),
            "input 0 is {",
        ),
        (
            "source_member",
            Box::new(|r: &Path| {
                edit(
                    &index(r),
                    "{\"file\": \"adds.in.arrow\"}",
                    "{\"file\": \"adds.in.arrow\", \"at\": 1}",
                )
            }),
            "input 0 has `at`",
        ),
        (
            "source_path",
            Box::new(|r: &Path| {
                edit(
                    &index(r),
                    "{\"file\": \"adds.in.arrow\"}",
                    "{\"file\": \"../adds.in.arrow\"}",
                )
            }),
            "the plain name of a file",
        ),
        (
            "source_slice",
            Box::new(|r: &Path| edit(&index(r), "\"slice\": [3, 4]", "\"slice\": [3]")),
            "`slice` is [offset, length]",
        ),
        (
            "source_slice_type",
            Box::new(|r: &Path| edit(&index(r), "\"slice\": [3, 4]", "\"slice\": [3, -4]")),
            "`slice` is [offset, length]",
        ),
        (
            "source_slice_long",
            Box::new(|r: &Path| edit(&index(r), "\"slice\": [3, 4]", "\"slice\": [3, 40]")),
            "is outside a batch of 8 rows",
        ),
        (
            "source_missing",
            Box::new(|r: &Path| {
                std::fs::remove_file(r.join("conformance/add/max.in.arrow")).expect("rm")
            }),
            "max.in.arrow",
        ),
        (
            "source_not_ipc",
            Box::new(|r: &Path| {
                std::fs::write(r.join("conformance/add/max.in.arrow"), "no").expect("write")
            }),
            "not an Arrow IPC file",
        ),
        (
            "expected_missing",
            Box::new(|r: &Path| {
                std::fs::remove_file(r.join("conformance/add/sliced.expected.arrow")).expect("rm")
            }),
            "sliced.expected.arrow",
        ),
        (
            "both_expected_and_error",
            Box::new(|r: &Path| {
                edit(
                    &index(r),
                    "\"error\": {\"at\": \"configure\"}",
                    "\"error\": {\"at\": \"configure\"}, \"expected\": \"adds.expected.arrow\"",
                )
            }),
            "needs one of `expected`",
        ),
        (
            "error_step",
            Box::new(|r: &Path| edit(&index(r), "\"at\": \"configure\"", "\"at\": \"build\"")),
            "`error.at` is",
        ),
        (
            "error_member",
            Box::new(|r: &Path| {
                edit(
                    &index(r),
                    "\"at\": \"configure\"",
                    "\"at\": \"configure\", \"code\": 1",
                )
            }),
            "`error` has `code`",
        ),
        (
            "error_contains",
            Box::new(|r: &Path| edit(&index(r), "\"contains\": \"overflow\"", "\"contains\": 1")),
            "`error.contains` is text",
        ),
        (
            "unnamed_file",
            Box::new(|r: &Path| {
                std::fs::write(r.join("conformance/add/stale.arrow"), "").expect("write")
            }),
            "stale.arrow is named by no case",
        ),
        (
            "required_tag",
            Box::new(|r: &Path| edit(&index(r), "\"tests\": [\"empty\"]", "\"tests\": [\"zero\"]")),
            "tests `empty`",
        ),
        (
            "no_corpus",
            Box::new(|r: &Path| {
                std::fs::rename(r.join("conformance/add"), r.join("conformance/_add")).expect("mv")
            }),
            "has a specification and no corpus",
        ),
        (
            "no_spec",
            Box::new(|r: &Path| std::fs::rename(spec(r), r.join("spec/_add.md")).expect("mv")),
            "has a corpus and no specification",
        ),
        (
            "allowed_not_per_element",
            Box::new(|r: &Path| {
                edit(
                    &index(r),
                    "\"expected\": \"adds.expected.arrow\"",
                    "\"expected\": \"adds.expected.arrow\", \"allowed\": {\"x\": {\"file\": \"adds.expected.arrow\"}}",
                )
            }),
            "declares Exact, not per-element",
        ),
        (
            "allowed_with_error",
            Box::new(|r: &Path| {
                edit(
                    &index(r),
                    "\"error\": {\"at\": \"configure\"}",
                    "\"error\": {\"at\": \"configure\"}, \"allowed\": {}",
                )
            }),
            "goes with `expected`",
        ),
        (
            "allowed_not_object",
            Box::new(|r: &Path| {
                edit(
                    &scale_index(r),
                    "\"allowed\": {\n        \"x\": {\n          \"file\": \"x.allowed.arrow\"\n        }\n      }",
                    "\"allowed\": 1",
                )
            }),
            "`allowed` is a non-empty object",
        ),
        (
            "allowed_other_column",
            Box::new(|r: &Path| {
                edit(
                    &scale_index(r),
                    "\"x\": {\n          \"file\": \"x.allowed.arrow\"",
                    "\"y\": {\n          \"file\": \"x.allowed.arrow\"",
                )
            }),
            "names `y`, which the expected output has not",
        ),
        (
            "allowed_rows",
            Box::new(|r: &Path| {
                edit(
                    &scale_index(r),
                    "\"file\": \"x.allowed.arrow\"",
                    "\"file\": \"sliced.allowed.arrow\"",
                )
            }),
            "has 4 rows, and the expected output 7",
        ),
        (
            "allowed_negative",
            Box::new(|r: &Path| {
                edit(
                    &scale_index(r),
                    "\"file\": \"x.allowed.arrow\"",
                    "\"file\": \"x.in.arrow\"",
                )
            }),
            "each is a number from 0",
        ),
        (
            "allowed_type",
            Box::new(|r: &Path| {
                write_ints(&r.join("conformance/scale/ints.arrow"), 7);
                edit(
                    &scale_index(r),
                    "\"file\": \"x.allowed.arrow\"",
                    "\"file\": \"ints.arrow\"",
                );
            }),
            "is one column of type Float64",
        ),
        (
            "per_element_with_bounds",
            Box::new(|r: &Path| {
                edit(
                    &scale_index(r),
                    "\"cases\"",
                    "\"tolerance\": {\"1\": {\"x\": {\"abs\": 0, \"rel\": 0}}}, \"cases\"",
                )
            }),
            "the tolerance of example.scale@1",
        ),
    ];
    for (name, change, words) in cases {
        let root = broken(name, change);
        let problems = problems_of(&root);
        assert!(
            problems.contains(words),
            "{name}: `{words}` not in:\n{problems}"
        );
    }
    // Another namespace than the catalogue's.
    let problems = check(&example_root(), "moruna", &EXAMPLES)
        .problems
        .join("\n");
    assert!(
        problems.contains("is not in the namespace `moruna`"),
        "{problems}"
    );
}

/// `example.scale@1` off by a relative `error`.
struct Off {
    factor: f64,
    error: f64,
}

impl Kernel for Off {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint([3; 32])
    }
    fn kind(&self) -> KernelKind {
        KernelKind::Stateless
    }
    fn accepts(&self) -> PayloadSpec {
        Scale { factor: 1.0 }.accepts()
    }
    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        Scale { factor: 1.0 }.output_schema(input)
    }
    fn init(&self, _ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }
    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        Scale {
            factor: self.factor * (1.0 + self.error),
        }
        .apply(state, input)
    }
}

fn configure_near(_: u32, p: &Params) -> Result<Arc<dyn Kernel>> {
    let factor = p.json["factor"].as_f64().expect("factor");
    Ok(Arc::new(Off {
        factor,
        error: 2f64.powi(-51),
    }))
}

fn configure_far(_: u32, p: &Params) -> Result<Arc<dyn Kernel>> {
    let factor = p.json["factor"].as_f64().expect("factor");
    Ok(Arc::new(Off {
        factor,
        error: 1e-9,
    }))
}

/// Within each element's allowed error an implementation passes; past it, or where the case
/// wants the exact value, it cannot claim the version.
#[test]
fn a_per_element_tolerance_is_held_element_by_element() {
    let near = Implementation {
        implementation: "near",
        configure: configure_near,
        ..EXAMPLE_SCALE
    };
    let far = Implementation {
        implementation: "far",
        configure: configure_far,
        ..EXAMPLE_SCALE
    };
    let report = run(
        &example_root(),
        "example",
        &[EXAMPLE_ADD, EXAMPLE_SCALE, near, far],
        alloc(),
    );
    let verdict = report.verdict().expect_err("far fails");
    let near = report
        .outcomes
        .iter()
        .find(|o| o.implementation == "near")
        .expect("near");
    // A little off passes where an allowed error is given, and fails where the case wants the
    // exact value or gives none.
    let failed: Vec<&str> = near.failures.iter().map(|(c, _)| c.as_str()).collect();
    assert_eq!(
        failed,
        ["exact_where_unbounded", "two_batches_exact"],
        "{verdict}"
    );
    let far = report
        .outcomes
        .iter()
        .find(|o| o.implementation == "far")
        .expect("far");
    assert!(
        far.failures
            .iter()
            .any(|(c, why)| c == "scales" && why.contains("within")),
        "{verdict}"
    );
}

/// Work item 4.3: the corpora of `filter` and `take` state that they test nulls, empty input,
/// all-false and all-true predicates, sliced inputs, and dictionary and nested values.
#[test]
fn filter_and_take_corpora_cover_what_the_item_names() {
    let catalogue = check(&root(), "moruna", implementations());
    let tags = |name: &str| -> std::collections::BTreeSet<String> {
        catalogue.corpora[name]
            .cases
            .iter()
            .flat_map(|c| c.tests.clone())
            .collect()
    };
    let filter = tags("filter");
    for tag in ["nulls", "empty", "all_false", "all_true", "sliced"] {
        assert!(filter.contains(tag), "filter: {tag}");
    }
    let take = tags("take");
    for tag in ["nulls", "empty", "sliced", "dictionary", "nested"] {
        assert!(take.contains(tag), "take: {tag}");
    }
}
