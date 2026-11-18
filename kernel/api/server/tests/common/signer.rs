//! The tests' token helper (item 2.3): makes an Ed25519 key pair and signs claims with it, as a
//! token issuer would. The crate's unit tests and its end-to-end tests both use it.

use ring::{
    rand::SystemRandom,
    signature::{Ed25519KeyPair, KeyPair},
};
use std::{format, string::String, vec::Vec};

/// `bytes` in base64url without padding (RFC 7515, section 2).
pub fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let bits = chunk
            .iter()
            .enumerate()
            .fold(0u32, |bits, (i, b)| bits | u32::from(*b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            text.push(char::from(ALPHABET[(bits >> (18 - 6 * i) & 63) as usize]));
        }
    }
    text
}

/// `bytes` in standard base64 with padding (RFC 4648, section 4), as node config writes keys.
#[allow(dead_code)] // Not every crate that includes this file registers functions.
pub fn base64(bytes: &[u8]) -> String {
    let url = base64url(bytes).replace('-', "+").replace('_', "/");
    let padding = (4 - url.len() % 4) % 4;
    format!("{url}{}", "=".repeat(padding))
}

/// The SHA-256 of `bytes`, as 64 lowercase hexadecimal digits.
#[allow(dead_code)] // Not every crate that includes this file registers functions.
pub fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A function's registration (system design 3E) for `image` at `address`, signed by `signer`.
#[allow(dead_code)] // Not every crate that includes this file registers functions.
pub fn registration(name: &str, address: &str, image: &[u8], signer: &Signer) -> String {
    let sha256 = sha256_hex(image);
    let digest = ring::digest::digest(&ring::digest::SHA256, image);
    let signature = base64(&signer.signature(digest.as_ref()));
    format!(
        r#"{{"name":"{name}","address":"{address}","sha256":"{sha256}","signature":"{signature}"}}"#
    )
}

/// A token issuer's key pair.
pub struct Signer {
    pair: Ed25519KeyPair,
}

impl Default for Signer {
    fn default() -> Self {
        Self::new()
    }
}

impl Signer {
    /// A new key pair.
    pub fn new() -> Self {
        let random = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&random).expect("a new Ed25519 key");
        Self {
            pair: Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("the key just made"),
        }
    }

    /// The public key's 32 bytes, as node config's `token_keys` carries them.
    pub fn public(&self) -> [u8; 32] {
        self.pair
            .public_key()
            .as_ref()
            .try_into()
            .expect("an Ed25519 public key is 32 bytes")
    }

    /// An Ed25519 signature over `message` by this key: a function image's is over the 32 bytes
    /// of its SHA-256 (system design 3E).
    #[allow(dead_code)] // Not every crate that includes this file registers functions.
    pub fn signature(&self, message: &[u8]) -> Vec<u8> {
        self.pair.sign(message).as_ref().to_vec()
    }

    /// A compact JWT whose header and claims are these JSON texts, signed by this key.
    pub fn sign(&self, header: &str, claims: &str) -> String {
        let input = format!(
            "{}.{}",
            base64url(header.as_bytes()),
            base64url(claims.as_bytes())
        );
        let signature = self.pair.sign(input.as_bytes());
        format!("{input}.{}", base64url(signature.as_ref()))
    }

    /// A token for the node `aud`, expiring at `exp`, with `scope`, with the header
    /// `{"alg":"EdDSA","typ":"JWT"}`.
    pub fn token(&self, aud: &str, exp: u64, scope: &[&str]) -> String {
        let scope: Vec<String> = scope.iter().map(|s| format!("\"{s}\"")).collect();
        self.sign(
            r#"{"alg":"EdDSA","typ":"JWT"}"#,
            &format!(
                r#"{{"aud":"{aud}","exp":{exp},"scope":[{}]}}"#,
                scope.join(",")
            ),
        )
    }
}
