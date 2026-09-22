//! The Amoru aligned binary format (`AMB1`) writer.
//!
//! The layout is exactly the table of `architecture/sdd/01-contracts.md` section
//! e.4, the single section of that document this crate reads (preamble section 9
//! permits a cited section):
//!
//! | Offset | Size | Field | Value |
//! |---|---|---|---|
//! | 0 | 4 | magic | ASCII `AMB1` |
//! | 4 | 2 | version | 1 |
//! | 6 | 1 | dtype | the `DType` code |
//! | 7 | 1 | ndim | 0 to 8 |
//! | 8 | 8 per dimension | shape | `i64` each |
//! | 8 + 8 times ndim | 8 | `data_offset` | the smallest multiple of `page_bytes` that is at or above the header end |
//! | `data_offset` | element count times item size | payload | row major, contiguous |
//! | after the payload | to the next 64 | padding | zeros |
//!
//! Little endian throughout. This writer does not depend on `amoru-kernel`: the
//! kernel crate's own `amb1` module is being written in parallel, and a
//! benchmark generator that shared its code could not catch a disagreement
//! between the two. The header test in this module is written against the table
//! above, not against another implementation.

use crate::dtype::{DType, element_count};
use crate::error::{BenchError, Result};
use crate::rng::Rng;

/// The page the payload is aligned to. Contracts e.4 names 4096 as the default
/// `page_bytes`, and a reader rejects a `data_offset` that is not a multiple of
/// it, so the generator writes 4096 and nothing else.
pub const PAGE_BYTES: u64 = 4096;

/// The trailing padding boundary of contracts e.4.
pub const TAIL_ALIGN: u64 = 64;

/// The format version this writer emits.
pub const VERSION: u16 = 1;

/// The largest `ndim` contracts e.4 allows.
pub const MAX_NDIM: usize = 8;

/// A tensor to write: a name, an element type and a shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TensorSpec {
    /// The tensor's name: the file stem for `AMB1`, the key for safetensors.
    pub name: String,
    /// The element type.
    pub dtype: DType,
    /// The shape, row major, 0 to 8 dimensions.
    pub shape: Vec<i64>,
}

