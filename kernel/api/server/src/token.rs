//! Tokens (system design, section 2C): a JWT in compact form, signed with Ed25519, sent as
//! `Authorization: Bearer <token>`. A call is allowed when the signature verifies under one of
//! the node's token keys, `aud` is the node's instance ID, `exp` is in the future and at most an
//! hour ahead, and the scope covers the operation.

use crate::{http::Authorization, identity::hex};
use alloc::{string::String, vec::Vec};
use moruna_api::{Claims, Scope};
use ring::{
    digest,
    signature::{ED25519, UnparsedPublicKey},
};
use serde_json::Value;

/// How far ahead a token's `exp` may be, in seconds: one hour.
pub const MOST_AHEAD_SECONDS: u64 = 3_600;

/// One of the node's token keys: an Ed25519 public key from node config's `token_keys`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenKey {
    public: [u8; 32],
    id: String,
}

impl TokenKey {
    /// The key whose public bytes are `public`.
    pub fn new(public: [u8; 32]) -> Self {
        let digest = digest::digest(&digest::SHA256, &public);
        let mut id = hex(digest.as_ref());
        id.truncate(16);
        Self { public, id }
    }

    /// The key's ID in the audit record: the first 16 hex characters of the SHA-256 of its
    /// public bytes.
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// What a verified token grants: the key that signed it and its scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Grant {
    /// The ID of the key that verified the token.
    pub key: String,
    /// The token's scope.
    pub scope: Vec<Scope>,
}

impl Grant {
    /// Whether the token's scope covers `needed`.
    pub(crate) fn covers(&self, needed: Scope) -> bool {
        self.scope.iter().any(|scope| scope.covers(needed))
    }
}

/// The value of one base64url digit, without padding (RFC 7515, section 2).
fn digit(byte: u8) -> Option<u32> {
    Some(u32::from(match byte {
        b'A'..=b'Z' => byte - b'A',
        b'a'..=b'z' => byte - b'a' + 26,
        b'0'..=b'9' => byte - b'0' + 52,
        b'-' => 62,
        b'_' => 63,
        _ => return None,
    }))
}

/// `text` decoded from base64url without padding, refusing any other alphabet, padding, and a
/// final digit whose unused bits are not zero.
pub(crate) fn base64url(text: &[u8]) -> Option<Vec<u8>> {
    if text.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3 + 2);
    for chunk in text.chunks(4) {
        let mut bits = 0u32;
        for (index, byte) in chunk.iter().enumerate() {
            bits |= digit(*byte)? << (18 - 6 * index);
        }
        let bytes = bits.to_be_bytes();
        let whole = chunk.len() - 1;
        out.extend_from_slice(&bytes[1..1 + whole]);
        // The bits below the last whole byte must be zero.
        if bits & (0x00ff_ffff >> (8 * whole)) != 0 {
            return None;
        }
    }
    Some(out)
}

/// Checks tokens against the node's keys and instance ID.
#[derive(Clone, Debug)]
pub(crate) struct Verifier {
    keys: Vec<TokenKey>,
}

impl Verifier {
    pub(crate) fn new(keys: Vec<TokenKey>) -> Self {
        Self { keys }
    }

    /// What the token in `authorization` grants on the node `instance` at `now`, Unix seconds,
    /// or why it is refused. Every refusal is answered `unauthorized`.
    pub(crate) fn verify(
        &self,
        authorization: &Authorization,
        instance: &str,
        now: u64,
    ) -> Result<Grant, &'static str> {
        let value = match authorization {
            Authorization::Missing => return Err("the call carries no token"),
            Authorization::Many => {
                return Err("the call carries more than one Authorization header");
            }
            Authorization::One(value) => value,
        };
        let token = match value.split_at_checked(7) {
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case(b"bearer ") => rest.trim_ascii(),
            _ => return Err("the Authorization header does not carry a bearer token"),
        };
        let mut parts = token.split(|b| *b == b'.');
        let (Some(header), Some(payload), Some(signature), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err("the token is not a JWT in compact form");
        };
        let header: Value = base64url(header)
            .and_then(|json| serde_json::from_slice(&json).ok())
            .ok_or("the token's header is malformed")?;
        let header = header
            .as_object()
            .ok_or("the token's header is malformed")?;
        if header.keys().any(|name| name != "alg" && name != "typ") {
            return Err("the token's header has a field other than alg and typ");
        }
        if header
            .get("typ")
            .is_some_and(|typ| typ.as_str() != Some("JWT"))
        {
            return Err("the token's typ is not JWT");
        }
        if header.get("alg").and_then(Value::as_str) != Some("EdDSA") {
            return Err("the token's algorithm is not EdDSA");
        }
        let signature = base64url(signature).ok_or("the token's signature is malformed")?;
        let signed = &token[..header_and_payload_length(token)];
        let key = self
            .keys
            .iter()
            .find(|key| {
                UnparsedPublicKey::new(&ED25519, &key.public)
                    .verify(signed, &signature)
                    .is_ok()
            })
            .ok_or("the token's signature does not verify under any of the node's keys")?;
        let claims: Claims = base64url(payload)
            .and_then(|json| serde_json::from_slice(&json).ok())
            .ok_or("the token's claims are malformed")?;
        if claims.aud != instance {
            return Err("the token is for another node");
        }
        if claims.exp <= now {
            return Err("the token has expired");
        }
        if claims.exp - now > MOST_AHEAD_SECONDS {
            return Err("the token expires more than an hour from now");
        }
        Ok(Grant {
            key: key.id.clone(),
            scope: claims.scope,
        })
    }
}

/// The length of a compact token's signing input: its header and payload and the dot between.
fn header_and_payload_length(token: &[u8]) -> usize {
    token
        .iter()
        .rposition(|b| *b == b'.')
        .unwrap_or(token.len())
}

#[cfg(test)]
mod tests;
