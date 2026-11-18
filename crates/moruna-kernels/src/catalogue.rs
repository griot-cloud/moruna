//! The kernel catalogue (system design 4A): versioned identities, and the implementations the
//! build has of each.
//!
//! A kernel is named `<namespace>.<name>@<version>`, for example `moruna.filter@1`. Its
//! signature and behaviour never change once published; a change in behaviour is a new version,
//! and old versions stay callable. The identity is what a job and the plane name. The
//! fingerprint stays `sha256(name, canonical args, crate version)` ([`fingerprint`]), with the
//! identity as the name.
//!
//! Every kernel version has a written specification (`spec/<name>.md`) and a conformance corpus
//! (`conformance/<name>/`), both described in [`crate::conformance`]. An [`Implementation`]
//! claims the versions it implements; the conformance runner runs every case of a version
//! against every implementation that claims it, and one failed case is a failed claim.

use std::collections::BTreeMap;
use std::sync::Arc;

use moruna_kernel::arrow::ipc::writer::StreamWriter;
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{Fingerprint, Kernel, MorunaError, Result};
use serde_json::Value;
use sha2::Digest;

use crate::std_fingerprint;

/// Moruna's own namespace. A vendor or a package publishes kernels in its own.
pub const MORUNA: &str = "moruna";

/// A kernel's identity: its namespace, name and version, written `moruna.filter@1`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KernelId {
    /// One or more dot-separated words, `moruna` for Moruna's own kernels.
    pub namespace: String,
    /// One word: lower-case ASCII letters, digits and `_`, starting with a letter.
    pub name: String,
    /// The version, from 1.
    pub version: u32,
}

/// True for a word of an identity: `[a-z][a-z0-9_]*`.
pub(crate) fn is_word(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some('a'..='z'))
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// A version as written in an identity: a decimal from 1, with no leading zero.
pub(crate) fn parse_version(s: &str) -> Option<u32> {
    if s.is_empty() || s.starts_with('0') || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

impl KernelId {
    /// Parse `<namespace>.<name>@<version>`. The name is the last dot-separated word; the
    /// namespace is every word before it.
    pub fn parse(text: &str) -> Result<KernelId> {
        let refuse = || {
            MorunaError::Plan(format!(
                "`{text}` is not a kernel identity: write <namespace>.<name>@<version>, for \
                 example moruna.filter@1"
            ))
        };
        let (kernel, version) = text.split_once('@').ok_or_else(refuse)?;
        let version = parse_version(version).ok_or_else(refuse)?;
        let (namespace, name) = kernel.rsplit_once('.').ok_or_else(refuse)?;
        if !is_word(name) || !namespace.split('.').all(is_word) {
            return Err(refuse());
        }
        Ok(KernelId {
            namespace: namespace.to_string(),
            name: name.to_string(),
            version,
        })
    }

    /// The kernel without its version: `moruna.filter`.
    pub fn kernel(&self) -> String {
        format!("{}.{}", self.namespace, self.name)
    }
}

impl core::fmt::Display for KernelId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}@{}", self.namespace, self.name, self.version)
    }
}

/// What a kernel is configured with (system design 4C): its parameters as a JSON object, and
/// the parameters that are Arrow data (`take`'s values, a model's weights), each a batch.
#[derive(Clone, Debug, Default)]
pub struct Params {
    /// The parameters that are JSON: an object, or `null` for none.
    pub json: Value,
    /// The parameters that are Arrow data, by name.
    pub arrays: BTreeMap<String, RecordBatch>,
}

impl Params {
    /// Parameters of JSON alone.
    pub fn json(json: Value) -> Params {
        Params {
            json,
            arrays: BTreeMap::new(),
        }
    }
}

/// Configure one version of a kernel: a configured [`Kernel`], or a `Plan` error naming what in
/// the parameters is refused.
pub type Configure = fn(version: u32, params: &Params) -> Result<Arc<dyn Kernel>>;

/// One implementation of a kernel: the versions it claims and how it is configured.
#[derive(Clone, Copy)]
pub struct Implementation {
    /// The kernel it implements, without a version: `moruna.filter`.
    pub kernel: &'static str,
    /// What implements it: `cpu-reference` for the CPU reference that defines the right answer.
    pub implementation: &'static str,
    /// The versions it claims. It must pass every case of each in the conformance corpus.
    pub versions: &'static [u32],
    /// Configure one of `versions`.
    pub configure: Configure,
}

