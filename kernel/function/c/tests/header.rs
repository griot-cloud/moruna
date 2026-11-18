//! The header's calls driven from C (tests/c and examples/double.c, compiled by build.rs with
//! the host's compilers) on the data plane in memory, `moruna_function::host::Plane` playing the
//! engine: rows in and out, ownership and release, errors, and the log.

mod common;

use std::ffi::c_int;
use std::process::ExitCode;
use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::{Int32Type, Int64Type};
use arrow_array::{
    Array, ArrayRef, BooleanArray, Float64Array, Int64Array, ListArray, RecordBatch, StringArray,
    StructArray,
};
use arrow_schema::{DataType, Field};
use common::*;
use sdk::code;
use sdk::host::{Outcome, Plane};

/// `outcome` ended with `code`, said on the rings and returned by `main`.
fn ended(outcome: &Outcome, code: i64) {
    assert_eq!(outcome.said, Some(code), "{:?}", outcome.log);
    assert_eq!(
        outcome.returned,
        ExitCode::from(code as u8),
        "{:?}",
        outcome.log
    );
}

/// A batch of several types, nulls and offsets included.
fn mixed(n: usize) -> RecordBatch {
    let i: ArrayRef = Arc::new(Int64Array::from_iter(
        (0..n as i64).map(|v| (v % 3 != 0).then_some(v)),
    ));
    let f: ArrayRef = Arc::new(Float64Array::from_iter_values(
        (0..n).map(|v| v as f64 / 7.0),
    ));
    let s: ArrayRef = Arc::new(StringArray::from_iter(
        (0..n).map(|v| (v % 5 != 0).then(|| format!("s{v}"))),
    ));
    let b: ArrayRef = Arc::new(BooleanArray::from_iter((0..n).map(|v| Some(v % 2 == 0))));
    let l: ArrayRef = Arc::new(ListArray::from_iter_primitive::<Int32Type, _, _>(
        (0..n).map(|v| Some((0..(v % 4) as i32).map(Some).collect::<Vec<_>>())),
    ));
    let st: ArrayRef = Arc::new(StructArray::from(vec![(
        Arc::new(Field::new("x", DataType::Int64, true)),
        i.clone(),
    )]));
    RecordBatch::try_from_iter([("i", i), ("f", f), ("s", s), ("b", b), ("l", l), ("st", st)])
        .unwrap()
        .slice(1, n - 2)
}

fn answers(outcome: &Outcome) -> Vec<Vec<RecordBatch>> {
    outcome.answers.iter().map(|a| a.batches.clone()).collect()
}

#[test]
fn every_batch_comes_back_as_it_went_in() {
    let inputs = vec![mixed(100), ints(0, 0), mixed(3000), ints(5, 1)];
    let outcome = Plane::new()
        .named("echo")
        .run(inputs.clone(), || exit(unsafe { moruna_test_echo() }))
        .unwrap();
    ended(&outcome, code::DONE);
    assert_eq!(outcome.batches(), inputs);
    assert!(outcome.answers.iter().all(|a| a.released));
    assert!(outcome.refused.is_empty());
    assert_eq!(outcome.log, ["echo: answered 4 batches"]);
}

#[test]
fn many_inputs_pass_through_a_small_region_and_one_window() {
    let inputs: Vec<RecordBatch> = (0..40).map(|i| ints(i * 100, 100)).collect();
    let outcome = Plane::new()
        .region(5 * 4096)
        .windows(1, 3 * 4096)
        .run(inputs.clone(), || exit(unsafe { moruna_test_echo() }))
        .unwrap();
    ended(&outcome, code::DONE);
    assert_eq!(outcome.batches(), inputs);
}

#[test]
fn an_answer_bigger_than_a_window_comes_back_in_parts() {
    let input = mixed(20_000);
    let outcome = Plane::new()
        .windows(2, 128 << 10)
        .run([input.clone()], || exit(unsafe { moruna_test_echo() }))
        .unwrap();
    ended(&outcome, code::DONE);
    let parts = &outcome.answers[0].batches;
    assert!(parts.len() > 3, "{} parts", parts.len());
    let rows: usize = parts.iter().map(|b| b.num_rows()).sum();
    assert_eq!(rows, input.num_rows());
    let mut at = 0;
    for p in parts {
        assert_eq!(*p, input.slice(at, p.num_rows()));
        at += p.num_rows();
    }
}

