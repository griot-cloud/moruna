//! Verifying tokens: base64url, the header, the signature, and 2C's conditions on the claims.

use super::*;
use crate::signer::{Signer, base64url as encode};
use alloc::{format, vec};

const NOW: u64 = 1_794_931_200;
const NODE: &str = "i-1";

fn bearer(token: &str) -> Authorization {
    Authorization::One(format!("Bearer {token}").into_bytes())
}

#[test]
fn base64url_round_trips_and_refuses_what_is_not_canonical() {
    for length in 0..40 {
        let bytes: Vec<u8> = (0..length).map(|i| (i * 37 + 11) as u8).collect();
        assert_eq!(base64url(encode(&bytes).as_bytes()).unwrap(), bytes);
    }
    assert_eq!(base64url(b"-_-_").unwrap(), [0xfb, 0xff, 0xbf]);
    assert!(base64url(b"A").is_none(), "one digit left over");
    assert!(base64url(b"AA==").is_none(), "padding");
    assert!(base64url(b"AB").is_none(), "unused bits set");
    assert!(base64url(b"AAB").is_none(), "unused bits set");
    assert!(base64url(b"+/AA").is_none(), "base64, not base64url");
    assert!(base64url(b"AA A").is_none());
}

#[test]
fn key_ids_are_the_first_16_hex_of_the_public_key_digest() {
    let key = TokenKey::new([7; 32]);
    let digest = digest::digest(&digest::SHA256, &[7; 32]);
    assert_eq!(key.id(), &hex(digest.as_ref())[..16]);
    assert_eq!(key.id().len(), 16);
}

#[test]
fn a_valid_token_grants_its_scope_under_the_key_that_signed_it() {
    let first = Signer::new();
    let second = Signer::new();
    let verifier = Verifier::new(vec![
        TokenKey::new(first.public()),
        TokenKey::new(second.public()),
    ]);
    let token = second.token(NODE, NOW + 60, &["observe", "admin"]);
    let grant = verifier.verify(&bearer(&token), NODE, NOW).unwrap();
    assert_eq!(grant.key, TokenKey::new(second.public()).id());
    assert_eq!(grant.scope, [Scope::Observe, Scope::Admin]);
    assert!(grant.covers(Scope::Submit));
    // The scheme is case-insensitive, and `typ` may be left out.
    let token = first.sign(
        r#"{"alg":"EdDSA"}"#,
        &format!(r#"{{"aud":"{NODE}","exp":{},"scope":[]}}"#, NOW + 3_600),
    );
    let lower = Authorization::One(format!("bearer {token}").into_bytes());
    let grant = verifier.verify(&lower, NODE, NOW).unwrap();
    assert_eq!(grant.key, TokenKey::new(first.public()).id());
    assert!(!grant.covers(Scope::Observe));
}

#[test]
fn each_failure_is_refused_with_its_reason() {
    let signer = Signer::new();
    let stranger = Signer::new();
    let verifier = Verifier::new(vec![TokenKey::new(signer.public())]);
    let refused = |authorization: Authorization| {
        verifier
            .verify(&authorization, NODE, NOW)
            .expect_err("refused")
    };
    let good = signer.token(NODE, NOW + 60, &["observe"]);

    assert_eq!(refused(Authorization::Missing), "the call carries no token");
    assert!(refused(Authorization::Many).contains("more than one"));
    assert!(refused(Authorization::One(good.clone().into_bytes())).contains("bearer"));
    assert!(refused(Authorization::One(format!("Basic {good}").into_bytes())).contains("bearer"));
    assert!(refused(Authorization::One(b"Bearer".to_vec())).contains("bearer"));

    // The signature: changed, from a key not in the list, or not base64url.
    let mut tampered = good.clone().into_bytes();
    let last = tampered.len() - 2;
    tampered[last] = if tampered[last] == b'A' { b'B' } else { b'A' };
    let tampered = String::from_utf8(tampered).unwrap();
    assert!(refused(bearer(&tampered)).contains("does not verify"));
    let foreign = stranger.token(NODE, NOW + 60, &["observe"]);
    assert!(refused(bearer(&foreign)).contains("does not verify"));
    assert!(refused(bearer(&format!("{good}!"))).contains("signature is malformed"));
    let (input, _) = good.rsplit_once('.').unwrap();
    let swapped = signer.token(NODE, NOW + 61, &["admin"]);
    let (_, other_signature) = swapped.rsplit_once('.').unwrap();
    assert!(refused(bearer(&format!("{input}.{other_signature}"))).contains("does not verify"));

    // The claims.
    assert_eq!(
        refused(bearer(&signer.token("i-2", NOW + 60, &["observe"]))),
        "the token is for another node"
    );
    assert_eq!(
        refused(bearer(&signer.token(NODE, NOW, &["observe"]))),
        "the token has expired"
    );
    assert_eq!(
        refused(bearer(&signer.token(NODE, NOW - 1, &["observe"]))),
        "the token has expired"
    );
    assert_eq!(
        refused(bearer(&signer.token(NODE, NOW + 3_601, &["observe"]))),
        "the token expires more than an hour from now"
    );
    let header = r#"{"alg":"EdDSA","typ":"JWT"}"#;
    for claims in [
        format!(
            r#"{{"aud":"{NODE}","exp":{},"scope":["observe"],"sub":"x"}}"#,
            NOW + 60
        ),
        format!(r#"{{"aud":"{NODE}","exp":{}}}"#, NOW + 60),
        format!(r#"{{"aud":"{NODE}","exp":{},"scope":["root"]}}"#, NOW + 60),
        format!(r#"{{"aud":"{NODE}","exp":-1,"scope":[]}}"#),
        String::from("[]"),
        String::from("not json"),
    ] {
        assert_eq!(
            refused(bearer(&signer.sign(header, &claims))),
            "the token's claims are malformed",
            "{claims}"
        );
    }

    // The header.
    for (header, reason) in [
        (r#"{"alg":"HS256","typ":"JWT"}"#, "algorithm"),
        (r#"{"alg":"none"}"#, "algorithm"),
        (r#"{"typ":"JWT"}"#, "algorithm"),
        (r#"{"alg":"EdDSA","typ":"JWS"}"#, "typ"),
        (r#"{"alg":"EdDSA","kid":"1"}"#, "other than alg and typ"),
        (r#"["EdDSA"]"#, "header is malformed"),
        ("{", "header is malformed"),
    ] {
        let token = signer.sign(
            header,
            &format!(r#"{{"aud":"{NODE}","exp":{},"scope":[]}}"#, NOW + 60),
        );
        assert!(refused(bearer(&token)).contains(reason), "{header}");
    }

    // The form.
    for token in ["", "a.b", "a.b.c.d", "!.b.c", "..", &format!("{good}.")] {
        assert!(!refused(bearer(token)).is_empty(), "{token}");
    }
}
