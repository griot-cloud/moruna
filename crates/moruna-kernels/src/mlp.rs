//! `mlp_forward`'s CPU reference (system design 4B, item 4.6): dense layers (multiply, bias,
//! activation) over a fixed-width feature vector per row.
//!
//! The model is fixed at configure ([`MlpForward::configure`]): its layers' weights and biases are
//! converted once to the declared [`Precision`] and held for every batch after. A batch is
//! processed in blocks of [`BLOCK_ROWS`] rows: a block's features are gathered into one matrix,
//! each layer is one matrix multiply (`matrixmultiply`'s `sgemm` or `dgemm`) into a scratch block
//! held across batches, then its bias and activation in one pass, and the last layer writes the
//! block's rows straight into the output buffer. No value of one row depends on another row, so
//! the block size never changes an answer.
//!
//! The specification is `spec/mlp_forward.md`; what follows is the arithmetic it fixes.
//!
//! - **A layer** is `y = activation(x · W + b)`, with `W` held `inputs × outputs` row-major (NumPy's
//!   `x @ W`). The product is computed first and the bias added after it, as NumPy's
//!   `x @ W + b` does; a layer without a bias adds nothing.
//! - **The precision** is the arithmetic of every step: inputs are converted to it (exactly when
//!   they widen, to nearest-even when they narrow), weights and biases are converted to it at
//!   configure, products are summed in it, and the output has its type.
//! - **Activations:** `identity`; `relu`, `x` when `x > 0` or `x` is NaN, else `+0.0`;
//!   `sigmoid`, `1 / (1 + exp(-x))`; `tanh`. NaN goes through every one of them.
//! - **Nulls:** a row with a null input (a null column value, a null list or a null value inside
//!   it) gives a null output row, whose values are zero; every other row is computed as if the
//!   null row were not there.

use std::sync::Arc;

use moruna_kernel::arrow::array::{
    Array, ArrayRef, FixedSizeListArray, Float16Array, Float32Array, Float64Array, PrimitiveArray,
};
use moruna_kernel::arrow::buffer::{BooleanBuffer, NullBuffer, ScalarBuffer};
use moruna_kernel::arrow::datatypes::{
    ArrowPrimitiveType, DataType, Field, Float16Type, Float32Type, Float64Type,
};
use moruna_kernel::{MorunaError, Result};
use serde_json::{Map, Value, json};

/// The kernel's name in the `moruna` namespace.
pub const NAME: &str = "mlp_forward";

/// Rows per block of the blocked multiply: a block's hidden activations (rows × width) stay in
/// the core's cache between one layer and the next.
pub const BLOCK_ROWS: usize = 256;

type F16 = <Float16Type as ArrowPrimitiveType>::Native;

fn refuse(msg: impl core::fmt::Display) -> MorunaError {
    MorunaError::Plan(format!("moruna.{NAME}: {msg}"))
}

/// A layer's activation, applied to each of its outputs after the bias.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activation {
    /// `x`.
    Identity,
    /// `x` when `x > 0` or `x` is NaN, else `+0.0`.
    Relu,
    /// `1 / (1 + exp(-x))`.
    Sigmoid,
    /// `tanh(x)`.
    Tanh,
}

impl Activation {
    /// Every activation, by its name.
    pub const ALL: [Activation; 4] = [
        Activation::Identity,
        Activation::Relu,
        Activation::Sigmoid,
        Activation::Tanh,
    ];

    /// The activation named `name` (`identity`, `relu`, `sigmoid` or `tanh`).
    pub fn parse(name: &str) -> Result<Activation> {
        Activation::ALL
            .into_iter()
            .find(|a| a.name() == name)
            .ok_or_else(|| {
                refuse(format!(
                    "unknown activation `{name}` (use identity, relu, sigmoid or tanh)"
                ))
            })
    }

    /// The activation's name.
    pub fn name(self) -> &'static str {
        match self {
            Activation::Identity => "identity",
            Activation::Relu => "relu",
            Activation::Sigmoid => "sigmoid",
            Activation::Tanh => "tanh",
        }
    }
}

