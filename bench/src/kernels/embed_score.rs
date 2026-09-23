//! `embed-score`: numeric columns to a tensor, a small matmul, back to a column
//! (preamble 6.5).
//!
//! The kernel reads every numeric column of the input in schema order, forms the
//! `rows x in_dim` matrix they make, multiplies it by a `in_dim x out_dim`
//! weight matrix, adds a bias of `out_dim`, and appends the result as one column
//! named `score`. With `out_dim` of one that column is `Float64`; above one it
//! is `FixedSizeList(Float64, out_dim)`, which is the shape contracts section
//! e.3 gives a two dimensional tensor when it becomes an Arrow column.
//!
//! Which columns are numeric follows contracts e.3: `Int8` through `Int64`,
//! `UInt8` through `UInt64`, `Float16`, `Float32` and `Float64` are numeric;
//! `Boolean` is not, and neither is anything else. A null in a numeric column is
//! an error, as it is for `Payload::as_tensor` (`HasNulls`), because the mapping
//! has nowhere to put one.
//!
//! # Amplification
//!
//! The kernel appends `out_dim` f64 values to a row of `in_dim` numeric values,
//! so on an all numeric table of eight byte columns the amplification is
//! `1 + out_dim / in_dim` exactly, and `hints()` reports that figure rather than
//! a constant. On `numeric-embed` (two i64 and sixteen f64) with a weight of
//! `[18, 64]` it is 1 + 64/18 = 4.56.
//!
//! # Weights
//!
//! Weights come from the generator's own `embed-weights` datasets, read with the
//! `safetensors` crate: a tensor named `weight` of two dimensions and a tensor
//! named `bias` of one. Every float and integer dtype the generator writes is
//! accepted and widened to f64, `f16` and `bf16` included, so
//! `embed-weights-half` loads as well as `embed-weights`.
//!
//! One thing the documents leave open, reported rather than decided here: the
//! suite's `embed-weights` is `[128, 64]` while `numeric-embed` carries eighteen
//! numeric columns, so those two files do not fit each other. This branch adds
//! `embed-weights-numeric`, a `[18, 64]` weight beside the others, which is an
//! addition to the dataset set recorded in `bench/README.md` and therefore that
//! file's to make (preamble 6.5, PM 2026-09-22). A mismatched pair is a clear
//! error naming both shapes, never a silent projection.

use std::path::Path;
use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, FixedSizeListArray, Float64Array, Float64Builder, RecordBatch,
};
use arrow::datatypes::{DataType, Field, Schema};

use crate::error::{BenchError, Result};
use crate::kernels::{
    BenchKernel, BenchKernelHints, BenchKernelState, BenchPayload, NoState, PayloadKind,
};

/// The column the kernel appends.
pub const SCORE_COLUMN: &str = "score";

/// The tensor names the weights file must carry.
pub const WEIGHT_TENSOR: &str = "weight";
/// The bias tensor's name.
pub const BIAS_TENSOR: &str = "bias";

/// Widen an IEEE 754 binary16 value to f32. Hand rolled because `half` is not in
/// the preamble's dependency table (6.2) and adding a crate it lacks is E2.
pub fn f16_to_f32(bits: u16) -> f32 {
    let sign = ((bits >> 15) as u32) << 31;
    let exponent = ((bits >> 10) & 0x1f) as u32;
    let mantissa = (bits & 0x3ff) as u32;
    match exponent {
        // Zero and subnormal: normalise the mantissa into a binary32 exponent.
        0 => {
            if mantissa == 0 {
                f32::from_bits(sign)
            } else {
                // The highest set bit of a ten bit mantissa sits at position
                // `10 - shift`, and a subnormal binary16 is `mantissa x 2^-24`,
                // so the binary32 exponent is `127 + (10 - shift) - 24`.
                let shift = mantissa.leading_zeros() - 21;
                let exponent = 113 - shift;
                let mantissa = (mantissa << shift) & 0x3ff;
                f32::from_bits(sign | (exponent << 23) | (mantissa << 13))
            }
        }
        // Infinity and NaN.
        0x1f => f32::from_bits(sign | 0x7f80_0000 | (mantissa << 13)),
        _ => f32::from_bits(sign | ((exponent + 127 - 15) << 23) | (mantissa << 13)),
    }
}

