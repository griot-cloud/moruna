//! The benchmark kernels (preamble section 6.5).
//!
//! Preamble 6.5 names six kernels for wave 1: identity (amplification about 1),
//! normalise (about 1.5), tokenise-explode (5 to 10), adversarial (the
//! amplification jumps fourfold at the midpoint of the dataset),
//! wide-intermediate (Python and NumPy, about 20, releases the GIL) and
//! embed-score (numeric columns to a tensor, a small matmul, back to a column).
//! torch-score is not built: it needs the reference GPU host, which does not
//! exist (escalation E1, decided 2026-09-22).
//!
//! # The shape of a kernel
//!
//! The runtime that will host these kernels is not built yet: `moruna-kernel` is
//! a wave 0 stub, so there is no `Kernel` trait to implement and no `Payload` to
//! carry. The trait below is therefore local to this crate, but its method shape
//! is the one `architecture/sdd/01-contracts.md` section d.7 gives `Kernel`: an
//! `init` that makes one instance's state, and an `apply` that takes that state,
//! an input payload and returns an output payload or an error. `BenchPayload`
//! mirrors section d.4's `Payload` (a table or a tensor) and `BenchKernelHints`
//! mirrors its `KernelHints`. A wave 4 facade can wrap each struct here in the
//! real `Kernel` without changing it, because every method it needs is present
//! with the same arguments in the same order.
//!
//! Three deliberate differences from d.4 and d.7, each because the runtime does
//! not exist yet rather than because the shape is disputed:
//!
//! 1. `BenchPayload` carries no `Tier`: tiering is the arena's, and nothing here
//!    allocates outside the host heap.
//! 2. `BenchTensor` owns a `Vec<u8>` rather than wrapping a DLPack tensor,
//!    because `dlpark` enters through `moruna-kernel` and this crate does not
//!    depend on it.
//! 3. `BenchKernelHints` carries an `amplification_band` that `KernelHints` does
//!    not. The band is what preamble 6.5 states in words ("about 1.5", "5 to
//!    10"), and each kernel's amplification test asserts the measured ratio
//!    falls inside its own band, so the band belongs beside the kernel rather
//!    than in a test's constant.
//!
//! # Amplification
//!
//! Amplification is output bytes over input bytes, measured by `payload_bytes`,
//! which sums the exact bytes each Arrow buffer slice occupies. Every kernel
//! declares its band in `hints()` and `bench/tests/kernels.rs` proves the
//! measured ratio on a generated dataset falls inside it.
//!
//! Every kernel here is deterministic: the same input bytes produce the same
//! output bytes, with no clock, no host name, no address and no iteration order
//! over a hash map reaching a value.

pub mod adversarial;
pub mod embed_score;
pub mod identity;
pub mod normalise;
pub mod runner;
pub mod tokenise;
pub mod wide_intermediate;

use arrow::array::{Array, RecordBatch};

use crate::dtype::DType;
use crate::error::{BenchError, Result};

/// What a payload holds, mirroring `PayloadKind` of contracts d.4 without the
/// `Either` arm, which is a kernel's preference rather than a payload's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadKind {
    /// An Arrow record batch.
    Table,
    /// A dense row major tensor.
    Tensor,
}

/// A dense, contiguous, row major tensor: what contracts d.4 calls a
/// `ManagedTensor`, reduced to what a benchmark kernel needs. The bytes are
/// owned, because nothing in this crate allocates from an arena.
#[derive(Debug, Clone, PartialEq)]
pub struct BenchTensor {
    dtype: DType,
    shape: Vec<i64>,
    data: Vec<u8>,
}

impl BenchTensor {
    /// Wrap `data` as a tensor of `dtype` and `shape`. Errors when the byte
    /// length does not match the shape, or when a dimension is negative.
    pub fn new(dtype: DType, shape: Vec<i64>, data: Vec<u8>) -> Result<BenchTensor> {
        let mut elements: i64 = 1;
        for dim in &shape {
            if *dim < 0 {
                return Err(BenchError::Shape(format!(
                    "tensor dimension {dim} is negative"
                )));
            }
            elements = elements.saturating_mul(*dim);
        }
        let expected = elements as usize * dtype.item_size();
        if data.len() != expected {
            return Err(BenchError::Shape(format!(
                "tensor of {} {:?} needs {expected} bytes, got {}",
                shape.len(),
                dtype,
                data.len()
            )));
        }
        Ok(BenchTensor { dtype, shape, data })
    }

    /// The element type.
    pub fn dtype(&self) -> DType {
        self.dtype
    }

    /// The shape, row major.
    pub fn shape(&self) -> &[i64] {
        &self.shape
    }

    /// The bytes, row major and contiguous.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Rows, which is the leading dimension; a scalar tensor is one row, as
    /// contracts d.4 defines `Payload::rows`.
    pub fn rows(&self) -> u64 {
        match self.shape.first() {
            Some(rows) => *rows as u64,
            None => 1,
        }
    }
}

