//! `tokenise-explode`: one input row becomes one output row per token
//! (preamble 6.5, amplification 5 to 10).
//!
//! The kernel splits a text column on runs of whitespace and emits one row per
//! token. Every other column of the input is carried on each of that row's
//! tokens, and five columns are added:
//!
//! | Column | Type | What it is |
//! |---|---|---|
//! | `row_id` | i64 | the input row's ordinal in the dataset, counted across morsels |
//! | `token_index` | i64 | the token's position inside its row, from zero |
//! | `token_start` | i64 | the token's first byte in the source value |
//! | `token_end` | i64 | one past its last byte |
//! | `token` | utf8 | the token itself |
//!
//! `row_id` with the offset pair is the standard offset mapping a tokeniser
//! returns, and it is what lets the exploded rows be put back together, so it is
//! there for the same reason a real tokenise stage would carry it and not to
//! reach a number. It is also most of the amplification: a token of the
//! generator's corpus averages about 7.5 bytes and carries 40 bytes of integers,
//! which is why a 2 KiB text row of `text-explode` comes out between five and
//! ten times its size.
//!
//! A null value produces no rows, as does a value with no tokens. `row_id`
//! counts input rows, so the counter lives in the kernel's state and a morsel
//! boundary does not restart it.

use std::sync::Arc;

use arrow::array::UInt32Array;
use arrow::array::{Array, ArrayRef, Int64Builder, RecordBatch, StringArray, StringBuilder};
use arrow::datatypes::{DataType, Field, Schema};

use crate::error::{BenchError, Result};
use crate::kernels::{BenchKernel, BenchKernelHints, BenchKernelState, BenchPayload, PayloadKind};

/// The five columns the kernel adds, in the order it adds them.
pub const ADDED_COLUMNS: [&str; 5] = ["row_id", "token_index", "token_start", "token_end", "token"];

