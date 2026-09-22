//! The kernel fingerprint (contracts d.7, e.6).

/// A stable identity for a kernel: a 32-byte BLAKE3 digest over its identity string and its
/// configuration bytes. Two kernels with equal fingerprints are assumed to have equal
/// amplification behaviour; that is a profile-store assumption, not a correctness one (e.6).
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
    /// BLAKE3 over `identity.len() as u64 LE || identity || config` (e.6). For a Rust kernel,
    /// `identity` is the crate name, version and type path; for a Python kernel, the adapters
    /// SDD defines it.
    pub fn compute(identity: &str, config: &[u8]) -> Fingerprint {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&(identity.len() as u64).to_le_bytes());
        hasher.update(identity.as_bytes());
        hasher.update(config);
        Fingerprint(*hasher.finalize().as_bytes())
    }

    /// The digest as 64 lowercase hex characters.
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
}

impl core::fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.to_hex())
    }
}
