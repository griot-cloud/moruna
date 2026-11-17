//! Node config version 1 (system design, section 1D; work item 1.4, step 5): the instance's
//! user-data, and every reason it is refused.

use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use core::fmt;

use crate::{
    base64,
    json::{self, Json},
    pem::{self, PemError},
};

/// The version this node reads: the value of the field `moruna`.
pub const VERSION: u64 = 1;
/// The most keys `api.token_keys` or `functions.keys` holds.
pub const KEYS_MAX: usize = 8;
/// One MiB: `functions.cache_bytes` is a whole number of them.
pub const MIB: u64 = 1 << 20;
/// `functions.cache_bytes` when node config does not give it: 1 GiB, or half the node's memory
/// when that is less.
pub const CACHE_BYTES_DEFAULT: u64 = 1 << 30;
/// The least `functions.cache_bytes` may be: 64 MiB.
pub const CACHE_BYTES_MIN: u64 = 64 * MIB;

/// An Ed25519 public key: its 32 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublicKey(pub [u8; 32]);

impl PublicKey {
    /// The key `text` writes: the standard base64 of exactly 32 bytes.
    pub fn from_base64(text: &str) -> Option<PublicKey> {
        let bytes = base64::decode(text.as_bytes())?;
        Some(PublicKey(bytes.try_into().ok()?))
    }

    /// The key as node config writes it.
    pub fn to_base64(&self) -> String {
        base64::encode(&self.0)
    }
}

/// A validated node config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeConfig {
    /// What `api` needs.
    pub api: Api,
    /// What the node needs to take function images; the defaults when node config has no
    /// `functions`.
    pub functions: Functions,
}

/// Node config's `api`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Api {
    /// The keys that verify API tokens: 1 to 8.
    pub token_keys: Vec<PublicKey>,
    /// The operator's TLS identity. Without it `api` makes its own at each boot.
    pub tls: Option<Tls>,
}

/// Node config's `api.tls`, checked only as far as its PEM armour: `api` checks what the blocks
/// hold.
#[derive(Clone, PartialEq, Eq)]
pub struct Tls {
    /// One or more PEM `CERTIFICATE` blocks.
    pub certificate: String,
    /// One PEM `PRIVATE KEY` block.
    pub key: String,
}

impl fmt::Debug for Tls {
    /// The key is a secret, so it is never printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tls")
            .field("certificate", &self.certificate)
            .field("key", &"<not shown>")
            .finish()
    }
}

/// Node config's `functions`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Functions {
    /// The keys that may sign function images: 0 to 8.
    pub keys: Vec<PublicKey>,
    /// The bytes of memory the function cache may hold.
    pub cache_bytes: u64,
}

impl Default for Functions {
    fn default() -> Functions {
        Functions {
            keys: Vec::new(),
            cache_bytes: CACHE_BYTES_DEFAULT,
        }
    }
}

