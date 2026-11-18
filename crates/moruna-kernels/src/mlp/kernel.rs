//! `moruna.mlp_forward@1` under the `Kernel` trait (system design 4A): configured from the
//! catalogue's [`Params`], a table in and a table of one column out.
//!
//! The model handle is Arrow data: `layer_1` … `layer_L`, one batch per layer in the network's
//! order, one row per output unit, with a `weights` column (`FixedSizeList<Float64, inputs>`, row
//! `j` holding output `j`'s weight from each input) and an optional `bias` column (`Float64`). The
//! JSON parameters are `precision`, `activations` (one per layer), `inputs` (the feature columns)
//! or `input` (one list column), and `output` (the output column's name, `y` by default).

use std::sync::Arc;

use moruna_kernel::arrow::array::{Array, ArrayRef, AsArray, RecordBatch};
use moruna_kernel::arrow::datatypes::{DataType, Field, Float64Type, Schema, SchemaRef};
use moruna_kernel::declare::{ColumnDecl, Declared, SchemaDecl};
use moruna_kernel::{
    Fingerprint, InitCtx, Kernel, KernelHints, KernelKind, KernelState, Payload, PayloadKind,
    PayloadSpec, Result, ResumePolicy, SourceSchema, TierPref,
};
use serde_json::{Map, Value};

use super::{Activation, Layer, MlpForward, Model, NAME, Precision, Scratch, refuse};
use crate::catalogue::{KernelId, MORUNA, Params, fingerprint};

/// The versions the CPU reference implements.
pub const VERSIONS: &[u32] = &[1];

/// The feature columns a batch is read from.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Features {
    /// `k` Float columns, by name, in the model's input order.
    Columns(Vec<String>),
    /// One `FixedSizeList<Float, k>` column.
    List(String),
}

impl Features {
    fn names(&self) -> Vec<String> {
        match self {
            Features::Columns(names) => names.clone(),
            Features::List(name) => vec![name.clone()],
        }
    }
}

/// `moruna.mlp_forward@1`, configured.
#[derive(Debug)]
pub struct MlpKernel {
    forward: MlpForward,
    features: Features,
    output: String,
    schema: SchemaRef,
    fingerprint: Fingerprint,
}

/// Configure `moruna.mlp_forward@version` from the catalogue's parameters (its
/// [`crate::catalogue::Configure`]).
pub fn configure(version: u32, params: &Params) -> Result<Arc<dyn Kernel>> {
    Ok(Arc::new(MlpKernel::configure(version, params)?))
}

fn strings(value: &Value, what: &str) -> Result<Vec<String>> {
    let list = value
        .as_array()
        .filter(|l| !l.is_empty())
        .ok_or_else(|| refuse(format!("`{what}` is a non-empty list of names")))?;
    list.iter()
        .map(|v| {
            v.as_str()
                .map(str::to_string)
                .ok_or_else(|| refuse(format!("`{what}` holds {v}, which is not a name")))
        })
        .collect()
}

