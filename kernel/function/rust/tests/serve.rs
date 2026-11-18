//! `serve` on the data plane in memory: the task on a thread of its own, the engine's side
//! driving the core's ring protocol, records checked by the engine's check and read back.

use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arrow_array::types::Int32Type;
use arrow_array::{
    Array, ArrayRef, BooleanArray, Float64Array, Int64Array, ListArray, RecordBatch, StringArray,
    StructArray,
};
use arrow_schema::{DataType, Field};
use moruna_function::host::{HostError, Input, Plane, record};
use moruna_function::{Emit, Function, FunctionError, code, log, serve};

fn ints(from: i64, n: i64) -> RecordBatch {
    let a: ArrayRef = Arc::new(Int64Array::from_iter_values(from..from + n));
    RecordBatch::try_from_iter([("a", a)]).unwrap()
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

fn echo(b: RecordBatch, out: &mut Emit) -> Result<(), FunctionError> {
    out.batch(&b)
}

#[test]
fn every_batch_comes_back_as_it_went_in() {
    let inputs = vec![mixed(100), ints(0, 0), mixed(3000), ints(5, 1)];
    let outcome = Plane::new()
        .named("echo")
        .run(inputs.clone(), || serve(echo))
        .unwrap();
    assert_eq!(outcome.said, Some(code::DONE));
    assert_eq!(outcome.returned, ExitCode::from(0));
    assert_eq!(outcome.batches(), inputs);
    assert!(
        outcome
            .answers
            .iter()
            .all(|a| a.released && a.records.len() == 1)
    );
    assert!(outcome.refused.is_empty());
    assert_eq!(outcome.log, ["echo: answered 4 batches"]);
}

#[test]
fn a_function_may_answer_with_many_batches_or_none() {
    let outcome = Plane::new()
        .run([ints(0, 10), ints(10, 10), ints(20, 10)], || {
            serve(|b, out| {
                let first = b
                    .column(0)
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .unwrap()
                    .value(0);
                if first == 10 {
                    return Ok(());
                }
                out.batch(&b.slice(0, 4))?;
                out.batch(&b.slice(4, 6))
            })
        })
        .unwrap();
    assert_eq!(outcome.said, Some(code::DONE));
    let counts: Vec<usize> = outcome.answers.iter().map(|a| a.batches.len()).collect();
    assert_eq!(counts, [2, 0, 2]);
    assert!(outcome.answers.iter().all(|a| a.released));
    assert_eq!(
        outcome.batches(),
        [ints(0, 4), ints(4, 6), ints(20, 4), ints(24, 6)]
    );
}

#[test]
fn an_answer_bigger_than_a_window_comes_back_in_parts_in_order() {
    let input = mixed(20_000);
    let outcome = Plane::new()
        .windows(2, 128 << 10)
        .run([input.clone()], || serve(echo))
        .unwrap();
    assert_eq!(outcome.said, Some(code::DONE));
    let answer = &outcome.answers[0];
    assert!(answer.records.len() > 3, "{} records", answer.records.len());
    assert!(answer.records.iter().all(|r| r.len() <= 128 << 10));
    let parts = concat(&answer.batches);
    assert_eq!(parts, input);
}

/// The batches joined, row for row.
fn concat(batches: &[RecordBatch]) -> RecordBatch {
    let schema = batches[0].schema();
    let columns = (0..schema.fields().len())
        .map(|i| {
            let parts: Vec<&dyn Array> = batches.iter().map(|b| b.column(i).as_ref()).collect();
            arrow_select::concat::concat(&parts).unwrap()
        })
        .collect();
    RecordBatch::try_new(schema, columns).unwrap()
}

#[test]
fn a_row_bigger_than_a_window_ends_the_task() {
    let outcome = Plane::new()
        .named("wide")
        .windows(1, 4096)
        .run([ints(0, 1)], || serve(echo))
        .unwrap();
    assert_eq!(outcome.said, Some(code::WINDOW_SMALL));
    assert_eq!(outcome.returned, ExitCode::from(8));
    assert!(!outcome.answers[0].released);
    assert!(
        outcome.log[0].starts_with("wide: the output record takes "),
        "{:?}",
        outcome.log
    );
}

#[test]
fn many_inputs_reuse_a_small_region_and_one_window() {
    let inputs: Vec<RecordBatch> = (0..40).map(|i| ints(i * 100, 100)).collect();
    let outcome = Plane::new()
        .region(5 * 4096)
        .windows(1, 3 * 4096)
        .run(inputs.clone(), || {
            serve(|b, out| {
                out.batch(&b)?;
                out.batch(&b)
            })
        })
        .unwrap();
    assert_eq!(outcome.said, Some(code::DONE));
    let doubled: Vec<RecordBatch> = inputs.iter().flat_map(|b| [b.clone(), b.clone()]).collect();
    assert_eq!(outcome.batches(), doubled);
}

#[test]
fn a_returned_error_ends_the_task_with_its_message() {
    let outcome = Plane::new()
        .named("picky")
        .run([ints(0, 3), ints(3, 3)], || {
            serve(|_b, _out| Err(FunctionError::new("no column named x")))
        })
        .unwrap();
    assert_eq!(outcome.said, Some(code::REFUSED));
    assert_eq!(outcome.returned, ExitCode::from(7));
    assert_eq!(outcome.log, ["picky: no column named x"]);
    assert!(
        outcome
            .answers
            .iter()
            .all(|a| !a.released && a.records.is_empty())
    );
}

#[test]
fn a_kept_batch_ends_the_task_naming_the_function_and_is_never_released() {
    for keep in ["batch", "column", "slice", "buffer"] {
        let kept: Arc<Mutex<Vec<Box<dyn std::any::Any + Send>>>> = Arc::default();
        let held = kept.clone();
        let outcome = Plane::new()
            .named("hoarder")
            .run([ints(0, 8), ints(8, 8)], move || {
                serve(move |b, out| {
                    out.batch(&b)?;
                    let part: Box<dyn std::any::Any + Send> = match keep {
                        "batch" => Box::new(b),
                        "column" => Box::new(b.column(0).clone()),
                        "slice" => Box::new(b.slice(2, 2)),
                        _ => Box::new(b.column(0).to_data().buffers()[0].clone()),
                    };
                    held.lock().unwrap().push(part);
                    Ok(())
                })
            })
            .unwrap();
        assert_eq!(outcome.said, Some(code::REFUSED), "{keep}");
        assert_eq!(
            outcome.answers[0].batches,
            [ints(0, 8)],
            "{keep}: answered, then kept"
        );
        assert!(!outcome.answers[0].released, "{keep}");
        assert!(outcome.answers[1].records.is_empty(), "{keep}");
        assert_eq!(outcome.log.len(), 1);
        assert!(
            outcome.log[0].starts_with("hoarder: the function kept its input batch"),
            "{keep}: {:?}",
            outcome.log
        );
        // What it kept is still readable: the memory lives while it is held.
        assert_eq!(kept.lock().unwrap().len(), 1);
    }
}

#[test]
fn what_a_function_copies_is_its_own() {
    let copies: Arc<Mutex<Vec<Int64Array>>> = Arc::default();
    let held = copies.clone();
    let outcome = Plane::new()
        .run([ints(0, 4), ints(4, 4)], move || {
            serve(move |b, _out| {
                let a = b.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
                held.lock()
                    .unwrap()
                    .push(Int64Array::from(a.values().to_vec()));
                Ok(())
            })
        })
        .unwrap();
    assert_eq!(outcome.said, Some(code::DONE));
    let copies = copies.lock().unwrap();
    assert_eq!(copies[1].values().to_vec(), [4, 5, 6, 7]);
}

#[test]
fn lines_logged_by_the_function_are_the_tasks() {
    let outcome = Plane::new()
        .named("chatty")
        .run([ints(0, 2)], || {
            serve(|b, out| {
                log(format!("{} rows", b.num_rows()));
                log("two\nlines");
                log("");
                out.batch(&b)
            })
        })
        .unwrap();
    assert_eq!(
        outcome.log,
        ["2 rows", "two", "lines", "", "chatty: answered 1 batches"]
    );
}

#[test]
fn serve_outside_a_plane_or_twice_finds_none() {
    assert_eq!(serve(echo), ExitCode::from(code::NO_PLANE as u8));
    log("a line outside any run goes to standard error");
    let outcome = Plane::new()
        .run([ints(0, 1)], || {
            let first = serve(echo);
            let second = serve(echo);
            assert_eq!(second, ExitCode::from(2));
            first
        })
        .unwrap();
    assert_eq!(outcome.said, Some(code::DONE));
    assert!(outcome.log[1].contains("no plane"), "{:?}", outcome.log);
}

#[test]
fn damaged_records_are_refused() {
    let good = record(&mixed(50)).unwrap();
    let inputs = vec![
        Input::Record(Vec::new()),
        Input::Record(good[..100].to_vec()),
        Input::Record(vec![0xff; 4096]),
    ];
    for input in inputs {
        let outcome = Plane::new().run([input], || serve(echo)).unwrap();
        assert_eq!(outcome.said, Some(code::REFUSED));
        assert!(
            outcome.log[0].starts_with("function: the input record: "),
            "{:?}",
            outcome.log
        );
    }
    let outcome = Plane::new()
        .run([Input::Record(good)], || serve(echo))
        .unwrap();
    assert_eq!(outcome.batches(), [mixed(50)]);
}

#[test]
fn a_task_that_never_serves_says_nothing() {
    let outcome = Plane::new()
        .run([ints(0, 1)], || ExitCode::from(42))
        .unwrap();
    assert_eq!((outcome.said, outcome.returned), (None, ExitCode::from(42)));
    assert!(!outcome.answers[0].released);
}

#[test]
fn a_run_says_why_it_did_not_complete() {
    let bad = Plane::new()
        .windows(0, 4096)
        .run([ints(0, 1)], || serve(echo));
    assert!(matches!(bad, Err(HostError::Geometry(_))), "{bad:?}");
    let big = Plane::new()
        .region(4096)
        .run([ints(0, 10_000)], || serve(echo));
    assert!(
        matches!(big, Err(HostError::TooBig { region: 4096, .. })),
        "{big:?}"
    );
    let d: ArrayRef = Arc::new(
        vec!["a", "b"]
            .into_iter()
            .collect::<arrow_array::DictionaryArray<Int32Type>>(),
    );
    let dict = RecordBatch::try_from_iter([("d", d)]).unwrap();
    let input = Plane::new().run([dict], || serve(echo));
    assert!(matches!(input, Err(HostError::Input(_))), "{input:?}");
    let panicked = Plane::new().run([ints(0, 1)], || serve(|_b, _out| panic!("oops")));
    assert!(
        matches!(&panicked, Err(HostError::Panicked(m)) if m == "oops"),
        "{panicked:?}"
    );
    let slow = Plane::new()
        .timeout(Duration::from_millis(50))
        .run([ints(0, 1)], || {
            std::thread::sleep(Duration::from_millis(400));
            ExitCode::SUCCESS
        });
    assert!(matches!(slow, Err(HostError::TimedOut)), "{slow:?}");
}

#[test]
fn a_dictionary_output_is_refused_by_name() {
    let outcome = Plane::new()
        .run([ints(0, 2)], || {
            serve(|_b, out| {
                let d: ArrayRef = Arc::new(
                    vec!["a", "b"]
                        .into_iter()
                        .collect::<arrow_array::DictionaryArray<Int32Type>>(),
                );
                out.batch(&RecordBatch::try_from_iter([("d", d)])?)
            })
        })
        .unwrap();
    assert_eq!(outcome.said, Some(code::REFUSED));
    assert!(
        outcome.log[0].contains("column \"d\" is dictionary encoded"),
        "{:?}",
        outcome.log
    );
}

/// The sum of column "a" over every batch, written once at the end, and the batches passed
/// through as they come.
#[derive(Default)]
struct Sum(i64);

impl Function for Sum {
    fn batch(&mut self, b: RecordBatch, out: &mut Emit) -> Result<(), FunctionError> {
        let a = b.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
        self.0 += a.values().iter().sum::<i64>();
        out.batch(&b)
    }

    fn end(&mut self, out: &mut Emit) -> Result<(), FunctionError> {
        if self.0 < 0 {
            return Err(FunctionError::new("a negative sum"));
        }
        out.batch(&ints(self.0, 1))
    }
}

fn answers_of(outcome: &moruna_function::host::Outcome) -> Vec<Vec<RecordBatch>> {
    outcome.answers.iter().map(|a| a.batches.clone()).collect()
}

#[test]
fn a_function_with_an_end_writes_once_the_input_has_ended() {
    let outcome = Plane::new()
        .named("sum")
        .run([ints(0, 3), ints(3, 4)], || Sum::default().serve())
        .unwrap();
    assert_eq!(outcome.said, Some(code::DONE));
    assert_eq!(answers_of(&outcome), [vec![ints(0, 3)], vec![ints(3, 4)]]);
    assert!(outcome.answers.iter().all(|a| a.released));
    assert_eq!(outcome.unheld.batches, [ints(21, 1)]);
    assert_eq!(
        outcome.batches(),
        [ints(0, 3), ints(3, 4), ints(21, 1)],
        "the answers, then what answers none"
    );
    assert_eq!(outcome.log, ["sum: answered 2 batches"]);
    // No input: the end alone.
    let none: [RecordBatch; 0] = [];
    let outcome = Plane::new().run(none, || Sum::default().serve()).unwrap();
    assert_eq!(outcome.said, Some(code::DONE));
    assert_eq!(outcome.batches(), [ints(0, 1)]);
}

#[test]
fn an_end_that_fails_ends_the_task() {
    let outcome = Plane::new()
        .named("sum")
        .run([ints(-5, 2)], || Sum::default().serve())
        .unwrap();
    assert_eq!(outcome.said, Some(code::REFUSED));
    assert_eq!(outcome.returned, ExitCode::from(7));
    assert!(outcome.answers[0].released, "answered before the end");
    assert!(outcome.unheld.records.is_empty());
    assert_eq!(outcome.log, ["sum: a negative sum"]);
    let outcome = Plane::new()
        .windows(1, 4096)
        .run([ints(0, 0)], || Sum::default().serve())
        .unwrap();
    assert_eq!(outcome.said, Some(code::WINDOW_SMALL));
    // A function served twice in one task finds no plane the second time.
    let outcome = Plane::new()
        .run([ints(0, 1)], || {
            let first = Sum::default().serve();
            assert_eq!(Sum::default().serve(), ExitCode::from(2));
            first
        })
        .unwrap();
    assert_eq!(outcome.said, Some(code::DONE));
}