/// The byte ranges of the whitespace separated tokens of `value`, in order.
///
/// Byte ranges rather than slices, because the kernel emits the offsets as
/// columns. Splitting is on `char::is_whitespace`, so a run of any length is one
/// separator and a value of only whitespace has no tokens.
pub fn token_ranges(value: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start: Option<usize> = None;
    for (index, character) in value.char_indices() {
        if character.is_whitespace() {
            if let Some(from) = start.take() {
                ranges.push((from, index));
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(from) = start {
        ranges.push((from, value.len()));
    }
    ranges
}

/// The kernel's per instance state: how many input rows it has seen, which is
/// what `row_id` counts.
#[derive(Debug, Default)]
pub struct TokeniseState {
    rows_seen: u64,
}

impl TokeniseState {
    /// Input rows seen so far by this instance.
    pub fn rows_seen(&self) -> u64 {
        self.rows_seen
    }
}

impl BenchKernelState for TokeniseState {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

/// The tokenise-explode kernel over one named text column.
#[derive(Debug, Clone)]
pub struct TokeniseExplode {
    column: String,
}

impl Default for TokeniseExplode {
    fn default() -> Self {
        TokeniseExplode::new(TokeniseExplode::DEFAULT_COLUMN)
    }
}

impl TokeniseExplode {
    /// The name preamble 6.5 gives it.
    pub const NAME: &'static str = "tokenise-explode";

    /// The text column of the `text-explode` dataset.
    pub const DEFAULT_COLUMN: &'static str = "text_0";

    /// A kernel that explodes `column`.
    pub fn new(column: impl Into<String>) -> TokeniseExplode {
        TokeniseExplode {
            column: column.into(),
        }
    }

    /// The column it reads and replaces.
    pub fn column(&self) -> &str {
        &self.column
    }
}

impl BenchKernel for TokeniseExplode {
    fn name(&self) -> &'static str {
        TokeniseExplode::NAME
    }

    fn accepts(&self) -> PayloadKind {
        PayloadKind::Table
    }

    fn hints(&self) -> BenchKernelHints {
        // Preamble 6.5 gives the band itself: 5 to 10, on `text-explode`.
        BenchKernelHints {
            // Rows out are a few hundred times rows in, so a morsel that is
            // comfortable on the way in is not on the way out.
            preferred_rows: Some(2_048),
            ..BenchKernelHints::amplifying(6.0, 5.0, 10.0)
        }
    }

    fn init(&self) -> Result<Box<dyn BenchKernelState>> {
        Ok(Box::new(TokeniseState::default()))
    }

    fn apply(&self, state: &mut dyn BenchKernelState, input: BenchPayload) -> Result<BenchPayload> {
        let counter = state
            .as_any_mut()
            .downcast_mut::<TokeniseState>()
            .ok_or_else(|| BenchError::Kernel {
                kernel: TokeniseExplode::NAME,
                detail: "state was not built by this kernel's init".to_string(),
            })?;
        let batch = input.table(TokeniseExplode::NAME)?;
        let index = batch
            .schema()
            .index_of(&self.column)
            .map_err(|_| BenchError::Kernel {
                kernel: TokeniseExplode::NAME,
                detail: format!("no column named {}", self.column),
            })?;
        let source = batch
            .column(index)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| BenchError::Kernel {
                kernel: TokeniseExplode::NAME,
                detail: format!(
                    "column {} is {}, not a utf8 string column",
                    self.column,
                    batch.schema().field(index).data_type()
                ),
            })?;

        let first_row_id = counter.rows_seen;
        let mut take_indices: Vec<u32> = Vec::new();
        let mut row_id = Int64Builder::new();
        let mut token_index = Int64Builder::new();
        let mut token_start = Int64Builder::new();
        let mut token_end = Int64Builder::new();
        let mut token = StringBuilder::new();

        for row in 0..source.len() {
            if source.is_null(row) {
                continue;
            }
            let value = source.value(row);
            for (position, (from, to)) in token_ranges(value).into_iter().enumerate() {
                take_indices.push(row as u32);
                row_id.append_value((first_row_id + row as u64) as i64);
                token_index.append_value(position as i64);
                token_start.append_value(from as i64);
                token_end.append_value(to as i64);
                token.append_value(&value[from..to]);
            }
        }
        counter.rows_seen += source.len() as u64;

        let take = UInt32Array::from(take_indices);
        let mut fields: Vec<Field> = Vec::new();
        let mut columns: Vec<ArrayRef> = Vec::new();
        for (position, field) in batch.schema().fields().iter().enumerate() {
            if position == index {
                continue;
            }
            fields.push(field.as_ref().clone());
            columns.push(arrow::compute::take(batch.column(position), &take, None)?);
        }
        for (name, array) in [
            (ADDED_COLUMNS[0], row_id.finish()),
            (ADDED_COLUMNS[1], token_index.finish()),
            (ADDED_COLUMNS[2], token_start.finish()),
            (ADDED_COLUMNS[3], token_end.finish()),
        ] {
            fields.push(Field::new(name, DataType::Int64, false));
            columns.push(Arc::new(array));
        }
        fields.push(Field::new(ADDED_COLUMNS[4], DataType::Utf8, false));
        columns.push(Arc::new(token.finish()));

        let schema = Arc::new(Schema::new(fields));
        let batch = RecordBatch::try_new_with_options(
            schema,
            columns,
            &arrow::array::RecordBatchOptions::new().with_row_count(Some(take.len())),
        )?;
        Ok(BenchPayload::Table(batch))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::Int64Array;

    fn batch(values: Vec<Option<&str>>) -> RecordBatch {
        let schema = Arc::new(Schema::new(vec![
            Field::new("i64_0", DataType::Int64, false),
            Field::new("text_0", DataType::Utf8, true),
        ]));
        let ids: ArrayRef = Arc::new(Int64Array::from(
            (0..values.len() as i64)
                .map(|v| v * 10)
                .collect::<Vec<i64>>(),
        ));
        let text: ArrayRef = Arc::new(StringArray::from(values));
        match RecordBatch::try_new(schema, vec![ids, text]) {
            Ok(batch) => batch,
            Err(err) => panic!("{err}"),
        }
    }

    fn run_all(kernel: &TokeniseExplode, batches: Vec<RecordBatch>) -> Vec<RecordBatch> {
        let mut state = match kernel.init() {
            Ok(state) => state,
            Err(err) => panic!("{err}"),
        };
        batches
            .into_iter()
            .map(
                |input| match kernel.apply(state.as_mut(), BenchPayload::Table(input)) {
                    Ok(BenchPayload::Table(out)) => out,
                    Ok(BenchPayload::Tensor(_)) => panic!("a table went in"),
                    Err(err) => panic!("{err}"),
                },
            )
            .collect()
    }

    fn ints(batch: &RecordBatch, name: &str) -> Vec<i64> {
        let index = match batch.schema().index_of(name) {
            Ok(index) => index,
            Err(err) => panic!("{err}"),
        };
        match batch.column(index).as_any().downcast_ref::<Int64Array>() {
            Some(array) => array.values().to_vec(),
            None => panic!("{name} is not i64"),
        }
    }

    fn strings(batch: &RecordBatch, name: &str) -> Vec<String> {
        let index = match batch.schema().index_of(name) {
            Ok(index) => index,
            Err(err) => panic!("{err}"),
        };
        match batch.column(index).as_any().downcast_ref::<StringArray>() {
            Some(array) => (0..array.len())
                .map(|i| array.value(i).to_string())
                .collect(),
            None => panic!("{name} is not utf8"),
        }
    }

    #[test]
    fn a_run_of_whitespace_is_one_separator_and_the_offsets_are_byte_offsets() {
        assert_eq!(token_ranges("ab  cd"), vec![(0, 2), (4, 6)]);
        assert_eq!(token_ranges("  ab\t\ncd  "), vec![(2, 4), (6, 8)]);
        assert_eq!(token_ranges(""), Vec::new());
        assert_eq!(token_ranges("   "), Vec::new());
        assert_eq!(token_ranges("one"), vec![(0, 3)]);
    }

    #[test]
    fn one_row_becomes_one_row_per_token_and_carries_the_other_columns() {
        let kernel = TokeniseExplode::default();
        assert_eq!(kernel.column(), "text_0");
        let out = run_all(
            &kernel,
            vec![batch(vec![Some("alpha bravo"), Some("charlie")])],
        );
        let out = &out[0];
        assert_eq!(out.num_rows(), 3);
        assert_eq!(out.num_columns(), 6);
        assert!(out.schema().index_of("text_0").is_err());
        assert_eq!(strings(out, "token"), ["alpha", "bravo", "charlie"]);
        assert_eq!(ints(out, "row_id"), [0, 0, 1]);
        assert_eq!(ints(out, "token_index"), [0, 1, 0]);
        assert_eq!(ints(out, "token_start"), [0, 6, 0]);
        assert_eq!(ints(out, "token_end"), [5, 11, 7]);
        assert_eq!(ints(out, "i64_0"), [0, 0, 10]);
    }

    #[test]
    fn a_null_and_a_whitespace_only_value_produce_no_rows_but_still_advance_row_id() {
        let out = run_all(
            &TokeniseExplode::default(),
            vec![batch(vec![None, Some("   "), Some("kilo")])],
        );
        let out = &out[0];
        assert_eq!(out.num_rows(), 1);
        assert_eq!(ints(out, "row_id"), [2]);
    }

    #[test]
    fn a_batch_with_no_tokens_at_all_is_an_empty_batch_not_an_error() {
        let out = run_all(&TokeniseExplode::default(), vec![batch(vec![None, None])]);
        assert_eq!(out[0].num_rows(), 0);
        assert_eq!(out[0].num_columns(), 6);
    }

    #[test]
    fn row_id_counts_input_rows_across_morsels() {
        let out = run_all(
            &TokeniseExplode::default(),
            vec![
                batch(vec![Some("a b"), Some("c")]),
                batch(vec![Some("d"), Some("e f")]),
            ],
        );
        assert_eq!(ints(&out[0], "row_id"), [0, 0, 1]);
        assert_eq!(ints(&out[1], "row_id"), [2, 3, 3]);
    }

    #[test]
    fn the_state_reports_the_rows_it_has_seen_and_a_foreign_state_is_an_error() {
        let kernel = TokeniseExplode::default();
        let mut state = kernel.init().expect("init");
        let _ = kernel.apply(state.as_mut(), BenchPayload::Table(batch(vec![Some("a")])));
        let inner = state
            .as_any_mut()
            .downcast_mut::<TokeniseState>()
            .expect("its own state downcasts");
        assert_eq!(inner.rows_seen(), 1);
        let mut foreign = crate::kernels::NoState;
        let err = kernel
            .apply(&mut foreign, BenchPayload::Table(batch(vec![Some("a")])))
            .expect_err("a foreign state is refused");
        assert!(err.to_string().contains("this kernel's init"), "{err}");
    }

    #[test]
    fn a_missing_column_and_a_wrong_type_are_kernel_errors() {
        let kernel = TokeniseExplode::new("nope");
        let mut state = kernel.init().expect("init");
        let err = kernel
            .apply(state.as_mut(), BenchPayload::Table(batch(vec![Some("a")])))
            .expect_err("no such column");
        assert!(err.to_string().contains("no column named nope"), "{err}");

        let kernel = TokeniseExplode::new("i64_0");
        let mut state = kernel.init().expect("init");
        let err = kernel
            .apply(state.as_mut(), BenchPayload::Table(batch(vec![Some("a")])))
            .expect_err("wrong type");
        assert!(
            err.to_string().contains("not a utf8 string column"),
            "{err}"
        );
    }

    #[test]
    fn the_declared_band_is_the_five_to_ten_of_preamble_6_5() {
        let kernel = TokeniseExplode::default();
        assert_eq!(kernel.name(), "tokenise-explode");
        assert_eq!(kernel.accepts(), PayloadKind::Table);
        let hints = kernel.hints();
        assert_eq!(hints.amplification_band, Some((5.0, 10.0)));
        assert_eq!(hints.expected_amplification, Some(6.0));
        assert_eq!(hints.preferred_rows, Some(2_048));
    }

    #[test]
    fn the_same_input_twice_gives_the_same_bytes() {
        let input = batch(vec![Some("Morsel arena; SPILL"), Some("row-group Tier")]);
        let first = run_all(&TokeniseExplode::default(), vec![input.clone()]);
        let second = run_all(&TokeniseExplode::default(), vec![input]);
        assert_eq!(first, second);
    }
}