fn name(map: &Map<String, Value>, key: &str) -> Result<Option<String>> {
    match map.get(key) {
        None => Ok(None),
        Some(Value::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
        Some(other) => Err(refuse(format!("`{key}` is a column name, not {other}"))),
    }
}

/// One layer from its batch: a `weights` list per output unit, and an optional `bias`.
fn layer(label: &str, batch: &RecordBatch, activation: Activation) -> Result<Layer> {
    let outputs = batch.num_rows();
    let schema = batch.schema();
    if let Some(other) = schema
        .fields()
        .iter()
        .find(|f| f.name() != "weights" && f.name() != "bias")
    {
        return Err(refuse(format!(
            "`{label}` has a column `{}`; a layer has `weights` and `bias` alone",
            other.name()
        )));
    }
    let weights = batch.column_by_name("weights").ok_or_else(|| {
        refuse(format!(
            "`{label}` has no `weights` column (FixedSizeList<Float64, inputs>, one row per output)"
        ))
    })?;
    let inputs = match weights.data_type() {
        DataType::FixedSizeList(item, size) if item.data_type() == &DataType::Float64 => {
            *size as usize
        }
        other => {
            return Err(refuse(format!(
                "`{label}`'s `weights` is FixedSizeList<Float64, inputs>, not {other}"
            )));
        }
    };
    let list = weights.as_fixed_size_list();
    let values = list.values().as_primitive::<Float64Type>();
    if list.null_count() > 0 || values.null_count() > 0 {
        return Err(refuse(format!("`{label}`'s `weights` has a null")));
    }
    // Row j holds output j's weights; the layer holds them inputs × outputs, row-major.
    let mut w = vec![0.0; inputs * outputs];
    for (j, row) in values.values().chunks_exact(inputs.max(1)).enumerate() {
        for (i, v) in row.iter().enumerate() {
            w[i * outputs + j] = *v;
        }
    }
    let bias = match batch.column_by_name("bias") {
        None => None,
        Some(b) if b.data_type() == &DataType::Float64 && b.null_count() == 0 => {
            Some(b.as_primitive::<Float64Type>().values().to_vec())
        }
        Some(b) => {
            return Err(refuse(format!(
                "`{label}`'s `bias` is Float64 with no nulls, not {}{}",
                b.data_type(),
                if b.null_count() > 0 {
                    " with nulls"
                } else {
                    ""
                }
            )));
        }
    };
    Layer::new(inputs, outputs, w, bias, activation).map_err(|e| refuse(format!("`{label}`: {e}")))
}

impl MlpKernel {
    /// Configure version `version` (1) from `params`, refusing what the specification refuses.
    pub fn configure(version: u32, params: &Params) -> Result<MlpKernel> {
        if !VERSIONS.contains(&version) {
            return Err(refuse(format!("has no version {version}")));
        }
        let map = params
            .json
            .as_object()
            .ok_or_else(|| refuse("parameters are an object"))?;
        let known = ["precision", "activations", "inputs", "input", "output"];
        if let Some(key) = map.keys().find(|k| !known.contains(&k.as_str())) {
            return Err(refuse(format!(
                "has no parameter `{key}` (its parameters are {})",
                known.join(", ")
            )));
        }
        let precision = match map.get("precision") {
            Some(Value::String(p)) => Precision::parse(p)?,
            _ => {
                return Err(refuse(
                    "`precision` is required: \"float32\" or \"float64\"",
                ));
            }
        };
        let activations: Vec<Activation> = strings(
            map.get("activations")
                .ok_or_else(|| refuse("`activations` is required, one per layer"))?,
            "activations",
        )?
        .iter()
        .map(|a| Activation::parse(a))
        .collect::<Result<_>>()?;
        let features = match (map.get("inputs"), name(map, "input")?) {
            (Some(list), None) => {
                let names = strings(list, "inputs")?;
                let mut seen = std::collections::BTreeSet::new();
                if let Some(dup) = names.iter().find(|n| !seen.insert(n.as_str())) {
                    return Err(refuse(format!("`inputs` names `{dup}` twice")));
                }
                Features::Columns(names)
            }
            (None, Some(one)) => Features::List(one),
            _ => {
                return Err(refuse(
                    "takes either `inputs` (the feature columns) or `input` (one list column)",
                ));
            }
        };
        let output = name(map, "output")?.unwrap_or_else(|| "y".to_string());
        let wanted: Vec<String> = (1..=activations.len())
            .map(|l| format!("layer_{l}"))
            .collect();
        if let Some(extra) = params.arrays.keys().find(|k| !wanted.contains(k)) {
            return Err(refuse(format!(
                "has no Arrow parameter `{extra}` (it takes {})",
                wanted.join(", ")
            )));
        }
        let mut layers = Vec::with_capacity(activations.len());
        for (label, activation) in wanted.iter().zip(activations) {
            let batch = params.arrays.get(label).ok_or_else(|| {
                refuse(format!(
                    "`activations` names {} layers and `{label}` is not given",
                    wanted.len()
                ))
            })?;
            layers.push(layer(label, batch, activation)?);
        }
        let model = Model::new(layers)?;
        if let Features::Columns(names) = &features
            && names.len() != model.inputs()
        {
            return Err(refuse(format!(
                "`inputs` names {} columns and `layer_1` takes {} inputs",
                names.len(),
                model.inputs()
            )));
        }
        let forward = MlpForward::configure(model, precision);
        let schema = Arc::new(Schema::new(vec![Field::new(
            output.clone(),
            forward.output_type(),
            true,
        )]));
        let id = KernelId {
            namespace: MORUNA.to_string(),
            name: NAME.to_string(),
            version,
        };
        Ok(MlpKernel {
            fingerprint: fingerprint(&id, params)?,
            forward,
            features,
            output,
            schema,
        })
    }

    /// The configured forward pass.
    pub fn forward(&self) -> &MlpForward {
        &self.forward
    }

    /// Whether a batch of `schema` holds the feature columns, of types the model takes.
    fn check_schema(&self, schema: &Schema) -> Result<()> {
        let types = self
            .features
            .names()
            .iter()
            .map(|n| {
                schema
                    .field_with_name(n)
                    .map(|f| f.data_type().clone())
                    .map_err(|_| refuse(format!("takes the column `{n}`, which the input has not")))
            })
            .collect::<Result<Vec<_>>>()?;
        self.forward.check_inputs(&types)
    }
}

/// An instance's scratch, held across batches.
struct State {
    scratch: Scratch,
}

impl KernelState for State {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn footprint(&self) -> Result<Option<u64>> {
        Ok(Some(self.scratch.bytes() as u64))
    }
}

impl Kernel for MlpKernel {
    fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    fn kind(&self) -> KernelKind {
        KernelKind::Stateless
    }

    fn hints(&self) -> KernelHints {
        KernelHints {
            expected_amplification: None,
            uses_device_memory: false,
            releases_gil: Some(true),
            preferred_rows: None,
            resume: ResumePolicy::Reinit,
            state_bytes: Some(self.forward.state_bytes()),
        }
    }

    fn declared(&self) -> Declared {
        Declared {
            input: Some(SchemaDecl::Subset(
                self.features
                    .names()
                    .into_iter()
                    .map(ColumnDecl::any)
                    .collect(),
            )),
            output: Some(SchemaDecl::Exact(vec![ColumnDecl::new(
                self.output.clone(),
                self.forward.output_type(),
            )])),
        }
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
                self.check_schema(schema)?;
                Ok(SourceSchema::Table(Arc::clone(&self.schema)))
            }
            SourceSchema::Tensor { .. } => Err(refuse("takes a table, and the input is a tensor")),
        }
    }

    fn init(&self, _ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        Ok(Box::new(State {
            scratch: Scratch::default(),
        }))
    }

    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        let Payload::Table(batch, _) = &input else {
            return Err(refuse("takes a table, and was given a tensor"));
        };
        let state = state
            .as_any_mut()
            .downcast_mut::<State>()
            .ok_or_else(|| refuse("was given a state it did not make"))?;
        let names = self.features.names();
        let arrays: Vec<ArrayRef> = names
            .iter()
            .map(|n| {
                batch.column_by_name(n).cloned().ok_or_else(|| {
                    refuse(format!("takes the column `{n}`, which the input has not"))
                })
            })
            .collect::<Result<_>>()?;
        let y = self.forward.forward(&arrays, &mut state.scratch)?;
        let out = RecordBatch::try_new(Arc::clone(&self.schema), vec![y])
            .map_err(|e| refuse(format!("the output batch: {e}")))?;
        drop(input);
        Payload::table(out)
    }
}