/// The arithmetic the kernel computes in, and the type of its output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Precision {
    /// IEEE binary32 throughout.
    Float32,
    /// IEEE binary64 throughout.
    Float64,
}

impl Precision {
    /// The precision named `name` (`float32` or `float64`).
    pub fn parse(name: &str) -> Result<Precision> {
        match name {
            "float32" => Ok(Precision::Float32),
            "float64" => Ok(Precision::Float64),
            other => Err(refuse(format!(
                "unknown precision `{other}` (use float32 or float64)"
            ))),
        }
    }

    /// The precision's name.
    pub fn name(self) -> &'static str {
        match self {
            Precision::Float32 => "float32",
            Precision::Float64 => "float64",
        }
    }

    /// The Arrow type of a value in this precision.
    pub fn data_type(self) -> DataType {
        match self {
            Precision::Float32 => DataType::Float32,
            Precision::Float64 => DataType::Float64,
        }
    }

    /// Bytes per value.
    pub fn width(self) -> usize {
        match self {
            Precision::Float32 => 4,
            Precision::Float64 => 8,
        }
    }
}

/// One dense layer: `activation(x · weights + bias)`.
#[derive(Clone, Debug, PartialEq)]
pub struct Layer {
    inputs: usize,
    outputs: usize,
    weights: Vec<f64>,
    bias: Option<Vec<f64>>,
    activation: Activation,
}

impl Layer {
    /// A layer of `inputs × outputs` weights, row-major (row `i` holds input `i`'s weight to each
    /// output, as NumPy's `W` in `x @ W`), an optional bias of `outputs` values, and its
    /// activation. Every weight and bias must be finite.
    pub fn new(
        inputs: usize,
        outputs: usize,
        weights: Vec<f64>,
        bias: Option<Vec<f64>>,
        activation: Activation,
    ) -> Result<Layer> {
        if inputs == 0 || outputs == 0 {
            return Err(refuse(format!(
                "a layer has at least one input and one output, not {inputs} × {outputs}"
            )));
        }
        if inputs.checked_mul(outputs) != Some(weights.len()) {
            return Err(refuse(format!(
                "a {inputs} × {outputs} layer has {} weights, not {}",
                inputs.saturating_mul(outputs),
                weights.len()
            )));
        }
        if let Some(b) = &bias
            && b.len() != outputs
        {
            return Err(refuse(format!(
                "a layer of {outputs} outputs has {outputs} biases, not {}",
                b.len()
            )));
        }
        if weights
            .iter()
            .chain(bias.iter().flatten())
            .any(|v| !v.is_finite())
        {
            return Err(refuse("every weight and bias is a finite number"));
        }
        Ok(Layer {
            inputs,
            outputs,
            weights,
            bias,
            activation,
        })
    }

    /// The layer's input width.
    pub fn inputs(&self) -> usize {
        self.inputs
    }

    /// The layer's output width.
    pub fn outputs(&self) -> usize {
        self.outputs
    }

    /// The weights, `inputs × outputs` row-major.
    pub fn weights(&self) -> &[f64] {
        &self.weights
    }

    /// The bias, when the layer has one.
    pub fn bias(&self) -> Option<&[f64]> {
        self.bias.as_deref()
    }

    /// The activation.
    pub fn activation(&self) -> Activation {
        self.activation
    }
}

/// A model: its layers, each taking the previous one's outputs.
#[derive(Clone, Debug, PartialEq)]
pub struct Model {
    layers: Vec<Layer>,
}

impl Model {
    /// The model of `layers`: at least one, each taking as many inputs as the one before gives.
    pub fn new(layers: Vec<Layer>) -> Result<Model> {
        if layers.is_empty() {
            return Err(refuse("a model has at least one layer"));
        }
        for (i, pair) in layers.windows(2).enumerate() {
            if pair[0].outputs != pair[1].inputs {
                return Err(refuse(format!(
                    "layer {} gives {} outputs and layer {} takes {} inputs",
                    i + 1,
                    pair[0].outputs,
                    i + 2,
                    pair[1].inputs
                )));
            }
        }
        Ok(Model { layers })
    }

