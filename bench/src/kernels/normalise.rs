//! `normalise`: text normalisation over one text column (preamble 6.5,
//! amplification about 1.5).
//!
//! # No regex engine, and why
//!
//! Preamble 6.5 calls this kernel "normalise (Rust regex over text, A about
//! 1.5)", but the preamble's own dependency table (section 6.2) holds no regex
//! crate, and adding a crate that table lacks is escalation E2. The
//! normalisation here is therefore hand rolled: one pass over `&str`, no engine.
//!
//! That is the better benchmark as well as the cheaper decision. What the suite
//! measures is the runtime's behaviour under a kernel of a known cost and a
//! known amplification. A regex engine brings its own literal prefilters, its
//! own DFA cache and its own allocation behaviour, all of which would sit
//! between the measurement and the thing measured. A single pass over the bytes
//! of a string is the honest floor for "text work", and it is what a hand tuned
//! wave 5 baseline would do.
//!
//! If a later benchmark genuinely needs a regex engine (to compare the runtime
//! against a user's regex workload rather than against text work in general),
//! that is an E2 item naming `regex` for `bench/` alone, and preamble 6.2 says
//! section 6.5 is the bench agent's d.2, so the PM may approve it in the pull
//! request that needs it. Nothing here is written so as to make that hard: the
//! normalisation is one function, `normalise_text`.
//!
//! # What it does
//!
//! For each value of the chosen text column:
//!
//! 1. split on runs of whitespace, which collapses every run to one separator;
//! 2. strip the punctuation at each end of a token, leaving the inside alone, so
//!    `row-group` keeps its hyphen and `staging,` loses its comma;
//! 3. lowercase the token;
//! 4. drop a token that is empty after stripping;
//! 5. join what is left with a single space.
//!
//! A null stays null. The result is appended as a new column, named after the
//! source with `_normalised`, which is what makes the amplification about 1.5:
//! the normalised text is roughly the size of the source column and the source
//! column is roughly half of a row of `text-normalise`.

use std::sync::Arc;

use arrow::array::{Array, ArrayRef, RecordBatch, StringArray, StringBuilder};
use arrow::datatypes::{DataType, Field, Schema};

use crate::error::{BenchError, Result};
use crate::kernels::{
    BenchKernel, BenchKernelHints, BenchKernelState, BenchPayload, NoState, PayloadKind,
};

/// The suffix appended to the source column's name.
pub const SUFFIX: &str = "_normalised";

/// Normalise one string: lowercase, collapse runs of whitespace, strip the
/// punctuation at the edges of each token.
///
/// Deterministic and allocation bounded: one output `String` whose length never
/// exceeds the input's, because every rule either removes bytes or leaves the
/// count alone. `char::to_lowercase` is the one exception in principle (a few
/// code points lengthen when folded) and the generator's corpus is ASCII, so the
/// capacity is a hint rather than a bound.
pub fn normalise_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for token in value.split_whitespace() {
        let trimmed = token.trim_matches(|c: char| c.is_ascii_punctuation());
        if trimmed.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        for lowered in trimmed.chars().flat_map(char::to_lowercase) {
            out.push(lowered);
        }
    }
    out
}

/// The normalise kernel over one named text column.
#[derive(Debug, Clone)]
pub struct Normalise {
    column: String,
}

impl Default for Normalise {
    fn default() -> Self {
        Normalise::new(Normalise::DEFAULT_COLUMN)
    }
}

impl Normalise {
    /// The name preamble 6.5 gives it.
    pub const NAME: &'static str = "normalise";

    /// The column the `text-normalise` dataset's first text column carries.
    pub const DEFAULT_COLUMN: &'static str = "text_0";

    /// A kernel that normalises `column`.
    pub fn new(column: impl Into<String>) -> Normalise {
        Normalise {
            column: column.into(),
        }
    }

    /// The column it reads.
    pub fn column(&self) -> &str {
        &self.column
    }

    /// The name of the column it appends.
    pub fn output_column(&self) -> String {
        format!("{}{SUFFIX}", self.column)
    }
}

