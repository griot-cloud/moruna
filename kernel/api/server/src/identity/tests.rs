//! The node's own certificate, checked by parsing its DER against 2B's template, and the
//! operator's identity from PEM.

use super::*;
use alloc::{format, string::ToString};
use ring::signature::{ECDSA_P256_SHA256_ASN1, UnparsedPublicKey};

/// The design's example clock, 2026-11-17T16:00:00Z.
const WALL: u64 = 1_794_931_200;

/// A DER reader for the tests: each value's tag, its content, and its whole encoding.
struct Der<'a>(&'a [u8]);

impl<'a> Der<'a> {
    fn next(&mut self) -> (u8, &'a [u8], &'a [u8]) {
        let tag = self.0[0];
        let first = self.0[1];
        let (length, header) = if first < 0x80 {
            (usize::from(first), 2)
        } else {
            let count = usize::from(first & 0x7f);
            assert!((1..=4).contains(&count), "length of length {count}");
            assert_ne!(self.0[2], 0, "a long length with a leading zero is not DER");
            let length = self.0[2..2 + count]
                .iter()
                .fold(0usize, |n, b| (n << 8) | usize::from(*b));
            assert!(length >= 0x80, "a long length under 128 is not DER");
            (length, 2 + count)
        };
        let raw = &self.0[..header + length];
        let content = &self.0[header..header + length];
        self.0 = &self.0[header + length..];
        (tag, content, raw)
    }

    fn expect(&mut self, tag: u8) -> &'a [u8] {
        let (found, content, _) = self.next();
        assert_eq!(found, tag, "tag");
        content
    }

    fn done(&self) {
        assert!(self.0.is_empty(), "{} bytes left over", self.0.len());
    }
}

/// The common name of a Name holding exactly one: `CN=<name>`, a UTF8String.
fn common_name(name: &[u8]) -> &str {
    let mut name = Der(name);
    let mut set = Der(name.expect(0x31));
    name.done();
    let mut attribute = Der(set.expect(0x30));
    set.done();
    assert_eq!(attribute.expect(0x06), [0x55, 0x04, 0x03], "commonName");
    let value = attribute.expect(0x0c);
    attribute.done();
    core::str::from_utf8(value).unwrap()
}

/// Each extension: its OID, whether it is critical, and its value.
fn extensions(content: &[u8]) -> Vec<(&[u8], bool, &[u8])> {
    let mut outer = Der(content);
    let mut list = Der(outer.expect(0x30));
    outer.done();
    let mut found = Vec::new();
    while !list.0.is_empty() {
        let mut extension = Der(list.expect(0x30));
        let oid = extension.expect(0x06);
        let (tag, content, _) = extension.next();
        let (critical, value) = if tag == 0x01 {
            assert_eq!(content, [0xff], "a critical flag is TRUE when present");
            (true, extension.expect(0x04))
        } else {
            assert_eq!(tag, 0x04);
            (false, content)
        };
        extension.done();
        found.push((oid, critical, value));
    }
    found
}

#[test]
fn the_node_certificate_follows_the_template() {
    let identity = Identity::generate("i-0123456789abcdef", WALL).unwrap();
    let mut outer = Der(identity.certificate());
    let mut certificate = Der(outer.expect(0x30));
    outer.done();
    let (tag, tbs_content, tbs) = certificate.next();
    assert_eq!(tag, 0x30);
    let algorithm = certificate.expect(0x30);
    let signature = certificate.expect(0x03);
    certificate.done();
    assert_eq!(
        algorithm,
        [0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02]
    );

    let mut fields = Der(tbs_content);
    assert_eq!(fields.expect(0xa0), [0x02, 0x01, 0x02], "version 3");
    let serial = fields.expect(0x02);
    assert!(
        serial.len() <= 17 && serial[0] & 0x80 == 0,
        "a positive serial of at most 16 bytes"
    );
    assert!(serial.len() < 17 || serial[0] == 0);
    assert!(
        serial.len() == 1 || serial[0] != 0 || serial[1] & 0x80 != 0,
        "minimal"
    );
    assert_eq!(
        fields.expect(0x30),
        algorithm,
        "the signature algorithm, twice"
    );
    assert_eq!(
        common_name(fields.expect(0x30)),
        "i-0123456789abcdef",
        "issuer"
    );
    let mut validity = Der(fields.expect(0x30));
    assert_eq!(
        validity.expect(0x17),
        b"261117150000Z",
        "an hour before the clock"
    );
    assert_eq!(validity.expect(0x17), b"271219160000Z", "397 days after it");
    validity.done();
    assert_eq!(
        common_name(fields.expect(0x30)),
        "i-0123456789abcdef",
        "subject"
    );
    let mut key_info = Der(fields.expect(0x30));
    let mut key_algorithm = Der(key_info.expect(0x30));
    assert_eq!(
        key_algorithm.expect(0x06),
        [0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01]
    );
    assert_eq!(
        key_algorithm.expect(0x06),
        [0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07]
    );
    key_algorithm.done();
    let key = key_info.expect(0x03);
    key_info.done();
    assert_eq!(key.len(), 66, "an uncompressed P-256 point");
    assert_eq!(&key[..2], [0x00, 0x04]);
    let found = extensions(fields.expect(0xa3));
    fields.done();
    assert_eq!(
        found,
        [
            (&[0x55, 0x1d, 0x13][..], true, &[0x30, 0x00][..]),
            (&[0x55, 0x1d, 0x0f][..], true, &[0x03, 0x02, 0x07, 0x80][..]),
            (
                &[0x55, 0x1d, 0x25][..],
                false,
                &[
                    0x30, 0x0a, 0x06, 0x08, 0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01
                ][..]
            ),
        ],
        "basicConstraints CA false, keyUsage digitalSignature, extendedKeyUsage serverAuth"
    );

    assert_eq!(signature[0], 0, "no unused bits");
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, &key[1..])
        .verify(tbs, &signature[1..])
        .expect("self-signed by its own key");

    let digest = digest::digest(&digest::SHA256, identity.certificate());
    assert_eq!(identity.fingerprint(), hex(digest.as_ref()));
    assert_eq!(identity.fingerprint().len(), 64);
    assert!(
        identity
            .fingerprint()
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    );
}