impl core::fmt::Debug for Implementation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Implementation")
            .field("kernel", &self.kernel)
            .field("implementation", &self.implementation)
            .field("versions", &self.versions)
            .finish_non_exhaustive()
    }
}

/// The name of every CPU reference implementation.
pub const CPU_REFERENCE: &str = "cpu-reference";

/// Every implementation this build has, the CPU references first.
pub const IMPLEMENTATIONS: &[Implementation] = &[
    crate::group_aggregate::CPU_REFERENCE,
    crate::interval_coverage_count::CPU,
    Implementation {
        kernel: "moruna.mlp_forward",
        implementation: CPU_REFERENCE,
        versions: crate::mlp::VERSIONS,
        configure: crate::mlp::configure,
    },
    Implementation {
        kernel: "moruna.filter",
        implementation: CPU_REFERENCE,
        versions: &[1],
        configure: crate::selection::configure_filter,
    },
    Implementation {
        kernel: "moruna.take",
        implementation: CPU_REFERENCE,
        versions: &[1],
        configure: crate::selection::configure_take,
    },
];

/// Every implementation this build has.
pub fn implementations() -> &'static [Implementation] {
    IMPLEMENTATIONS
}

/// Configure the kernel `identity` (`moruna.filter@1`) with `params`, by the first
/// implementation that claims it, which is its CPU reference.
pub fn configure(identity: &str, params: &Params) -> Result<Arc<dyn Kernel>> {
    configure_from(implementations(), identity, params)
}

/// [`configure`] over a given list of implementations.
pub fn configure_from(
    implementations: &[Implementation],
    identity: &str,
    params: &Params,
) -> Result<Arc<dyn Kernel>> {
    let id = KernelId::parse(identity)?;
    let kernel = id.kernel();
    implementations
        .iter()
        .find(|i| i.kernel == kernel && i.versions.contains(&id.version))
        .ok_or_else(|| {
            let mut known: Vec<String> = implementations
                .iter()
                .flat_map(|i| i.versions.iter().map(move |v| format!("{}@{v}", i.kernel)))
                .collect();
            known.sort();
            known.dedup();
            MorunaError::Plan(format!(
                "no kernel {id} in this build (it has {})",
                if known.is_empty() {
                    "none".to_string()
                } else {
                    known.join(", ")
                }
            ))
        })
        .and_then(|i| (i.configure)(id.version, params))
}

/// A kernel's fingerprint: `sha256(name, canonical args, crate version)` (MH 4.9), with the
/// identity (`moruna.filter@1`) as the name. The canonical args are the JSON parameters with,
/// when there are Arrow parameters, an `"arrays"` member mapping each one's name to the SHA-256
/// of its Arrow IPC stream, so two kernels configured with different data never share one.
pub fn fingerprint(id: &KernelId, params: &Params) -> Result<Fingerprint> {
    let mut args = match &params.json {
        Value::Null => Value::Object(serde_json::Map::new()),
        other => other.clone(),
    };
    if !params.arrays.is_empty() {
        let Value::Object(map) = &mut args else {
            return Err(MorunaError::Plan(format!(
                "{id}: parameters must be an object, not {args}"
            )));
        };
        let mut digests = serde_json::Map::new();
        for (name, batch) in &params.arrays {
            digests.insert(name.clone(), Value::String(ipc_digest(batch)?));
        }
        map.insert("arrays".to_string(), Value::Object(digests));
    }
    Ok(std_fingerprint(&id.to_string(), &args))
}