/// Why a node config is refused. Its `Display` is the reason of the console's
/// `moruna: fatal config: <reason>` line, always one line of printable ASCII.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The instance has no user-data, or it is empty.
    Missing,
    /// The cloud gives user-data as base64 (Azure), and this is not base64.
    NotBase64,
    /// The user-data is not JSON; the reader's message.
    NotJson(String),
    /// An object names a field twice; the reader's message.
    Twice(String),
    /// The named value is not an object.
    NotObject(&'static str),
    /// The object `within` has a field node config does not define.
    Unknown {
        /// The object.
        within: &'static str,
        /// The field's name, as far as it is printable.
        field: String,
    },
    /// The named field is not there.
    Absent(&'static str),
    /// The named field is not a string.
    NotString(&'static str),
    /// The named field is not a list of strings.
    NotStrings(&'static str),
    /// The named field is not a whole number.
    NotNumber(&'static str),
    /// The field `moruna` is not there.
    NoVersion,
    /// The field `moruna` is not 1; what it is.
    Version(String),
    /// A list of keys holds too few or too many.
    KeyCount {
        /// The list.
        path: &'static str,
        /// How many it holds.
        count: usize,
        /// The fewest it may hold.
        least: usize,
    },
    /// A key is not the standard base64 of 32 bytes.
    Key {
        /// The list.
        path: &'static str,
        /// The key's place in it, from 0.
        index: usize,
    },
    /// A TLS field is not a run of PEM blocks.
    Pem {
        /// The field.
        path: &'static str,
        /// What is wrong with it.
        error: PemError,
    },
    /// A TLS field holds PEM blocks, and they are not the ones it must hold.
    Blocks {
        /// The field.
        path: &'static str,
        /// What it must hold.
        wanted: &'static str,
    },
    /// `functions.cache_bytes` is not a whole number of MiB.
    CacheNotMib(u64),
    /// `functions.cache_bytes` is under 64 MiB.
    CacheTooSmall(u64),
    /// `functions.cache_bytes` is over half the node's memory.
    CacheTooLarge {
        /// What node config gives.
        given: u64,
        /// Half the node's memory.
        most: u64,
    },
    /// `functions.cache_bytes` is given, and the node's memory is not known, so its upper bound
    /// cannot be checked.
    CacheUnchecked,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Missing => f.write_str("the instance has no user-data"),
            Refusal::NotBase64 => f.write_str("the user-data is not base64"),
            Refusal::NotJson(message) => write!(f, "not JSON: {message}"),
            Refusal::Twice(message) => f.write_str(message),
            Refusal::NotObject(within) => write!(f, "{within} is not an object"),
            Refusal::Unknown { within, field } => {
                write!(f, "unknown field `{field}` in {within}")
            }
            Refusal::Absent(path) => write!(f, "`{path}` is missing"),
            Refusal::NotString(path) => write!(f, "`{path}` is not a string"),
            Refusal::NotStrings(path) => write!(f, "`{path}` is not a list of strings"),
            Refusal::NotNumber(path) => write!(f, "`{path}` is not a whole number"),
            Refusal::NoVersion => f.write_str("`moruna` is missing: node config says its version"),
            Refusal::Version(found) => {
                write!(f, "`moruna` is {found}: this node reads version {VERSION}")
            }
            Refusal::KeyCount { path, count, least } => {
                write!(
                    f,
                    "`{path}` holds {count} keys: {least} to {KEYS_MAX} are allowed"
                )
            }
            Refusal::Key { path, index } => write!(
                f,
                "`{path}[{index}]` is not the standard base64 of a 32-byte Ed25519 public key"
            ),
            Refusal::Pem { path, error } => {
                let what = match error {
                    PemError::Outside => "holds text outside its PEM blocks",
                    PemError::Unfinished => "holds a PEM block that does not end",
                    PemError::Body => "holds a PEM block whose body is not base64",
                };
                write!(f, "`{path}` {what}")
            }
            Refusal::Blocks { path, wanted } => write!(f, "`{path}` does not hold {wanted}"),
            Refusal::CacheNotMib(given) => write!(
                f,
                "`functions.cache_bytes` is {given}: it is a whole number of MiB"
            ),
            Refusal::CacheTooSmall(given) => write!(
                f,
                "`functions.cache_bytes` is {given}: the least is {CACHE_BYTES_MIN} (64 MiB)"
            ),
            Refusal::CacheTooLarge { given, most } => write!(
                f,
                "`functions.cache_bytes` is {given}: the most is {most}, half the node's memory"
            ),
            Refusal::CacheUnchecked => f.write_str(
                "`functions.cache_bytes` cannot be checked: the node's memory is not known",
            ),
        }
    }
}

/// How `functions.cache_bytes`' upper bound is checked.
#[derive(Clone, Copy)]
pub(crate) enum Bound {
    /// Against half the node's memory, when it is known.
    Node(Option<u64>),
    /// Not again: `platform` checked it before it handed the config over.
    Checked,
}

impl NodeConfig {
    /// Validates an instance's user-data as node config version 1.
    ///
    /// `node_memory_bytes` is the node's memory, which bounds `functions.cache_bytes` at its
    /// half. A config that leaves `cache_bytes` out takes 1 GiB or that half, whichever is
    /// less, and is never refused for it: a default is not the operator's choice. With `None`
    /// the bound cannot be checked, so a config that gives `cache_bytes` is refused, and the
    /// default is 1 GiB.
    pub fn validate(
        user_data: &[u8],
        node_memory_bytes: Option<u64>,
    ) -> Result<NodeConfig, Refusal> {
        if user_data.iter().all(u8::is_ascii_whitespace) {
            return Err(Refusal::Missing);
        }
        NodeConfig::read(&json::parse(user_data)?, Bound::Node(node_memory_bytes))
    }

    /// Reads a node config from its JSON.
    pub(crate) fn read(json: &Json, bound: Bound) -> Result<NodeConfig, Refusal> {
        let root = json.object("node config")?;
        // The version first: a later version may have fields this one does not know.
        match root.get("moruna") {
            None => return Err(Refusal::NoVersion),
            Some(Json::Unsigned(VERSION)) => {}
            Some(other) => return Err(Refusal::Version(json::kind(other))),
        }
        root.only(&["moruna", "api", "functions"])?;

        let api = root.need("api", "api")?.object("api")?;
        api.only(&["token_keys", "tls"])?;
        let token_keys = keys(
            api.need("token_keys", "api.token_keys")?,
            "api.token_keys",
            1,
        )?;
        let tls = api.get("tls").map(tls).transpose()?;

        let functions = match root.get("functions") {
            None => Functions {
                keys: Vec::new(),
                cache_bytes: default_cache_bytes(bound),
            },
            Some(functions) => {
                let functions = functions.object("functions")?;
                functions.only(&["keys", "cache_bytes"])?;
                Functions {
                    keys: match functions.get("keys") {
                        None => Vec::new(),
                        Some(list) => keys(list, "functions.keys", 0)?,
                    },
                    cache_bytes: match functions.get("cache_bytes") {
                        None => default_cache_bytes(bound),
                        Some(given) => {
                            cache_bytes(given.unsigned("functions.cache_bytes")?, bound)?
                        }
                    },
                }
            }
        };
        Ok(NodeConfig {
            api: Api { token_keys, tls },
            functions,
        })
    }

    /// The config as the JSON of node config version 1, every default written out.
    pub(crate) fn document(&self) -> serde_json::Value {
        let written =
            |keys: &[PublicKey]| keys.iter().map(PublicKey::to_base64).collect::<Vec<_>>();
        let mut api = serde_json::json!({ "token_keys": written(&self.api.token_keys) });
        if let Some(tls) = &self.api.tls {
            api["tls"] = serde_json::json!({ "certificate": tls.certificate, "key": tls.key });
        }
        serde_json::json!({
            "moruna": VERSION,
            "api": api,
            "functions": {
                "keys": written(&self.functions.keys),
                "cache_bytes": self.functions.cache_bytes,
            },
        })
    }
}

/// A list of keys at `path`, holding `least` to [`KEYS_MAX`] of them.
fn keys(list: &Json, path: &'static str, least: usize) -> Result<Vec<PublicKey>, Refusal> {
    let texts = list.strings(path)?;
    if texts.len() < least || texts.len() > KEYS_MAX {
        return Err(Refusal::KeyCount {
            path,
            count: texts.len(),
            least,
        });
    }
    texts
        .iter()
        .enumerate()
        .map(|(index, text)| PublicKey::from_base64(text).ok_or(Refusal::Key { path, index }))
        .collect()
}

/// `api.tls`: its two fields, each the PEM blocks it must be.
fn tls(tls: &Json) -> Result<Tls, Refusal> {
    let fields = tls.object("api.tls")?;
    fields.only(&["certificate", "key"])?;
    let field = |name: &str,
                 path: &'static str,
                 wanted: &'static str,
                 right: fn(&[&str]) -> bool|
     -> Result<String, Refusal> {
        let text = fields.need(name, path)?.string(path)?;
        let labels = pem::labels(text).map_err(|error| Refusal::Pem { path, error })?;
        if right(&labels) {
            Ok(text.to_string())
        } else {
            Err(Refusal::Blocks { path, wanted })
        }
    };
    Ok(Tls {
        certificate: field(
            "certificate",
            "api.tls.certificate",
            "one or more CERTIFICATE blocks and nothing else",
            |labels| !labels.is_empty() && labels.iter().all(|label| *label == "CERTIFICATE"),
        )?,
        key: field(
            "key",
            "api.tls.key",
            "exactly one PRIVATE KEY block",
            |labels| labels == ["PRIVATE KEY"],
        )?,
    })
}

/// `functions.cache_bytes` when node config does not give it: 1 GiB, or half the node's memory
/// when that is less (PM, 2026-11-18). Never refused, even under 64 MiB.
fn default_cache_bytes(bound: Bound) -> u64 {
    match bound {
        Bound::Node(Some(memory)) => CACHE_BYTES_DEFAULT.min(memory / 2),
        Bound::Node(None) | Bound::Checked => CACHE_BYTES_DEFAULT,
    }
}

/// `functions.cache_bytes` as node config gives it: a whole number of MiB, from 64 MiB to half
/// the node's memory.
fn cache_bytes(given: u64, bound: Bound) -> Result<u64, Refusal> {
    if !given.is_multiple_of(MIB) {
        return Err(Refusal::CacheNotMib(given));
    }
    if given < CACHE_BYTES_MIN {
        return Err(Refusal::CacheTooSmall(given));
    }
    match bound {
        Bound::Checked => Ok(given),
        Bound::Node(None) => Err(Refusal::CacheUnchecked),
        Bound::Node(Some(memory)) if given > memory / 2 => Err(Refusal::CacheTooLarge {
            given,
            most: memory / 2,
        }),
        Bound::Node(Some(_)) => Ok(given),
    }
}
