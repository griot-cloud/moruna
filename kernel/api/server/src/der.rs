//! A minimal DER writer for the node's one certificate template (system design, section 2B):
//! an X.509 v3 certificate, self-signed with ECDSA P-256 and SHA-256, whose subject and issuer
//! are `CN=<instance id>`, with basicConstraints (CA false), keyUsage (digitalSignature) and
//! extendedKeyUsage (serverAuth).

use alloc::vec::Vec;

const BOOLEAN: u8 = 0x01;
const INTEGER: u8 = 0x02;
const BIT_STRING: u8 = 0x03;
const OCTET_STRING: u8 = 0x04;
const OID: u8 = 0x06;
const UTF8_STRING: u8 = 0x0c;
const UTC_TIME: u8 = 0x17;
const GENERALIZED_TIME: u8 = 0x18;
const SEQUENCE: u8 = 0x30;
const SET: u8 = 0x31;
/// `[0]` and `[3]`, constructed and explicit.
const VERSION_TAG: u8 = 0xa0;
const EXTENSIONS_TAG: u8 = 0xa3;

/// 1.2.840.10045.4.3.2, ecdsa-with-SHA256.
const ECDSA_WITH_SHA256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];
/// 1.2.840.10045.2.1, id-ecPublicKey.
const EC_PUBLIC_KEY: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
/// 1.2.840.10045.3.1.7, prime256v1 (P-256).
const PRIME256V1: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
/// 2.5.4.3, commonName.
const COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];
/// 2.5.29.19, basicConstraints.
const BASIC_CONSTRAINTS: &[u8] = &[0x55, 0x1d, 0x13];
/// 2.5.29.15, keyUsage.
const KEY_USAGE: &[u8] = &[0x55, 0x1d, 0x0f];
/// 2.5.29.37, extKeyUsage.
const EXT_KEY_USAGE: &[u8] = &[0x55, 0x1d, 0x25];
/// 1.3.6.1.5.5.7.3.1, id-kp-serverAuth.
const SERVER_AUTH: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01];

/// The last second a GeneralizedTime can write: 9999-12-31T23:59:59Z.
const LAST_SECOND: u64 = 253_402_300_799;
/// UTCTime writes years 1950 to 2049; from 2050 a certificate uses GeneralizedTime (RFC 5280,
/// 4.1.2.5). 2050-01-01T00:00:00Z.
const FIRST_GENERALIZED: u64 = 2_524_608_000;

/// One TLV: `tag`, the DER length of `content`, then `content`.
fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(content.len() + 6);
    out.push(tag);
    let length = content.len();
    if length < 0x80 {
        out.push(length as u8);
    } else {
        let bytes = length.to_be_bytes();
        let skip = bytes.iter().take_while(|b| **b == 0).count();
        out.push(0x80 | (bytes.len() - skip) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }
    out.extend_from_slice(content);
    out
}

/// A constructed value whose content is `parts`, one after another.
fn constructed(tag: u8, parts: &[&[u8]]) -> Vec<u8> {
    tlv(tag, &parts.concat())
}

/// A non-negative INTEGER from big-endian `magnitude`, in its minimal DER form.
fn unsigned(magnitude: &[u8]) -> Vec<u8> {
    let skip = magnitude.iter().take_while(|b| **b == 0).count();
    let digits = &magnitude[skip..];
    let mut content = Vec::with_capacity(digits.len() + 1);
    if digits.first().is_none_or(|b| b & 0x80 != 0) {
        content.push(0);
    }
    content.extend_from_slice(digits);
    tlv(INTEGER, &content)
}

/// A BIT STRING of whole bytes.
fn bits(bytes: &[u8]) -> Vec<u8> {
    let mut content = Vec::with_capacity(bytes.len() + 1);
    content.push(0);
    content.extend_from_slice(bytes);
    tlv(BIT_STRING, &content)
}

/// One extension: its OID, `critical` when it is, and its value wrapped in an OCTET STRING.
fn extension(oid: &[u8], critical: bool, value: &[u8]) -> Vec<u8> {
    let oid = tlv(OID, oid);
    let value = tlv(OCTET_STRING, value);
    if critical {
        constructed(SEQUENCE, &[&oid, &tlv(BOOLEAN, &[0xff]), &value])
    } else {
        constructed(SEQUENCE, &[&oid, &value])
    }
}

