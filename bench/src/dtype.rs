//! Tensor element types, their `MRB1` codes and their deterministic payloads.
//!
//! The codes and the item sizes are exactly the `dtype` row of the aligned
//! binary format table in `architecture/sdd/01-contracts.md` section e.4, the one
//! section of that document this crate reads (preamble section 9 permits a cited
//! section). The same set is mapped to the safetensors names for the safetensors
//! writer.

use crate::error::{BenchError, Result};
use crate::rng::Rng;

/// An element type of a generated tensor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DType {
    /// Signed 8 bit integer, `MRB1` code 0.
    I8,
    /// Signed 16 bit integer, `MRB1` code 1.
    I16,
    /// Signed 32 bit integer, `MRB1` code 2.
    I32,
    /// Signed 64 bit integer, `MRB1` code 3.
    I64,
    /// Unsigned 8 bit integer, `MRB1` code 4.
    U8,
    /// Unsigned 16 bit integer, `MRB1` code 5.
    U16,
    /// Unsigned 32 bit integer, `MRB1` code 6.
    U32,
    /// Unsigned 64 bit integer, `MRB1` code 7.
    U64,
    /// Half precision float, `MRB1` code 8.
    F16,
    /// Brain float, `MRB1` code 9.
    BF16,
    /// Single precision float, `MRB1` code 10.
    F32,
    /// Double precision float, `MRB1` code 11.
    F64,
    /// Boolean, one byte per element, `MRB1` code 12.
    Bool,
}

/// Every type the generator writes, in `MRB1` code order.
pub const ALL: [DType; 13] = [
    DType::I8,
    DType::I16,
    DType::I32,
    DType::I64,
    DType::U8,
    DType::U16,
    DType::U32,
    DType::U64,
    DType::F16,
    DType::BF16,
    DType::F32,
    DType::F64,
    DType::Bool,
];

impl DType {
    /// The `MRB1` dtype code (contracts e.4).
    pub fn code(self) -> u8 {
        match self {
            DType::I8 => 0,
            DType::I16 => 1,
            DType::I32 => 2,
            DType::I64 => 3,
            DType::U8 => 4,
            DType::U16 => 5,
            DType::U32 => 6,
            DType::U64 => 7,
            DType::F16 => 8,
            DType::BF16 => 9,
            DType::F32 => 10,
            DType::F64 => 11,
            DType::Bool => 12,
        }
    }

    /// The size of one element in bytes.
    pub fn item_size(self) -> usize {
        match self {
            DType::I8 | DType::U8 | DType::Bool => 1,
            DType::I16 | DType::U16 | DType::F16 | DType::BF16 => 2,
            DType::I32 | DType::U32 | DType::F32 => 4,
            DType::I64 | DType::U64 | DType::F64 => 8,
        }
    }

    /// The name used on the command line and in the manifest.
    pub fn name(self) -> &'static str {
        match self {
            DType::I8 => "i8",
            DType::I16 => "i16",
            DType::I32 => "i32",
            DType::I64 => "i64",
            DType::U8 => "u8",
            DType::U16 => "u16",
            DType::U32 => "u32",
            DType::U64 => "u64",
            DType::F16 => "f16",
            DType::BF16 => "bf16",
            DType::F32 => "f32",
            DType::F64 => "f64",
            DType::Bool => "bool",
        }
    }

    /// Parse a command line dtype name.
    pub fn parse(text: &str) -> Result<DType> {
        let lower = text.to_ascii_lowercase();
        ALL.iter()
            .copied()
            .find(|candidate| candidate.name() == lower)
            .ok_or_else(|| {
                let names: Vec<&str> = ALL.iter().map(|d| d.name()).collect();
                BenchError::Shape(format!(
                    "dtype {text}, expected one of {}",
                    names.join(", ")
                ))
            })
    }

    /// The safetensors name for this type.
    pub fn safetensors(self) -> safetensors::Dtype {
        match self {
            DType::I8 => safetensors::Dtype::I8,
            DType::I16 => safetensors::Dtype::I16,
            DType::I32 => safetensors::Dtype::I32,
            DType::I64 => safetensors::Dtype::I64,
            DType::U8 => safetensors::Dtype::U8,
            DType::U16 => safetensors::Dtype::U16,
            DType::U32 => safetensors::Dtype::U32,
            DType::U64 => safetensors::Dtype::U64,
            DType::F16 => safetensors::Dtype::F16,
            DType::BF16 => safetensors::Dtype::BF16,
            DType::F32 => safetensors::Dtype::F32,
            DType::F64 => safetensors::Dtype::F64,
            DType::Bool => safetensors::Dtype::BOOL,
        }
    }

    /// `count` elements as little endian bytes, drawn from `rng`.
    ///
    /// Floats are built from bit patterns with the exponent held inside a
    /// modest range, so every value is finite and normal: a benchmark payload
    /// with a NaN in it is not a payload anyone can time against.
    pub fn fill(self, rng: &mut Rng, count: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(count * self.item_size());
        for _ in 0..count {
            match self {
                DType::I8 => out.push((rng.next_u32() as i8).to_le_bytes()[0]),
                DType::U8 => out.push(rng.next_u32() as u8),
                DType::Bool => out.push(u8::from(rng.bernoulli(0.5))),
                DType::I16 => out.extend_from_slice(&(rng.next_u32() as i16).to_le_bytes()),
                DType::U16 => out.extend_from_slice(&(rng.next_u32() as u16).to_le_bytes()),
                DType::I32 => out.extend_from_slice(&(rng.next_u32() as i32).to_le_bytes()),
                DType::U32 => out.extend_from_slice(&rng.next_u32().to_le_bytes()),
                DType::I64 => out.extend_from_slice(&(rng.next_u64() as i64).to_le_bytes()),
                DType::U64 => out.extend_from_slice(&rng.next_u64().to_le_bytes()),
                DType::F32 => {
                    let value = (rng.next_f64() * 2.0 - 1.0) as f32;
                    out.extend_from_slice(&value.to_le_bytes());
                }
                DType::F64 => {
                    let value = rng.next_f64() * 2.0 - 1.0;
                    out.extend_from_slice(&value.to_le_bytes());
                }
                DType::F16 => out.extend_from_slice(&finite_half(rng, 5, 10, 9, 25).to_le_bytes()),
                DType::BF16 => {
                    out.extend_from_slice(&finite_half(rng, 8, 7, 121, 137).to_le_bytes())
                }
            }
        }
        out
    }
}