impl TensorSpec {
    /// A spec, validated against the limits of contracts e.4.
    pub fn new(name: impl Into<String>, dtype: DType, shape: Vec<i64>) -> Result<TensorSpec> {
        let name = name.into();
        if shape.len() > MAX_NDIM {
            return Err(BenchError::Shape(format!(
                "{name}: ndim {} exceeds the {MAX_NDIM} dimensions contracts e.4 allows",
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

/// Round `value` up to the next multiple of `align`.
fn round_up(value: u64, align: u64) -> u64 {
    value.div_ceil(align) * align
}

/// The header end offset: magic, version, dtype, ndim, the shape, `data_offset`.
pub fn header_end(ndim: usize) -> u64 {
    8 + 8 * ndim as u64 + 8
}

/// The `data_offset` field for a tensor of `ndim` dimensions.
pub fn data_offset(ndim: usize) -> u64 {
    round_up(header_end(ndim), PAGE_BYTES)
}

/// The header bytes, from the magic up to `data_offset` (exclusive), including
/// the zero fill that carries the file to the payload.
pub fn header(spec: &TensorSpec) -> Vec<u8> {
    let ndim = spec.shape.len();
    let offset = data_offset(ndim);
    let mut out = Vec::with_capacity(offset as usize);
    out.extend_from_slice(b"AMB1");
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.push(spec.dtype.code());
    out.push(ndim as u8);
    for dim in &spec.shape {
        out.extend_from_slice(&dim.to_le_bytes());
    }
    out.extend_from_slice(&offset.to_le_bytes());
    out.resize(offset as usize, 0);
    out
}

/// The complete file: header, payload, zero padding to the next 64 bytes.
pub fn encode(spec: &TensorSpec, seed: u64) -> Vec<u8> {
    let mut out = header(spec);
    out.extend_from_slice(&spec.payload(seed));
    let padded = round_up(out.len() as u64, TAIL_ALIGN);
    out.resize(padded as usize, 0);
    out
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
    fn the_header_is_byte_for_byte_the_table_of_contracts_e4() {
        let spec = spec(DType::F32, vec![3, 5]);
        let bytes = header(&spec);
        // magic
        assert_eq!(&bytes[0..4], b"AMB1");
        // version 1, little endian
        assert_eq!(&bytes[4..6], &[1, 0]);
        // dtype code: F32 is 10
        assert_eq!(bytes[6], 10);
        // ndim
        assert_eq!(bytes[7], 2);
        // shape, i64 each
        assert_eq!(&bytes[8..16], &3i64.to_le_bytes());
        assert_eq!(&bytes[16..24], &5i64.to_le_bytes());
        // data_offset at 8 + 8 * ndim
        assert_eq!(&bytes[24..32], &4096u64.to_le_bytes());
        // everything from the header end to data_offset is zero fill
        assert!(bytes[32..].iter().all(|b| *b == 0));
        assert_eq!(bytes.len(), 4096);

        // The whole prefix, spelled out, so a change to any field fails here.
        let mut expected = Vec::new();
        expected.extend_from_slice(b"AMB1");
        expected.extend_from_slice(&1u16.to_le_bytes());
        expected.push(10);
        expected.push(2);
        expected.extend_from_slice(&3i64.to_le_bytes());
        expected.extend_from_slice(&5i64.to_le_bytes());
        expected.extend_from_slice(&4096u64.to_le_bytes());
        assert_eq!(&bytes[..expected.len()], expected.as_slice());
    }

    #[test]
    fn a_reader_s_four_checks_hold_for_every_dtype_and_rank() {
        for dtype in crate::dtype::ALL {
            for ndim in 0..=MAX_NDIM {
                let shape: Vec<i64> = (0..ndim).map(|i| (i as i64 % 3) + 1).collect();
                let spec = spec(dtype, shape);
                let file = encode(&spec, 5);
                let offset = data_offset(ndim);
                assert_eq!(&file[0..4], b"AMB1");
                assert_eq!(u16::from_le_bytes([file[4], file[5]]), VERSION);
                assert!(usize::from(file[7]) <= MAX_NDIM);
                assert_eq!(offset % PAGE_BYTES, 0);
                assert!(offset >= header_end(ndim));
                assert!(file.len() as u64 >= offset + spec.payload_len());
                assert_eq!(file.len() as u64 % TAIL_ALIGN, 0);
            }
        }
    }

    #[test]
    fn the_payload_sits_at_data_offset_and_is_row_major() {
        let spec = spec(DType::U8, vec![4, 4]);
        let file = encode(&spec, 11);
        let payload = &file[4096..4096 + 16];
        assert_eq!(payload, spec.payload(11).as_slice());
    }

    #[test]
    fn encoding_is_a_pure_function_of_the_seed_and_the_shape() {
        let spec = spec(DType::F64, vec![2, 8]);
        assert_eq!(encode(&spec, 3), encode(&spec, 3));
        assert_ne!(encode(&spec, 3), encode(&spec, 4));
    }

    #[test]
    fn a_scalar_has_ndim_zero_and_one_element() {
        let spec = spec(DType::I64, vec![]);
        assert_eq!(spec.elements(), 1);
        assert_eq!(spec.payload_len(), 8);
        let file = encode(&spec, 1);
        assert_eq!(file[7], 0);
        assert_eq!(&file[8..16], &4096u64.to_le_bytes());
        assert_eq!(file.len(), 4096 + 64);
        assert_eq!(spec.shape_text(), "[]");
    }

    #[test]
    fn shapes_beyond_the_contract_are_refused() {
        let too_deep = TensorSpec::new("t", DType::F32, vec![1; 9]);
        assert!(matches!(too_deep, Err(BenchError::Shape(_))));
        let negative = TensorSpec::new("t", DType::F32, vec![-1]);
        assert!(matches!(negative, Err(BenchError::Shape(_))));
        match TensorSpec::new("t", DType::F32, vec![2, 3]) {
            Ok(spec) => assert_eq!(spec.shape_text(), "[2,3]"),
            Err(err) => panic!("{err}"),
        }
    }

    #[test]
    fn round_up_and_the_header_end_agree_with_the_table() {
        assert_eq!(round_up(0, 64), 0);
        assert_eq!(round_up(1, 64), 64);
        assert_eq!(round_up(64, 64), 64);
        assert_eq!(header_end(0), 16);
        assert_eq!(header_end(8), 80);
        assert_eq!(data_offset(8), 4096);
    }
}
