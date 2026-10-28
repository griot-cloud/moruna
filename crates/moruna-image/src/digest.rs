//! A content address: `sha256:` and 64 lowercase hex digits.

use std::fmt;
use std::io::Read;
use std::str::FromStr;

use sha2::{Digest as _, Sha256};

use crate::error::{Error, Result};

/// The algorithm every digest here uses.
const ALGORITHM: &str = "sha256";

/// A sha256 content address, as the OCI image spec writes it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest {
    hex: String,
}

impl Digest {
    /// The digest of `bytes`.
    pub fn of(bytes: &[u8]) -> Digest {
        Digest {
            hex: hex(&Sha256::digest(bytes)),
        }
    }

    /// The 64 hex digits, which name the blob's file and the cache entry.
    pub fn hex(&self) -> &str {
        &self.hex
    }

    /// Whether `text` is written as a digest (`sha256:` and 64 hex digits): the test of a
    /// reference that names a manifest by its content rather than by a name.
    pub fn is_digest(text: &str) -> bool {
        text.parse::<Digest>().is_ok()
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{ALGORITHM}:{}", self.hex)
    }
}

impl FromStr for Digest {
    type Err = Error;

    fn from_str(text: &str) -> Result<Digest> {
        let Some(hex) = text.strip_prefix("sha256:") else {
            return Err(Error::Format(format!(
                "`{text}` is not a digest: only `sha256:<64 hex digits>` is read"
            )));
        };
        if hex.len() != 64 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(Error::Format(format!(
                "`{text}` is not a digest: `sha256:` is followed by 64 lowercase hex digits"
            )));
        }
        Ok(Digest {
            hex: hex.to_string(),
        })
    }
}

impl<'de> serde::Deserialize<'de> for Digest {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Digest, D::Error> {
        let text = <String as serde::Deserialize>::deserialize(d)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    out
}

/// A reader that hashes and counts what passes through it.
pub(crate) struct Hashing<R> {
    inner: R,
    hasher: Sha256,
    read: u64,
}

impl<R: Read> Hashing<R> {
    pub(crate) fn new(inner: R) -> Hashing<R> {
        Hashing {
            inner,
            hasher: Sha256::new(),
            read: 0,
        }
    }

    /// Read what is left (a decompressor stops at the end of its stream and may leave padding
    /// behind), then the digest and the length of everything read.
    pub(crate) fn finish(mut self) -> std::io::Result<(Digest, u64)> {
        std::io::copy(&mut self, &mut std::io::sink())?;
        Ok((
            Digest {
                hex: hex(&self.hasher.finalize()),
            },
            self.read,
        ))
    }
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.read += n as u64;
        Ok(n)
    }
}

/// Check that `bytes` hash to `expected`.
pub(crate) fn verify(expected: &Digest, bytes: &[u8]) -> Result<()> {
    let actual = Digest::of(bytes);
    if &actual != expected {
        return Err(Error::Digest {
            digest: expected.to_string(),
            actual: actual.to_string(),
        });
    }
    Ok(())
}