/// A finite, normal 16 bit float bit pattern: one sign bit, an exponent field of
/// `exp_bits` held in `[exp_lo, exp_hi]` (never 0, never all ones, so never a
/// subnormal, an infinity or a NaN) and a random mantissa of `mant_bits`.
fn finite_half(rng: &mut Rng, exp_bits: u32, mant_bits: u32, exp_lo: u32, exp_hi: u32) -> u16 {
    let bits = rng.next_u32();
    let sign = (bits & 1) as u16;
    let span = exp_hi - exp_lo + 1;
    let exponent = exp_lo + ((bits >> 1) % span);
    let mantissa = (bits >> 12) & ((1u32 << mant_bits) - 1);
    debug_assert_eq!(exp_bits + mant_bits + 1, 16);
    (sign << 15) | ((exponent as u16) << mant_bits) | mantissa as u16
}

/// The number of elements a shape holds. An empty shape is one scalar, which is
/// what a product over no dimensions means and what `MRB1` `ndim` 0 encodes.
pub fn element_count(shape: &[i64]) -> u64 {
    shape.iter().map(|d| *d as u64).product()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_sizes_match_contracts_e4() {
        let expected: [(DType, u8, usize); 13] = [
            (DType::I8, 0, 1),
            (DType::I16, 1, 2),
            (DType::I32, 2, 4),
            (DType::I64, 3, 8),
            (DType::U8, 4, 1),
            (DType::U16, 5, 2),
            (DType::U32, 6, 4),
            (DType::U64, 7, 8),
            (DType::F16, 8, 2),
            (DType::BF16, 9, 2),
            (DType::F32, 10, 4),
            (DType::F64, 11, 8),
            (DType::Bool, 12, 1),
        ];
        for (dtype, code, size) in expected {
            assert_eq!(dtype.code(), code, "{}", dtype.name());
            assert_eq!(dtype.item_size(), size, "{}", dtype.name());
        }
        assert_eq!(ALL.len(), 13);
    }

    #[test]
    fn names_round_trip_through_parse() {
        for dtype in ALL {
            assert_eq!(DType::parse(dtype.name()).ok(), Some(dtype));
            assert_eq!(
                DType::parse(&dtype.name().to_ascii_uppercase()).ok(),
                Some(dtype)
            );
            assert_eq!(dtype.safetensors().bitsize(), dtype.item_size() * 8);
        }
        let err = DType::parse("float128").unwrap_err().to_string();
        assert!(err.contains("float128"), "{err}");
        assert!(err.contains("bf16"), "{err}");
    }

    #[test]
    fn fill_produces_the_right_number_of_bytes_and_repeats() {
        for dtype in ALL {
            let mut a = Rng::new(42);
            let mut b = Rng::new(42);
            let first = dtype.fill(&mut a, 37);
            let second = dtype.fill(&mut b, 37);
            assert_eq!(first.len(), 37 * dtype.item_size(), "{}", dtype.name());
            assert_eq!(first, second, "{}", dtype.name());
        }
    }

    #[test]
    fn floats_are_finite() {
        let mut rng = Rng::new(9);
        for chunk in DType::F32.fill(&mut rng, 4096).as_chunks::<4>().0 {
            assert!(f32::from_le_bytes(*chunk).is_finite());
        }
        for chunk in DType::F64.fill(&mut rng, 4096).as_chunks::<8>().0 {
            assert!(f64::from_le_bytes(*chunk).is_finite());
        }
    }

    #[test]
    fn halves_are_normal_and_finite() {
        let mut rng = Rng::new(21);
        for chunk in DType::F16.fill(&mut rng, 8192).as_chunks::<2>().0 {
            let exponent = (u16::from_le_bytes(*chunk) >> 10) & 0x1f;
            assert!(exponent != 0 && exponent != 0x1f, "f16 exponent {exponent}");
        }
        for chunk in DType::BF16.fill(&mut rng, 8192).as_chunks::<2>().0 {
            let exponent = (u16::from_le_bytes(*chunk) >> 7) & 0xff;
            assert!(
                exponent != 0 && exponent != 0xff,
                "bf16 exponent {exponent}"
            );
        }
    }

    #[test]
    fn bools_are_zero_or_one_and_roughly_balanced() {
        let mut rng = Rng::new(4);
        let bytes = DType::Bool.fill(&mut rng, 20_000);
        assert!(bytes.iter().all(|b| *b <= 1));
        let ones = bytes.iter().filter(|b| **b == 1).count() as f64 / 20_000.0;
        assert!((ones - 0.5).abs() < 0.03, "ones {ones}");
    }

    #[test]
    fn element_count_multiplies_and_a_scalar_is_one() {
        assert_eq!(element_count(&[]), 1);
        assert_eq!(element_count(&[7]), 7);
        assert_eq!(element_count(&[2, 3, 4]), 24);
        assert_eq!(element_count(&[0, 5]), 0);
    }
}
