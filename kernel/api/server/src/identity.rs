//! The server's TLS identity: the operator's certificate and key (node config's `tls`), or the
//! node's own, made at boot (system design, sections 1D and 2B).

use crate::der::{Template, certificate};
use alloc::{string::String, sync::Arc, vec::Vec};
use core::fmt;
use ring::{
    digest,
    rand::{SecureRandom, SystemRandom},
    signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair},
};
use rustls::{
    InconsistentKeys,
    pki_types::{
        CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer,
        pem::{PemObject, SectionKind},
    },
    sign::CertifiedKey,
};

/// How long before the wall clock the node's own certificate is valid: one hour.
pub const VALID_BEFORE_SECONDS: u64 = 3_600;
/// How long after the wall clock it is valid: 397 days.
pub const VALID_AFTER_SECONDS: u64 = 397 * 86_400;

/// Why an identity could not be made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityError {
    /// The source of randomness failed.
    Random,
    /// The certificate chain is missing or does not parse.
    Certificate,
    /// The key does not parse, or is of a kind TLS cannot sign with.
    Key,
    /// The key is not the certificate's.
    Mismatch,
}

impl fmt::Display for IdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            IdentityError::Random => "the source of randomness failed",
            IdentityError::Certificate => "the certificate chain is missing or does not parse",
            IdentityError::Key => "the key does not parse or cannot sign for TLS",
            IdentityError::Mismatch => "the key is not the certificate's",
        })
    }
}

/// A certificate chain and its key, which the server presents in every TLS handshake.
#[derive(Clone, Debug)]
pub struct Identity {
    key: Arc<CertifiedKey>,
    fingerprint: [u8; 32],
}

impl Identity {
    /// The node's own identity (system design, 2B): a new ECDSA P-256 key and a self-signed
    /// X.509 v3 certificate for it. Its subject and issuer are `CN=<instance>`, its serial 16
    /// random bytes, and it is valid from one hour before `wall_clock` (Unix seconds) to 397 days
    /// after it, with basicConstraints CA false, keyUsage digitalSignature and extendedKeyUsage
    /// serverAuth.
    pub fn generate(instance: &str, wall_clock: u64) -> Result<Self, IdentityError> {
        let random = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &random)
            .map_err(|_| IdentityError::Random)?;
        let pair =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &random)
                .map_err(|_| IdentityError::Key)?;
        let mut serial = [0u8; 16];
        random
            .fill(&mut serial)
            .map_err(|_| IdentityError::Random)?;
        let tbs = Template {
            serial: &serial,
            common_name: instance,
            not_before: wall_clock.saturating_sub(VALID_BEFORE_SECONDS),
            not_after: wall_clock.saturating_add(VALID_AFTER_SECONDS),
            public_key: pair.public_key().as_ref(),
        }
        .to_be_signed();
        let signature = pair
            .sign(&random, &tbs)
            .map_err(|_| IdentityError::Random)?;
        Self::new(
            Vec::from([CertificateDer::from(certificate(&tbs, signature.as_ref()))]),
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pkcs8.as_ref().to_vec())),
        )
    }

    /// The operator's identity, from node config's `tls`: `certificate`, a PEM chain with the
    /// server's own certificate first, and `key`, its PEM private key (PKCS #8, SEC 1 or
    /// PKCS #1).
    pub fn from_pem(certificate: &str, key: &str) -> Result<Self, IdentityError> {
        let mut chain = Vec::new();
        for section in <(SectionKind, Vec<u8>)>::pem_slice_iter(certificate.as_bytes()) {
            match section.map_err(|_| IdentityError::Certificate)? {
                (SectionKind::Certificate, der) => chain.push(CertificateDer::from(der)),
                _ => return Err(IdentityError::Certificate),
            }
        }
        if chain.is_empty() {
            return Err(IdentityError::Certificate);
        }
        let key = PrivateKeyDer::from_pem_slice(key.as_bytes()).map_err(|_| IdentityError::Key)?;
        Self::new(chain, key)
    }

    fn new(
        chain: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
    ) -> Result<Self, IdentityError> {
        let signer =
            rustls::crypto::ring::sign::any_supported_type(&key).map_err(|_| IdentityError::Key)?;
        let key = CertifiedKey::new(chain, signer);
        // Parses the server's certificate (webpki: X.509 v3, well-formed DER) and compares its
        // public key with the key's.
        key.keys_match().map_err(|error| match error {
            rustls::Error::InconsistentKeys(InconsistentKeys::KeyMismatch) => {
                IdentityError::Mismatch
            }
            rustls::Error::InconsistentKeys(InconsistentKeys::Unknown) => IdentityError::Key,
            _ => IdentityError::Certificate,
        })?;
        let end_entity = key
            .end_entity_cert()
            .map_err(|_| IdentityError::Certificate)?;
        let digest = digest::digest(&digest::SHA256, end_entity);
        let mut fingerprint = [0u8; 32];
        fingerprint.copy_from_slice(digest.as_ref());
        Ok(Self {
            key: Arc::new(key),
            fingerprint,
        })
    }

    /// The SHA-256 of the server's certificate's DER, in lowercase hex: what the console line
    /// `moruna: tls sha256=<fingerprint>` shows and what a client pins.
    pub fn fingerprint(&self) -> String {
        hex(&self.fingerprint)
    }

    /// The server's own certificate, DER.
    pub fn certificate(&self) -> &[u8] {
        self.key.cert.first().map_or(&[], |c| c.as_ref())
    }

    pub(crate) fn certified_key(&self) -> Arc<CertifiedKey> {
        self.key.clone()
    }
}

/// `bytes` in lowercase hex.
pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(DIGITS[usize::from(byte >> 4)] as char);
        text.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    text
}

#[cfg(test)]
mod tests;
