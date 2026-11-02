//! What a cost is kept against (design section 3.2's identity; SDD 24 e.1).

use std::fmt;

use moruna_job::Role;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A 32-byte digest: a function's fingerprint, a settings digest, an input schema's hash.
/// Written as 64 lowercase hex characters.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Digest(pub [u8; 32]);

impl Digest {
    /// The digest as 64 lowercase hex characters.
    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for byte in self.0 {
            out.push(HEX[usize::from(byte >> 4)] as char);
            out.push(HEX[usize::from(byte & 0x0f)] as char);
        }
        out
    }

    /// The digest 64 hex characters spell, or `None` when they do not spell one.
    pub fn from_hex(text: &str) -> Option<Digest> {
        let bytes = text.as_bytes();
        if bytes.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for (at, pair) in bytes.as_chunks::<2>().0.iter().enumerate() {
            out[at] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
        }
        Some(Digest(out))
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl Serialize for Digest {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Digest, D::Error> {
        let text = String::deserialize(d)?;
        Digest::from_hex(&text)
            .ok_or_else(|| D::Error::custom(format!("`{text}` is not 64 hex characters")))
    }
}

/// The identity of what is costed: a kind of a role, the function it runs, its settings and
/// the schema it reads. Two units with one key are the same work on different data.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CostKey {
    /// The part of a run the kind is.
    pub role: Role,
    /// The kind's name, as the job document names it.
    pub kind: String,
    /// The function's fingerprint; for a kind that runs no function, a digest the embedder
    /// chooses for the kind (its name's, for instance).
    pub fingerprint: Digest,
    /// The digest of the entry's settings.
    pub settings: Digest,
    /// The hash of the input schema.
    pub schema: Digest,
}

impl CostKey {
    /// The key in one line, which is what the local store hashes to name the key's directory.
    pub fn canonical(&self) -> String {
        format!(
            "{}/{}/{}/{}/{}",
            self.role.as_str(),
            self.kind,
            self.fingerprint,
            self.settings,
            self.schema
        )
    }
}

impl fmt::Display for CostKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} `{}` {}",
            self.role,
            self.kind,
            &self.fingerprint.to_hex()[..12]
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_hex_round_trip() {
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i * 37) as u8;
        }
        let d = Digest(bytes);
        let hex = d.to_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(Digest::from_hex(&hex), Some(d));
        assert_eq!(Digest::from_hex(&hex.to_uppercase()), Some(d));
        assert_eq!(Digest::from_hex("ab"), None);
        assert_eq!(Digest::from_hex(&"g".repeat(64)), None);
        assert_eq!(format!("{d:?}"), hex);
        let json = serde_json::to_string(&d).expect("serialises");
        assert_eq!(
            serde_json::from_str::<Digest>(&json).expect("reads"),
            d,
            "a digest reads back"
        );
        assert!(serde_json::from_str::<Digest>("\"zz\"").is_err());
    }

    #[test]
    fn key_display_and_canonical() {
        let key = CostKey {
            role: Role::Kernel,
            kind: "std".into(),
            fingerprint: Digest([1; 32]),
            settings: Digest([2; 32]),
            schema: Digest([3; 32]),
        };
        assert!(key.canonical().starts_with("kernel/std/0101"));
        assert_eq!(key.to_string(), "kernel `std` 010101010101");
    }
}