    /// The model from its JSON form (the measurement example's file form; the kernel's model handle
    /// is Arrow data, `kernel.rs`): `{"layers": [{"weights": [[...], ...], "bias": [...],
    /// "activation": "relu"}, ...]}`, each layer's `weights` a list of one row per input, each
    /// row a list of one number per output (NumPy's `W.tolist()`); `bias` may be absent or
    /// `null`, and `activation` absent is `identity`. A misspelt key is refused.
    pub fn from_json(value: &Value) -> Result<Model> {
        let map = object(value, "the model")?;
        only(map, &["layers"], "the model")?;
        let layers = map
            .get("layers")
            .and_then(Value::as_array)
            .ok_or_else(|| refuse("the model's `layers` is a list of layers"))?;
        let mut out = Vec::with_capacity(layers.len());
        for (i, layer) in layers.iter().enumerate() {
            let what = format!("layer {}", i + 1);
            let map = object(layer, &what)?;
            only(map, &["weights", "bias", "activation"], &what)?;
            let rows = map
                .get("weights")
                .and_then(Value::as_array)
                .ok_or_else(|| refuse(format!("{what}'s `weights` is a list of rows")))?;
            let outputs = rows.first().and_then(Value::as_array).map_or(0, Vec::len);
            let mut weights = Vec::with_capacity(rows.len() * outputs);
            for (r, row) in rows.iter().enumerate() {
                let row = numbers(row, &format!("{what}'s weights row {r}"))?;
                if row.len() != outputs {
                    return Err(refuse(format!(
                        "{what}'s weights row {r} has {} numbers and row 0 has {outputs}",
                        row.len()
                    )));
                }
                weights.extend(row);
            }
            let bias = match map.get("bias") {
                None | Some(Value::Null) => None,
                Some(b) => Some(numbers(b, &format!("{what}'s `bias`"))?),
            };
            let activation = match map.get("activation") {
                None => Activation::Identity,
                Some(Value::String(name)) => Activation::parse(name)?,
                Some(other) => {
                    return Err(refuse(format!(
                        "{what}'s `activation` is a name, not {other}"
                    )));
                }
            };
            out.push(Layer::new(rows.len(), outputs, weights, bias, activation)?);
        }
        Model::new(out)
    }

    /// The model's JSON form, as [`Model::from_json`] reads it; a layer without a bias has no
    /// `bias` key.
    pub fn to_json(&self) -> Value {
        let layers: Vec<Value> = self
            .layers
            .iter()
            .map(|l| {
                let mut map = Map::new();
                let rows: Vec<Value> = l.weights.chunks(l.outputs).map(|r| json!(r)).collect();
                map.insert("weights".into(), Value::Array(rows));
                if let Some(b) = &l.bias {
                    map.insert("bias".into(), json!(b));
                }
                map.insert("activation".into(), json!(l.activation.name()));
                Value::Object(map)
            })
            .collect();
        json!({ "layers": layers })
    }

    /// The layers.
    pub fn layers(&self) -> &[Layer] {
        &self.layers
    }

    /// The width of a row's features.
    pub fn inputs(&self) -> usize {
        self.layers[0].inputs
    }

    /// The width of a row's output.
    pub fn outputs(&self) -> usize {
        self.layers[self.layers.len() - 1].outputs
    }

    /// The count of weights and biases.
    pub fn parameters(&self) -> usize {
        self.layers
            .iter()
            .map(|l| l.weights.len() + l.bias.as_ref().map_or(0, Vec::len))
            .sum()
    }

    /// The widest hidden layer's output width (0 for a model of one layer).
    fn widest_hidden(&self) -> usize {
        self.layers[..self.layers.len() - 1]
            .iter()
            .map(|l| l.outputs)
            .max()
            .unwrap_or(0)
    }
}

fn object<'a>(value: &'a Value, what: &str) -> Result<&'a Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| refuse(format!("{what} is an object, not {value}")))
}

