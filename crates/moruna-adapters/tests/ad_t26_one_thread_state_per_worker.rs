//! AD-T26 (05 f.12, 2026-11-14): a worker keeps one Python thread state for its life. Two calls
//! on one thread Python did not make see the same `threading.local`, and once the thread has
//! exited its thread state is gone, with what the function kept in it. Every call made a thread
//! state and deleted it, so a function's `threading.local` was empty at every batch and, in the
//! free-threaded build, the thread's allocator heaps started cold at every batch.

mod common;

use std::sync::Arc;

use moruna_adapters::python::{PyKernel, PyKernelSpec};
use moruna_kernel::{Allocator, Kernel, NoState, Payload};
use moruna_testkit::FakeAllocator;
use pyo3::prelude::*;

const SOURCE: &str = r#"
import gc, threading, weakref

local = threading.local()
fresh = []

class Kept:
    pass

def remember(batch):
    if not hasattr(local, "kept"):
        local.kept = Kept()
        fresh.append(weakref.ref(local.kept))
    return batch

def thread_states_made():
    return len(fresh)

def all_released():
    gc.collect()
    return all(ref() is None for ref in fresh)
"#;

fn module() -> Py<PyAny> {
    Python::attach(|py| {
        let code = std::ffi::CString::new(SOURCE).expect("no nul");
        pyo3::types::PyModule::from_code(py, &code, c"ad_t26_mod.py", c"ad_t26_mod")
            .expect("the fixture imports")
            .into_any()
            .unbind()
    })
}

fn ask(module: &Py<PyAny>, name: &str) -> Py<PyAny> {
    Python::attach(|py| {
        module
            .bind(py)
            .call_method0(name)
            .expect("the fixture answers")
            .unbind()
    })
}

#[test]
fn ad_t26_a_worker_keeps_one_thread_state() {
    let module = module();
    let function = Python::attach(|py| {
        module
            .bind(py)
            .getattr("remember")
            .expect("defined")
            .unbind()
    });
    let fake = FakeAllocator::new();
    let kernel = PyKernel::new(PyKernelSpec::new(function)).expect("a kernel");
    kernel.bind_allocator(Arc::new(fake.clone()) as Arc<dyn Allocator>);
    let kernel = Arc::new(kernel);

    let worker = {
        let kernel = Arc::clone(&kernel);
        let fake = fake.clone();
        std::thread::spawn(move || {
            for _ in 0..3 {
                let batch = common::arena_i64_batch(&fake, &[1, 2, 3]);
                kernel
                    .apply(&mut NoState, Payload::table(batch).expect("arena owned"))
                    .expect("the call completes");
            }
        })
    };
    worker.join().expect("the worker finishes");

    let made: usize = Python::attach(|py| {
        ask(&module, "thread_states_made")
            .extract(py)
            .expect("an int")
    });
    assert_eq!(made, 1, "three calls on one worker are one thread state");
    let released: bool =
        Python::attach(|py| ask(&module, "all_released").extract(py).expect("a bool"));
    assert!(
        released,
        "the worker's thread state, and what the function kept in it, went with the thread"
    );
}
