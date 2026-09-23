//! CT-T10 fingerprint_stability: the same inputs give the same fingerprint across processes
//! (a golden value); one byte of configuration change gives a different one. Proves e.6.

use std::sync::Arc;

use moruna_kernel::{DType, Fingerprint, SourceSchema};
use arrow::datatypes::{DataType, Field, Schema};

/// The golden schema hashes: a table schema's is BLAKE3 over its Arrow IPC schema message, a
/// tensor schema's over `"tensor:" || dtype code || shape` (d.4). The profile store keys
/// records by them (RC e.3), so they too must be stable across processes.
const GOLDEN_TABLE: &str = "1c7c1fe559633263e6fce39ddeb3627b9d1beac7bab327ea4d36506e5b9264b1";
const GOLDEN_TENSOR: &str = "8cfdd1197e31c7a791f5b0444b560d8e382b24c1bb9083b42fc42618e17dd87c";

/// The golden value: BLAKE3 over `identity.len() as u64 LE || identity || config` for the
/// identity and configuration below (e.6). It is a constant of the format, so it must not
/// change between processes, machines or releases.
// Recomputed on 2026-09-23 when the project was renamed from amoru to moruna: the
// identity string below carries the crate name, so the digest changed by design, and
// this test failing was the rename being noticed rather than a defect. A change to
// this value for any other reason means the fingerprint is not stable and profiles
// keyed by it are worthless.
const GOLDEN: &str = "7012697cd7e1d11da503dbbdfa46cf65e5ab5f84efe44a6bb7482e33ec31306f";

#[test]
fn ct_t10_fingerprint_stability() {
    let identity = "moruna-bench 0.1.0 ::kernels::Normalise";
    let config = b"width=64,mode=nfkc";

    let first = Fingerprint::compute(identity, config);
    let again = Fingerprint::compute(identity, config);
    assert_eq!(first, again);
    assert_eq!(
        first.to_hex(),
        GOLDEN,
        "the fingerprint of a fixed input must never change"
    );
    assert_eq!(first.to_string(), GOLDEN);
    assert_eq!(first.0.len(), 32);

    // One byte of configuration change is a different fingerprint.
    let mut changed = config.to_vec();
    changed[0] ^= 1;
    assert_ne!(Fingerprint::compute(identity, &changed), first);

    // So is a different identity of the same length, and the length prefix keeps
    // `identity || config` from being ambiguous.
    assert_ne!(
        Fingerprint::compute("moruna-bench 0.1.0 ::kernels::normalise", config),
        first
    );
    assert_ne!(
        Fingerprint::compute("ab", b"c"),
        Fingerprint::compute("a", b"bc")
    );

    // A schema hash is stable in the same way, for a table
    let table = SourceSchema::Table(Arc::new(Schema::new(vec![
        Field::new("i", DataType::Int32, false),
        Field::new("s", DataType::Utf8, true),
    ])));
    assert_eq!(hex(&table.hash()), GOLDEN_TABLE);
    assert_eq!(table.hash(), table.hash());
    // and for a tensor,
    let tensor = SourceSchema::Tensor {
        dtype: DType::F32,
        shape: vec![-1, 128],
    };
    assert_eq!(hex(&tensor.hash()), GOLDEN_TENSOR);
    assert_eq!(tensor.hash(), tensor.hash());
    // and a different schema is a different key (RC e.3 keys the profile store by it).
    let renamed = SourceSchema::Table(Arc::new(Schema::new(vec![
        Field::new("j", DataType::Int32, false),
        Field::new("s", DataType::Utf8, true),
    ])));
    assert_ne!(renamed.hash(), table.hash());
    let reshaped = SourceSchema::Tensor {
        dtype: DType::F32,
        shape: vec![-1, 64],
    };
    assert_ne!(reshaped.hash(), tensor.hash());
    let retyped = SourceSchema::Tensor {
        dtype: DType::F64,
        shape: vec![-1, 128],
    };
    assert_ne!(retyped.hash(), tensor.hash());
    assert_ne!(tensor.hash(), table.hash());
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
