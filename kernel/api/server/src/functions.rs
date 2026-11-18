//! Functions (system design, section 3E): registration, the list, and the check a job's
//! functions get at `validate`.
//!
//! - **Registration** (`POST /v1/functions`, admin) takes a name, the `https://` address the image
//!   file is fetched from (an object store's private object as a pre-signed or SAS URL), the
//!   file's SHA-256 and an Ed25519 signature over the 32 bytes of that SHA-256. The signature must
//!   verify under one of node config's `functions.keys` ([`FunctionKey`]); then the entry is kept
//!   in `api`'s memory, at most [`FUNCTIONS_LIMIT`] of them. Nothing is fetched. Registering a
//!   name again replaces its entry.
//! - **The list** (`GET /v1/functions`, observe): every entry, by name.
//! - **A job names a function** by its registered name: in version 1 a `kernels` entry
//!   `{"kind": "function", "function": "<name>"}`, in version 2 a task of kind `function` whose
//!   settings name it. The engine's check answers those names only (its `functions`; the
//!   engine's own kinds are never among them), and each that is not registered is a refusal of
//!   the job ([`Registry::refusals`]).

use crate::{api::Answer, identity::hex, token::base64};
use alloc::{collections::BTreeMap, format, string::String, vec::Vec};
use moruna_api::{ErrorCode, Function, FunctionRegistration, Functions, Refusal};
use ring::{
    digest,
    signature::{ED25519, UnparsedPublicKey},
};

/// The most functions the node keeps registered: each takes a few hundred bytes of `api`'s
/// memory, which is bounded (2B).
pub const FUNCTIONS_LIMIT: usize = 256;
/// The longest address taken, in bytes: a pre-signed or SAS URL's query included.
pub const ADDRESS_LIMIT: usize = 2048;

/// One of node config's `functions.keys`: an Ed25519 public key that may sign function images.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionKey {
    public: [u8; 32],
    id: String,
}

impl FunctionKey {
    /// The key whose public bytes are `public`.
    pub fn new(public: [u8; 32]) -> Self {
        let mut id = hex(digest::digest(&digest::SHA256, &public).as_ref());
        id.truncate(16);
        Self { public, id }
    }

    /// The key's ID, as a registered function names the key that signed it: the first 16 hex
    /// characters of the SHA-256 of its public bytes.
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// Whether `name` may name a function: the rule for a kind's name (`moruna_job::is_kind_name`).
/// 1 to 64 characters, a lowercase letter first, then lowercase letters, digits, `_`, `-` and
/// `.`.
pub(crate) fn is_function_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 64
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.'))
}

/// Whether `address` is one an image may be fetched from: `https://<host>[/...]`, printable,
/// with no space. Only HTTPS for now (the PM, 2026-11-18): an object store's scheme would need
/// its credentials in `node-admission`, which fetches; a private object is given as a
/// pre-signed or SAS URL instead.
fn is_address(address: &str) -> bool {
    let Some(rest) = address.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    address.len() <= ADDRESS_LIMIT
        && !host.is_empty()
        && address.bytes().all(|b| b.is_ascii_graphic())
}

/// The 32 bytes `text` writes as 64 lowercase hexadecimal digits.
fn sha256_of(text: &str) -> Option<[u8; 32]> {
    let digits = text.as_bytes();
    if digits.len() != 64 {
        return None;
    }
    let value = |d: u8| match d {
        b'0'..=b'9' => Some(d - b'0'),
        b'a'..=b'f' => Some(d - b'a' + 10),
        _ => None,
    };
    let mut out = [0u8; 32];
    for (i, pair) in digits.chunks(2).enumerate() {
        out[i] = (value(pair[0])? << 4) | value(pair[1])?;
    }
    Some(out)
}

/// The registered functions, and the keys a registration's signature is checked against.
#[derive(Debug, Default)]
pub(crate) struct Registry {
    keys: Vec<FunctionKey>,
    entries: BTreeMap<String, Function>,
}

impl Registry {
    pub(crate) fn new(keys: Vec<FunctionKey>) -> Self {
        Self {
            keys,
            entries: BTreeMap::new(),
        }
    }

    /// `POST /v1/functions` with `body` at `now`: the entry, answered `201`, or why not.
    pub(crate) fn register(&mut self, body: &[u8], now: u64) -> Answer {
        match self.registration(body, now) {
            Ok(function) => {
                let mut answer = Answer::ok(serde_json::to_vec(&function));
                if answer.status == 200 {
                    answer.status = 201;
                }
                self.entries.insert(function.name.clone(), function);
                answer
            }
            Err((code, why)) => Answer::error(code, &why),
        }
    }

    /// The function `body` registers, checked, or the error and why.
    fn registration(&self, body: &[u8], now: u64) -> Result<Function, (ErrorCode, String)> {
        let invalid = |why: &str| (ErrorCode::Invalid, String::from(why));
        let given: FunctionRegistration = serde_json::from_slice(body).map_err(|_| {
            invalid(
                "the body is not a function's registration: name, address, sha256 and signature",
            )
        })?;
        if !is_function_name(&given.name) {
            return Err(invalid(
                "the name is not a function's: 1 to 64 characters, a lowercase letter first, then \
                 lowercase letters, digits, `_`, `-` or `.`",
            ));
        }
        if !is_address(&given.address) {
            return Err(invalid(
                "the address is where the image is fetched from: an https:// URL (an object \
                 store's private object as a pre-signed or SAS URL)",
            ));
        }
        let sha256 = sha256_of(&given.sha256).ok_or_else(|| {
            invalid("sha256 is the image file's SHA-256, as 64 lowercase hexadecimal digits")
        })?;
        let signature = base64(given.signature.as_bytes())
            .filter(|bytes| bytes.len() == 64)
            .ok_or_else(|| {
                invalid("the signature is an Ed25519 signature, 64 bytes in standard base64")
            })?;
        if self.keys.is_empty() {
            return Err(invalid(
                "the node takes no function images: node config has no functions.keys",
            ));
        }
        let key = self
            .keys
            .iter()
            .find(|key| {
                UnparsedPublicKey::new(&ED25519, key.public)
                    .verify(&sha256, &signature)
                    .is_ok()
            })
            .ok_or_else(|| {
                invalid(
                    "the signature does not verify under any of node config's functions.keys: it \
                     is made over the 32 bytes of the image's SHA-256",
                )
            })?;
        if !self.entries.contains_key(&given.name) && self.entries.len() >= FUNCTIONS_LIMIT {
            return Err((
                ErrorCode::Conflict,
                format!("the node holds {FUNCTIONS_LIMIT} functions, the most it keeps"),
            ));
        }
        Ok(Function {
            name: given.name,
            address: given.address,
            sha256: given.sha256,
            key: String::from(key.id()),
            registered: now,
        })
    }

    /// `GET /v1/functions`.
    pub(crate) fn list(&self) -> Functions {
        Functions {
            functions: self.entries.values().cloned().collect(),
        }
    }

    /// The function registered as `name`.
    #[cfg(test)]
    pub(crate) fn get(&self, name: &str) -> Option<&Function> {
        self.entries.get(name)
    }

    /// A refusal for each of `names`, the functions a job uses, that is not registered.
    pub(crate) fn refusals(&self, names: &[String]) -> Vec<Refusal> {
        names
            .iter()
            .filter(|name| !self.entries.contains_key(name.as_str()))
            .map(|name| Refusal {
                field: String::from("functions"),
                message: format!(
                    "the function `{name}` is not registered on this node: register its image \
                     with POST /v1/functions"
                ),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
