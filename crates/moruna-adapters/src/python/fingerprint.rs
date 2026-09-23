//! The Python kernel fingerprint (e.4).
//!
//! A Python kernel's identity is its qualified name; its configuration is the text of its source
//! plus the decorator's arguments. Editing the function therefore invalidates the profile the
//! controller keyed on it, which is the point: the same name with a different body is a different
//! kernel, and a profile that survived the edit would size the wrong thing.

use moruna_kernel::{Fingerprint, PayloadKind, ResumePolicy, TierPref};
use pyo3::prelude::*;

use super::kernel::PyKernelSpec;

/// A kernel's fingerprint and whether its source text was available.
pub struct Fingerprinted {
    /// The fingerprint.
    pub fingerprint: Fingerprint,
    /// False when `inspect.getsource` could not read the callable (a lambda typed at a REPL), in
    /// which case the code object stood in for the source and the run report says so.
    pub source_available: bool,
}

/// Compute a Python kernel's fingerprint (e.4).
///
/// `identity` is `py:<module>.<qualname>` of the callable, or of the class for a class kernel.
/// `config` is a 32 byte digest of the source text followed by the decorator arguments as
/// canonical JSON.
///
/// The digest is `Fingerprint::compute("py:source", source)` rather than a bare BLAKE3 of the
/// source, because `blake3` is not a dependency of this crate and adding one is an E2 item; the
/// digest is BLAKE3 over the source either way, and AD-T8 holds.
pub fn compute(py: Python<'_>, spec: &PyKernelSpec) -> Fingerprinted {
    let target = fingerprint_target(py, spec);
    let identity = identity_of(py, &target);
    let (source, source_available) = source_of(py, &target);
    let digest = Fingerprint::compute("py:source", source.as_bytes()).0;
    let mut config = Vec::with_capacity(digest.len() + 128);
    config.extend_from_slice(&digest);
    config.extend_from_slice(config_json(spec).as_bytes());
    Fingerprinted {
        fingerprint: Fingerprint::compute(&identity, &config),
        source_available,
    }
}

/// What the fingerprint is taken of: the callable itself, or the class of a class kernel (b).
fn fingerprint_target<'py>(py: Python<'py>, spec: &PyKernelSpec) -> Bound<'py, PyAny> {
    let callable = spec.callable.bind(py).clone();
    if spec.stateful {
        callable.get_type().into_any()
    } else {
        callable
    }
}

fn identity_of(py: Python<'_>, target: &Bound<'_, PyAny>) -> String {
    let module = attr_string(py, target, "__module__").unwrap_or_else(|| "<unknown>".to_string());
    let qualname = attr_string(py, target, "__qualname__")
        .or_else(|| attr_string(py, target, "__name__"))
        .unwrap_or_else(|| "<anonymous>".to_string());
    format!("py:{module}.{qualname}")
}

fn attr_string(py: Python<'_>, target: &Bound<'_, PyAny>, name: &str) -> Option<String> {
    let _ = py;
    target
        .getattr(name)
        .ok()
        .and_then(|value| value.extract::<String>().ok())
}

/// The source text of the callable, or the code object's bytes and constants when the source is
/// not available (e.4).
fn source_of(py: Python<'_>, target: &Bound<'_, PyAny>) -> (String, bool) {
    if let Ok(inspect) = py.import("inspect")
        && let Ok(source) = inspect.call_method1("getsource", (target,))
        && let Ok(text) = source.extract::<String>()
    {
        return (text, true);
    }
    (code_identity(target), false)
}

/// `co_code` plus the repr of `co_consts`, the stand in e.4 names for a callable whose source
/// cannot be read.
fn code_identity(target: &Bound<'_, PyAny>) -> String {
    let code = target.getattr("__code__").or_else(|_| {
        target
            .getattr("__call__")
            .and_then(|c| c.getattr("__code__"))
    });
    let Ok(code) = code else {
        return format!("<no source, no code object: {target:?}>");
    };
    let co_code = code
        .getattr("co_code")
        .and_then(|b| b.extract::<Vec<u8>>())
        .unwrap_or_default();
    let co_consts = code
        .getattr("co_consts")
        .map(|c| format!("{c:?}"))
        .unwrap_or_default();
    let mut out = String::with_capacity(co_code.len() * 2 + co_consts.len() + 8);
    for byte in co_code {
        out.push_str(&format!("{byte:02x}"));
    }
    out.push('|');
    out.push_str(&co_consts);
    out
}

/// Every `PyKernelSpec` field except the callable, as JSON with the keys in one fixed order, so
/// that two specs are equal exactly when their JSON is (e.4).
fn config_json(spec: &PyKernelSpec) -> String {
    format!(
        concat!(
            "{{\"accepts_kind\":\"{}\",\"accepts_tier\":\"{}\",\"device_memory\":{},",
            "\"expected_amplification\":{},\"instances\":{},\"preferred_rows\":{},",
            "\"releases_gil\":{},\"resume\":\"{}\",\"state_bytes\":{},\"stateful\":{}}}"
        ),
        kind_name(spec.accepts.kind),
        tier_name(spec.accepts.tier),
        spec.device_memory,
        optional_f64(spec.expected_amplification),
        spec.instances,
        optional_u64(spec.preferred_rows),
        optional_bool(spec.releases_gil),
        resume_name(spec.resume),
        optional_u64(spec.state_bytes),
        spec.stateful,
    )
}

fn kind_name(kind: PayloadKind) -> &'static str {
    match kind {
        PayloadKind::Table => "table",
        PayloadKind::Tensor => "tensor",
        PayloadKind::Either => "either",
    }
}

fn tier_name(tier: TierPref) -> &'static str {
    match tier {
        TierPref::Host => "host",
        TierPref::Device => "device",
        TierPref::Any => "any",
    }
}

fn resume_name(resume: ResumePolicy) -> &'static str {
    match resume {
        ResumePolicy::Reinit => "reinit",
        ResumePolicy::Checkpoint => "checkpoint",
        ResumePolicy::Forbid => "forbid",
    }
}

fn optional_f64(value: Option<f64>) -> String {
    match value {
        Some(v) => format!("{v:?}"),
        None => "null".to_string(),
    }
}

fn optional_u64(value: Option<u64>) -> String {
    match value {
        Some(v) => v.to_string(),
        None => "null".to_string(),
    }
}

fn optional_bool(value: Option<bool>) -> String {
    match value {
        Some(v) => v.to_string(),
        None => "null".to_string(),
    }
}