impl BenchKernel for Normalise {
    fn name(&self) -> &'static str {
        Normalise::NAME
    }

    fn accepts(&self) -> PayloadKind {
        PayloadKind::Table
    }

    fn hints(&self) -> BenchKernelHints {
        // Preamble 6.5: about 1.5. The band is what "about" is taken to mean
        // here, and `bench/tests/kernels.rs` measures the ratio on
        // `text-normalise`, the dataset `bench/README.md` pairs with this
        // kernel. The figure follows from the dataset's shape: appending the
        // normalised form of one of two 512 byte text columns adds about half a
        // row to a row.
        BenchKernelHints {
            preferred_rows: Some(8_192),
            ..BenchKernelHints::amplifying(1.5, 1.25, 1.75)
        }
    }

    fn init(&self) -> Result<Box<dyn BenchKernelState>> {
        Ok(Box::new(NoState))
    }

    fn apply(
        &self,
        _state: &mut dyn BenchKernelState,
        input: BenchPayload,
    ) -> Result<BenchPayload> {
        let batch = input.table(Normalise::NAME)?;
        let index = batch
            .schema()
            .index_of(&self.column)
            .map_err(|_| BenchError::Kernel {
                kernel: Normalise::NAME,
                detail: format!(
                    "no column named {}; the batch holds {}",
                    self.column,
                    batch
                        .schema()
                        .fields()
                        .iter()
                        .map(|f| f.name().clone())
                        .collect::<Vec<String>>()
                        .join(", ")
                ),
            })?;
        let source = batch
            .column(index)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| BenchError::Kernel {
                kernel: Normalise::NAME,
                detail: format!(
                    "column {} is {}, not a utf8 string column",
                    self.column,
                    batch.schema().field(index).data_type()
                ),
            })?;

        let mut builder = StringBuilder::with_capacity(source.len(), source.value_data().len());
        for row in 0..source.len() {
            if source.is_null(row) {
                builder.append_null();
            } else {
                builder.append_value(normalise_text(source.value(row)));
            }
        }
        let normalised: ArrayRef = Arc::new(builder.finish());

        let mut fields: Vec<Field> = batch
            .schema()
            .fields()
            .iter()
            .map(|field| field.as_ref().clone())
            .collect();
        fields.push(Field::new(
            self.output_column(),
            DataType::Utf8,
            source.null_count() > 0,
        ));
        let mut columns: Vec<ArrayRef> = batch.columns().to_vec();
        columns.push(normalised);
        let batch = RecordBatch::try_new(Arc::new(Schema::new(fields)), columns)?;
        Ok(BenchPayload::Table(batch))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dtype::DType;
    use crate::kernels::{BenchTensor, table_bytes};

    fn text_batch(values: Vec<Option<&str>>) -> RecordBatch {
        let schema = Arc::new(Schema::new(vec![Field::new(
            "text_0",
            DataType::Utf8,
            true,
        )]));
        let column: ArrayRef = Arc::new(StringArray::from(values));
        RecordBatch::try_new(schema, vec![column]).expect("batch")
    }

    fn run(kernel: &Normalise, batch: RecordBatch) -> Result<RecordBatch> {
        let mut state = kernel.init()?;
        let payload = kernel.apply(state.as_mut(), BenchPayload::Table(batch))?;
        Ok(payload.table(Normalise::NAME)?.clone())
    }

    #[test]
    fn lowercases_collapses_whitespace_and_strips_edge_punctuation() {
        assert_eq!(normalise_text("Morsel   arena;"), "morsel arena");
        assert_eq!(normalise_text("\t SPILL \n staging, "), "spill staging");
        assert_eq!(normalise_text("row-group"), "row-group");
        assert_eq!(normalise_text("...checkpoint..."), "checkpoint");
        assert_eq!(normalise_text("resume?"), "resume");
        assert_eq!(normalise_text("throughput!"), "throughput");
    }

    #[test]
    fn a_token_that_is_only_punctuation_disappears_and_an_empty_value_stays_empty() {
        assert_eq!(normalise_text("--- ;;; ..."), "");
        assert_eq!(normalise_text(""), "");
        assert_eq!(normalise_text("   "), "");
        assert_eq!(normalise_text("a --- b"), "a b");
    }

    #[test]
    fn a_null_stays_null_and_the_appended_column_carries_the_normalised_text() {
        let kernel = Normalise::default();
        assert_eq!(kernel.column(), "text_0");
        assert_eq!(kernel.output_column(), "text_0_normalised");
        let batch = text_batch(vec![Some("Tier DEVICE;"), None, Some("  host,  queue ")]);
        let out = run(&kernel, batch).expect("normalise");
        assert_eq!(out.num_columns(), 2);
        let column = out
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("the appended column is utf8");
        assert_eq!(column.value(0), "tier device");
        assert!(column.is_null(1));
        assert_eq!(column.value(2), "host queue");
    }

    #[test]
    fn the_appended_column_is_not_nullable_when_the_source_has_no_nulls() {
        let batch = text_batch(vec![Some("a"), Some("b")]);
        let out = run(&Normalise::default(), batch).expect("normalise");
        assert!(!out.schema().field(1).is_nullable());
    }

    #[test]
    fn the_same_input_twice_gives_the_same_bytes() {
        let batch = text_batch(vec![Some("Morsel arena; SPILL"), Some("row-group Tier")]);
        let first = run(&Normalise::default(), batch.clone()).expect("first");
        let second = run(&Normalise::default(), batch).expect("second");
        assert_eq!(first, second);
    }

    #[test]
    fn a_missing_column_a_wrong_type_and_a_tensor_are_each_a_kernel_error() {
        let kernel = Normalise::new("text_9");
        let err = run(&kernel, text_batch(vec![Some("x")])).expect_err("no such column");
        let text = err.to_string();
        assert!(text.contains("no column named text_9"), "{text}");
        assert!(text.contains("text_0"), "{text}");

        let schema = Arc::new(Schema::new(vec![Field::new(
            "text_0",
            DataType::Int64,
            false,
        )]));
        let column: ArrayRef = Arc::new(arrow::array::Int64Array::from(vec![1i64]));
        let wrong = RecordBatch::try_new(schema, vec![column]).expect("batch");
        let err = run(&Normalise::default(), wrong).expect_err("wrong type");
        assert!(
            err.to_string().contains("not a utf8 string column"),
            "{err}"
        );

        let kernel = Normalise::default();
        let mut state = kernel.init().expect("init");
        let tensor = BenchTensor::new(DType::F32, vec![1], vec![0u8; 4]).expect("tensor");
        let err = kernel
            .apply(state.as_mut(), BenchPayload::Tensor(tensor))
            .expect_err("a tensor is not a table");
        assert!(err.to_string().contains("got a tensor"), "{err}");
    }

    #[test]
    fn the_declared_band_is_about_one_and_a_half() {
        let kernel = Normalise::default();
        assert_eq!(kernel.name(), "normalise");
        assert_eq!(kernel.accepts(), PayloadKind::Table);
        let hints = kernel.hints();
        assert_eq!(hints.expected_amplification, Some(1.5));
        assert_eq!(hints.amplification_band, Some((1.25, 1.75)));
        assert_eq!(hints.preferred_rows, Some(8_192));
        assert_eq!(hints.releases_gil, None);
    }

    #[test]
    fn appending_the_normalised_column_grows_the_batch() {
        let batch = text_batch(vec![Some("Morsel ARENA; spill staging,"); 256]);
        let before = table_bytes(&batch);
        let out = run(&Normalise::default(), batch).expect("normalise");
        let after = table_bytes(&out);
        assert!(after > before, "{after} should exceed {before}");
        // A single text column doubles rather than halves: the band of 1.5
        // belongs to `text-normalise`, which carries two text columns and an
        // integer, and that pairing is what the integration test measures.
        assert!((after as f64 / before as f64) > 1.5);
    }
}