#[test]
fn a_row_bigger_than_a_window_ends_the_task() {
    let outcome = Plane::new()
        .named("wide")
        .windows(1, 4096)
        .run([ints(0, 1)], || exit(unsafe { moruna_test_echo() }))
        .unwrap();
    ended(&outcome, code::WINDOW_SMALL);
    assert!(!outcome.answers[0].released);
    assert!(
        outcome.log[0].starts_with("wide: the output record takes "),
        "{:?}",
        outcome.log
    );
}

#[test]
fn an_answer_written_after_its_input_is_released_answers_that_input() {
    let outcome = Plane::new()
        .run([ints(0, 3), ints(3, 2)], || {
            exit(unsafe { moruna_test_copy() })
        })
        .unwrap();
    ended(&outcome, code::DONE);
    assert_eq!(
        answers(&outcome),
        [vec![ints(1000, 3)], vec![ints(1003, 2)]]
    );
    assert!(outcome.answers.iter().all(|a| a.released));
}

#[test]
fn answers_go_to_the_oldest_batch_held_and_both_are_given_back() {
    let outcome = Plane::new()
        .run([ints(0, 2), ints(10, 2)], || {
            exit(unsafe { moruna_test_hold_two() })
        })
        .unwrap();
    ended(&outcome, code::DONE);
    assert_eq!(
        answers(&outcome),
        [vec![ints(110, 2), ints(100, 2)], vec![]]
    );
    assert!(outcome.answers.iter().all(|a| a.released));
}

#[test]
fn a_batch_held_at_the_finish_or_a_finish_before_the_end_is_refused() {
    let outcome = Plane::new()
        .named("keeper")
        .run([ints(0, 1)], || exit(unsafe { moruna_test_keep(0) }))
        .unwrap();
    ended(&outcome, code::REFUSED);
    assert!(!outcome.answers[0].released);
    assert!(
        outcome.log[0].starts_with("keeper: the function finished holding 1 batches"),
        "{:?}",
        outcome.log
    );
    let outcome = Plane::new()
        .named("quitter")
        .run([ints(0, 1), ints(1, 1)], || {
            exit(unsafe { moruna_test_keep(1) })
        })
        .unwrap();
    ended(&outcome, code::REFUSED);
    assert!(outcome.answers[0].released);
    assert_eq!(
        outcome.log,
        [format!("quitter: {}", sdk::task::BEFORE_THE_END)]
    );
}

#[test]
fn moruna_error_ends_the_task_and_every_later_call_says_so() {
    let outcome = Plane::new()
        .named("refuse")
        .run([ints(0, 1), ints(1, 1)], || {
            exit(unsafe { moruna_test_refuse() })
        })
        .unwrap();
    ended(&outcome, code::REFUSED);
    assert!(
        outcome
            .answers
            .iter()
            .all(|a| !a.released && a.records.is_empty())
    );
    assert_eq!(
        outcome.log,
        [
            "refuse: no column named x",
            "refuse: the function ended the task with no message"
        ]
    );
}

#[test]
fn what_moruna_emit_cannot_write_is_refused_and_released() {
    let cases = [
        "moruna_emit: the batch or its schema is NULL or released",
        "moruna_emit: the batch or its schema is NULL or released",
        "moruna_emit: the batch or its schema is NULL or released",
        "moruna_emit: the batch: the schema: ",
        "children, where it has 1",
        "column \"d\" is dictionary encoded",
        "the struct array has nulls",
        "has 1 buffers, where it has 2",
        "Offset invariant failure",
        "a schema has no format",
        "a schema's children are NULL",
    ];
    for (which, said) in cases.into_iter().enumerate() {
        let outcome = Plane::new()
            .named("bad")
            .run([ints(0, 1)], move || {
                exit(unsafe { moruna_test_emit_bad(which as c_int) })
            })
            .unwrap();
        ended(&outcome, code::REFUSED);
        assert!(!outcome.answers[0].released, "{which}");
        assert!(outcome.answers[0].records.is_empty(), "{which}");
        assert_eq!(outcome.log.len(), 1, "{which}: {:?}", outcome.log);
        assert!(
            outcome.log[0].starts_with("bad: ") && outcome.log[0].contains(said),
            "{which}: {:?}",
            outcome.log
        );
    }
}