fn only(map: &Map<String, Value>, known: &[&str], what: &str) -> Result<()> {
    match map.keys().find(|k| !known.contains(&k.as_str())) {
        Some(key) => Err(refuse(format!(
            "{what} has no key `{key}` (its keys are {})",
            known.join(", ")
        ))),
        None => Ok(()),
    }
}

fn numbers(value: &Value, what: &str) -> Result<Vec<f64>> {
    let list = value
        .as_array()
        .ok_or_else(|| refuse(format!("{what} is a list of numbers")))?;
    list.iter()
        .map(|v| {
            v.as_f64()
                .ok_or_else(|| refuse(format!("{what} holds {v}, which is not a number")))
        })
        .collect()
}

/// The arithmetic of one precision: conversions into it, its activations, its multiply and its
/// Arrow array.
trait Real: Copy + Default + PartialOrd + Send + Sync + 'static {
    const ZERO: Self;
    const DATA_TYPE: DataType;
    fn of_f64(v: f64) -> Self;
    fn of_f32(v: f32) -> Self;
    fn of_f16(v: F16) -> Self;
    fn add(self, other: Self) -> Self;
    fn relu(self) -> Self;
    fn sigmoid(self) -> Self;
    fn tanh(self) -> Self;
    /// The slice of `col` itself when its values are already of this type.
    fn direct<'a>(col: &Col<'a>) -> Option<&'a [Self]>;
    /// `C ← A B` for `A` of `m × k`, `B` of `k × n` and `C` of `m × n` row-major in `c` (row
    /// stride `n`), through `matrixmultiply`.
    fn gemm(m: usize, k: usize, n: usize, a: Mat<'_, Self>, b: Mat<'_, Self>, c: &mut [Self]);
    fn array(values: Vec<Self>, nulls: Option<NullBuffer>) -> ArrayRef;
}

macro_rules! real {
    ($t:ty, $arrow:ty, $dt:expr, $gemm:path, $variant:ident) => {
        // The conversions are one macro for both types, so one of each pair is a cast to itself.
        #[allow(clippy::unnecessary_cast)]
        impl Real for $t {
            const ZERO: Self = 0.0;
            const DATA_TYPE: DataType = $dt;
            #[inline]
            fn of_f64(v: f64) -> Self {
                v as $t
            }
            #[inline]
            fn of_f32(v: f32) -> Self {
                v as $t
            }
            #[inline]
            fn of_f16(v: F16) -> Self {
                v.to_f64() as $t
            }
            #[inline]
            fn add(self, other: Self) -> Self {
                self + other
            }
            #[inline]
            fn relu(self) -> Self {
                if self > 0.0 || self.is_nan() {
                    self
                } else {
                    0.0
                }
            }
            #[inline]
            fn sigmoid(self) -> Self {
                1.0 / (1.0 + (-self).exp())
            }
            #[inline]
            fn tanh(self) -> Self {
                <$t>::tanh(self)
            }
            fn direct<'a>(col: &Col<'a>) -> Option<&'a [Self]> {
                match col {
                    Col::$variant(values) => Some(*values),
                    _ => None,
                }
            }
            /// The one call into `matrixmultiply`, every index it will touch checked first.
            #[allow(unsafe_code)]
            fn gemm(
                m: usize,
                k: usize,
                n: usize,
                a: Mat<'_, Self>,
                b: Mat<'_, Self>,
                c: &mut [Self],
            ) {
                if !check_gemm(m, k, n, &a, &b, c.len()) {
                    return;
                }
                // SAFETY: `check_gemm` puts every element of A, B and C the strides name inside
                // its slice; `c` is borrowed mutably, so it aliases neither `a` nor `b`; C's
                // strides (n, 1) name each element once. With β = 0, C is written, never read.
                unsafe {
                    $gemm(
                        m,
                        k,
                        n,
                        1.0,
                        a.data.as_ptr(),
                        a.rs as isize,
                        a.cs as isize,
                        b.data.as_ptr(),
                        b.rs as isize,
                        b.cs as isize,
                        0.0,
                        c.as_mut_ptr(),
                        n as isize,
                        1,
                    )
                }
            }
            fn array(values: Vec<Self>, nulls: Option<NullBuffer>) -> ArrayRef {
                Arc::new(PrimitiveArray::<$arrow>::new(
                    ScalarBuffer::from(values),
                    nulls,
                ))
            }
        }
    };
}