/// The year, month, day, hour, minute and second of `unix`, a count of seconds since
/// 1970-01-01T00:00:00Z (the proleptic Gregorian calendar, from Howard Hinnant's
/// `civil_from_days`).
fn civil(unix: u64) -> (u64, u64, u64, u64, u64, u64) {
    let days = unix / 86_400;
    let second_of_day = unix % 86_400;
    // Days from 0000-03-01, the start of a 400-year era whose leap day is its last day.
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    (
        year,
        month,
        day,
        second_of_day / 3_600,
        second_of_day / 60 % 60,
        second_of_day % 60,
    )
}

/// `unix` as an X.509 Time: UTCTime before 2050, GeneralizedTime from 2050, in UTC to the
/// second. A time past the year 9999 is written as its last second.
fn time(unix: u64) -> Vec<u8> {
    let unix = unix.min(LAST_SECOND);
    let (year, month, day, hour, minute, second) = civil(unix);
    let mut text = Vec::with_capacity(15);
    let mut digits = |value: u64, width: u32| {
        for place in (0..width).rev() {
            text.push(b'0' + (value / 10u64.pow(place) % 10) as u8);
        }
    };
    let tag = if unix < FIRST_GENERALIZED {
        digits(year % 100, 2);
        UTC_TIME
    } else {
        digits(year, 4);
        GENERALIZED_TIME
    };
    for part in [month, day, hour, minute, second] {
        digits(part, 2);
    }
    text.push(b'Z');
    tlv(tag, &text)
}

/// The template's fields, before the signature.
pub(crate) struct Template<'a> {
    /// The serial number's bytes, as big-endian magnitude.
    pub serial: &'a [u8],
    /// The subject's and the issuer's common name: the instance ID.
    pub common_name: &'a str,
    /// The first second the certificate is valid, Unix seconds.
    pub not_before: u64,
    /// The last second it is valid, Unix seconds.
    pub not_after: u64,
    /// The P-256 public key, as an uncompressed point (`04 || X || Y`).
    pub public_key: &'a [u8],
}

/// The ecdsa-with-SHA256 AlgorithmIdentifier, whose parameters are absent (RFC 5758, 3.2).
fn signature_algorithm() -> Vec<u8> {
    constructed(SEQUENCE, &[&tlv(OID, ECDSA_WITH_SHA256)])
}

impl Template<'_> {
    /// The DER of the TBSCertificate, which the key signs.
    pub(crate) fn to_be_signed(&self) -> Vec<u8> {
        let name = constructed(
            SEQUENCE,
            &[&constructed(
                SET,
                &[&constructed(
                    SEQUENCE,
                    &[
                        &tlv(OID, COMMON_NAME),
                        &tlv(UTF8_STRING, self.common_name.as_bytes()),
                    ],
                )],
            )],
        );
        let validity = constructed(SEQUENCE, &[&time(self.not_before), &time(self.not_after)]);
        let public_key = constructed(
            SEQUENCE,
            &[
                &constructed(SEQUENCE, &[&tlv(OID, EC_PUBLIC_KEY), &tlv(OID, PRIME256V1)]),
                &bits(self.public_key),
            ],
        );
        // basicConstraints with cA FALSE, which DER leaves out as the default: an empty
        // SEQUENCE. keyUsage digitalSignature is bit 0: one byte, 0x80, seven bits unused.
        let extensions = constructed(
            EXTENSIONS_TAG,
            &[&constructed(
                SEQUENCE,
                &[
                    &extension(BASIC_CONSTRAINTS, true, &tlv(SEQUENCE, &[])),
                    &extension(KEY_USAGE, true, &tlv(BIT_STRING, &[0x07, 0x80])),
                    &extension(
                        EXT_KEY_USAGE,
                        false,
                        &constructed(SEQUENCE, &[&tlv(OID, SERVER_AUTH)]),
                    ),
                ],
            )],
        );
        constructed(
            SEQUENCE,
            &[
                &constructed(VERSION_TAG, &[&unsigned(&[2])]),
                &unsigned(self.serial),
                &signature_algorithm(),
                &name,
                &validity,
                &name,
                &public_key,
                &extensions,
            ],
        )
    }
}

/// The Certificate: `tbs`, the algorithm, and `signature` (an ASN.1 ECDSA signature).
pub(crate) fn certificate(tbs: &[u8], signature: &[u8]) -> Vec<u8> {
    constructed(SEQUENCE, &[tbs, &signature_algorithm(), &bits(signature)])
}

#[cfg(test)]
mod tests;
