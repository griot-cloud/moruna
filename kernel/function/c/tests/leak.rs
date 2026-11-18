//! Nothing leaks: every byte the library allocates for a task and its batches, through every
//! path the C functions of tests/header.rs take (answers, refusals, batches held past the
//! finish, columns moved out), is freed by the time each run is over. A counting allocator
//! measures the whole program, so this file holds one test, which nothing else runs beside.
//! What the C side allocates, the C functions count themselves (tests/c/support.c).

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::ffi::c_int;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicIsize, Ordering};

use arrow_array::{ArrayRef, Int64Array, RecordBatch, StringArray};
use common::*;
use sdk::host::Plane;

/// The global allocator, counting the bytes allocated and not freed.
struct Counting;

static LIVE: AtomicIsize = AtomicIsize::new(0);

// SAFETY: every call is passed on to the system allocator as it came.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        // SAFETY: the caller's contract, passed on.
        unsafe { System.dealloc(p, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn pair() -> RecordBatch {
    let a: ArrayRef = Arc::new(Int64Array::from(vec![Some(1), None, Some(3)]));
    let s: ArrayRef = Arc::new(StringArray::from(vec!["x", "yy", "zzz"]));
    RecordBatch::try_from_iter([("a", a), ("s", s)]).unwrap()
}

/// One run of every C function, each on its own plane, every outcome dropped.
fn every_path() {
    type Main = Box<dyn FnOnce() -> c_int + Send>;
    let mut runs: Vec<(Vec<RecordBatch>, Main)> = vec![
        (
            vec![pair(), ints(0, 0)],
            Box::new(|| unsafe { moruna_test_echo() }),
        ),
        (
            vec![ints(0, 3), ints(3, 2)],
            Box::new(|| unsafe { moruna_test_copy() }),
        ),
        (
            vec![ints(0, 2), ints(9, 2)],
            Box::new(|| unsafe { moruna_test_hold_two() }),
        ),
        (
            vec![ints(0, 1)],
            Box::new(|| unsafe { moruna_test_keep(0) }),
        ),
        (
            vec![ints(0, 1), ints(1, 1)],
            Box::new(|| unsafe { moruna_test_keep(1) }),
        ),
        (
            vec![ints(0, 1)],
            Box::new(|| unsafe { moruna_test_refuse() }),
        ),
        (vec![ints(0, 1)], Box::new(|| unsafe { moruna_test_log() })),
        (
            vec![ints(0, 1)],
            Box::new(|| unsafe { moruna_test_null_task() }),
        ),
        (
            vec![ints(0, 1)],
            Box::new(|| unsafe { moruna_test_null_out() }),
        ),
        (
            vec![pair()],
            Box::new(|| unsafe { moruna_test_child_moved(0) }),
        ),
        (
            vec![pair()],
            Box::new(|| unsafe { moruna_test_child_moved(1) }),
        ),
        (
            vec![pair(), ints(5, 4)],
            Box::new(|| unsafe { moruna_example_double() }),
        ),
        (
            vec![ints(i64::MAX - 1, 1)],
            Box::new(|| unsafe { moruna_example_double() }),
        ),
        (
            vec![ints(0, 3), ints(3, 2)],
            Box::new(|| unsafe { moruna_test_sum() }),
        ),
    ];
    for which in 0..11 {
        runs.push((
            vec![ints(0, 1)],
            Box::new(move || unsafe { moruna_test_emit_bad(which) }),
        ));
    }
    for (inputs, main) in runs {
        let outcome = Plane::new().run(inputs, move || exit(main())).unwrap();
        assert_ne!(outcome.returned, ExitCode::from(99), "{:?}", outcome.log);
    }
}

#[test]
fn every_byte_allocated_for_a_task_is_freed() {
    // A first round, for what std and arrow-rs set up once and keep.
    without_a_task();
    every_path();
    every_path();
    let before = LIVE.load(Ordering::SeqCst);
    for _ in 0..3 {
        every_path();
    }
    let after = LIVE.load(Ordering::SeqCst);
    assert_eq!(
        after - before,
        0,
        "bytes still allocated after three rounds"
    );
}