real!(
    f32,
    Float32Type,
    DataType::Float32,
    matrixmultiply::sgemm,
    F32
);
real!(
    f64,
    Float64Type,
    DataType::Float64,
    matrixmultiply::dgemm,
    F64
);

/// A matrix inside a slice: element `(i, j)` at `i * rs + j * cs`.
#[derive(Clone, Copy)]
struct Mat<'a, T> {
    data: &'a [T],
    rs: usize,
    cs: usize,
}

/// The index of the last element of a `rows × cols` matrix of strides `rs` and `cs`.
fn last_index(rows: usize, cols: usize, rs: usize, cs: usize) -> usize {
    (rows - 1) * rs + (cols - 1) * cs
}

/// Whether there is a product to compute (`m > 0`), after checking that every element of A, B
/// and C (`m × n`, row stride `n`) lies inside its slice; a failed check is a bug here, so it
/// panics.
fn check_gemm<T>(m: usize, k: usize, n: usize, a: &Mat<'_, T>, b: &Mat<'_, T>, c: usize) -> bool {
    if m == 0 {
        return false;
    }
    assert!(
        k > 0 && n > 0,
        "a layer has at least one input and one output"
    );
    assert!(
        last_index(m, k, a.rs, a.cs) < a.data.len(),
        "A lies inside its slice"
    );
    assert!(
        last_index(k, n, b.rs, b.cs) < b.data.len(),
        "B lies inside its slice"
    );
    assert!(m * n <= c, "C lies inside its slice");
    true
}

/// One layer converted to the precision, held across batches.
#[derive(Debug)]
struct Held<T> {
    inputs: usize,
    outputs: usize,
    weights: Vec<T>,
    bias: Option<Vec<T>>,
    activation: Activation,
}

impl<T: Real> Held<T> {
    fn of(layer: &Layer) -> Held<T> {
        Held {
            inputs: layer.inputs,
            outputs: layer.outputs,
            weights: layer.weights.iter().map(|&w| T::of_f64(w)).collect(),
            bias: layer
                .bias
                .as_ref()
                .map(|b| b.iter().map(|&v| T::of_f64(v)).collect()),
            activation: layer.activation,
        }
    }

    /// The bias, then the activation, over `rows` rows of this layer's outputs.
    fn finish_rows(&self, out: &mut [T]) {
        if let Some(bias) = &self.bias {
            for row in out.chunks_exact_mut(self.outputs) {
                for (v, b) in row.iter_mut().zip(bias) {
                    *v = v.add(*b);
                }
            }
        }
        match self.activation {
            Activation::Identity => {}
            Activation::Relu => out.iter_mut().for_each(|v| *v = v.relu()),
            Activation::Sigmoid => out.iter_mut().for_each(|v| *v = v.sigmoid()),
            Activation::Tanh => out.iter_mut().for_each(|v| *v = Real::tanh(*v)),
        }
    }
}

/// The model's layers in one precision.
#[derive(Debug)]
enum Compute {
    F32(Vec<Held<f32>>),
    F64(Vec<Held<f64>>),
}

