//! Tokens, as the tests make their own (item 2.5, "Not yours: issuing tokens"): a JWT in compact
//! form, header `{"alg":"EdDSA","typ":"JWT"}`, signed with Ed25519, whose claims are `moruna-api`'s
//! `Claims` (system design 2C, item 2.3).

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use moruna_api::{Claims, Scope};
use ring::rand::SystemRandom;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};

/// The header every token carries.
const HEADER: &str = r#"{"alg":"EdDSA","typ":"JWT"}"#;

/// A token key: an Ed25519 key pair whose public half is one of the node's `token_keys`.
pub struct Key(Ed25519KeyPair);

impl Key {
    /// A new key pair.
    pub fn generate() -> Self {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).expect("an Ed25519 key");
        Self(Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("the key just made"))
    }

    /// The public key's 32 bytes, as node config's `token_keys` holds them (base64 there).
    pub fn public(&self) -> Vec<u8> {
        self.0.public_key().as_ref().to_vec()
    }

    /// A token for the node `aud`, with this scope, expiring `ttl` seconds from now.
    pub fn token(&self, aud: &str, scope: &[Scope], ttl: u64) -> String {
        let claims = Claims {
            aud: aud.to_string(),
            exp: super::now() + ttl,
            scope: scope.to_vec(),
        };
        let payload = serde_json::to_vec(&claims).expect("claims serialize");
        let signed = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(HEADER),
            URL_SAFE_NO_PAD.encode(payload)
        );
        let signature = self.0.sign(signed.as_bytes());
        format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature.as_ref()))
    }
}

/// The claims of `token` when it is a well-formed EdDSA JWT whose signature verifies under one of
/// `keys`, with the index of that key.
pub fn verify(token: &str, keys: &[Vec<u8>]) -> Option<(usize, Claims)> {
    let mut parts = token.split('.');
    let (header, payload, signature) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let header: serde_json::Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(header).ok()?).ok()?;
    if header.get("alg")?.as_str()? != "EdDSA" {
        return None;
    }
    let signature = URL_SAFE_NO_PAD.decode(signature).ok()?;
    let signed = &token[..token.rfind('.')?];
    let index = keys.iter().position(|key| {
        UnparsedPublicKey::new(&ED25519, key)
            .verify(signed.as_bytes(), &signature)
            .is_ok()
    })?;
    let claims = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
    Some((index, claims))
}
