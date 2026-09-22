//! `identity`: returns its input unchanged (preamble 6.5, amplification about 1).
//!
//! It is the floor of the suite. Its output is its input, so its amplification
//! is exactly 1.0 for every payload and every dataset, and what a benchmark run
//! measures through it is the runtime and the reader rather than the kernel.

use crate::error::Result;
use crate::kernels::{
    BenchKernel, BenchKernelHints, BenchKernelState, BenchPayload, NoState, PayloadKind,
};

/// The identity kernel.
#[derive(Debug, Default, Clone, Copy)]
pub struct Identity;

impl Identity {
    /// The name preamble 6.5 gives it.
    pub const NAME: &'static str = "identity";

    /// A new instance.
    pub fn new() -> Identity {
        Identity
    }
}

impl BenchKernel for Identity {
    fn name(&self) -> &'static str {
        Identity::NAME
    }

    fn accepts(&self) -> PayloadKind {
        // A table, because every dataset of the suite that feeds it is a table.
        // The body would carry a tensor through unchanged as well, and does.
        PayloadKind::Table
    }

    fn hints(&self) -> BenchKernelHints {
        // Exactly one, not about one: the output is the input.
        BenchKernelHints::amplifying(1.0, 1.0, 1.0)
    }

    fn init(&self) -> Result<Box<dyn BenchKernelState>> {
        Ok(Box::new(NoState))
    }

    fn apply(
        &self,
        _state: &mut dyn BenchKernelState,
        input: BenchPayload,
    ) -> Result<BenchPayload> {
        Ok(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dtype::DType;
    use crate::kernels::tests::int_batch;
    use crate::kernels::{BenchTensor, table_bytes};

    fn apply(input: BenchPayload) -> BenchPayload {
        let kernel = Identity::new();
        let mut state = kernel.init().expect("init");
        kernel.apply(state.as_mut(), input).expect("apply")
    }

    #[test]
    fn the_output_is_the_input() {
        let batch = int_batch(vec![7, 8, 9]);
        let output = apply(BenchPayload::Table(batch.clone()));
        assert_eq!(output, BenchPayload::Table(batch));
    }

    #[test]
    fn a_tensor_goes_through_unchanged_too() {
        let tensor = BenchTensor::new(DType::F32, vec![2, 2], vec![1u8; 16]).expect("tensor");
        let output = apply(BenchPayload::Tensor(tensor.clone()));
        assert_eq!(output, BenchPayload::Tensor(tensor));
    }

    #[test]
    fn the_amplification_is_exactly_one() {
        let kernel = Identity::new();
        assert_eq!(kernel.name(), "identity");
        assert_eq!(kernel.accepts(), PayloadKind::Table);
        let batch = int_batch((0..64).collect());
        let input_bytes = table_bytes(&batch);
        let output = apply(BenchPayload::Table(batch));
        let measured = output.bytes() as f64 / input_bytes as f64;
        assert_eq!(measured, 1.0);
        assert!(kernel.hints().band_holds(measured));
        assert_eq!(kernel.hints().expected_amplification, Some(1.0));
    }
}
