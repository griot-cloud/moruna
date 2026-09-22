//! `adversarial`: the amplification jumps fourfold at the midpoint of the
//! dataset (preamble 6.5).
//!
//! This is the kernel criterion S6 is measured against, so the jump must be a
//! property of the data the kernel has seen and of nothing else. The kernel is
//! told the dataset's row count when it is constructed; its state counts input
//! rows; a row whose ordinal is below `rows / 2` is emitted once and a row at or
//! above it is emitted four times. No clock, no morsel count, no byte count and
//! no host property takes part, so:
//!
//! - the same dataset fed as one morsel, as a thousand morsels, or one row at a
//!   time produces the same output rows in the same order, and the jump lands on
//!   the same input row;
//! - a morsel that straddles the midpoint is amplified partly at one and partly
//!   at four, which is what makes the jump land on a row rather than on a morsel
//!   boundary;
//! - running the same dataset on a faster or slower host changes nothing.
//!
//! `bench/tests/kernels.rs` proves the jump lands on the row the document names,
//! by feeding a generated dataset at three different morsel sizes and finding the
//! first row whose output rows exceed its input rows.
//!
//! Amplification: 1.0 over the first half, 4.0 over the second, and 2.5 over the
//! whole dataset, which is what a run over `small-row-groups` measures.

use arrow::array::{ArrayRef, RecordBatch, UInt32Array};

use crate::error::{BenchError, Result};
use crate::kernels::{BenchKernel, BenchKernelHints, BenchKernelState, BenchPayload, PayloadKind};

/// How many times a row at or after the midpoint is emitted.
pub const JUMP: usize = 4;

/// The kernel's per instance state: how many input rows it has seen.
#[derive(Debug, Default)]
pub struct AdversarialState {
    rows_seen: u64,
}

impl AdversarialState {
    /// Input rows seen so far by this instance.
    pub fn rows_seen(&self) -> u64 {
        self.rows_seen
    }
}

impl BenchKernelState for AdversarialState {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

/// The adversarial kernel over a dataset of a known row count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Adversarial {
    dataset_rows: u64,
}

impl Adversarial {
    /// The name preamble 6.5 gives it.
    pub const NAME: &'static str = "adversarial";

    /// A kernel for a dataset of `dataset_rows` rows. The midpoint is
    /// `dataset_rows / 2`, rounded down, so a dataset of an odd row count has
    /// the extra row in its amplified half.
    pub fn new(dataset_rows: u64) -> Adversarial {
        Adversarial { dataset_rows }
    }

    /// The dataset row count the kernel was told at construction.
    pub fn dataset_rows(self) -> u64 {
        self.dataset_rows
    }

    /// The input row ordinal at which the amplification jumps. Rows below it are
    /// emitted once, rows from it on are emitted `JUMP` times.
    pub fn midpoint(self) -> u64 {
        self.dataset_rows / 2
    }

    /// How many output rows the input row at `ordinal` produces.
    pub fn repeats(self, ordinal: u64) -> usize {
        if ordinal < self.midpoint() { 1 } else { JUMP }
    }
}