/// One input column's values, in the type they came in.
enum Col<'a> {
    F16(&'a [F16]),
    F32(&'a [f32]),
    F64(&'a [f64]),
}

impl<'a> Col<'a> {
    fn of(array: &'a dyn Array, what: &str) -> Result<Col<'a>> {
        let any = array.as_any();
        if let Some(a) = any.downcast_ref::<Float64Array>() {
            Ok(Col::F64(a.values()))
        } else if let Some(a) = any.downcast_ref::<Float32Array>() {
            Ok(Col::F32(a.values()))
        } else if let Some(a) = any.downcast_ref::<Float16Array>() {
            Ok(Col::F16(a.values()))
        } else {
            Err(refuse(format!(
                "{what} is a Float column, not {}",
                array.data_type()
            )))
        }
    }

    /// Values `from .. from + dst.len()`, converted to the precision, into `dst`.
    fn copy_into<T: Real>(&self, from: usize, dst: &mut [T]) {
        let to = from + dst.len();
        match self {
            Col::F16(v) => dst
                .iter_mut()
                .zip(&v[from..to])
                .for_each(|(d, s)| *d = T::of_f16(*s)),
            Col::F32(v) => dst
                .iter_mut()
                .zip(&v[from..to])
                .for_each(|(d, s)| *d = T::of_f32(*s)),
            Col::F64(v) => dst
                .iter_mut()
                .zip(&v[from..to])
                .for_each(|(d, s)| *d = T::of_f64(*s)),
        }
    }
}

/// A batch's features: `k` columns, or one fixed-size list of `k` values per row, row-major.
enum Features<'a> {
    Columns(Vec<Col<'a>>),
    List(Col<'a>),
}

/// Per-instance scratch, held across batches: one block's gathered features and two blocks of
/// hidden activations, in each precision.
#[derive(Debug, Default)]
pub struct Scratch {
    f32: Blocks<f32>,
    f64: Blocks<f64>,
}

impl Scratch {
    /// Bytes the scratch holds.
    pub fn bytes(&self) -> usize {
        self.f32.len() * 4 + self.f64.len() * 8
    }
}

#[derive(Debug, Default)]
struct Blocks<T> {
    features: Vec<T>,
    even: Vec<T>,
    odd: Vec<T>,
}

impl<T: Real> Blocks<T> {
    fn len(&self) -> usize {
        self.features.len() + self.even.len() + self.odd.len()
    }

    fn reserve(&mut self, features: usize, hidden: usize) {
        if self.features.len() < features {
            self.features.resize(features, T::ZERO);
        }
        for block in [&mut self.even, &mut self.odd] {
            if block.len() < hidden {
                block.resize(hidden, T::ZERO);
            }
        }
    }
}

trait PickBlocks<T> {
    fn blocks(&mut self) -> &mut Blocks<T>;
}

impl PickBlocks<f32> for Scratch {
    fn blocks(&mut self) -> &mut Blocks<f32> {
        &mut self.f32
    }
}

impl PickBlocks<f64> for Scratch {
    fn blocks(&mut self) -> &mut Blocks<f64> {
        &mut self.f64
    }
}

/// `mlp_forward` configured: the model, its precision, and its layers converted to that
/// precision, held for every batch.
#[derive(Debug)]
pub struct MlpForward {
    model: Model,
    precision: Precision,
    compute: Compute,
}

impl MlpForward {
    /// Configure the kernel: convert `model`'s weights and biases to `precision` once.
    pub fn configure(model: Model, precision: Precision) -> MlpForward {
        let compute = match precision {
            Precision::Float32 => Compute::F32(model.layers.iter().map(Held::of).collect()),
            Precision::Float64 => Compute::F64(model.layers.iter().map(Held::of).collect()),
        };
        MlpForward {
            model,
            precision,
            compute,
        }
    }

    /// The model.
    pub fn model(&self) -> &Model {
        &self.model
    }

    /// The declared precision.
    pub fn precision(&self) -> Precision {
        self.precision
    }

    /// Bytes of the weights and biases held in the precision: the state a configured kernel
    /// keeps.
    pub fn state_bytes(&self) -> u64 {
        (self.model.parameters() * self.precision.width()) as u64
    }

    /// The output's Arrow type: the precision's Float when the last layer has one output, else
    /// a `FixedSizeList` of that many non-null values of it.
    pub fn output_type(&self) -> DataType {
        let width = self.model.outputs();
        if width == 1 {
            self.precision.data_type()
        } else {
            DataType::FixedSizeList(
                Arc::new(Field::new_list_field(self.precision.data_type(), false)),
                width as i32,
            )
        }
    }

    /// Whether inputs of `types` are what the model takes: `k` Float columns (Float16, Float32
    /// or Float64, each its own), or one `FixedSizeList` of `k` Float values, `k` the model's
    /// input width.
    pub fn check_inputs(&self, types: &[DataType]) -> Result<()> {
        let k = self.model.inputs();
        let float =
            |t: &DataType| matches!(t, DataType::Float16 | DataType::Float32 | DataType::Float64);
        match types {
            [DataType::FixedSizeList(field, size)] => {
                if *size as usize != k || !float(field.data_type()) {
                    return Err(refuse(format!(
                        "takes {k} Float columns or one FixedSizeList<Float, {k}>, not \
                         FixedSizeList<{}, {size}>",
                        field.data_type()
                    )));
                }
                Ok(())
            }
            _ if types.len() == k && types.iter().all(float) => Ok(()),
            _ => Err(refuse(format!(
                "takes {k} Float columns or one FixedSizeList<Float, {k}>, not {} column(s) of {}",
                types.len(),
                types
                    .iter()
                    .map(DataType::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }

    /// The forward pass over one batch: `inputs` as [`MlpForward::check_inputs`] takes them,
    /// all of one length; one output row per input row, in order, of [`MlpForward::output_type`].
    pub fn forward(&self, inputs: &[ArrayRef], scratch: &mut Scratch) -> Result<ArrayRef> {
        let types: Vec<DataType> = inputs.iter().map(|a| a.data_type().clone()).collect();
        self.check_inputs(&types)?;
        let rows = inputs[0].len();
        if inputs.iter().any(|a| a.len() != rows) {
            return Err(refuse(format!(
                "takes columns of one length, and is given lengths {}",
                inputs
                    .iter()
                    .map(|a| a.len().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        let (features, nulls) = match types.as_slice() {
            [DataType::FixedSizeList(..)] => {
                let list = inputs[0]
                    .as_any()
                    .downcast_ref::<FixedSizeListArray>()
                    .ok_or_else(|| refuse("a FixedSizeList input is not one"))?;
                let values = list.values();
                (
                    Features::List(Col::of(values.as_ref(), "the list's values")?),
                    list_nulls(list),
                )
            }
            _ => {
                let mut cols = Vec::with_capacity(inputs.len());
                for (i, a) in inputs.iter().enumerate() {
                    cols.push(Col::of(a.as_ref(), &format!("input {i}"))?);
                }
                let each: Vec<Option<NullBuffer>> =
                    inputs.iter().map(|a| a.logical_nulls()).collect();
                (
                    Features::Columns(cols),
                    NullBuffer::union_many(each.iter().map(Option::as_ref)),
                )
            }
        };
        Ok(match &self.compute {
            Compute::F32(layers) => {
                self.finish(run(layers, &self.model, &features, rows, scratch), nulls)
            }
            Compute::F64(layers) => {
                self.finish(run(layers, &self.model, &features, rows, scratch), nulls)
            }
        })
    }

    /// The output array: null rows zeroed, then the values as a Float array, or as a list of
    /// them, with the rows' nulls.
    fn finish<T: Real>(&self, mut values: Vec<T>, nulls: Option<NullBuffer>) -> ArrayRef {
        let width = self.model.outputs();
        zero_nulls(&mut values, width, nulls.as_ref());
        if width == 1 {
            T::array(values, nulls)
        } else {
            Arc::new(FixedSizeListArray::new(
                Arc::new(Field::new_list_field(T::DATA_TYPE, false)),
                width as i32,
                T::array(values, None),
                nulls,
            ))
        }
    }
}

/// A list input's row validity: the list's own, and every value inside the row valid.
fn list_nulls(list: &FixedSizeListArray) -> Option<NullBuffer> {
    let width = list.value_length() as usize;
    let outer = list.logical_nulls();
    match list.values().logical_nulls() {
        Some(inner) if inner.null_count() > 0 => {
            let rows = BooleanBuffer::collect_bool(list.len(), |r| {
                (r * width..(r + 1) * width).all(|i| inner.is_valid(i))
            });
            NullBuffer::union(outer.as_ref(), Some(&NullBuffer::new(rows)))
        }
        _ => outer,
    }
}

/// Zero every value of a null row, so the output's bytes are a function of its input's values.
fn zero_nulls<T: Real>(values: &mut [T], width: usize, nulls: Option<&NullBuffer>) {
    if let Some(nulls) = nulls {
        for r in nulls.iter().enumerate().filter(|(_, v)| !v).map(|(r, _)| r) {
            values[r * width..(r + 1) * width].fill(T::ZERO);
        }
    }
}

/// The blocked forward pass in `T`: one output vector of `rows × outputs`, row-major.
fn run<T: Real>(
    layers: &[Held<T>],
    model: &Model,
    features: &Features<'_>,
    rows: usize,
    scratch: &mut Scratch,
) -> Vec<T>
where
    Scratch: PickBlocks<T>,
{
    let k = model.inputs();
    let width = model.outputs();
    let mut out = vec![T::ZERO; rows * width];
    let blocks = scratch.blocks();
    blocks.reserve(BLOCK_ROWS * k, BLOCK_ROWS * model.widest_hidden());
    let Blocks {
        features: gathered,
        even,
        odd,
    } = blocks;
    let last = layers.len() - 1;
    for start in (0..rows).step_by(BLOCK_ROWS) {
        let m = BLOCK_ROWS.min(rows - start);
        let a = gather(features, k, start, m, gathered);
        let mut from_even = false;
        for (l, layer) in layers.iter().enumerate() {
            let n = layer.outputs;
            let b = Mat {
                data: &layer.weights,
                rs: n,
                cs: 1,
            };
            let (src, dst): (Mat<'_, T>, &mut [T]) = match (l == 0, l == last, from_even) {
                (true, true, _) => (a, &mut out[start * n..(start + m) * n]),
                (true, false, _) => (a, &mut even[..m * n]),
                (false, true, true) => (
                    row_major(even, layer.inputs),
                    &mut out[start * n..(start + m) * n],
                ),
                (false, true, false) => (
                    row_major(odd, layer.inputs),
                    &mut out[start * n..(start + m) * n],
                ),
                (false, false, true) => (row_major(even, layer.inputs), &mut odd[..m * n]),
                (false, false, false) => (row_major(odd, layer.inputs), &mut even[..m * n]),
            };
            T::gemm(m, layer.inputs, n, src, b, dst);
            layer.finish_rows(dst);
            from_even = match (l == 0, from_even) {
                (true, _) => true,
                (false, e) => !e,
            };
        }
    }
    out
}

fn row_major<T>(data: &[T], cols: usize) -> Mat<'_, T> {
    Mat {
        data,
        rs: cols,
        cs: 1,
    }
}

/// Rows `start .. start + m` of the features as an `m × k` matrix: the input itself when it is
/// a list of the precision's type, else gathered into `into` and converted to the precision.
fn gather<'f: 'a, 'a, T: Real>(
    features: &'a Features<'f>,
    k: usize,
    start: usize,
    m: usize,
    into: &'a mut [T],
) -> Mat<'a, T> {
    match features {
        Features::List(values) => {
            if let Some(direct) = T::direct(values) {
                return Mat {
                    data: &direct[start * k..(start + m) * k],
                    rs: k,
                    cs: 1,
                };
            }
            values.copy_into(start * k, &mut into[..m * k]);
            Mat {
                data: &into[..m * k],
                rs: k,
                cs: 1,
            }
        }
        Features::Columns(cols) => {
            // Column-major (each column one contiguous run), which is how the columns lie.
            for (c, col) in cols.iter().enumerate() {
                col.copy_into(start, &mut into[c * m..(c + 1) * m]);
            }
            Mat {
                data: &into[..m * k],
                rs: 1,
                cs: m,
            }
        }
    }
}

mod kernel;
pub use kernel::{MlpKernel, VERSIONS, configure};

#[cfg(test)]
mod tests;