#[test]
fn lines_logged_are_the_tasks_as_given() {
    let outcome = Plane::new()
        .named("log")
        .run([ints(0, 1)], || exit(unsafe { moruna_test_log() }))
        .unwrap();
    ended(&outcome, code::DONE);
    assert_eq!(
        outcome.log,
        [
            "plain",
            "two",
            "lines",
            "",
            "a line with no task",
            "log: answered 1 batches"
        ]
    );
}

#[test]
fn calls_with_no_task_fail_without_harm() {
    let outcome = Plane::new()
        .named("null")
        .run([ints(0, 1)], || exit(unsafe { moruna_test_null_task() }))
        .unwrap();
    ended(&outcome, code::DONE);
    assert_eq!(outcome.batches(), [ints(0, 1)]);
    assert_eq!(outcome.log.len(), 4, "{:?}", outcome.log);
    assert!(outcome.log[0].contains(": no plane: "), "{:?}", outcome.log);
    assert!(outcome.log[1].starts_with("moruna_open: task is NULL"));
    assert_eq!(
        outcome.log[2..],
        [
            "moruna_error with no task: nobody to end",
            "null: answered 1 batches"
        ]
    );
    let outcome = Plane::new()
        .named("null")
        .run([ints(0, 1)], || exit(unsafe { moruna_test_null_out() }))
        .unwrap();
    ended(&outcome, code::REFUSED);
    assert!(outcome.log[0].starts_with("null: moruna_next: out or schema is NULL"));
}

#[test]
fn calls_outside_a_plane_find_none() {
    // The C function outside any run: moruna_open finds no plane.
    assert_eq!(unsafe { moruna_test_echo() }, 2);
    without_a_task();
}

#[test]
fn a_column_moved_out_keeps_its_batch_held() {
    let a: ArrayRef = Arc::new(Int64Array::from(vec![1, 2, 3]));
    let s: ArrayRef = Arc::new(StringArray::from(vec!["x", "yy", "zzz"]));
    let input = RecordBatch::try_from_iter([("a", a), ("s", s)]).unwrap();
    let outcome = Plane::new()
        .run([input.clone()], || {
            exit(unsafe { moruna_test_child_moved(0) })
        })
        .unwrap();
    ended(&outcome, code::DONE);
    assert_eq!(answers(&outcome), [vec![ints(42, 1)]]);
    assert!(outcome.answers[0].released);
    let outcome = Plane::new()
        .named("mover")
        .run([input], || exit(unsafe { moruna_test_child_moved(1) }))
        .unwrap();
    ended(&outcome, code::REFUSED);
    assert_eq!(answers(&outcome), [vec![ints(42, 1)]]);
    assert!(!outcome.answers[0].released);
    assert!(
        outcome.log[0].starts_with("mover: the function finished holding 1 batches"),
        "{:?}",
        outcome.log
    );
}

/// What double.c does, done by arrow-rs: every Int64 column doubled, the rest as they are.
fn doubled(batch: &RecordBatch) -> RecordBatch {
    let columns = batch
        .columns()
        .iter()
        .map(|c| match c.as_primitive_opt::<Int64Type>() {
            Some(v) => Arc::new(v.unary::<_, Int64Type>(|x| x * 2)) as ArrayRef,
            None => c.clone(),
        })
        .collect();
    RecordBatch::try_new(batch.schema(), columns).unwrap()
}

fn double_input(values: Vec<Option<i64>>) -> RecordBatch {
    let n: ArrayRef = Arc::new(Int64Array::from(values));
    let s: ArrayRef = Arc::new(StringArray::from(vec!["kept"; n.len()]));
    RecordBatch::try_from_iter([("n", n), ("s", s)]).unwrap()
}

