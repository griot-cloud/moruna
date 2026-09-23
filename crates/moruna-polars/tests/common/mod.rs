//! The `normalise` kernel as an `moruna_kernel::Kernel`, for AD-T9.
//!
//! `bench/src/kernels/normalise.rs` holds the kernel preamble 6.5 names, but it implements the
//! bench crate's own `BenchKernel` trait (the runtime did not exist when it was written) and
//! `bench` is a dependency of no crate in preamble 6.1, so it cannot be imported here. The one
//! pass normalisation below is `bench`'s `normalise_text`, byte for byte; only the trait around
//! it is Moruna's. Reported.

#![allow(dead_code)]
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use moruna_kernel::arrow::array::{Array, ArrayRef, StringArray, StringBuilder};
use moruna_kernel::arrow::datatypes::{DataType, Field, Schema};
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{
    MorunaError, Fingerprint, InitCtx, Kernel, KernelKind, KernelState, NoState, Payload,
    PayloadKind, PayloadSpec, Result, SourceSchema, TierPref,
};

/// The suffix the kernel appends to the source column's name.
pub const SUFFIX: &str = "_normalised";

/// Normalise one string: lowercase, collapse runs of whitespace, strip the punctuation at the
/// edges of each token. Copied from `bench/src/kernels/normalise.rs`.
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

/// The normalise kernel over the first column of its input; one column in, one column out, which
/// is what both engine bridges can carry.
pub struct Normalise;

fn output_name(input_name: &str) -> String {
    format!("{input_name}{SUFFIX}")
}

impl Kernel for Normalise {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint::compute(
            "moruna-bench:normalise",
            b"one pass, edges stripped, lowercased",
        )
    }

    fn kind(&self) -> KernelKind {
        KernelKind::Stateless
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        let SourceSchema::Table(schema) = input else {
            return Err(MorunaError::Plan("normalise wants a table".into()));
        };
        let Some(field) = schema.fields().first() else {
            return Err(MorunaError::Plan("normalise wants one text column".into()));
        };
        Ok(SourceSchema::Table(Arc::new(Schema::new(vec![
            Field::new(output_name(field.name()), DataType::Utf8, true),
        ]))))
    }

    fn init(&self, _ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }

    fn apply(&self, _state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        let Payload::Table(batch, _) = input else {
            return Err(MorunaError::Plan("normalise wants a table".into()));
        };
        let Some(column) = batch.columns().first() else {
            return Err(MorunaError::Plan("normalise wants one text column".into()));
        };
        let text = moruna_kernel::arrow::compute::cast(column, &DataType::Utf8)
            .map_err(|e| MorunaError::Plan(format!("normalise: {e}")))?;
        let Some(values) = text.as_any().downcast_ref::<StringArray>() else {
            return Err(MorunaError::Plan("normalise: not a text column".into()));
        };
        let mut builder = StringBuilder::new();
        for index in 0..values.len() {
            if values.is_null(index) {
                builder.append_null();
            } else {
                builder.append_value(normalise_text(values.value(index)));
            }
        }
        let out: ArrayRef = Arc::new(builder.finish());
        let name = output_name(batch.schema().field(0).name());
        let schema = Arc::new(Schema::new(vec![Field::new(name, DataType::Utf8, true)]));
        let batch = RecordBatch::try_new(schema, vec![out])
            .map_err(|e| MorunaError::Plan(format!("normalise: {e}")))?;
        Payload::table(batch)
    }
}

/// The rows both halves of AD-T9 and AD-T10 run on, nulls included.
pub const ROWS: [Option<&str>; 7] = [
    Some("  Row-group  STAGING,  "),
    Some("Direct IO; O_DIRECT!"),
    None,
    Some("...."),
    Some(""),
    Some("MiXeD Case\tand\ttabs"),
    Some("one"),
];

/// What `Normalise` produces for [`ROWS`], computed directly rather than through either engine.
pub fn expected() -> Vec<Option<String>> {
    ROWS.iter().map(|row| row.map(normalise_text)).collect()
}
/// A kernel that answers with a tensor, which neither engine's scalar contract can carry.
pub struct TensorOut;

impl Kernel for TensorOut {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint::compute("moruna-test:tensor-out", b"")
    }

    fn kind(&self) -> KernelKind {
        KernelKind::Stateless
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Either,
            tier: TierPref::Host,
        }
    }

    fn output_schema(&self, _input: &SourceSchema) -> Result<SourceSchema> {
        Ok(SourceSchema::Tensor {
            dtype: moruna_kernel::DType::I64,
            shape: vec![-1],
        })
    }

    fn init(&self, _ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }

    fn apply(&self, _state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        input.as_tensor(None).and_then(Payload::tensor)
    }
}

/// A kernel that answers with a batch of no columns.
pub struct NoColumns;

impl Kernel for NoColumns {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint::compute("moruna-test:no-columns", b"")
    }

    fn kind(&self) -> KernelKind {
        KernelKind::Stateless
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    fn output_schema(&self, _input: &SourceSchema) -> Result<SourceSchema> {
        Ok(SourceSchema::Table(Arc::new(Schema::empty())))
    }

    fn init(&self, _ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }

    fn apply(&self, _state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        let rows = input.rows() as usize;
        let batch = RecordBatch::try_new_with_options(
            Arc::new(Schema::empty()),
            vec![],
            &moruna_kernel::arrow::record_batch::RecordBatchOptions::new()
                .with_row_count(Some(rows)),
        )
        .map_err(|e| MorunaError::Plan(format!("no columns: {e}")))?;
        Payload::table(batch)
    }
}

/// A kernel that refuses every morsel, so a test can see the bridge carry a kernel's own error.
pub struct Failing;

impl Kernel for Failing {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint::compute("moruna-test:failing", b"")
    }

    fn kind(&self) -> KernelKind {
        KernelKind::Stateless
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        Ok(input.clone())
    }

    fn init(&self, _ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }

    fn apply(&self, _state: &mut dyn KernelState, _input: Payload) -> Result<Payload> {
        Err(MorunaError::Plan("this kernel always refuses".into()))
    }
}
