//! The safetensors writer.
//!
//! The file is produced by the `safetensors` crate itself, so what the generator
//! writes is what that crate reads back (the round trip is tested). Two
//! properties matter for a benchmark corpus and both are held here: the header
//! order is fixed by the crate (tensors are sorted by descending dtype alignment
//! and then by name), and the `__metadata__` map is written with exactly one
//! key, because a map with more than one key is serialised in hash order and a
//! file whose bytes moved between runs would not be a fixture.

use std::borrow::Cow;
use std::collections::HashMap;

use crate::amb1::TensorSpec;
use crate::error::Result;

/// One tensor's bytes, ready for the `safetensors` writer.
struct OwnedView {
    dtype: safetensors::Dtype,
    shape: Vec<usize>,
    data: Vec<u8>,
}

impl safetensors::View for OwnedView {
    fn dtype(&self) -> safetensors::Dtype {
        self.dtype
    }

    fn shape(&self) -> &[usize] {
        &self.shape
    }

    fn data(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(&self.data)
    }

    fn data_len(&self) -> usize {
        self.data.len()
    }
}

/// The provenance line written into `__metadata__`. One key, one value, so the
/// header is byte stable.
pub fn provenance(dataset: &str, seed: u64) -> String {
    format!(
        "generator=amoru-bench version={} format={} dataset={dataset} seed={seed}",
        crate::GENERATOR_VERSION,
        crate::GENERATOR_FORMAT
    )
}

/// Encode a safetensors file holding every tensor of `specs`.
pub fn encode(dataset: &str, specs: &[TensorSpec], seed: u64) -> Result<Vec<u8>> {
    let mut tensors: Vec<(String, OwnedView)> = Vec::with_capacity(specs.len());
    for spec in specs {
        tensors.push((
            spec.name.clone(),
            OwnedView {
                dtype: spec.dtype.safetensors(),
                shape: spec.shape.iter().map(|d| *d as usize).collect(),
                data: spec.payload(seed),
            },
        ));
    }
    let mut info: HashMap<String, String> = HashMap::with_capacity(1);
    info.insert("amoru_bench".to_string(), provenance(dataset, seed));
    Ok(safetensors::serialize(tensors, Some(info))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dtype::DType;

    fn specs() -> Vec<TensorSpec> {
        let mut out = Vec::new();
        for (name, dtype, shape) in [
            ("weight", DType::F32, vec![4i64, 3]),
            ("bias", DType::F32, vec![3i64]),
            ("mask", DType::Bool, vec![3i64]),
            ("ids", DType::I64, vec![5i64]),
        ] {
            match TensorSpec::new(name, dtype, shape) {
                Ok(spec) => out.push(spec),
                Err(err) => panic!("{err}"),
            }
        }
        out
    }

    #[test]
    fn a_file_round_trips_through_the_safetensors_crate() {
        let specs = specs();
        let bytes = encode("unit", &specs, 7).expect("encode");
        let parsed = safetensors::SafeTensors::deserialize(&bytes).expect("deserialize");
        assert_eq!(parsed.len(), specs.len());
        for spec in &specs {
            let view = parsed.tensor(&spec.name).expect("tensor present");
            assert_eq!(view.dtype(), spec.dtype.safetensors());
            let shape: Vec<i64> = view.shape().iter().map(|d| *d as i64).collect();
            assert_eq!(shape, spec.shape);
            assert_eq!(view.data(), spec.payload(7).as_slice());
        }
    }

    #[test]
    fn the_metadata_names_the_generator_the_dataset_and_the_seed() {
        let bytes = encode("embed-weights", &specs(), 99).expect("encode");
        let (_, metadata) = safetensors::SafeTensors::read_metadata(&bytes).expect("read_metadata");
        let info = metadata.metadata().as_ref().expect("metadata present");
        assert_eq!(info.len(), 1, "more than one key is not byte stable");
        let line = info.get("amoru_bench").expect("provenance");
        assert!(line.contains("amoru-bench"), "{line}");
        assert!(line.contains("dataset=embed-weights"), "{line}");
        assert!(line.contains("seed=99"), "{line}");
        assert!(line.contains(crate::GENERATOR_VERSION), "{line}");
    }

    #[test]
    fn encoding_is_a_pure_function_of_the_seed() {
        let specs = specs();
        let first = encode("unit", &specs, 3).expect("encode");
        let again = encode("unit", &specs, 3).expect("encode");
        let other = encode("unit", &specs, 4).expect("encode");
        assert_eq!(first, again);
        assert_ne!(first, other);
    }

    #[test]
    fn the_tensor_order_does_not_follow_the_order_the_specs_arrive_in() {
        let mut forwards = specs();
        let mut backwards = forwards.clone();
        backwards.reverse();
        forwards.rotate_left(1);
        assert_eq!(
            encode("unit", &forwards, 1).expect("encode"),
            encode("unit", &backwards, 1).expect("encode")
        );
    }
}
