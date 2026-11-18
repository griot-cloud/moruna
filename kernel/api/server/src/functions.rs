//! Functions (system design, section 3E): registration, the list, and the check a job's
//! functions get at `validate`.
//!
//! - **Registration** (`POST /v1/functions`, admin) takes a name, the `https://` address the image
//!   file is fetched from (an object store's private object as a pre-signed or SAS URL), the
//!   file's SHA-256 and an Ed25519 signature over the 32 bytes of that SHA-256. The signature must
//!   verify under one of node config's `functions.keys` ([`FunctionKey`]); then the entry is kept
//!   in `api`'s memory, at most [`FUNCTIONS_LIMIT`] of them. Nothing is fetched. Registering a
//!   name again replaces its entry.
//! - **The list** (`GET /v1/functions`, observe): every entry, by name. An address's query is a
//!   credential (a SAS, a pre-signed URL's signature): the list and the registration's answer
//!   show each address as [`moruna_api::address_shown`] does, `?…` in its place, and only
//!   `node-admission`'s fetch is given it whole.
//! - **A job names a function** by its registered name: in version 1 a `kernels` entry
//!   `{"kind": "function", "function": "<name>"}`, in version 2 a task of kind `function` whose
//!   settings name it. The engine's check answers those names only (its `functions`; the
//!   engine's own kinds are never among them), and each that is not registered is a refusal of
//!   the job ([`Registry::refusals`]).

use crate::{api::Answer, identity::hex, token::base64};
use alloc::{collections::BTreeMap, format, string::String, vec::Vec};
use moruna_api::{
    ErrorCode, Function, FunctionRegistration, Functions, Measured, MeasuredBy, Refusal,
};
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

/// The most readings the node keeps of a function's image: its newest.
pub const READINGS_KEPT: usize = 64;

/// What the node keeps of a function's image beside its entry (system design 3A): its client's
/// measurement, as the registration carried it, and the node's own readings, each instance's
/// account peak beside the largest piece it was given, newest last.
#[derive(Clone, Debug, Default, PartialEq)]
struct Readings {
    /// The image's SHA-256, as the entry has it: readings are of an image, not of a name.
    sha256: String,
    client: Option<Measured>,
    /// `(largest piece, account peak)`, in bytes, at most [`READINGS_KEPT`].
    pairs: Vec<(u64, u64)>,
}

/// The registered functions, and the keys a registration's signature is checked against.
#[derive(Debug, Default)]
pub(crate) struct Registry {
    keys: Vec<FunctionKey>,
    entries: BTreeMap<String, Function>,
    readings: BTreeMap<String, Readings>,
}

impl Registry {
    pub(crate) fn new(keys: Vec<FunctionKey>) -> Self {
        Self {
            keys,
            entries: BTreeMap::new(),
            readings: BTreeMap::new(),
        }
    }

    /// `POST /v1/functions` with `body` at `now`: the entry, answered `201` with its address as
    /// the node shows it ([`moruna_api::address_shown`]), or why not. The entry keeps the whole
    /// address, which only `node-admission`'s fetch is given ([`Registry::registered`]).
    pub(crate) fn register(&mut self, body: &[u8], now: u64) -> Answer {
        match self.registration(body, now) {
            Ok(mut function) => {
                // What the node read of the image stays with it; a client's newer measurement
                // gives the readings its slope (system design 3A).
                let readings = self.readings.entry(function.name.clone()).or_default();
                if readings.sha256 != function.sha256 {
                    *readings = Readings {
                        sha256: function.sha256.clone(),
                        ..Readings::default()
                    };
                }
                if function.measured_by == Some(MeasuredBy::Registration) {
                    readings.client = function.measured.clone();
                }
                if let Some(node) =
                    Measured::from_readings(&readings.pairs, readings.client.as_ref())
                {
                    function.measured = Some(node);
                    function.measured_by = Some(MeasuredBy::Node);
                }
                let mut answer = Answer::ok(serde_json::to_vec(&function.shown()));
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
        if let Some(why) = given.measured.as_ref().and_then(Measured::refusal) {
            return Err((
                ErrorCode::Invalid,
                format!("measured is not a measurement of the function's memory: {why}"),
            ));
        }
        if !self.entries.contains_key(&given.name) && self.entries.len() >= FUNCTIONS_LIMIT {
            return Err((
                ErrorCode::Conflict,
                format!("the node holds {FUNCTIONS_LIMIT} functions, the most it keeps"),
            ));
        }
        // A measurement is of the image (system design 3A): the same image registered again
        // with none keeps the one it had, and another image starts with what it brings.
        let (measured, measured_by) = match given.measured {
            Some(measured) => (Some(measured), Some(MeasuredBy::Registration)),
            None => self
                .entries
                .get(&given.name)
                .filter(|kept| kept.sha256 == given.sha256)
                .map_or((None, None), |kept| {
                    (kept.measured.clone(), kept.measured_by)
                }),
        };
        Ok(Function {
            name: given.name,
            address: given.address,
            sha256: given.sha256,
            key: String::from(key.id()),
            registered: now,
            measured,
            measured_by,
        })
    }

    /// `GET /v1/functions`: each entry with its address as the node shows it
    /// ([`moruna_api::address_shown`]), never its query (3E).
    pub(crate) fn list(&self) -> Functions {
        Functions {
            functions: self.entries.values().map(Function::shown).collect(),
        }
    }

    /// The function registered as `name`.
    #[cfg(test)]
    pub(crate) fn get(&self, name: &str) -> Option<&Function> {
        self.entries.get(name)
    }

    /// The registration of each of `names`, the functions a job uses, in that order: those
    /// registered, which are all of them for a job the server starts (item 3.6).
    pub(crate) fn registered(&self, names: &[String]) -> Vec<Function> {
        names
            .iter()
            .filter_map(|name| self.entries.get(name.as_str()).cloned())
            .collect()
    }

    /// What a job's run read of the function `name` whose image was `sha256` when the job
    /// started (system design 3A): each instance's largest piece and account peak. The node
    /// keeps the newest [`READINGS_KEPT`] of the image's readings and measures the function
    /// from them ([`Measured::from_readings`]), its measurement then the node's. Readings of an image the
    /// name no longer has, or of a name no longer registered, are of nothing kept.
    pub(crate) fn observe(&mut self, name: &str, sha256: &str, pairs: &[(u64, u64)]) {
        let (Some(function), Some(readings)) =
            (self.entries.get_mut(name), self.readings.get_mut(name))
        else {
            return;
        };
        if function.sha256 != sha256 || readings.sha256 != sha256 || pairs.is_empty() {
            return;
        }
        readings
            .pairs
            .extend(pairs.iter().copied().filter(|(x, _)| *x > 0));
        let excess = readings.pairs.len().saturating_sub(READINGS_KEPT);
        readings.pairs.drain(..excess);
        if let Some(node) = Measured::from_readings(&readings.pairs, readings.client.as_ref()) {
            function.measured = Some(node);
            function.measured_by = Some(MeasuredBy::Node);
        }
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
