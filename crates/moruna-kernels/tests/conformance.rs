//! The conformance runner (system design 4A): every corpus against every implementation the
//! build has, and the runner's own checks on the worked example, `example.add@1`.

#![allow(clippy::result_large_err)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use moruna_kernel::arrow::array::{AsArray, Float64Array, Int64Array, RecordBatch};
use moruna_kernel::arrow::datatypes::{DataType, Field, Float64Type, Int64Type, Schema};
use moruna_kernel::{
    Fingerprint, InitCtx, Kernel, KernelKind, KernelState, MorunaError, NoState, Payload,
    PayloadKind, PayloadSpec, Result, SourceSchema, TierPref,
};
use moruna_kernels::catalogue::{CPU_REFERENCE, Implementation, Params, implementations};
use moruna_kernels::conformance::{Precision, check};

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