#[test]
fn double_doubles_the_integers_and_keeps_the_rest() {
    let outcome = Plane::new()
        .named("double")
        .run(
            [
                double_input(vec![Some(1), None, Some(-21)]),
                double_input(vec![]),
            ],
            || exit(unsafe { moruna_example_double() }),
        )
        .unwrap();
    ended(&outcome, code::DONE);
    assert_eq!(
        outcome.batches(),
        [
            double_input(vec![Some(2), None, Some(-42)]),
            double_input(vec![])
        ]
    );
    assert!(outcome.answers.iter().all(|a| a.released));
    assert_eq!(outcome.log, ["double: answered 2 batches"]);
}

#[test]
fn double_passes_every_other_type_through() {
    let inputs = vec![mixed(100), mixed(3000), ints(-5, 10)];
    let outcome = Plane::new()
        .run(inputs.clone(), || exit(unsafe { moruna_example_double() }))
        .unwrap();
    ended(&outcome, code::DONE);
    let want: Vec<RecordBatch> = inputs.iter().map(doubled).collect();
    assert_eq!(outcome.batches(), want);
}

#[test]
fn an_overflow_refuses_the_batch_and_a_null_slot_never_does() {
    let outcome = Plane::new()
        .named("double")
        .run([double_input(vec![Some(i64::MAX)])], || {
            exit(unsafe { moruna_example_double() })
        })
        .unwrap();
    ended(&outcome, code::REFUSED);
    assert!(!outcome.answers[0].released);
    assert_eq!(
        outcome.log,
        [format!("double: {} doubled is past Int64 in n", i64::MAX)]
    );
    // A null whose slot holds a value past the range: no refusal.
    let n: ArrayRef = Arc::new(Int64Array::new(
        vec![i64::MIN, 4].into(),
        Some(vec![false, true].into()),
    ));
    let input = RecordBatch::try_from_iter([("n", n)]).unwrap();
    let outcome = Plane::new()
        .run([input], || exit(unsafe { moruna_example_double() }))
        .unwrap();
    ended(&outcome, code::DONE);
    let want: ArrayRef = Arc::new(Int64Array::from(vec![None, Some(8)]));
    assert_eq!(
        outcome.batches(),
        [RecordBatch::try_from_iter([("n", want)]).unwrap()]
    );
}

#[test]
fn the_structures_are_the_same_in_c_cplusplus_and_rust() {
    use arrow_array::ffi::{FFI_ArrowArray, FFI_ArrowSchema};
    let (array, schema) = (size_of::<FFI_ArrowArray>(), size_of::<FFI_ArrowSchema>());
    unsafe {
        assert_eq!(moruna_test_array_size(), array);
        assert_eq!(moruna_test_schema_size(), schema);
        assert_eq!(moruna_test_cpp_sizes(), array * 1000 + schema);
    }
}

#[test]
fn the_headers_codes_are_moruna_functions() {
    let mut named = [-1 as c_int; 6];
    unsafe { moruna_test_codes(named.as_mut_ptr()) };
    let codes = [
        code::DONE,
        code::NO_PLANE,
        code::NO_RINGS,
        code::RINGS,
        code::REFUSED,
        code::WINDOW_SMALL,
    ];
    assert_eq!(named.map(i64::from), codes);
}

#[test]
fn an_aggregate_writes_before_its_first_batch_and_after_the_end() {
    let outcome = Plane::new()
        .named("sum")
        .run([ints(0, 3), ints(3, 4), ints(0, 0)], || {
            exit(unsafe { moruna_test_sum() })
        })
        .unwrap();
    ended(&outcome, code::DONE);
    assert!(
        outcome
            .answers
            .iter()
            .all(|a| a.released && a.records.is_empty())
    );
    assert_eq!(outcome.unheld.batches, [ints(-1, 1), ints(21, 1)]);
    assert_eq!(outcome.log, ["sum: answered 3 batches"]);
}