/// Widen a bfloat16 value to f32: a bfloat16 is the top half of a binary32.
pub fn bf16_to_f32(bits: u16) -> f32 {
    f32::from_bits((bits as u32) << 16)
}

fn widen(dtype: safetensors::Dtype, bytes: &[u8], what: &str) -> Result<Vec<f64>> {
    fn chunks<const N: usize>(bytes: &[u8]) -> impl Iterator<Item = [u8; N]> + '_ {
        bytes.as_chunks::<N>().0.iter().copied()
    }
    let values: Vec<f64> = match dtype {
        safetensors::Dtype::F64 => chunks::<8>(bytes).map(f64::from_le_bytes).collect(),
        safetensors::Dtype::F32 => chunks::<4>(bytes)
            .map(|b| f32::from_le_bytes(b) as f64)
            .collect(),
        safetensors::Dtype::F16 => chunks::<2>(bytes)
            .map(|b| f16_to_f32(u16::from_le_bytes(b)) as f64)
            .collect(),
        safetensors::Dtype::BF16 => chunks::<2>(bytes)
            .map(|b| bf16_to_f32(u16::from_le_bytes(b)) as f64)
            .collect(),
        safetensors::Dtype::I64 => chunks::<8>(bytes)
            .map(|b| i64::from_le_bytes(b) as f64)
            .collect(),
        safetensors::Dtype::I32 => chunks::<4>(bytes)
            .map(|b| i32::from_le_bytes(b) as f64)
            .collect(),
        safetensors::Dtype::I16 => chunks::<2>(bytes)
            .map(|b| i16::from_le_bytes(b) as f64)
            .collect(),
        safetensors::Dtype::I8 => bytes.iter().map(|b| *b as i8 as f64).collect(),
        safetensors::Dtype::U8 => bytes.iter().map(|b| *b as f64).collect(),
        other => {
            return Err(BenchError::Kernel {
                kernel: EmbedScore::NAME,
                detail: format!("{what} has dtype {other:?}, which is not a number"),
            });
        }
    };
    Ok(values)
}

/// A weight matrix and its bias, widened to f64.
#[derive(Debug, Clone, PartialEq)]
pub struct Weights {
    in_dim: usize,
    out_dim: usize,
    weight: Vec<f64>,
    bias: Vec<f64>,
}

impl Weights {
    /// Build from a row major `in_dim x out_dim` weight and a bias of `out_dim`.
    pub fn new(in_dim: usize, out_dim: usize, weight: Vec<f64>, bias: Vec<f64>) -> Result<Weights> {
        if in_dim == 0 || out_dim == 0 {
            return Err(BenchError::Kernel {
                kernel: EmbedScore::NAME,
                detail: format!("weight shape [{in_dim}, {out_dim}] has a zero dimension"),
            });
        }
        if weight.len() != in_dim * out_dim {
            return Err(BenchError::Kernel {
                kernel: EmbedScore::NAME,
                detail: format!(
                    "weight of shape [{in_dim}, {out_dim}] needs {} values, got {}",
                    in_dim * out_dim,
                    weight.len()
                ),
            });
        }
        if bias.len() != out_dim {
            return Err(BenchError::Kernel {
                kernel: EmbedScore::NAME,
                detail: format!("bias needs {out_dim} values, got {}", bias.len()),
            });
        }
        Ok(Weights {
            in_dim,
            out_dim,
            weight,
            bias,
        })
    }

    /// Read `weight` and `bias` out of a safetensors file the generator wrote.
    pub fn from_safetensors(path: &Path) -> Result<Weights> {
        let bytes = std::fs::read(path).map_err(|e| BenchError::io("read", path, e))?;
        Weights::from_safetensors_bytes(&bytes)
    }

    /// The same, from bytes already in memory.
    pub fn from_safetensors_bytes(bytes: &[u8]) -> Result<Weights> {
        let file = safetensors::SafeTensors::deserialize(bytes)?;
        let weight = file.tensor(WEIGHT_TENSOR)?;
        let bias = file.tensor(BIAS_TENSOR)?;
        if weight.shape().len() != 2 {
            return Err(BenchError::Kernel {
                kernel: EmbedScore::NAME,
                detail: format!(
                    "{WEIGHT_TENSOR} has shape {:?}; a matmul wants two dimensions",
                    weight.shape()
                ),
            });
        }
        if bias.shape().len() != 1 {
            return Err(BenchError::Kernel {
                kernel: EmbedScore::NAME,
                detail: format!("{BIAS_TENSOR} has shape {:?}; it wants one", bias.shape()),
            });
        }
        Weights::new(
            weight.shape()[0],
            weight.shape()[1],
            widen(weight.dtype(), weight.data(), WEIGHT_TENSOR)?,
            widen(bias.dtype(), bias.data(), BIAS_TENSOR)?,
        )
    }