/// The SHA-256 of `batch` as an Arrow IPC stream, in lower-case hex.
fn ipc_digest(batch: &RecordBatch) -> Result<String> {
    let refuse = |e: moruna_kernel::arrow::error::ArrowError| {
        MorunaError::Plan(format!("an Arrow parameter: {e}"))
    };
    let mut bytes = Vec::new();
    {
        let mut writer = StreamWriter::try_new(&mut bytes, &batch.schema()).map_err(refuse)?;
        writer.write(batch).map_err(refuse)?;
        writer.finish().map_err(refuse)?;
    }
    let digest = sha2::Sha256::digest(&bytes);
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities_parse_and_print() {
        let id = KernelId::parse("moruna.filter@1").expect("parse");
        assert_eq!(id.namespace, "moruna");
        assert_eq!(id.name, "filter");
        assert_eq!(id.version, 1);
        assert_eq!(id.to_string(), "moruna.filter@1");
        assert_eq!(id.kernel(), "moruna.filter");
        let vendor = KernelId::parse("acme.geo.contains@12").expect("vendor");
        assert_eq!(vendor.namespace, "acme.geo");
        assert_eq!(vendor.name, "contains");
        assert_eq!(vendor.version, 12);
        for bad in [
            "moruna.filter",
            "filter@1",
            "moruna.filter@0",
            "moruna.filter@01",
            "moruna.filter@",
            "moruna.Filter@1",
            "moruna..filter@1",
            ".filter@1",
            "moruna.filter@1x",
            "moruna.9filter@1",
        ] {
            assert!(KernelId::parse(bad).is_err(), "{bad} parsed");
        }
    }

    #[test]
    fn an_unknown_identity_names_what_the_build_has() {
        let err = configure("moruna.nothing@1", &Params::default())
            .err()
            .expect("refused");
        assert!(
            err.to_string().contains("no kernel moruna.nothing@1"),
            "{err}"
        );
        for kernel in [
            "moruna.filter@1",
            "moruna.interval_coverage_count@1",
            "moruna.mlp_forward@1",
            "moruna.take@1",
        ] {
            assert!(err.to_string().contains(kernel), "names {kernel}: {err}");
        }
        assert!(configure("not an identity", &Params::default()).is_err());
        let none = configure_from(&[], "moruna.filter@1", &Params::default())
            .err()
            .expect("none");
        assert!(none.to_string().contains("(it has none)"), "{none}");
        assert!(configure("moruna.filter@2", &Params::default()).is_err());
    }

    #[test]
    fn a_kernel_is_configured_by_its_identity() {
        let kernel = configure(
            "moruna.filter@1",
            &Params::json(serde_json::json!({"predicate": "p"})),
        )
        .expect("filter");
        let id = KernelId::parse("moruna.filter@1").expect("id");
        let params = Params::json(serde_json::json!({"predicate": "p"}));
        assert_eq!(kernel.fingerprint(), fingerprint(&id, &params).expect("fp"));
    }

    #[test]
    fn the_fingerprint_is_of_the_identity_the_arguments_and_the_data() {
        use moruna_kernel::arrow::array::Int64Array;
        use moruna_kernel::arrow::datatypes::{DataType, Field, Schema};
        let id = KernelId::parse("moruna.take@1").expect("id");
        let other = KernelId::parse("moruna.take@2").expect("id");
        let empty = fingerprint(&id, &Params::default()).expect("empty");
        assert_eq!(
            empty,
            fingerprint(&id, &Params::json(serde_json::json!({}))).expect("{}")
        );
        assert_ne!(empty, fingerprint(&other, &Params::default()).expect("v2"));
        let a = serde_json::json!({"a": 1, "b": [2, 3]});
        let b = serde_json::json!({"b": [2, 3], "a": 1});
        assert_eq!(
            fingerprint(&id, &Params::json(a)).expect("a"),
            fingerprint(&id, &Params::json(b)).expect("b")
        );
        let values = |v: Vec<i64>| {
            let schema = Arc::new(Schema::new(vec![Field::new("v", DataType::Int64, true)]));
            let batch =
                RecordBatch::try_new(schema, vec![Arc::new(Int64Array::from(v))]).expect("batch");
            Params {
                json: Value::Null,
                arrays: BTreeMap::from([("values".to_string(), batch)]),
            }
        };
        let one = fingerprint(&id, &values(vec![1, 2])).expect("one");
        assert_eq!(one, fingerprint(&id, &values(vec![1, 2])).expect("same"));
        assert_ne!(one, fingerprint(&id, &values(vec![1, 3])).expect("other"));
        assert_ne!(one, empty);
        let not_object = Params {
            json: serde_json::json!([1]),
            ..values(vec![1])
        };
        assert!(fingerprint(&id, &not_object).is_err());
    }
}
