//! A function reads its params and the run's arguments (item 5.5) on the data plane in memory,
//! from the process's environment, as a native function on the core reads what its start slot
//! `env` gave std. The one test of its own test program, so setting the variables races no other
//! thread.

use std::process::ExitCode;
use std::sync::Arc;

use arrow_array::{ArrayRef, Float64Array, Int64Array, RecordBatch};
use moruna_function::host::Plane;
use moruna_function::{Emit, FunctionError, code, serve};

/// Each batch answered by one row: the params' threshold and the arguments' year.
fn threshold(_batch: RecordBatch, out: &mut Emit) -> Result<(), FunctionError> {
    let threshold = moruna_function::params()?
        .get("threshold")
        .and_then(|t| t.as_f64())
        .ok_or("the params name no threshold")?;
    let year = moruna_function::args()?
        .get("year")
        .and_then(|y| y.as_i64())
        .ok_or("the arguments name no year")?;
    let t: ArrayRef = Arc::new(Float64Array::from(vec![threshold]));
    let y: ArrayRef = Arc::new(Int64Array::from(vec![year]));
    out.batch(&RecordBatch::try_from_iter([
        ("threshold", t),
        ("year", y),
    ])?)
}

#[test]
fn a_function_reads_its_params_and_the_runs_arguments() {
    let a: ArrayRef = Arc::new(Int64Array::from(vec![1, 2, 3]));
    let input = RecordBatch::try_from_iter([("a", a)]).unwrap();
    // Before the variables are set: none, so the function refuses its batch.
    let outcome = Plane::new()
        .named("threshold")
        .run([input.clone()], || serve(threshold))
        .unwrap();
    assert_eq!(outcome.said, Some(code::REFUSED), "{:?}", outcome.log);
    assert!(
        outcome
            .log
            .iter()
            .any(|l| l.contains("the params name no threshold")),
        "{:?}",
        outcome.log
    );
    // SAFETY: this program's one test, and no thread of its runs now.
    unsafe {
        std::env::set_var("MORUNA_PARAMS", r#"{"threshold":0.5}"#);
        std::env::set_var("MORUNA_ARGS", r#"{"year":2024}"#);
    }
    let outcome = Plane::new()
        .named("threshold")
        .run([input], || serve(threshold))
        .unwrap();
    assert_eq!(outcome.said, Some(code::DONE), "{:?}", outcome.log);
    assert_eq!(outcome.returned, ExitCode::from(0));
    let t: ArrayRef = Arc::new(Float64Array::from(vec![0.5]));
    let y: ArrayRef = Arc::new(Int64Array::from(vec![2024]));
    let expected = RecordBatch::try_from_iter([("threshold", t), ("year", y)]).unwrap();
    assert_eq!(outcome.batches(), [expected]);
    // Params that are not an object end the task with the variable named.
    // SAFETY: as above.
    unsafe { std::env::set_var("MORUNA_PARAMS", "[0.5]") };
    let a: ArrayRef = Arc::new(Int64Array::from(vec![1]));
    let outcome = Plane::new()
        .named("threshold")
        .run([RecordBatch::try_from_iter([("a", a)]).unwrap()], || {
            serve(threshold)
        })
        .unwrap();
    assert_eq!(outcome.said, Some(code::REFUSED));
    assert!(
        outcome
            .log
            .iter()
            .any(|l| l.contains("MORUNA_PARAMS is a JSON object, not an array")),
        "{:?}",
        outcome.log
    );
}