    /// Numeric columns the kernel expects on its input.
    pub fn in_dim(&self) -> usize {
        self.in_dim
    }

    /// Width of the score column.
    pub fn out_dim(&self) -> usize {
        self.out_dim
    }

    /// Bytes the weights hold, which is contracts d.7's `state_bytes` hint.
    pub fn bytes(&self) -> u64 {
        ((self.weight.len() + self.bias.len()) * size_of::<f64>()) as u64
    }
}

/// Whether contracts e.3 maps this Arrow type to a `DType`.
pub fn is_numeric(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Int8
            | DataType::Int16
            | DataType::Int32
            | DataType::Int64
            | DataType::UInt8
            | DataType::UInt16
            | DataType::UInt32
            | DataType::UInt64
            | DataType::Float16
            | DataType::Float32
            | DataType::Float64
    )
}

/// The embed-score kernel.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbedScore {
    weights: Weights,
}

impl EmbedScore {
    /// The name preamble 6.5 gives it.
    pub const NAME: &'static str = "embed-score";

    /// A kernel with these weights.
    pub fn new(weights: Weights) -> EmbedScore {
        EmbedScore { weights }
    }

    /// A kernel whose weights come from a generated safetensors file.
    pub fn from_safetensors(path: &Path) -> Result<EmbedScore> {
        Ok(EmbedScore::new(Weights::from_safetensors(path)?))
    }

    /// The weights it holds.
    pub fn weights(&self) -> &Weights {
        &self.weights
    }
}

