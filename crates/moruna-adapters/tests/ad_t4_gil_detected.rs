//! AD-T4 gil_detected (AD-I3): the adapter reads the interpreter's GIL state and behaves the way
//! it says. Under a serialised interpreter two concurrent `apply` calls do not overlap, because
//! the adapter serialises them itself; under a free threaded one they do.
//!
//! One interpreter, two runs: CPython 3.14 free-threaded, and the same interpreter with
//! `PYTHON_GIL=1` (preamble 6.6). The test reads `gil_state()` and asserts the property that
//! state implies, so both runs exercise it.
//!
//! The timestamps are taken inside the Python call, not around `apply`: a call that is waiting
//! for the adapter's mutex has not started, and timing the wait would make every run overlap.

#![cfg(feature = "python")]
#![allow(clippy::result_large_err)]

mod common;

use std::sync::Arc;

use moruna_adapters::GilState;
use moruna_adapters::python::{PyKernel, PyKernelSpec};
use moruna_kernel::{Allocator, Kernel, NoState, Payload};
use moruna_testkit::FakeAllocator;
use pyo3::prelude::*;

const SLOW: &str = r#"
import time

def slow(batch):
    entered = time.monotonic()
    time.sleep(0.25)
    slow.spans.append((entered, time.monotonic()))
    return batch

slow.spans = []
"#;

/// The (entered, left) pairs the kernel recorded, earliest first.
fn spans(callable: &Py<PyAny>) -> Vec<(f64, f64)> {
    let mut spans: Vec<(f64, f64)> = Python::attach(|py| {
        callable
            .bind(py)
            .getattr("spans")
            .expect("the kernel records its spans")
            .extract()
            .expect("a list of pairs of floats")
    });
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    spans
}

#[test]
fn ad_t4_gil_detected() {
    let fake = FakeAllocator::new();
    let alloc: Arc<dyn Allocator> = Arc::new(fake.clone());
    let callable = common::py_object(SLOW, "slow", "ad_t4_slow");
    let kernel = Python::attach(|py| {
        PyKernel::new(PyKernelSpec::new(callable.clone_ref(py))).expect("a well formed kernel")
    });
    kernel.bind_allocator(alloc);
    let kernel = Arc::new(kernel);

    // AD-I3: the state is read from the interpreter, not assumed.
    let expected = if moruna_adapters::python_gil_enabled() {
        GilState::Serialised
    } else {
        GilState::FreeThreaded
    };
    assert_eq!(
        kernel.gil_state(),
        expected,
        "the adapter's GIL state disagrees with sys._is_gil_enabled()"
    );

    let mut workers = Vec::new();
    for _ in 0..2 {
        let kernel = Arc::clone(&kernel);
        let batch = common::arena_i64_batch(&fake, &[1, 2, 3, 4]);
        let input = Payload::table(batch).expect("the batch is arena owned");
        workers.push(std::thread::spawn(move || {
            let mut state = NoState;
            kernel.apply(&mut state, input).expect("the kernel runs");
        }));
    }
    for worker in workers {
        worker.join().expect("the worker finished");
    }

    let spans = spans(&callable);
    assert_eq!(spans.len(), 2, "both calls should have recorded a span");
    let overlapped = spans[1].0 < spans[0].1;

    match kernel.gil_state() {
        GilState::Serialised => assert!(
            !overlapped,
            "a serialised kernel ran two applies at once; the adapter's mutex did not hold ({spans:?})"
        ),
        GilState::FreeThreaded => assert!(
            overlapped,
            "a free threaded kernel serialised two applies that should have overlapped ({spans:?})"
        ),
    }
    assert_eq!(kernel.stats().calls, 2);
}
