//! AD-T5 gil_flip (AD-I3, f.4): a kernel that imports an extension declaring `gil_used = true`
//! re-enables the GIL, and the adapter notices after the first `apply`.
//!
//! The extension is built here, as a module registered in the interpreter's inittab before the
//! interpreter starts, so that importing it goes through the same machinery a wheel's extension
//! would. This test therefore owns its binary: `append_to_inittab!` must run before any
//! interpreter exists.

#![cfg(feature = "python")]
#![allow(clippy::result_large_err)]

mod common;

use std::sync::Arc;

use amoru_adapters::GilState;
use amoru_kernel::{Allocator, Kernel, NoState, Payload};
use amoru_testkit::FakeAllocator;
use pyo3::prelude::*;

/// A module that declares it needs the GIL. Importing it on a free threaded interpreter turns
/// the GIL back on, which is the flip f.4 is about.
#[pymodule(gil_used = true)]
fn amoru_gil_probe(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add("marker", 1_u32)
}

const IMPORTER: &str = r#"
def importer(batch):
    import amoru_gil_probe
    return batch
"#;

#[test]
fn ad_t5_gil_flip() {
    pyo3::append_to_inittab!(amoru_gil_probe);

    let fake = FakeAllocator::new();
    let alloc: Arc<dyn Allocator> = Arc::new(fake.clone());
    let kernel = common::stateless_kernel(IMPORTER, "importer", "ad_t5_importer", alloc);
    let before = kernel.gil_state();

    let batch = common::arena_i64_batch(&fake, &[7, 8, 9]);
    let input = Payload::table(batch).expect("the batch is arena owned");
    let mut state = NoState;
    kernel.apply(&mut state, input).expect("the kernel runs");

    assert_eq!(
        kernel.gil_state(),
        GilState::Serialised,
        "importing a module that declares gil_used = true must leave the kernel serialised"
    );
    if before == GilState::FreeThreaded {
        assert!(
            amoru_adapters::python_gil_enabled(),
            "the interpreter itself should report the flip the adapter saw"
        );
    }
}
