//! The Python kernel fingerprint (e.4, as amended by MH 4.9).
//!
//! `sha256(canonical source || lockfile bytes, when given || Moruna ABI version)` (MH 4.9),
//! where the canonical source is the kernel's qualified name, its source text normalised, and
//! its declaration: the decorator's arguments and its schemas as canonical JSON. Editing the
//! function, its decorator, its schemas or its locked dependencies therefore changes the
//! fingerprint and invalidates the profile keyed on it, which is the point: the same name with a
//! different body is a different kernel. The module name is deliberately not part of it, so
//! `moruna check kernels.py` and a program that imports the same file under another name agree,
//! and the profile row the check writes is the one the run reads.

use moruna_kernel::declare::ABI_VERSION;
use moruna_kernel::{Fingerprint, PayloadKind, ResumePolicy, TierPref};
use pyo3::prelude::*;
use sha2::Digest;

use super::kernel::PyKernelSpec;

/// A kernel's fingerprint and whether its source text was available.
pub struct Fingerprinted {
    /// The fingerprint.
    pub fingerprint: Fingerprint,
    /// False when `inspect.getsource` could not read the callable (a lambda typed at a REPL), in
    /// which case the code object stood in for the source and the run report says so.
    pub source_available: bool,
}

/// Compute a Python kernel's fingerprint (MH 4.9).
pub fn compute(py: Python<'_>, spec: &PyKernelSpec) -> Fingerprinted {
    let target = fingerprint_target(py, spec);
    let qualname = qualname_of(&target);
    let (source, source_available) = source_of(py, &target);
    let canonical = format!(
        "py:{qualname}\n{source}\n{}{}",
        config_json(spec),
        spec.declared.canonical_json()
    );
    let mut hasher = sha2::Sha256::new();
    hasher.update(b"moruna-kernel\0");
    hasher.update(canonical.as_bytes());
    hasher.update(b"\0");
    if let Some(lockfile) = &spec.lockfile {
        hasher.update(lockfile);
    }
    hasher.update(b"\0abi:");
    hasher.update(ABI_VERSION.to_string().as_bytes());
    Fingerprinted {
        fingerprint: Fingerprint(hasher.finalize().into()),
        source_available,
    }
}

/// What the fingerprint is taken of: the function the author wrote (`origin`, for a Polars
/// kernel the decorator wrapped), the callable itself, or the class of a class kernel (b).
fn fingerprint_target<'py>(py: Python<'py>, spec: &PyKernelSpec) -> Bound<'py, PyAny> {
    if let Some(origin) = &spec.origin {
        return origin.bind(py).clone();
    }
    let callable = spec.callable.bind(py).clone();
    if spec.stateful {
        callable.get_type().into_any()
    } else {
        callable
    }
}

fn qualname_of(target: &Bound<'_, PyAny>) -> String {
    attr_string(target, "__qualname__")
        .or_else(|| attr_string(target, "__name__"))
        .unwrap_or_else(|| "<anonymous>".to_string())
}

fn attr_string(target: &Bound<'_, PyAny>, name: &str) -> Option<String> {
    target
        .getattr(name)
        .ok()
        .and_then(|value| value.extract::<String>().ok())
}

/// The source text of the target, normalised (MH 4.9), or the code object's bytes and constants
/// when the source is not available (e.4).
fn source_of(py: Python<'_>, target: &Bound<'_, PyAny>) -> (String, bool) {
    if let Ok(inspect) = py.import("inspect")
        && let Ok(source) = inspect.call_method1("getsource", (target,))
        && let Ok(text) = source.extract::<String>()
    {
        let dedented = py
            .import("textwrap")
            .and_then(|t| t.call_method1("dedent", (text.as_str(),)))
            .and_then(|d| d.extract::<String>())
            .unwrap_or(text);
        return (normalise(&dedented), true);
    }
    (code_identity(target), false)
}

/// `\r\n` as `\n`, trailing whitespace stripped from every line, leading and trailing blank
/// lines removed (MH 4.9).
pub fn normalise(source: &str) -> String {
    let lines: Vec<&str> = source
        .split('\n')
        .map(|line| line.trim_end_matches(['\r', ' ', '\t']))
        .collect();
    let first = lines
        .iter()
        .position(|l| !l.is_empty())
        .unwrap_or(lines.len());
    let last = lines
        .iter()
        .rposition(|l| !l.is_empty())
        .map_or(first, |i| i + 1);
    lines[first..last.max(first)].join("\n")
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

/// Every `PyKernelSpec` decorator argument, as JSON with the keys in one fixed order, so that two
/// specs are equal exactly when their JSON is (e.4).
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

#[cfg(test)]
mod tests {
    use super::normalise;

    #[test]
    fn normalise_strips_what_an_editor_changes() {
        assert_eq!(
            normalise("\n\ndef f(b):  \r\n    return b\t\n\n"),
            "def f(b):\n    return b"
        );
        assert_eq!(normalise(""), "");
        assert_eq!(normalise("\n \n"), "");
    }
}
