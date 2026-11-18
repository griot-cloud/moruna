//! What the tests share: the C functions build.rs compiles from tests/c and examples/double.c,
//! and the batches they are given.

#![allow(dead_code)]

use std::ffi::c_int;
use std::process::ExitCode;
use std::ptr;
use std::sync::Arc;

use arrow_array::{ArrayRef, Int64Array, RecordBatch};
use moruna_function::{
    moruna_emit, moruna_error, moruna_finish, moruna_log, moruna_next, moruna_open,
};

unsafe extern "C" {
    pub fn moruna_test_echo() -> c_int;
    pub fn moruna_test_copy() -> c_int;
    pub fn moruna_test_hold_two() -> c_int;
    pub fn moruna_test_keep(which: c_int) -> c_int;
    pub fn moruna_test_refuse() -> c_int;
    pub fn moruna_test_emit_bad(which: c_int) -> c_int;
    pub fn moruna_test_log() -> c_int;
    pub fn moruna_test_null_task() -> c_int;
    pub fn moruna_test_null_out() -> c_int;
    pub fn moruna_test_child_moved(which: c_int) -> c_int;
    pub fn moruna_example_double() -> c_int;
    pub fn moruna_test_array_size() -> usize;
    pub fn moruna_test_schema_size() -> usize;
    pub fn moruna_test_cpp_sizes() -> usize;
    pub fn moruna_test_codes(codes: *mut c_int);
    pub fn moruna_test_sum() -> c_int;
}

/// What a C `main` returns, as the program's exit code.
pub fn exit(r: c_int) -> ExitCode {
    ExitCode::from(u8::try_from(r).unwrap_or(u8::MAX))
}

/// A batch of one Int64 column "a": `n` values from `from`.
pub fn ints(from: i64, n: i64) -> RecordBatch {
    let a: ArrayRef = Arc::new(Int64Array::from_iter_values(from..from + n));
    RecordBatch::try_from_iter([("a", a)]).unwrap()
}

/// Each of the library's six calls with no task, as a program outside any run makes them: each
/// fails without harm. Called from Rust, they are also what links the library's calls into a
/// test program, for its C to find.
pub fn without_a_task() {
    let mut task = ptr::null_mut();
    // SAFETY: null pointers and a NUL-terminated line, which every call takes.
    unsafe {
        assert_eq!(moruna_open(&mut task), -2);
        assert!(task.is_null());
        assert_eq!(moruna_next(task, ptr::null_mut(), ptr::null_mut()), -2);
        assert_eq!(moruna_emit(task, ptr::null_mut(), ptr::null_mut()), -2);
        moruna_log(
            task,
            c"a line outside any run goes to standard error".as_ptr(),
        );
        moruna_error(task, ptr::null());
        assert_eq!(moruna_finish(task), 2);
    }
}