/// What a kernel is given and what it returns: contracts d.4's `Payload`,
/// without the `Tier` this crate has no arena to read.
#[derive(Debug, Clone, PartialEq)]
pub enum BenchPayload {
    /// An Arrow record batch.
    Table(RecordBatch),
    /// A dense tensor.
    Tensor(BenchTensor),
}

impl BenchPayload {
    /// Which arm this is.
    pub fn kind(&self) -> PayloadKind {
        match self {
            BenchPayload::Table(_) => PayloadKind::Table,
            BenchPayload::Tensor(_) => PayloadKind::Tensor,
        }
    }

    /// Rows: the batch's row count, or the tensor's leading dimension.
    pub fn rows(&self) -> u64 {
        match self {
            BenchPayload::Table(batch) => batch.num_rows() as u64,
            BenchPayload::Tensor(tensor) => tensor.rows(),
        }
    }

    /// The bytes this payload occupies, which is the numerator and the
    /// denominator of every amplification figure in this crate.
    pub fn bytes(&self) -> u64 {
        match self {
            BenchPayload::Table(batch) => table_bytes(batch),
            BenchPayload::Tensor(tensor) => tensor.data.len() as u64,
        }
    }

    /// The batch inside a `Table` payload, or a kernel error naming the kernel
    /// that wanted one.
    pub fn table(&self, kernel: &'static str) -> Result<&RecordBatch> {
        match self {
            BenchPayload::Table(batch) => Ok(batch),
            BenchPayload::Tensor(_) => Err(BenchError::Kernel {
                kernel,
                detail: "expected a table payload, got a tensor".to_string(),
            }),
        }
    }
}

/// The bytes an Arrow record batch occupies: the exact size of every buffer
/// slice its columns reference, offsets and validity bitmaps included.
///
/// `ArrayData::get_slice_memory_size` is the exact figure for the slice an array
/// refers to, which is what a morsel's accounting wants; `get_array_memory_size`
/// is the fallback for the rare layout it declines to measure, and reports the
/// whole allocation rather than the slice.
pub fn table_bytes(batch: &RecordBatch) -> u64 {
    batch
        .columns()
        .iter()
        .map(|column| {
            let data = column.to_data();
            data.get_slice_memory_size()
                .unwrap_or_else(|_| column.get_array_memory_size()) as u64
        })
        .sum()
}

/// A kernel's declared properties, contracts d.7's `KernelHints` plus the
/// amplification band this crate's tests assert against.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BenchKernelHints {
    /// The amplification the kernel expects on the dataset it is meant for.
    pub expected_amplification: Option<f64>,
    /// The inclusive band that amplification must fall inside, which is what
    /// preamble 6.5 states in words. `None` for a kernel whose amplification is
    /// not a single figure.
    pub amplification_band: Option<(f64, f64)>,
    /// Whether the kernel's work happens on a device. False for all six.
    pub uses_device_memory: bool,
    /// Whether the kernel releases the GIL, for a kernel that holds one.
    pub releases_gil: Option<bool>,
    /// The morsel size the kernel would prefer, in rows.
    pub preferred_rows: Option<u64>,
    /// Bytes one instance's state holds, a model's weights for example.
    pub state_bytes: Option<u64>,
}

impl BenchKernelHints {
    /// Hints for a kernel whose amplification is one figure inside one band.
    pub fn amplifying(expected: f64, low: f64, high: f64) -> BenchKernelHints {
        BenchKernelHints {
            expected_amplification: Some(expected),
            amplification_band: Some((low, high)),
            ..BenchKernelHints::default()
        }
    }

    /// Whether `measured` falls inside the declared band. A kernel with no band
    /// accepts anything, which is the honest answer for one whose amplification
    /// is not a single figure.
    pub fn band_holds(&self, measured: f64) -> bool {
        match self.amplification_band {
            None => true,
            Some((low, high)) => measured >= low && measured <= high,
        }
    }
}

/// Per instance state a kernel keeps between `apply` calls: contracts d.7's
/// `KernelState`, minus the checkpoint and footprint methods, which belong to a
/// resume protocol this crate has no manifest to take part in.
pub trait BenchKernelState: Send {
    /// Downcast hook, exactly as contracts d.7 has it.
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any;
}

/// Unit state for a stateless kernel, contracts d.7's `NoState`.
#[derive(Debug, Default)]
pub struct NoState;

impl BenchKernelState for NoState {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

/// A benchmark kernel.
///
/// `apply` has the shape contracts d.7 gives `Kernel::apply`: the instance's
/// state, an input payload, and a `Result` holding the output payload. The
/// methods a runtime needs and this crate cannot answer (`fingerprint`, which
/// hashes over `moruna-kernel`'s types, and `output_schema`, which returns a
/// `SourceSchema`) are left to the wave 4 facade that wraps these structs.
pub trait BenchKernel: Send + Sync {
    /// The kernel's name, which is the name preamble 6.5 gives it.
    fn name(&self) -> &'static str;

