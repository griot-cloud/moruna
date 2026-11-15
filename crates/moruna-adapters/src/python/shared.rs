//! Deferred reference counting for what a function shares across workers (05 f.13, 2026-11-14).
//!
//! In the free-threaded build a worker that reads a module global, or calls a method on one,
//! changes that object's reference count with an atomic instruction on a field every worker
//! changes, so workers running the same function at once fight over one cache line for every
//! lookup table, compiled pattern and helper they share. On the benchmark VM a function that
//! looks up two module-level dicts per row (Overture's building attributes) spent 2.6 to 3 times
//! as long in `LOAD_GLOBAL_MODULE` and `CALL_METHOD_DESCRIPTOR_FAST` with eight workers as one
//! worker does per row, and ran 39.5 s on eight files against the plain loop's 36.4; one worker
//! against one thread, Moruna was 13% faster. CPython 3.14's
//! `PyUnstable_Object_EnableDeferredRefcount` takes such an object out of reference counting
//! (the garbage collector frees it instead), and with it enabled on the function's module
//! objects the run took 35.2 s.
//!
//! When a kernel is built, on the thread that builds it (the caller of `moruna.run`, which is
//! normally the thread that imported the function's module), the objects the function reaches
//! by its own definition are marked: the callable, its module's globals, its defaults and its
//! closure, a bound method's instance, the attributes of a callable object or of a task a job
//! holds by weak reference, and the same one level further for a function found there. Only
//! special attributes are read, so no user code runs; other modules' namespaces are not entered;
//! the walk stops after `MOST` objects. Where the runtime has no such call (a build with the
//! GIL, a Python before 3.14) nothing is done.

use std::collections::HashSet;
use std::ffi::c_int;
use std::sync::OnceLock;

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyFunction, PyModule, PyTuple, PyWeakrefReference};

type Enable = unsafe extern "C" fn(*mut pyo3::ffi::PyObject) -> c_int;

/// The most objects one kernel's walk marks.
const MOST: usize = 4096;
/// How far from the callable the walk goes.
const DEPTH: u8 = 2;

/// The runtime's `PyUnstable_Object_EnableDeferredRefcount`, when it has one.
fn enable() -> Option<Enable> {
    static FOUND: OnceLock<Option<usize>> = OnceLock::new();
    let address = *FOUND.get_or_init(|| {
        // SAFETY: `dlsym` with `RTLD_DEFAULT` looks a symbol up in the process's global scope
        // and returns null when there is none; the name is a NUL-terminated literal.
        let symbol = unsafe {
            libc::dlsym(
                libc::RTLD_DEFAULT,
                c"PyUnstable_Object_EnableDeferredRefcount".as_ptr(),
            )
        };
        (!symbol.is_null()).then_some(symbol as usize)
    });
    // SAFETY: the address is the runtime's own function of that name, whose signature is
    // `int (PyObject *)` in every CPython that has it (3.14 onward).
    address.map(|address| unsafe { std::mem::transmute::<usize, Enable>(address) })
}

