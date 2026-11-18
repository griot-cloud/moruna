//! `moruna_params` (item 5.5), driven from C (tests/c/params.c) on the data plane in memory: the
//! function's params as the variable gives them, the same text `getenv` reads, its four variables
//! in its log, and its threshold read back as a record. The one test of its own test program, so
//! setting the variables races no other thread, and `moruna_params` reads them once.

mod common;

use std::sync::Arc;

use arrow_array::{ArrayRef, Float64Array, RecordBatch};
use common::*;
use sdk::code;
use sdk::host::Plane;

#[test]
fn a_c_function_reads_its_params_as_getenv_does() {
    // SAFETY: this program's one test, and no thread of its runs now.
    unsafe {
        std::env::set_var("MORUNA_ENTRY", "c-params");
        std::env::set_var("MORUNA_ROLE", "function");
        std::env::set_var("MORUNA_PARAMS", r#"{"threshold":0.5}"#);
        std::env::set_var("MORUNA_ARGS", "{}");
    }
    let outcome = Plane::new()
        .named("c-params")
        .run([ints(0, 3), ints(3, 2)], || {
            exit(unsafe { moruna_test_params() })
        })
        .unwrap();
    assert_eq!(outcome.said, Some(code::DONE), "{:?}", outcome.log);
    assert!(
        outcome
            .answers
            .iter()
            .all(|a| a.released && a.records.is_empty())
    );
    let t: ArrayRef = Arc::new(Float64Array::from(vec![0.5]));
    let threshold = RecordBatch::try_from_iter([("threshold", t)]).unwrap();
    assert_eq!(outcome.unheld.batches, [threshold]);
    assert_eq!(
        outcome.log,
        [
            r#"params: entry c-params, role function, params {"threshold":0.5}, args {}"#,
            "c-params: answered 2 batches"
        ]
    );
    // SAFETY: as above; the params were read once, for the program's life.
    unsafe { std::env::set_var("MORUNA_PARAMS", r#"{"threshold":0.25}"#) };
    // SAFETY: the library's text, NUL-terminated and never freed.
    let text = unsafe { std::ffi::CStr::from_ptr(sdk_params()) };
    assert_eq!(text, c"{\"threshold\":0.5}");
    without_a_task();
}

fn sdk_params() -> *const std::ffi::c_char {
    moruna_function::moruna_params()
}
