//! AD-T8 fingerprint_source (e.4): editing one character of the kernel's source changes the
//! fingerprint, and the same source with a different decorator argument changes it too.

#![cfg(feature = "python")]
#![allow(clippy::result_large_err)]

mod common;

use moruna_adapters::python::{PyKernel, PyKernelSpec};
use moruna_kernel::Kernel;

const ONE: &str = r#"
def scale(batch):
    return batch
"#;

/// The same function with one character changed: `scale` becomes `scald`.
const ONE_CHARACTER_CHANGED: &str = r#"
def scald(batch):
    return batch
"#;

/// The same function under the same name, with one character of its body changed.
const BODY_CHANGED: &str = r#"
def scale(batch):
    return  batch
"#;

fn fingerprint_of(spec: PyKernelSpec) -> moruna_kernel::Fingerprint {
    PyKernel::new(spec)
        .expect("the kernel is well formed")
        .fingerprint()
}

#[test]
fn ad_t8_fingerprint_source() {
    let fixture = common::SourceFixture::new();
    let base = fingerprint_of(PyKernelSpec::new(fixture.load(
        ONE,
        "scale",
        "ad_t8_kernel",
    )));

    // The same source, read again: the fingerprint is stable.
    let again = fingerprint_of(PyKernelSpec::new(fixture.load(
        ONE,
        "scale",
        "ad_t8_kernel",
    )));
    assert_eq!(
        base, again,
        "the same source must give the same fingerprint"
    );

    // One character of the body changed: a different kernel.
    let body = fingerprint_of(PyKernelSpec::new(fixture.load(
        BODY_CHANGED,
        "scale",
        "ad_t8_kernel",
    )));
    assert_ne!(
        base, body,
        "editing one character of the source must change the fingerprint"
    );

    // One character of the name changed: a different identity as well as a different source.
    let renamed = fingerprint_of(PyKernelSpec::new(fixture.load(
        ONE_CHARACTER_CHANGED,
        "scald",
        "ad_t8_kernel",
    )));
    assert_ne!(base, renamed);

    // The same source, a different decorator argument.
    let mut different_argument = PyKernelSpec::new(fixture.load(ONE, "scale", "ad_t8_kernel"));
    different_argument.expected_amplification = Some(2.5);
    assert_ne!(
        base,
        fingerprint_of(different_argument),
        "a different decorator argument must change the fingerprint"
    );

    let mut different_rows = PyKernelSpec::new(fixture.load(ONE, "scale", "ad_t8_kernel"));
    different_rows.preferred_rows = Some(4096);
    assert_ne!(base, fingerprint_of(different_rows));
}

/// e.4: a callable whose source cannot be read still has a fingerprint, and the report says the
/// source was not available.
#[test]
fn ad_t8_fingerprint_without_source() {
    let lambda = pyo3::Python::attach(|py| {
        py.eval(c"lambda batch: batch", None, None)
            .expect("a lambda")
            .unbind()
    });
    let kernel = PyKernel::new(PyKernelSpec::new(lambda)).expect("a lambda is a kernel");
    assert!(!kernel.stats().source_available);
}

/// 15 e.7: the declaration and the lockfile are part of the fingerprint, the module name is not,
/// and a wrapped function is fingerprinted as the function its author wrote.
#[test]
fn ad_t8_declaration_lockfile_origin_and_module() {
    use moruna_kernel::arrow::datatypes::DataType;
    use moruna_kernel::declare::{ColumnDecl, Declared, SchemaDecl};

    let fixture = common::SourceFixture::new();
    let base = fingerprint_of(PyKernelSpec::new(fixture.load(ONE, "scale", "ad_t8_a")));
    // The same file under another module name: the same kernel.
    let elsewhere = fingerprint_of(PyKernelSpec::new(fixture.load(ONE, "scale", "ad_t8_b")));
    assert_eq!(
        base, elsewhere,
        "the module name is not part of the fingerprint"
    );

    let declared = Declared {
        input: Some(SchemaDecl::Subset(vec![ColumnDecl::new(
            "a",
            DataType::Int64,
        )])),
        output: None,
    };
    let mut with_schema = PyKernelSpec::new(fixture.load(ONE, "scale", "ad_t8_a"));
    with_schema.declared = declared.clone();
    let kernel = PyKernel::new(with_schema).expect("kernel");
    assert_eq!(kernel.declared(), declared);
    assert_ne!(kernel.fingerprint(), base);

    let mut with_lock = PyKernelSpec::new(fixture.load(ONE, "scale", "ad_t8_a"));
    with_lock.lockfile = Some(b"numpy==2".to_vec());
    let locked = fingerprint_of(with_lock);
    assert_ne!(locked, base);
    let mut other_lock = PyKernelSpec::new(fixture.load(ONE, "scale", "ad_t8_a"));
    other_lock.lockfile = Some(b"numpy==3".to_vec());
    assert_ne!(fingerprint_of(other_lock), locked);

    // A wrapper whose origin is `scale` is fingerprinted as `scale`.
    let mut wrapped = PyKernelSpec::new(fixture.load(ONE_CHARACTER_CHANGED, "scald", "ad_t8_c"));
    wrapped.origin = Some(fixture.load(ONE, "scale", "ad_t8_a"));
    assert_eq!(fingerprint_of(wrapped), base);
}
