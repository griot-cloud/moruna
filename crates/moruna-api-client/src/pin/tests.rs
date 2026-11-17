//! Fingerprints, and the verifier's decision on its own. The verifier inside a handshake is
//! tested against a node in `tests/clients.rs`.

use super::*;

/// SHA-256 of `abc` (FIPS 180-2, appendix B.1).
const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

#[test]
fn a_fingerprint_is_the_sha256_of_the_der() {
    assert_eq!(Fingerprint::of(b"abc").to_string(), ABC);
    assert_eq!(Fingerprint::parse(ABC).unwrap(), Fingerprint::of(b"abc"));
    assert_eq!(Fingerprint::of(b"abc").as_bytes()[0], 0xba);
    assert_eq!(
        format!("{:?}", Fingerprint::of(b"abc")),
        format!("Fingerprint({ABC})")
    );
}

#[test]
fn a_fingerprint_is_read_in_either_case() {
    let upper = ABC.to_ascii_uppercase();
    assert_eq!(Fingerprint::parse(&upper).unwrap().to_string(), ABC);
}

#[test]
fn anything_but_64_hex_digits_is_refused() {
    let plus = format!("+{}", &ABC[1..]);
    let wide = format!("{}é", &ABC[..62]);
    for bad in [
        "",
        &ABC[1..],
        &format!("{ABC}0"),
        &ABC.replace('a', "g"),
        &plus,
        &wide,
    ] {
        assert!(
            matches!(Fingerprint::parse(bad), Err(Error::Pin(given)) if given == bad),
            "{bad:?} was accepted"
        );
    }
}

fn verifier(pin: Fingerprint) -> PinVerifier {
    PinVerifier {
        pin,
        algorithms: rustls::crypto::ring::default_provider().signature_verification_algorithms,
    }
}

fn verify(verifier: &PinVerifier, der: &[u8]) -> Result<ServerCertVerified, rustls::Error> {
    verifier.verify_server_cert(
        &CertificateDer::from(der.to_vec()),
        &[],
        &ServerName::try_from("node.example").unwrap(),
        &[],
        UnixTime::now(),
    )
}

#[test]
fn the_pinned_certificate_is_accepted_whatever_it_holds() {
    // Not even DER: nothing but the fingerprint is checked.
    assert!(verify(&verifier(Fingerprint::of(b"abc")), b"abc").is_ok());
}

#[test]
fn another_certificate_is_refused_with_its_fingerprint() {
    let refused = verify(&verifier(Fingerprint::of(b"abd")), b"abc").unwrap_err();
    assert_eq!(found_in(&refused), Some(Fingerprint::of(b"abc")));
    assert_eq!(
        Mismatch(Fingerprint::of(b"abc")).to_string(),
        format!("the certificate's SHA-256 is {ABC}, not the pin")
    );
}

#[test]
fn the_refusal_is_found_under_the_errors_that_carry_it() {
    let refused = verify(&verifier(Fingerprint::of(b"abd")), b"abc").unwrap_err();
    let io = std::io::Error::new(std::io::ErrorKind::InvalidData, refused);
    assert_eq!(mismatch(&io), Some(Fingerprint::of(b"abc")));
    let bare = Mismatch(Fingerprint::of(b"abc"));
    assert_eq!(mismatch(&bare), Some(Fingerprint::of(b"abc")));
}

#[test]
fn other_failures_are_not_the_pin() {
    let unknown = rustls::Error::InvalidCertificate(CertificateError::UnknownIssuer);
    assert_eq!(mismatch(&unknown), None);
    let other = rustls::Error::InvalidCertificate(CertificateError::Other(OtherError(Arc::new(
        std::io::Error::other("not the pin"),
    ))));
    assert_eq!(mismatch(&other), None);
    let io = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
    assert_eq!(mismatch(&io), None);
    assert_eq!(mismatch(&std::io::Error::from_raw_os_error(61)), None);
}

#[test]
fn the_verifier_offers_the_provider_schemes() {
    let schemes = verifier(Fingerprint::of(b"abc")).supported_verify_schemes();
    assert!(schemes.contains(&SignatureScheme::ECDSA_NISTP256_SHA256));
    assert!(schemes.contains(&SignatureScheme::ED25519));
}

#[test]
fn a_pinned_config_is_rustls_defaults_with_the_verifier() {
    let config = config(Fingerprint::of(b"abc")).unwrap();
    assert!(config.alpn_protocols.is_empty());
}
