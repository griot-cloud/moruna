//! `double`: doubles every `Int64` column of each batch and passes the other columns through.
//! A value whose double does not fit an `Int64` refuses the batch.

use std::process::ExitCode;
use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::Int64Type;
use arrow_array::{ArrayRef, RecordBatch};
use moruna_function::{Emit, FunctionError};

fn double(batch: RecordBatch, out: &mut Emit) -> Result<(), FunctionError> {
    let mut columns: Vec<ArrayRef> = Vec::with_capacity(batch.num_columns());
    for (field, column) in batch.schema().fields().iter().zip(batch.columns()) {
        match column.as_primitive_opt::<Int64Type>() {
            Some(values) => {
                let doubled = values.try_unary::<_, Int64Type, _>(|v| {
                    v.checked_mul(2).ok_or_else(|| {
                        FunctionError::new(format!("{v} doubled is past Int64 in {}", field.name()))
                    })
                })?;
                columns.push(Arc::new(doubled));
            }
            None => columns.push(column.clone()),
        }
    }
    out.batch(&RecordBatch::try_new(batch.schema(), columns)?)
}

fn main() -> ExitCode {
    moruna_function::serve(double)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{Array, Int64Array, StringArray};
    use moruna_function::code;
    use moruna_function::host::Plane;

    fn input(values: Vec<Option<i64>>) -> RecordBatch {
        let n: ArrayRef = Arc::new(Int64Array::from(values));
        let s: ArrayRef = Arc::new(StringArray::from(vec!["kept"; n.len()]));
        RecordBatch::try_from_iter([("n", n), ("s", s)]).unwrap()
    }

    #[test]
    fn doubles_the_integers_and_keeps_the_rest() {
        let outcome = Plane::new()
            .named("double")
            .run([input(vec![Some(1), None, Some(-21)]), input(vec![])], main)
            .unwrap();
        assert_eq!(outcome.said, Some(code::DONE));
        assert_eq!(
            outcome.batches(),
            [input(vec![Some(2), None, Some(-42)]), input(vec![])]
        );
        assert_eq!(outcome.log, ["double: answered 2 batches"]);
    }

    #[test]
    fn an_overflow_refuses_the_batch() {
        let outcome = Plane::new()
            .named("double")
            .run([input(vec![Some(i64::MAX)])], main)
            .unwrap();
        assert_eq!(outcome.said, Some(code::REFUSED));
        assert!(!outcome.answers[0].released);
        assert_eq!(
            outcome.log,
            [format!("double: {} doubled is past Int64 in n", i64::MAX)]
        );
    }
}
