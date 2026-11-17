//! The stand-in node's TLS identity, generated at each start as the node makes its own (system
//! design 2B): an ECDSA P-256 key, and a self-signed X.509 v3 certificate of 2B's template,
//! `CN=<instance id>`, a 16-byte random serial, valid from an hour ago for 397 days, with
//! basicConstraints CA false, keyUsage digitalSignature and extendedKeyUsage serverAuth.
//!
//! Replaced by `moruna-api-server`'s `Identity::generate` once item 2.2 is merged.

use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};

/// A certificate and its key.
pub struct Identity {
    /// The certificate's DER.
    pub certificate: Vec<u8>,
    /// The key, PKCS#8 DER.
    pub key: Vec<u8>,
}

/// ecdsa-with-SHA256, 1.2.840.10045.4.3.2.
const ECDSA_WITH_SHA256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];
/// id-ecPublicKey, 1.2.840.10045.2.1.
const EC_PUBLIC_KEY: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
/// prime256v1, 1.2.840.10045.3.1.7.
const PRIME256V1: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
/// commonName, 2.5.4.3.
const COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];
/// basicConstraints, 2.5.29.19.
const BASIC_CONSTRAINTS: &[u8] = &[0x55, 0x1d, 0x13];
/// keyUsage, 2.5.29.15.
const KEY_USAGE: &[u8] = &[0x55, 0x1d, 0x0f];
/// extKeyUsage, 2.5.29.37.
const EXT_KEY_USAGE: &[u8] = &[0x55, 0x1d, 0x25];
/// id-kp-serverAuth, 1.3.6.1.5.5.7.3.1.
const SERVER_AUTH: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01];

/// A new key and a self-signed certificate for `instance`, valid around `now` (Unix seconds).
pub fn generate(instance: &str, now: u64) -> Identity {
    let random = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &random)
        .expect("a P-256 key");
    let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &random)
        .expect("the key just made");
    let mut serial = [0u8; 16];
    random.fill(&mut serial).expect("random bytes");
    serial[0] &= 0x7f; // a positive INTEGER, without a leading zero byte
    serial[0] |= 0x01;

    let algorithm = seq(&[oid(ECDSA_WITH_SHA256)]);
    let name = seq(&[tlv(
        0x31,
        &seq(&[oid(COMMON_NAME), tlv(0x0c, instance.as_bytes())]),
    )]);
    let validity = seq(&[utc_time(now - 3600), utc_time(now + 397 * 24 * 3600)]);
    let key_info = seq(&[
        seq(&[oid(EC_PUBLIC_KEY), oid(PRIME256V1)]),
        bit_string(pair.public_key().as_ref()),
    ]);
    let extensions = seq(&[
        extension(BASIC_CONSTRAINTS, true, &seq(&[])),
        extension(KEY_USAGE, true, &[0x03, 0x02, 0x07, 0x80]),
        extension(EXT_KEY_USAGE, false, &seq(&[oid(SERVER_AUTH)])),
    ]);
    let tbs = seq(&[
        tlv(0xa0, &[0x02, 0x01, 0x02]),
        tlv(0x02, &serial),
        algorithm.clone(),
        name.clone(),
        validity,
        name,
        key_info,
        tlv(0xa3, &extensions),
    ]);
    let signature = pair.sign(&random, &tbs).expect("a signature");
    Identity {
        certificate: seq(&[tbs, algorithm, bit_string(signature.as_ref())]),
        key: pkcs8.as_ref().to_vec(),
    }
}

/// One extension: its ID, whether it is critical, and its value.
fn extension(id: &[u8], critical: bool, value: &[u8]) -> Vec<u8> {
    let mut parts = vec![oid(id)];
    if critical {
        parts.push(vec![0x01, 0x01, 0xff]);
    }
    parts.push(tlv(0x04, value));
    seq(&parts)
}

/// A BIT STRING with no unused bits.
fn bit_string(bytes: &[u8]) -> Vec<u8> {
    let mut content = vec![0];
    content.extend_from_slice(bytes);
    tlv(0x03, &content)
}

fn oid(id: &[u8]) -> Vec<u8> {
    tlv(0x06, id)
}

fn seq(parts: &[Vec<u8>]) -> Vec<u8> {
    tlv(0x30, &parts.concat())
}

/// A tag, its DER length and its content.
fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let len = content.len();
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes = len.to_be_bytes();
        let skip = bytes.iter().take_while(|&&b| b == 0).count();
        out.push(0x80 | (bytes.len() - skip) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }
    out.extend_from_slice(content);
    out
}

/// A UTCTime, `YYMMDDHHMMSSZ`, for a time before 2050.
fn utc_time(unix: u64) -> Vec<u8> {
    let days = (unix / 86_400) as i64;
    let seconds = unix % 86_400;
    // Days to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let text = format!(
        "{:02}{:02}{:02}{:02}{:02}{:02}Z",
        year % 100,
        month,
        day,
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    );
    tlv(0x17, text.as_bytes())
}
