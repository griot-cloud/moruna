//! Pinning (system design 2E): the server's certificate is accepted exactly when the SHA-256 of
//! its DER equals the pin, and nothing else about it is checked.
//!
//! "Nothing else" is the certificate's chain, names, dates and extensions. The TLS handshake's
//! own signature is still verified under the certificate's key, as every rustls verifier does:
//! without it, anyone who had seen the node's certificate could show it and pass.

use std::fmt;
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, OtherError, SignatureScheme};
use sha2::{Digest, Sha256};

use crate::Error;

/// The SHA-256 of a certificate's DER: what a node prints on its console as
/// `moruna: tls sha256=<fingerprint>`, and what a client is pinned to.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    /// The fingerprint of a certificate, from its DER.
    pub fn of(der: &[u8]) -> Self {
        Self(Sha256::digest(der).into())
    }

    /// A fingerprint written as 64 hexadecimal digits, in either case.
    pub fn parse(hex: &str) -> Result<Self, Error> {
        if hex.len() != 64 || !hex.bytes().all(|digit| digit.is_ascii_hexdigit()) {
            return Err(Error::Pin(hex.to_string()));
        }
        let mut bytes = [0u8; 32];
        let (pairs, _) = hex.as_bytes().as_chunks::<2>();
        for (byte, [high, low]) in bytes.iter_mut().zip(pairs) {
            *byte = nibble(*high) << 4 | nibble(*low);
        }
        Ok(Self(bytes))
    }

    /// The fingerprint's bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// The value of one hexadecimal digit, already known to be one.
fn nibble(digit: u8) -> u8 {
    match digit {
        b'0'..=b'9' => digit - b'0',
        _ => (digit | 0x20) - b'a' + 10,
    }
}

/// Lowercase hex, as the node's console prints it.
impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({self})")
    }
}

/// The rustls configuration of a pinned client: rustls's defaults on its `ring` provider, with
/// [`PinVerifier`] in place of the usual certificate verification.
pub(crate) fn config(pin: Fingerprint) -> Result<rustls::ClientConfig, rustls::Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = PinVerifier {
        pin,
        algorithms: provider.signature_verification_algorithms,
    };
    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth())
}

/// Accepts the server's certificate exactly when its fingerprint is the pin.
#[derive(Debug)]
struct PinVerifier {
    pin: Fingerprint,
    algorithms: WebPkiSupportedAlgorithms,
}

/// The certificate the server showed, when it is not the pinned one. It travels inside rustls's
/// error, so the client can say what the server showed.
#[derive(Debug)]
struct Mismatch(Fingerprint);

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the certificate's SHA-256 is {}, not the pin", self.0)
    }
}

impl std::error::Error for Mismatch {}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let found = Fingerprint::of(end_entity.as_ref());
        if found == self.pin {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(CertificateError::Other(
                OtherError(Arc::new(Mismatch(found))),
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// The fingerprint of the certificate a failed call was refused for, when the pin refused it.
/// rustls's error is carried, as the source of an I/O error, under the HTTP client's errors.
pub(crate) fn mismatch(error: &(dyn std::error::Error + 'static)) -> Option<Fingerprint> {
    let mut next = Some(error);
    while let Some(error) = next {
        if let Some(found) = found_in(error) {
            return Some(found);
        }
        // An I/O error's `source` skips the error it wraps; `get_ref` is that error.
        if let Some(inner) = error
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::get_ref)
            && let Some(found) = mismatch(inner)
        {
            return Some(found);
        }
        next = error.source();
    }
    None
}

/// The fingerprint in this one error, when it is the pin's refusal.
fn found_in(error: &(dyn std::error::Error + 'static)) -> Option<Fingerprint> {
    if let Some(Mismatch(found)) = error.downcast_ref::<Mismatch>() {
        return Some(*found);
    }
    match error.downcast_ref::<rustls::Error>() {
        Some(rustls::Error::InvalidCertificate(CertificateError::Other(OtherError(inner)))) => {
            inner
                .downcast_ref::<Mismatch>()
                .map(|Mismatch(found)| *found)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests;