#[test]
fn each_node_certificate_is_new() {
    let a = Identity::generate("node", WALL).unwrap();
    let b = Identity::generate("node", WALL).unwrap();
    assert_ne!(a.certificate(), b.certificate());
    assert_ne!(a.fingerprint(), b.fingerprint());
}

#[test]
fn validity_saturates_at_the_ends_of_time() {
    let early = Identity::generate("node", 10).unwrap();
    let late = Identity::generate("node", u64::MAX).unwrap();
    let validity = |identity: &Identity| {
        let mut outer = Der(identity.certificate());
        let mut certificate = Der(outer.expect(0x30));
        let mut fields = Der(certificate.expect(0x30));
        for _ in 0..4 {
            fields.next();
        }
        let mut validity = Der(fields.expect(0x30));
        let (_, from, _) = validity.next();
        let (_, to, _) = validity.next();
        (from.to_vec(), to.to_vec())
    };
    assert_eq!(validity(&early).0, b"700101000000Z");
    assert_eq!(validity(&late).1, b"99991231235959Z");
}

/// `der` as PEM, with the label `label`.
fn pem(label: &str, der: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut text = String::new();
    for chunk in der.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                text.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                text.push('=');
            }
        }
    }
    let mut out = format!("-----BEGIN {label}-----\n");
    for line in text.as_bytes().chunks(64) {
        out.push_str(core::str::from_utf8(line).unwrap());
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

/// A key pair and a certificate for it, made as `generate` makes them, as PEM.
fn operator(name: &str) -> (String, String) {
    let random = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &random).unwrap();
    let pair =
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &random).unwrap();
    let tbs = Template {
        serial: &[1, 2, 3],
        common_name: name,
        not_before: WALL,
        not_after: WALL + 86_400,
        public_key: pair.public_key().as_ref(),
    }
    .to_be_signed();
    let signature = pair.sign(&random, &tbs).unwrap();
    (
        pem("CERTIFICATE", &certificate(&tbs, signature.as_ref())),
        pem("PRIVATE KEY", pkcs8.as_ref()),
    )
}

#[test]
fn the_operator_identity_comes_from_pem() {
    let (chain, key) = operator("api.example.com");
    let identity = Identity::from_pem(&chain, &key).unwrap();
    let mut outer = Der(identity.certificate());
    let mut certificate = Der(outer.expect(0x30));
    let mut fields = Der(certificate.expect(0x30));
    for _ in 0..5 {
        fields.next();
    }
    assert_eq!(common_name(fields.expect(0x30)), "api.example.com");
    let digest = digest::digest(&digest::SHA256, identity.certificate());
    assert_eq!(identity.fingerprint(), hex(digest.as_ref()));

    // A chain: the server's certificate first, then the rest.
    let (other, _) = operator("ca.example.com");
    let identity = Identity::from_pem(&(chain.clone() + &other), &key).unwrap();
    assert_eq!(identity.key.cert.len(), 2);
}

#[test]
fn a_bad_operator_identity_is_refused() {
    let (chain, key) = operator("api.example.com");
    let (other_chain, other_key) = operator("other.example.com");
    assert_eq!(
        Identity::from_pem("", &key).unwrap_err(),
        IdentityError::Certificate
    );
    assert_eq!(
        Identity::from_pem(&key, &key).unwrap_err(),
        IdentityError::Certificate,
        "a key where the chain belongs"
    );
    assert_eq!(
        Identity::from_pem(
            "-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----\n",
            &key
        )
        .unwrap_err(),
        IdentityError::Certificate
    );
    assert_eq!(
        Identity::from_pem(&pem("CERTIFICATE", b"not a certificate"), &key).unwrap_err(),
        IdentityError::Certificate,
        "a certificate section that is not X.509"
    );
    assert_eq!(
        Identity::from_pem(&chain, "").unwrap_err(),
        IdentityError::Key
    );
    assert_eq!(
        Identity::from_pem(&chain, &pem("PRIVATE KEY", b"not a key")).unwrap_err(),
        IdentityError::Key
    );
    assert_eq!(
        Identity::from_pem(&chain, &other_key).unwrap_err(),
        IdentityError::Mismatch
    );
    assert_eq!(
        Identity::from_pem(&other_chain, &key).unwrap_err(),
        IdentityError::Mismatch
    );
}

#[test]
fn errors_say_what_failed() {
    for error in [
        IdentityError::Random,
        IdentityError::Certificate,
        IdentityError::Key,
        IdentityError::Mismatch,
    ] {
        assert!(!error.to_string().is_empty());
    }
    assert_eq!(hex(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
}
