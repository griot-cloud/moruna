//! AD-T6 deleter_attach (AD-I5): a tensor whose bytes a Python object owns can be dropped from a
//! thread that never touched the interpreter, because the adapter installs a deleter hook that
//! attaches first. Nothing crashes and the Python object's reference count reaches zero.

#![cfg(feature = "python")]
#![allow(clippy::result_large_err)]

use moruna_adapters::python::cross::{Imported, import};
use pyo3::prelude::*;

#[test]
fn ad_t6_deleter_attach() {
    let (tensor, weak) = Python::attach(|py| {
        let numpy = py.import("numpy").expect("numpy is installed");
        let array = numpy
            .call_method1("arange", (4096,))
            .expect("an array of 4096 ints");
        let weak = py
            .import("weakref")
            .expect("weakref")
            .call_method1("ref", (&array,))
            .expect("a weak reference to the array")
            .unbind();
        let imported = import(py, &array).expect("a numpy array crosses as a tensor");
        let Imported::Tensor(tensor) = imported else {
            panic!("a numpy array must import as a tensor");
        };
        (tensor, weak)
    });

    // The Python object is alive: the DLPack capsule holds a reference to it.
    Python::attach(|py| {
        assert!(
            !weak.bind(py).call0().expect("the weak reference").is_none(),
            "the numpy array should still be alive while the tensor holds it"
        );
    });

    // Drop it from a thread that has never attached to the interpreter.
    let dropper = std::thread::spawn(move || drop(tensor));
    dropper.join().expect("the dropping thread survived");

    // The deleter ran, and the reference it held is gone. On a free threaded interpreter the
    // object itself is freed by the thread that owns it: biased reference counting sends a
    // decrement from another thread to the owner's queue, which the owner drains the next time
    // it runs interpreter code, so the weak reference clears on the main thread's next call
    // rather than inside the dropping thread. That is the interpreter's bookkeeping, not the
    // adapter's: what AD-I5 asks is that the deleter attach and run from whatever thread drops
    // the tensor, which is what the released reference shows.
    Python::attach(|py| {
        py.import("gc")
            .expect("gc")
            .call_method0("collect")
            .expect("the owning thread drains its queue");
        assert!(
            weak.bind(py).call0().expect("the weak reference").is_none(),
            "the numpy array outlived the tensor; the deleter did not run"
        );
    });
}