    /// What the kernel wants delivered.
    fn accepts(&self) -> PayloadKind;

    /// The declared amplification class and the rest of contracts d.7's hints.
    fn hints(&self) -> BenchKernelHints;

    /// Once per instance, before any `apply`.
    fn init(&self) -> Result<Box<dyn BenchKernelState>>;

    /// Apply the kernel to one morsel. Deterministic: the same state and the
    /// same input bytes give the same output bytes.
    fn apply(&self, state: &mut dyn BenchKernelState, input: BenchPayload) -> Result<BenchPayload>;
}

/// Every kernel this crate ships, by the name preamble 6.5 gives it. The
/// `embed-score` kernel is absent because it cannot be built without weights;
/// `runner::kernel_by_name` builds it from a weights file.
pub const KERNEL_NAMES: [&str; 6] = [
    "identity",
    "normalise",
    "tokenise-explode",
    "adversarial",
    "wide-intermediate",
    "embed-score",
];

/// The kernel preamble 6.5 names but this repository does not build, with the
/// reason, so that a report can say why five of six Rust kernels exist.
pub const NOT_BUILT: [(&str, &str); 1] = [(
    "torch-score",
    "needs the reference GPU host, which does not exist (escalation E1, decided 2026-09-22)",
)];

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{ArrayRef, Int64Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use std::sync::Arc;

    pub(crate) fn int_batch(values: Vec<i64>) -> RecordBatch {
        let schema = Arc::new(Schema::new(vec![Field::new(
            "i64_0",
            DataType::Int64,
            false,
        )]));
        let column: ArrayRef = Arc::new(Int64Array::from(values));
        RecordBatch::try_new(schema, vec![column]).expect("batch")
    }

    #[test]
    fn a_table_payload_reports_its_rows_and_its_bytes() {
        let payload = BenchPayload::Table(int_batch(vec![1, 2, 3, 4]));
        assert_eq!(payload.kind(), PayloadKind::Table);
        assert_eq!(payload.rows(), 4);
        assert_eq!(payload.bytes(), 32);
        assert!(payload.table("t").is_ok());
    }

    #[test]
    fn a_tensor_payload_reports_its_leading_dimension_and_its_bytes() {
        let tensor = BenchTensor::new(DType::F64, vec![3, 2], vec![0u8; 48]).expect("tensor");
        assert_eq!(tensor.dtype(), DType::F64);
        assert_eq!(tensor.shape(), [3, 2]);
        assert_eq!(tensor.data().len(), 48);
        let payload = BenchPayload::Tensor(tensor);
        assert_eq!(payload.kind(), PayloadKind::Tensor);
        assert_eq!(payload.rows(), 3);
        assert_eq!(payload.bytes(), 48);
        let err = payload.table("k").expect_err("a tensor is not a table");
        assert!(err.to_string().contains("kernel k"), "{err}");
    }

    #[test]
    fn a_scalar_tensor_is_one_row() {
        let tensor = BenchTensor::new(DType::I32, Vec::new(), vec![0u8; 4]).expect("scalar");
        assert_eq!(tensor.rows(), 1);
    }

    #[test]
    fn a_tensor_whose_bytes_do_not_match_its_shape_is_an_error() {
        let err = BenchTensor::new(DType::F32, vec![4], vec![0u8; 8]).expect_err("short");
        assert!(err.to_string().contains("needs 16 bytes"), "{err}");
        let err = BenchTensor::new(DType::F32, vec![-1], Vec::new()).expect_err("negative");
        assert!(err.to_string().contains("negative"), "{err}");
    }

    #[test]
    fn a_band_holds_only_inside_itself_and_a_missing_band_holds_always() {
        let hints = BenchKernelHints::amplifying(1.5, 1.25, 1.75);
        assert!(hints.band_holds(1.5));
        assert!(hints.band_holds(1.25));
        assert!(hints.band_holds(1.75));
        assert!(!hints.band_holds(1.24));
        assert!(!hints.band_holds(1.76));
        assert!(BenchKernelHints::default().band_holds(1_000.0));
    }

    #[test]
    fn the_default_hints_claim_nothing() {
        let hints = BenchKernelHints::default();
        assert_eq!(hints.expected_amplification, None);
        assert_eq!(hints.releases_gil, None);
        assert_eq!(hints.preferred_rows, None);
        assert_eq!(hints.state_bytes, None);
        assert!(!hints.uses_device_memory);
    }

    #[test]
    fn unit_state_downcasts() {
        let mut state = NoState;
        let any = state.as_any_mut();
        assert!(any.downcast_mut::<NoState>().is_some());
    }

    #[test]
    fn the_kernel_list_names_the_six_of_preamble_6_5_and_the_one_not_built() {
        assert_eq!(KERNEL_NAMES.len(), 6);
        assert!(KERNEL_NAMES.contains(&"wide-intermediate"));
        assert_eq!(NOT_BUILT[0].0, "torch-score");
        assert!(NOT_BUILT[0].1.contains("E1"));
    }
}
