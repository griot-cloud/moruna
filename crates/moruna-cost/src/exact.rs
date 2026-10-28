//! Floats in fitted state written as their bits, so a save and a load give back the same model
//! to the last bit (CM-I3). JSON's decimal floats are read back to within a unit in the last
//! place only, unless the parser is asked for more, and that request is a feature every crate
//! in the build would share.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// One `f64`.
pub(crate) mod float {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(value: &f64, s: S) -> Result<S::Ok, S::Error> {
        value.to_bits().serialize(s)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
        u64::deserialize(d).map(f64::from_bits)
    }
}

/// A fixed array of `f64`.
pub(crate) mod array {
    use super::*;
    use serde::de::Error as _;

    pub(crate) fn serialize<S: Serializer, const N: usize>(
        value: &[f64; N],
        s: S,
    ) -> Result<S::Ok, S::Error> {
        let bits: Vec<u64> = value.iter().map(|v| v.to_bits()).collect();
        bits.serialize(s)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>, const N: usize>(
        d: D,
    ) -> Result<[f64; N], D::Error> {
        let bits = Vec::<u64>::deserialize(d)?;
        if bits.len() != N {
            return Err(D::Error::custom(format!(
                "{} values where {N} were expected",
                bits.len()
            )));
        }
        let mut out = [0.0; N];
        for (slot, b) in out.iter_mut().zip(bits) {
            *slot = f64::from_bits(b);
        }
        Ok(out)
    }
}

/// A list of `f64`.
pub(crate) mod list {
    use super::*;
    use std::collections::VecDeque;

    pub(crate) fn serialize<S: Serializer>(value: &VecDeque<f64>, s: S) -> Result<S::Ok, S::Error> {
        let bits: Vec<u64> = value.iter().map(|v| v.to_bits()).collect();
        bits.serialize(s)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<VecDeque<f64>, D::Error> {
        Ok(Vec::<u64>::deserialize(d)?
            .into_iter()
            .map(f64::from_bits)
            .collect())
    }
}