/// Mark what `callable` (and `origin`, the object the user wrote when the callable wraps it)
/// shares across workers. Returns how many objects took the mark.
pub(crate) fn defer_shared(
    callable: &Bound<'_, PyAny>,
    origin: Option<&Bound<'_, PyAny>>,
) -> usize {
    let Some(enable) = enable() else {
        return 0;
    };
    let mut seen: HashSet<usize> = HashSet::new();
    let mut marked = 0usize;
    let mut stack: Vec<(Bound<'_, PyAny>, u8)> = vec![(callable.clone(), 0)];
    if let Some(origin) = origin {
        stack.push((origin.clone(), 0));
    }
    while let Some((obj, depth)) = stack.pop() {
        if seen.len() >= MOST || !seen.insert(obj.as_ptr() as usize) {
            continue;
        }
        // SAFETY: `obj` is a live object the interpreter is attached for; the call only sets a
        // flag on it, is documented as thread-safe and cannot fail.
        marked += usize::from(unsafe { enable(obj.as_ptr()) } == 1);
        if depth >= DEPTH || obj.is_instance_of::<PyModule>() {
            continue;
        }
        let next = depth + 1;
        if let Ok(function) = obj.cast::<PyFunction>() {
            if let Ok(globals) = function.getattr("__globals__")
                && let Ok(globals) = globals.cast::<PyDict>()
            {
                for (_, value) in globals.iter() {
                    stack.push((value, next));
                }
            }
            for name in ["__defaults__", "__closure__"] {
                if let Ok(items) = function.getattr(name)
                    && let Ok(items) = items.cast::<PyTuple>()
                {
                    for item in items.iter() {
                        let item = item.getattr("cell_contents").unwrap_or(item);
                        stack.push((item, next));
                    }
                }
            }
            continue;
        }
        if let Ok(target) = obj.cast::<PyWeakrefReference>() {
            if let Some(target) = target.upgrade() {
                stack.push((target, next));
            }
            continue;
        }
        // A bound method's instance and function, and the attributes of an instance: read
        // through the generic slots only, so no `__getattr__` of the user's runs.
        for name in ["__self__", "__func__"] {
            if let Ok(value) = generic_attr(&obj, name) {
                stack.push((value, next));
            }
        }
        if let Ok(dict) = generic_attr(&obj, "__dict__")
            && let Ok(dict) = dict.cast::<PyDict>()
        {
            for (_, value) in dict.iter() {
                stack.push((value, next));
            }
        }
    }
    marked
}

/// `name` on `obj` by `PyObject_GenericGetAttr`, which reads the type's descriptors and the
/// instance dict and never calls a `__getattr__` the user defined.
fn generic_attr<'py>(obj: &Bound<'py, PyAny>, name: &str) -> PyResult<Bound<'py, PyAny>> {
    let py = obj.py();
    let name = pyo3::types::PyString::new(py, name);
    // SAFETY: both pointers are live for the call; the result is a new reference or null with
    // an exception set, which `from_owned_ptr_or_err` takes in either case.
    unsafe {
        Bound::from_owned_ptr_or_err(
            py,
            pyo3::ffi::PyObject_GenericGetAttr(obj.as_ptr(), name.as_ptr()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = r#"
import math, sys

TABLE = {"residential": 3.0, "commercial": 4.0}

def helper(x):
    return TABLE.get(x, 3.2)

def kernel(batch):
    return helper(batch)

def counts():
    return sys.getrefcount(TABLE), sys.getrefcount(helper), sys.getrefcount(math.pi)
"#;

    /// AD-T28 (f.13): building over a function marks what its module holds: a module-level
    /// dict and a helper function leave reference counting (their counts jump by the deferred
    /// bias), on a free-threaded runtime that has the call; elsewhere nothing changes.
    #[test]
    fn ad_t28_a_functions_module_objects_are_deferred() {
        Python::attach(|py| {
            let code = std::ffi::CString::new(SOURCE).expect("no nul");
            let module = PyModule::from_code(py, &code, c"ad_t28.py", c"ad_t28").expect("imports");
            let before: (isize, isize, isize) =
                module.call_method0("counts").unwrap().extract().unwrap();
            let kernel = module.getattr("kernel").unwrap();
            let marked = defer_shared(&kernel, None);
            let after: (isize, isize, isize) =
                module.call_method0("counts").unwrap().extract().unwrap();
            let free_threaded = py
                .import("sys")
                .unwrap()
                .call_method0("_is_gil_enabled")
                .unwrap()
                .extract::<bool>()
                .map(|gil| !gil)
                .unwrap_or(false);
            if free_threaded && enable().is_some() {
                assert!(marked >= 2, "the dict and the helper at least: {marked}");
                assert!(
                    after.0 > before.0 + 1000,
                    "the dict is deferred: {before:?} {after:?}"
                );
            } else {
                assert_eq!(marked, 0);
                assert_eq!(after.0, before.0);
            }
            // A float is not a GC object: no mark, its count as it was.
            assert_eq!(after.2, before.2);
        });
    }
}