impl BenchKernel for Adversarial {
    fn name(&self) -> &'static str {
        Adversarial::NAME
    }

    fn accepts(&self) -> PayloadKind {
        PayloadKind::Table
    }

    fn hints(&self) -> BenchKernelHints {
        // The figure over the whole dataset is the mean of the two halves, and
        // the band is tight because the ratio is exact but for the few bytes an
        // offset buffer's extra entry and a validity bitmap's rounding add.
        BenchKernelHints {
            preferred_rows: Some(4_096),
            ..BenchKernelHints::amplifying(2.5, 2.4, 2.6)
        }
    }

    fn init(&self) -> Result<Box<dyn BenchKernelState>> {
        Ok(Box::new(AdversarialState::default()))
    }

    fn apply(&self, state: &mut dyn BenchKernelState, input: BenchPayload) -> Result<BenchPayload> {
        let counter = state
            .as_any_mut()
            .downcast_mut::<AdversarialState>()
            .ok_or_else(|| BenchError::Kernel {
                kernel: Adversarial::NAME,
                detail: "state was not built by this kernel's init".to_string(),
            })?;
        let batch = input.table(Adversarial::NAME)?;
        let first = counter.rows_seen;
        let rows = batch.num_rows();
        counter.rows_seen += rows as u64;

        let mut indices: Vec<u32> = Vec::with_capacity(rows);
        for row in 0..rows {
            for _ in 0..self.repeats(first + row as u64) {
                indices.push(row as u32);
            }
        }
        let take = UInt32Array::from(indices);
        let columns: Vec<ArrayRef> = batch
            .columns()
            .iter()
            .map(|column| arrow::compute::take(column, &take, None))
            .collect::<std::result::Result<Vec<ArrayRef>, arrow::error::ArrowError>>()?;
        let batch = RecordBatch::try_new_with_options(
            batch.schema(),
            columns,
            &arrow::array::RecordBatchOptions::new().with_row_count(Some(take.len())),
        )?;
        Ok(BenchPayload::Table(batch))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernels::table_bytes;
    use crate::kernels::tests::int_batch;

    fn explode(kernel: Adversarial, morsel: usize, rows: i64) -> Vec<i64> {
        let mut state = kernel.init().expect("init");
        let mut out: Vec<i64> = Vec::new();
        let mut start = 0i64;
        while start < rows {
            let end = (start + morsel as i64).min(rows);
            let batch = int_batch((start..end).collect());
            let payload = kernel
                .apply(state.as_mut(), BenchPayload::Table(batch))
                .expect("apply");
            let batch = payload.table(Adversarial::NAME).expect("a table went in");
            let values = batch
                .column(0)
                .as_any()
                .downcast_ref::<arrow::array::Int64Array>()
                .expect("i64 in, i64 out");
            out.extend_from_slice(values.values());
            start = end;
        }
        out
    }

    #[test]
    fn the_midpoint_is_half_the_row_count_the_kernel_was_told() {
        let kernel = Adversarial::new(100);
        assert_eq!(kernel.dataset_rows(), 100);
        assert_eq!(kernel.midpoint(), 50);
        assert_eq!(kernel.repeats(49), 1);
        assert_eq!(kernel.repeats(50), 4);
        assert_eq!(kernel.repeats(99), 4);
        // An odd row count puts the extra row in the amplified half.
        assert_eq!(Adversarial::new(101).midpoint(), 50);
        assert_eq!(Adversarial::new(0).midpoint(), 0);
        assert_eq!(Adversarial::new(0).repeats(0), 4);
    }

    #[test]
    fn a_row_before_the_midpoint_is_emitted_once_and_one_after_it_four_times() {
        let out = explode(Adversarial::new(8), 8, 8);
        assert_eq!(
            out,
            vec![0, 1, 2, 3, 4, 4, 4, 4, 5, 5, 5, 5, 6, 6, 6, 6, 7, 7, 7, 7]
        );
    }

    #[test]
    fn the_jump_lands_on_the_same_row_at_every_morsel_size() {
        let kernel = Adversarial::new(64);
        let reference = explode(kernel, 64, 64);
        for morsel in [1usize, 3, 7, 16, 31, 64, 128] {
            assert_eq!(
                explode(kernel, morsel, 64),
                reference,
                "morsel size {morsel} changed the output"
            );
        }
        // The first repeated value is the midpoint row, whatever the morsel size.
        let mut counts = std::collections::BTreeMap::new();
        for value in &reference {
            *counts.entry(*value).or_insert(0usize) += 1;
        }
        let (row, count) = counts
            .iter()
            .find(|(_, count)| **count > 1)
            .expect("something must be amplified");
        assert_eq!(*row, 32);
        assert_eq!(*count, JUMP);
    }

    #[test]
    fn a_morsel_that_straddles_the_midpoint_is_amplified_on_one_side_only() {
        let kernel = Adversarial::new(10);
        let mut state = kernel.init().expect("init");
        let batch = int_batch((0..10).collect());
        let payload = kernel
            .apply(state.as_mut(), BenchPayload::Table(batch))
            .expect("apply");
        assert_eq!(payload.rows(), 5 + 5 * JUMP as u64);
    }

    #[test]
    fn the_two_halves_amplify_at_one_and_at_four() {
        let kernel = Adversarial::new(64);
        let mut state = kernel.init().expect("init");
        let first = int_batch((0..32).collect());
        let first_bytes = table_bytes(&first);
        let out = kernel
            .apply(state.as_mut(), BenchPayload::Table(first))
            .expect("first half");
        assert_eq!(out.bytes() as f64 / first_bytes as f64, 1.0);

        let second = int_batch((32..64).collect());
        let second_bytes = table_bytes(&second);
        let out = kernel
            .apply(state.as_mut(), BenchPayload::Table(second))
            .expect("second half");
        assert_eq!(out.bytes() as f64 / second_bytes as f64, 4.0);
    }

    #[test]
    fn the_state_reports_its_rows_and_a_foreign_state_is_an_error() {
        let kernel = Adversarial::new(4);
        let mut state = kernel.init().expect("init");
        let _ = kernel.apply(state.as_mut(), BenchPayload::Table(int_batch(vec![0, 1])));
        let inner = state
            .as_any_mut()
            .downcast_mut::<AdversarialState>()
            .expect("its own state downcasts");
        assert_eq!(inner.rows_seen(), 2);
        let mut foreign = crate::kernels::NoState;
        let err = kernel
            .apply(&mut foreign, BenchPayload::Table(int_batch(vec![0])))
            .expect_err("a foreign state is refused");
        assert!(err.to_string().contains("this kernel's init"), "{err}");
    }

    #[test]
    fn a_tensor_is_refused_and_the_declared_band_is_the_mean_of_the_halves() {
        let kernel = Adversarial::new(4);
        assert_eq!(kernel.name(), "adversarial");
        assert_eq!(kernel.accepts(), PayloadKind::Table);
        let hints = kernel.hints();
        assert_eq!(hints.expected_amplification, Some(2.5));
        assert_eq!(hints.amplification_band, Some((2.4, 2.6)));
        let mut state = match kernel.init() {
            Ok(state) => state,
            Err(err) => panic!("{err}"),
        };
        let tensor =
            match crate::kernels::BenchTensor::new(crate::dtype::DType::F32, vec![1], vec![0u8; 4])
            {
                Ok(tensor) => tensor,
                Err(err) => panic!("{err}"),
            };
        let err = kernel
            .apply(state.as_mut(), BenchPayload::Tensor(tensor))
            .expect_err("a tensor is not a table");
        assert!(err.to_string().contains("got a tensor"), "{err}");
    }
}