impl MlpKernel {
    /// The output column's name.
    pub fn output(&self) -> &str {
        &self.output
    }

    /// The feature columns' names.
    pub fn features(&self) -> Vec<String> {
        self.features.names()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moruna_kernel::arrow::array::{FixedSizeListArray, Float64Array, Int64Array};
    use moruna_kernel::arrow::buffer::NullBuffer;
    use serde_json::json;

    /// A layer batch of `outputs` rows: output j's weights, then its bias.
    fn layer_batch(rows: &[Vec<f64>], bias: Option<&[f64]>) -> RecordBatch {
        let inputs = rows[0].len();
        let values = Float64Array::from_iter_values(rows.iter().flatten().copied());
        let weights = FixedSizeListArray::new(
            Arc::new(Field::new_list_field(DataType::Float64, false)),
            inputs as i32,
            Arc::new(values),
            None,
        );
        let mut cols: Vec<(&str, ArrayRef)> = vec![("weights", Arc::new(weights))];
        if let Some(b) = bias {
            cols.push(("bias", Arc::new(Float64Array::from(b.to_vec()))));
        }
        RecordBatch::try_from_iter(cols).unwrap()
    }

    fn params(json: Value, layers: Vec<RecordBatch>) -> Params {
        Params {
            json,
            arrays: layers
                .into_iter()
                .enumerate()
                .map(|(i, b)| (format!("layer_{}", i + 1), b))
                .collect(),
        }
    }

    /// x0 * 2 + x1 * 3 + 1, then relu; then y = h * -1 (no bias).
    fn two_layers(extra: Value) -> Params {
        let mut j = json!({"precision": "float64", "activations": ["relu", "identity"], "inputs": ["a", "b"]});
        for (k, v) in extra.as_object().unwrap() {
            j[k] = v.clone();
        }
        params(
            j,
            vec![
                layer_batch(&[vec![2.0, 3.0]], Some(&[1.0])),
                layer_batch(&[vec![-1.0]], None),
            ],
        )
    }

    fn batch(a: Vec<Option<f64>>, b: Vec<f64>) -> RecordBatch {
        RecordBatch::try_from_iter(vec![
            (
                "id",
                Arc::new(Int64Array::from_iter_values(0..a.len() as i64)) as ArrayRef,
            ),
            ("a", Arc::new(Float64Array::from(a)) as ArrayRef),
            ("b", Arc::new(Float64Array::from(b)) as ArrayRef),
        ])
        .unwrap()
    }

    fn run(kernel: &dyn Kernel, batch: RecordBatch) -> Result<RecordBatch> {
        let ctx = InitCtx {
            instance: 0,
            device: None,
            alloc: Arc::new(moruna_testkit::FakeAllocator::new()),
        };
        let mut state = kernel.init(&ctx)?;
        match kernel.apply(state.as_mut(), Payload::table(batch)?)? {
            Payload::Table(out, _) => Ok(out),
            Payload::Tensor(..) => panic!("a tensor"),
        }
    }

    #[test]
    fn a_configured_network_reads_its_columns_and_gives_one() {
        let kernel = configure(1, &two_layers(json!({"output": "score"}))).unwrap();
        let input = batch(vec![Some(1.0), None, Some(-5.0)], vec![1.0, 2.0, 0.0]);
        let SourceSchema::Table(out_schema) = kernel
            .output_schema(&SourceSchema::Table(input.schema()))
            .unwrap()
        else {
            panic!("a tensor schema")
        };
        assert_eq!(out_schema.field(0).name(), "score");
        let out = run(kernel.as_ref(), input).unwrap();
        assert_eq!(out.schema(), out_schema);
        let y = out.column(0).as_primitive::<Float64Type>();
        assert_eq!(y.value(0), -6.0);
        assert!(y.is_null(1));
        assert_eq!(y.value(2), 0.0);
        assert!(matches!(kernel.kind(), KernelKind::Stateless));
        assert_eq!(kernel.hints().state_bytes, Some(4 * 8));
        assert!(kernel.declared().output.is_some());
        assert_eq!(kernel.accepts().kind, PayloadKind::Table);
    }

    #[test]
    fn a_list_column_and_float32_and_the_fingerprint_follows_the_weights() {
        let p = params(
            json!({"precision": "float32", "activations": ["tanh"], "input": "f"}),
            vec![layer_batch(&[vec![1.0, 0.0], vec![0.0, 1.0]], None)],
        );
        let k = MlpKernel::configure(1, &p).unwrap();
        assert_eq!(k.output(), "y");
        assert_eq!(k.features(), vec!["f".to_string()]);
        assert_eq!(k.forward().precision(), Precision::Float32);
        let values = Float64Array::from(vec![0.5, -0.5, 1.0, 2.0]);
        let list = FixedSizeListArray::new(
            Arc::new(Field::new_list_field(DataType::Float64, true)),
            2,
            Arc::new(values),
            Some(NullBuffer::from(vec![true, false])),
        );
        let input = RecordBatch::try_from_iter(vec![("f", Arc::new(list) as ArrayRef)]).unwrap();
        let out = run(&k, input).unwrap();
        let y = out.column(0).as_fixed_size_list();
        assert_eq!(y.value_length(), 2);
        assert!(y.is_null(1));
        let v = y
            .values()
            .as_primitive::<moruna_kernel::arrow::datatypes::Float32Type>();
        assert_eq!(v.value(0), 0.5f32.tanh());
        let other = params(
            json!({"precision": "float32", "activations": ["tanh"], "input": "f"}),
            vec![layer_batch(&[vec![1.0, 0.0], vec![0.0, 2.0]], None)],
        );
        assert_ne!(
            k.fingerprint(),
            MlpKernel::configure(1, &other).unwrap().fingerprint()
        );
    }

    #[test]
    fn parameters_that_break_the_specification_are_refused_at_configure() {
        let refused = |p: &Params| configure(1, p).err().expect("refused").to_string();
        assert!(configure(2, &two_layers(json!({}))).is_err());
        assert!(refused(&Params::json(Value::Null)).contains("object"));
        assert!(refused(&two_layers(json!({"extra": 1}))).contains("`extra`"));
        assert!(refused(&two_layers(json!({"precision": "float16"}))).contains("float16"));
        assert!(refused(&two_layers(json!({"precision": 64}))).contains("precision"));
        assert!(refused(&two_layers(json!({"activations": ["relu"]}))).contains("layer_2"));
        assert!(
            refused(&two_layers(json!({"activations": ["relu", "softmax"]}))).contains("softmax")
        );
        assert!(refused(&two_layers(json!({"activations": "relu"}))).contains("activations"));
        assert!(refused(&two_layers(json!({"input": "f"}))).contains("either"));
        assert!(refused(&two_layers(json!({"inputs": ["a", "a"]}))).contains("twice"));
        assert!(refused(&two_layers(json!({"inputs": ["a"]}))).contains("takes 2 inputs"));
        assert!(refused(&two_layers(json!({"inputs": [1, 2]}))).contains("not a name"));
        assert!(refused(&two_layers(json!({"output": ""}))).contains("column name"));
        let mut p = two_layers(json!({}));
        p.json.as_object_mut().unwrap().remove("activations");
        assert!(refused(&p).contains("required"));
        let mut p = two_layers(json!({}));
        p.arrays
            .insert("layer_3".into(), layer_batch(&[vec![1.0]], None));
        assert!(refused(&p).contains("layer_3"));
        // A layer that does not take what the one before gives.
        let p = params(
            json!({"precision": "float64", "activations": ["relu", "relu"], "inputs": ["a", "b"]}),
            vec![
                layer_batch(&[vec![1.0, 1.0]], None),
                layer_batch(&[vec![1.0, 1.0]], None),
            ],
        );
        assert!(refused(&p).contains("layer 1 gives 1 outputs"));
        // Layer batches of the wrong shape.
        let bad = |b: RecordBatch| {
            refused(&params(
                json!({"precision": "float64", "activations": ["relu"], "inputs": ["a"]}),
                vec![b],
            ))
        };
        let w = |ty: DataType, v: ArrayRef| -> ArrayRef {
            Arc::new(FixedSizeListArray::new(
                Arc::new(Field::new_list_field(ty, true)),
                1,
                v,
                None,
            ))
        };
        let f64s = Arc::new(Float64Array::from(vec![1.0])) as ArrayRef;
        assert!(
            bad(RecordBatch::try_from_iter(vec![("bias", f64s.clone())]).unwrap())
                .contains("no `weights`")
        );
        assert!(
            bad(RecordBatch::try_from_iter(vec![("weights", f64s.clone())]).unwrap())
                .contains("FixedSizeList<Float64")
        );
        assert!(
            bad(RecordBatch::try_from_iter(vec![
                ("weights", w(DataType::Float64, f64s.clone())),
                ("scale", f64s.clone())
            ])
            .unwrap())
            .contains("`scale`")
        );
        assert!(
            bad(RecordBatch::try_from_iter(vec![(
                "weights",
                w(
                    DataType::Float64,
                    Arc::new(Float64Array::from(vec![None::<f64>]))
                )
            )])
            .unwrap())
            .contains("null")
        );
        assert!(
            bad(RecordBatch::try_from_iter(vec![
                ("weights", w(DataType::Float64, f64s.clone())),
                ("bias", Arc::new(Int64Array::from(vec![1])) as ArrayRef)
            ])
            .unwrap())
            .contains("Int64")
        );
        assert!(
            bad(RecordBatch::try_from_iter(vec![(
                "weights",
                w(
                    DataType::Float64,
                    Arc::new(Float64Array::from(vec![f64::INFINITY]))
                )
            )])
            .unwrap())
            .contains("finite")
        );
    }

    #[test]
    fn inputs_without_the_columns_or_of_the_wrong_type_are_refused() {
        let kernel = configure(1, &two_layers(json!({}))).unwrap();
        let only_a = RecordBatch::try_from_iter(vec![(
            "a",
            Arc::new(Float64Array::from(vec![1.0])) as ArrayRef,
        )])
        .unwrap();
        let err = kernel
            .output_schema(&SourceSchema::Table(only_a.schema()))
            .unwrap_err();
        assert!(err.to_string().contains("`b`"), "{err}");
        assert!(
            run(kernel.as_ref(), only_a)
                .unwrap_err()
                .to_string()
                .contains("`b`")
        );
        let ints = RecordBatch::try_from_iter(vec![
            ("a", Arc::new(Int64Array::from(vec![1])) as ArrayRef),
            ("b", Arc::new(Float64Array::from(vec![1.0])) as ArrayRef),
        ])
        .unwrap();
        assert!(
            kernel
                .output_schema(&SourceSchema::Table(ints.schema()))
                .unwrap_err()
                .to_string()
                .contains("Int64")
        );
        let tensor = SourceSchema::Tensor {
            dtype: moruna_kernel::DType::F64,
            shape: vec![-1, 2],
        };
        assert!(kernel.output_schema(&tensor).is_err());
    }
}
