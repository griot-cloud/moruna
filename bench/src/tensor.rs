//! A generated tensor: its name, element type and shape, and the payload drawn
//! for it. The safetensors writer turns a list of these into a file.

use crate::dtype::{DType, element_count};
use crate::error::{BenchError, Result};
use crate::rng::Rng;

/// The largest `ndim` the generator writes.
pub const MAX_NDIM: usize = 8;

/// A tensor to write: a name, an element type and a shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TensorSpec {
    /// The tensor's name, the key in the safetensors header.
    pub name: String,
    /// The element type.
    pub dtype: DType,
    /// The shape, row major, 0 to 8 dimensions.
    pub shape: Vec<i64>,
}

impl TensorSpec {
    /// A spec, validated: at most `MAX_NDIM` dimensions, none negative.
    pub fn new(name: impl Into<String>, dtype: DType, shape: Vec<i64>) -> Result<TensorSpec> {
        let name = name.into();
        if shape.len() > MAX_NDIM {
            return Err(BenchError::Shape(format!(
                "{name}: ndim {} exceeds the {MAX_NDIM} dimensions the generator writes",
                shape.len()
            )));
        }
        if let Some(bad) = shape.iter().find(|d| **d < 0) {
            return Err(BenchError::Shape(format!(
                "{name}: dimension {bad} is negative"
            )));
        }
        Ok(TensorSpec { name, dtype, shape })
    }

    /// The number of elements.
    pub fn elements(&self) -> u64 {
        element_count(&self.shape)
    }

    /// The payload length in bytes.
    pub fn payload_len(&self) -> u64 {
        self.elements() * self.dtype.item_size() as u64
    }

    /// The payload bytes, drawn from the stream this tensor owns.
    pub fn payload(&self, seed: u64) -> Vec<u8> {
        let mut rng = Rng::substream(seed, &format!("tensor:{}:{}", self.name, self.dtype.name()));
        self.dtype.fill(&mut rng, self.elements() as usize)
    }

    /// The shape rendered for the manifest and for `list`.
    pub fn shape_text(&self) -> String {
        let dims: Vec<String> = self.shape.iter().map(i64::to_string).collect();
        format!("[{}]", dims.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(dtype: DType, shape: Vec<i64>) -> TensorSpec {
        match TensorSpec::new("t", dtype, shape) {
            Ok(spec) => spec,
            Err(err) => panic!("{err}"),
        }
    }

    #[test]
    fn the_payload_is_a_pure_function_of_the_seed_and_the_shape() {
        let spec = spec(DType::F64, vec![2, 8]);
        assert_eq!(spec.payload(3), spec.payload(3));
        assert_ne!(spec.payload(3), spec.payload(4));
    }

    #[test]
    fn the_payload_length_holds_for_every_dtype_and_rank() {
        for dtype in crate::dtype::ALL {
            for ndim in 0..=MAX_NDIM {
                let shape: Vec<i64> = (0..ndim).map(|i| (i as i64 % 3) + 1).collect();
                let spec = spec(dtype, shape);
                assert_eq!(spec.payload(5).len() as u64, spec.payload_len());
            }
        }
    }

    #[test]
    fn a_scalar_has_no_dimensions_and_one_element() {
        let spec = spec(DType::I64, vec![]);
        assert_eq!(spec.elements(), 1);
        assert_eq!(spec.payload_len(), 8);
        assert_eq!(spec.payload(1).len(), 8);
        assert_eq!(spec.shape_text(), "[]");
    }

    #[test]
    fn shapes_beyond_the_limit_are_refused() {
        let too_deep = TensorSpec::new("t", DType::F32, vec![1; 9]);
        assert!(matches!(too_deep, Err(BenchError::Shape(_))));
        let negative = TensorSpec::new("t", DType::F32, vec![-1]);
        assert!(matches!(negative, Err(BenchError::Shape(_))));
        match TensorSpec::new("t", DType::F32, vec![2, 3]) {
            Ok(spec) => assert_eq!(spec.shape_text(), "[2,3]"),
            Err(err) => panic!("{err}"),
        }
    }
}
