//! The digest of a job document (MH 4.1, SDD 22 e.4): `sha256` of the canonical form of what the
//! job is ([`moruna_job::Job::identity_json`]). The canonical form is `moruna-job`'s; the hash is
//! here, so a crate that only reads and checks documents carries no hash.

use sha2::{Digest, Sha256};

use moruna_job::Job;

/// The content address of `job` (MH 4.1): `sha256:` and the hex digest of its identity, the
/// canonical form with `resume` and `report` removed.
pub fn digest(job: &Job) -> String {
    sha256_hex(job.identity_json().as_bytes())
}

/// `sha256:` followed by the 64 lowercase hex characters of the SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(7 + 64);
    out.push_str("sha256:");
    for b in digest.iter() {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The digest of the empty string is SHA-256's published one.
    #[test]
    fn sha256_is_sha256() {
        assert_eq!(
            sha256_hex(b""),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