impl BenchKernel for EmbedScore {
    fn name(&self) -> &'static str {
        EmbedScore::NAME
    }

    fn accepts(&self) -> PayloadKind {
        PayloadKind::Table
    }

    fn hints(&self) -> BenchKernelHints {
        // On an all numeric table of eight byte columns the appended score
        // column is out_dim f64 beside in_dim f64, so the ratio is exact. The
        // band allows a tenth either way, which covers a table that also carries
        // a string column or a validity bitmap.
        let expected = 1.0 + self.weights.out_dim() as f64 / self.weights.in_dim() as f64;
        BenchKernelHints {
            preferred_rows: Some(16_384),
            state_bytes: Some(self.weights.bytes()),
            ..BenchKernelHints::amplifying(expected, expected * 0.9, expected * 1.1)
        }
    }

    fn init(&self) -> Result<Box<dyn BenchKernelState>> {
        Ok(Box::new(NoState))
    }

    fn apply(
        &self,
        _state: &mut dyn BenchKernelState,
        input: BenchPayload,
    ) -> Result<BenchPayload> {
        let batch = input.table(EmbedScore::NAME)?;
        let numeric: Vec<usize> = batch
            .schema()
            .fields()
            .iter()
            .enumerate()
            .filter(|(_, field)| is_numeric(field.data_type()))
            .map(|(index, _)| index)
            .collect();
        if numeric.len() != self.weights.in_dim() {
            return Err(BenchError::Kernel {
                kernel: EmbedScore::NAME,
                detail: format!(
                    "the weight has {} rows but the batch has {} numeric columns; \
                     the two files do not fit each other",
                    self.weights.in_dim(),
                    numeric.len()
                ),
            });
        }
        let rows = batch.num_rows();
        let mut features: Vec<Vec<f64>> = Vec::with_capacity(numeric.len());
        for index in &numeric {
            let field = batch.schema().field(*index).clone();
            let column = batch.column(*index);
            if column.null_count() > 0 {
                return Err(BenchError::Kernel {
                    kernel: EmbedScore::NAME,
                    detail: format!(
                        "column {} holds {} nulls; the tensor mapping of contracts e.3 has \
                         nowhere to put one",
                        field.name(),
                        column.null_count()
                    ),
                });
            }
            let widened = arrow::compute::cast(column, &DataType::Float64)?;
            match widened.as_any().downcast_ref::<Float64Array>() {
                Some(array) => features.push(array.values().to_vec()),
                None => {
                    return Err(BenchError::Kernel {
                        kernel: EmbedScore::NAME,
                        detail: format!("column {} did not widen to f64", field.name()),
                    });
                }
            }
        }

        let out_dim = self.weights.out_dim();
        let mut scores = Float64Builder::with_capacity(rows * out_dim);
        for row in 0..rows {
            for output in 0..out_dim {
                let mut sum = self.weights.bias[output];
                for (input_index, column) in features.iter().enumerate() {
                    sum += column[row] * self.weights.weight[input_index * out_dim + output];
                }
                scores.append_value(sum);
            }
        }
        let flat: ArrayRef = Arc::new(scores.finish());

        let (score_field, score_column): (Field, ArrayRef) = if out_dim == 1 {
            (Field::new(SCORE_COLUMN, DataType::Float64, false), flat)
        } else {
            let width = out_dim as i32;
            let item = Arc::new(Field::new("item", DataType::Float64, false));
            let list = FixedSizeListArray::try_new(Arc::clone(&item), width, flat, None)?;
            (
                Field::new(SCORE_COLUMN, DataType::FixedSizeList(item, width), false),
                Arc::new(list),
            )
        };

        let mut fields: Vec<Field> = batch
            .schema()
            .fields()
            .iter()
            .map(|field| field.as_ref().clone())
            .collect();
        fields.push(score_field);
        let mut columns: Vec<ArrayRef> = batch.columns().to_vec();
        columns.push(score_column);
        let batch = RecordBatch::try_new_with_options(
            Arc::new(Schema::new(fields)),
            columns,
            &arrow::array::RecordBatchOptions::new().with_row_count(Some(rows)),
        )?;
        Ok(BenchPayload::Table(batch))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dtype::DType;
    use crate::kernels::table_bytes;
    use crate::mrb1::TensorSpec;
    use arrow::array::{Int64Array, StringArray};

    fn weights(in_dim: usize, out_dim: usize) -> Weights {
        let weight = (0..in_dim * out_dim).map(|v| (v % 7) as f64).collect();
        let bias = (0..out_dim).map(|v| v as f64).collect();
        Weights::new(in_dim, out_dim, weight, bias).expect("weights")
    }

    fn numeric_batch(rows: usize, columns: usize) -> RecordBatch {
        let fields: Vec<Field> = (0..columns)
            .map(|c| Field::new(format!("f64_{c}"), DataType::Float64, false))
            .collect();
        let arrays: Vec<ArrayRef> = (0..columns)
            .map(|c| {
                let values: Vec<f64> = (0..rows).map(|r| (r + c) as f64).collect();
                Arc::new(Float64Array::from(values)) as ArrayRef
            })
            .collect();
        RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays).expect("batch")
    }

    fn run(kernel: &EmbedScore, batch: RecordBatch) -> Result<RecordBatch> {
        let mut state = kernel.init()?;
        let payload = kernel.apply(state.as_mut(), BenchPayload::Table(batch))?;
        Ok(payload.table(EmbedScore::NAME)?.clone())
    }

    #[test]
    fn half_precision_widens_the_way_ieee_754_says() {
        assert_eq!(f16_to_f32(0x0000), 0.0);
        assert_eq!(f16_to_f32(0x8000), -0.0);
        assert_eq!(f16_to_f32(0x3c00), 1.0);
        assert_eq!(f16_to_f32(0xc000), -2.0);
        assert_eq!(f16_to_f32(0x3555), 0.333_251_95);
        // The smallest subnormal and the largest one.
        assert_eq!(f16_to_f32(0x0001), 2f32.powi(-24));
        assert_eq!(f16_to_f32(0x03ff), 1023.0 * 2f32.powi(-24));
        assert!(f16_to_f32(0x7c00).is_infinite());
        assert!(f16_to_f32(0x7e00).is_nan());
        assert_eq!(bf16_to_f32(0x3f80), 1.0);
        assert_eq!(bf16_to_f32(0xc000), -2.0);
        assert_eq!(bf16_to_f32(0x0000), 0.0);
    }

    #[test]
    fn a_matmul_of_one_row_is_the_bias_plus_the_dot_product() {
        let weights =
            Weights::new(2, 2, vec![1.0, 2.0, 3.0, 4.0], vec![10.0, 20.0]).expect("weights");
        let fields = vec![
            Field::new("a", DataType::Float64, false),
            Field::new("b", DataType::Float64, false),
        ];
        let arrays: Vec<ArrayRef> = vec![
            Arc::new(Float64Array::from(vec![1.0])),
            Arc::new(Float64Array::from(vec![2.0])),
        ];
        let batch = RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays).expect("batch");
        let out = run(&EmbedScore::new(weights), batch).expect("score");
        let list = out
            .column(2)
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .expect("out_dim above one is a FixedSizeList");
        assert_eq!(list.value_length(), 2);
        let values = list
            .values()
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("the values are f64");
        // 10 + 1*1 + 2*3 = 17; 20 + 1*2 + 2*4 = 30.
        assert_eq!(values.values(), &[17.0, 30.0]);
    }

    #[test]
    fn an_out_dim_of_one_appends_a_plain_float_column() {
        let weights = Weights::new(1, 1, vec![2.0], vec![1.0]).expect("weights");
        let out = run(&EmbedScore::new(weights), numeric_batch(3, 1)).expect("score");
        assert_eq!(out.schema().field(1).data_type(), &DataType::Float64);
        let values = out
            .column(1)
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("f64");
        assert_eq!(values.values(), &[1.0, 3.0, 5.0]);
    }

    #[test]
    fn integers_widen_and_non_numeric_columns_are_ignored_but_carried() {
        let fields = vec![
            Field::new("i64_0", DataType::Int64, false),
            Field::new("str_0", DataType::Utf8, false),
            Field::new("f64_0", DataType::Float64, false),
        ];
        let arrays: Vec<ArrayRef> = vec![
            Arc::new(Int64Array::from(vec![1i64, 2])),
            Arc::new(StringArray::from(vec!["a", "b"])),
            Arc::new(Float64Array::from(vec![10.0, 20.0])),
        ];
        let batch = RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays).expect("batch");
        let weights = Weights::new(2, 1, vec![1.0, 1.0], vec![0.0]).expect("weights");
        let out = run(&EmbedScore::new(weights), batch).expect("score");
        assert_eq!(out.num_columns(), 4);
        let values = out
            .column(3)
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("f64");
        assert_eq!(values.values(), &[11.0, 22.0]);
        assert!(is_numeric(&DataType::UInt16));
        assert!(is_numeric(&DataType::Float16));
        assert!(!is_numeric(&DataType::Boolean));
        assert!(!is_numeric(&DataType::Utf8));
    }

    #[test]
    fn a_shape_mismatch_and_a_null_are_each_an_error_naming_what_is_wrong() {
        let err = run(&EmbedScore::new(weights(128, 64)), numeric_batch(4, 18))
            .expect_err("18 columns do not fit a 128 row weight");
        let text = err.to_string();
        assert!(text.contains("128 rows"), "{text}");
        assert!(text.contains("18 numeric columns"), "{text}");

        let fields = vec![Field::new("f64_0", DataType::Float64, true)];
        let arrays: Vec<ArrayRef> = vec![Arc::new(Float64Array::from(vec![Some(1.0), None]))];
        let batch = RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays).expect("batch");
        let err =
            run(&EmbedScore::new(weights(1, 4)), batch).expect_err("a null has nowhere to go");
        assert!(err.to_string().contains("holds 1 nulls"), "{err}");
    }

    #[test]
    fn a_weight_that_does_not_match_its_own_shape_is_refused() {
        let err = Weights::new(2, 2, vec![1.0], vec![0.0, 0.0]).expect_err("too few values");
        assert!(err.to_string().contains("needs 4 values"), "{err}");
        let err = Weights::new(2, 2, vec![1.0; 4], vec![0.0]).expect_err("bias too short");
        assert!(err.to_string().contains("bias needs 2"), "{err}");
        let err = Weights::new(0, 2, Vec::new(), vec![0.0, 0.0]).expect_err("zero dimension");
        assert!(err.to_string().contains("zero dimension"), "{err}");
    }

    #[test]
    fn weights_load_out_of_a_generated_safetensors_file_at_every_precision() {
        for (dtype, in_dim, out_dim) in [
            (DType::F32, 8usize, 4usize),
            (DType::F64, 6, 3),
            (DType::F16, 4, 2),
            (DType::BF16, 4, 2),
            (DType::I32, 3, 2),
            (DType::I64, 3, 2),
            (DType::I16, 3, 2),
            (DType::I8, 3, 2),
            (DType::U8, 3, 2),
        ] {
            let weight = TensorSpec::new(WEIGHT_TENSOR, dtype, vec![in_dim as i64, out_dim as i64])
                .expect("weight spec");
            let bias =
                TensorSpec::new(BIAS_TENSOR, dtype, vec![out_dim as i64]).expect("bias spec");
            let bytes = crate::safetensors_out::encode("unit", &[weight, bias], 7).expect("encode");
            let loaded = Weights::from_safetensors_bytes(&bytes).expect("load");
            assert_eq!(loaded.in_dim(), in_dim);
            assert_eq!(loaded.out_dim(), out_dim);
            assert_eq!(loaded.bytes(), ((in_dim * out_dim + out_dim) * 8) as u64);
        }
    }

    #[test]
    fn a_weights_file_of_the_wrong_rank_or_dtype_is_refused() {
        let encode = |specs: &[TensorSpec]| {
            crate::safetensors_out::encode("unit", specs, 3).expect("encode")
        };
        let spec = |name: &str, dtype: DType, shape: Vec<i64>| {
            TensorSpec::new(name, dtype, shape).expect("spec")
        };
        let bytes = encode(&[
            spec(WEIGHT_TENSOR, DType::F32, vec![4]),
            spec(BIAS_TENSOR, DType::F32, vec![4]),
        ]);
        let err = Weights::from_safetensors_bytes(&bytes).expect_err("rank 1 weight");
        assert!(err.to_string().contains("two dimensions"), "{err}");

        let bytes = encode(&[
            spec(WEIGHT_TENSOR, DType::F32, vec![2, 2]),
            spec(BIAS_TENSOR, DType::F32, vec![2, 1]),
        ]);
        let err = Weights::from_safetensors_bytes(&bytes).expect_err("rank 2 bias");
        assert!(err.to_string().contains("it wants one"), "{err}");

        let bytes = encode(&[
            spec(WEIGHT_TENSOR, DType::Bool, vec![2, 2]),
            spec(BIAS_TENSOR, DType::Bool, vec![2]),
        ]);
        let err = Weights::from_safetensors_bytes(&bytes).expect_err("bool is not a number");
        assert!(err.to_string().contains("not a number"), "{err}");

        let err = Weights::from_safetensors_bytes(b"not a safetensors file")
            .expect_err("not a safetensors file");
        assert!(err.to_string().starts_with("safetensors:"), "{err}");

        let missing = Path::new("/nonexistent/embed-weights.safetensors");
        let err = Weights::from_safetensors(missing).expect_err("no such file");
        assert!(err.to_string().starts_with("io: read"), "{err}");
        let err = EmbedScore::from_safetensors(missing).expect_err("no such file");
        assert!(err.to_string().starts_with("io: read"), "{err}");
    }

    #[test]
    fn the_declared_amplification_follows_from_the_weight_shape() {
        let kernel = EmbedScore::new(weights(18, 64));
        assert_eq!(kernel.name(), "embed-score");
        assert_eq!(kernel.accepts(), PayloadKind::Table);
        assert_eq!(kernel.weights().in_dim(), 18);
        let hints = kernel.hints();
        let expected = 1.0 + 64.0 / 18.0;
        assert_eq!(hints.expected_amplification, Some(expected));
        assert!(hints.band_holds(expected));
        assert_eq!(hints.state_bytes, Some(((18 * 64 + 64) * 8) as u64));
    }

    #[test]
    fn the_measured_ratio_on_an_all_numeric_table_is_the_declared_one() {
        let kernel = EmbedScore::new(weights(18, 64));
        let batch = numeric_batch(512, 18);
        let before = table_bytes(&batch);
        let out = run(&kernel, batch).expect("score");
        let measured = table_bytes(&out) as f64 / before as f64;
        assert!(kernel.hints().band_holds(measured), "measured {measured}");
    }

    #[test]
    fn a_tensor_payload_is_refused() {
        let kernel = EmbedScore::new(weights(1, 1));
        let mut state = kernel.init().expect("init");
        let tensor =
            crate::kernels::BenchTensor::new(DType::F32, vec![1], vec![0u8; 4]).expect("tensor");
        let err = kernel
            .apply(state.as_mut(), BenchPayload::Tensor(tensor))
            .expect_err("a tensor is not a table");
        assert!(err.to_string().contains("got a tensor"), "{err}");
    }
}
