//! `wide-intermediate`: the Python kernel (preamble 6.5, amplification about 20,
//! releases the GIL).
//!
//! The body of this kernel is Python, and it lives in
//! `bench/python/amoru_bench_kernels/wide_intermediate.py`. It is the only one of
//! the six that is not Rust, because preamble 6.5 asks for one that is not: the
//! suite needs a kernel whose cost and whose GIL behaviour are a Python
//! extension's, so that the runtime's Python adapter is measured and not a Rust
//! function wearing a Python name.
//!
//! # Why this struct refuses
//!
//! The thing that would let a Rust caller run that function is the runtime's
//! Python adapter, component 5, which does not exist: `crates/amoru-adapters` is
//! a wave 0 stub and `pyo3` enters the workspace through it. This struct
//! therefore declares what the kernel is, reports the hints a scheduler would
//! read, and returns `BenchError::NotWired` from `apply`, naming the module that
//! holds the body and the component that will call it.
//!
//! A binding that embedded an interpreter here would be a second Python path
//! beside the one component 5 is specified to build, measured in wave 5 against
//! a baseline it does not share. An honest refusal is smaller, and it fails at
//! the one place a reader looks.
//!
//! # What the Python side does, and which call releases the GIL
//!
//! `wide_intermediate(columns)` takes a mapping of column name to NumPy array,
//! selects the numeric columns, stacks them into an `n x k` f64 matrix, and
//! returns the degree two expansion of each row: the upper triangle of the outer
//! product of the row with itself, including the diagonal, so `k` features
//! become `k (k + 1) / 2`. On `wide-mixed`, whose 74 columns carry 64 numeric
//! ones, that is 2080 f64 out of a row of about 830 bytes, which is the "about
//! 20" of preamble 6.5.
//!
//! The call that releases the GIL is `numpy.matmul` on the stacked
//! `n x k x 1` by `n x 1 x k` batch: NumPy's matmul is a `gufunc` whose inner
//! loop runs inside `NPY_BEGIN_THREADS` and dispatches to the BLAS `dgemm` for
//! f64, so the interpreter lock is not held while the arithmetic runs. The
//! Python test `test_matmul_releases_the_gil` measures it: two threads each
//! running the expansion finish in well under twice the time one thread takes,
//! which cannot happen while a lock is held for the duration.

use crate::error::{BenchError, Result};
use crate::kernels::{
    BenchKernel, BenchKernelHints, BenchKernelState, BenchPayload, NoState, PayloadKind,
};

/// The Python module that holds the body of this kernel.
pub const PYTHON_MODULE: &str = "amoru_bench_kernels.wide_intermediate";

/// The function inside that module.
pub const PYTHON_FUNCTION: &str = "wide_intermediate";

/// The directory the module is importable from, relative to the repository root.
pub const PYTHON_ROOT: &str = "bench/python";

/// The NumPy call that releases the GIL, named here so a report can cite it.
pub const GIL_RELEASING_CALL: &str = "numpy.matmul";

/// The Rust side of the Python kernel: it declares the kernel and refuses to run
/// it, because the adapter that would run it does not exist yet.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WideIntermediate;

impl WideIntermediate {
    /// The name preamble 6.5 gives it.
    pub const NAME: &'static str = "wide-intermediate";

    /// A new instance.
    pub fn new() -> WideIntermediate {
        WideIntermediate
    }

    /// What is missing, in one sentence, for a report or an error message.
    pub fn why_not_wired() -> String {
        format!(
            "its body is Python, in {PYTHON_ROOT}/{}.py, and the runtime's Python adapter \
             (component 5) that would call it does not exist yet; run it with \
             `cd {PYTHON_ROOT} && uv run --python 3.14 --with numpy --with pyarrow --with pytest pytest`",
            PYTHON_MODULE.replace('.', "/")
        )
    }
}

impl BenchKernel for WideIntermediate {
    fn name(&self) -> &'static str {
        WideIntermediate::NAME
    }

    fn accepts(&self) -> PayloadKind {
        PayloadKind::Table
    }

    fn hints(&self) -> BenchKernelHints {
        // Preamble 6.5: about 20. The band is measured on the Python side, by
        // `test_the_amplification_on_wide_mixed_is_about_twenty`, because that is
        // where the kernel's body is.
        BenchKernelHints {
            releases_gil: Some(true),
            // A morsel of 200,000 rows would hold a 200,000 x 64 x 64 outer
            // product at once, so the intermediate wants a small morsel.
            preferred_rows: Some(1_024),
            ..BenchKernelHints::amplifying(20.0, 15.0, 25.0)
        }
    }

    fn init(&self) -> Result<Box<dyn BenchKernelState>> {
        Ok(Box::new(NoState))
    }

    fn apply(
        &self,
        _state: &mut dyn BenchKernelState,
        _input: BenchPayload,
    ) -> Result<BenchPayload> {
        Err(BenchError::NotWired {
            kernel: WideIntermediate::NAME,
            detail: WideIntermediate::why_not_wired(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernels::tests::int_batch;

    #[test]
    fn it_declares_the_hints_of_preamble_6_5() {
        let kernel = WideIntermediate::new();
        assert_eq!(kernel.name(), "wide-intermediate");
        assert_eq!(kernel.accepts(), PayloadKind::Table);
        let hints = kernel.hints();
        assert_eq!(hints.expected_amplification, Some(20.0));
        assert_eq!(hints.amplification_band, Some((15.0, 25.0)));
        assert_eq!(hints.releases_gil, Some(true));
        assert_eq!(hints.preferred_rows, Some(1_024));
    }

    #[test]
    fn applying_it_refuses_and_says_what_is_missing() {
        let kernel = WideIntermediate::new();
        let mut state = kernel.init().expect("init");
        let err = kernel
            .apply(state.as_mut(), BenchPayload::Table(int_batch(vec![1])))
            .expect_err("nothing here can run Python");
        let text = err.to_string();
        assert!(text.contains("wide-intermediate is not wired"), "{text}");
        assert!(text.contains("component 5"), "{text}");
        assert!(
            text.contains("bench/python/amoru_bench_kernels/wide_intermediate.py"),
            "{text}"
        );
    }

    #[test]
    fn the_module_the_body_lives_in_is_named_and_so_is_the_call_that_releases_the_gil() {
        assert_eq!(PYTHON_MODULE, "amoru_bench_kernels.wide_intermediate");
        assert_eq!(PYTHON_FUNCTION, "wide_intermediate");
        assert_eq!(GIL_RELEASING_CALL, "numpy.matmul");
        assert!(WideIntermediate::why_not_wired().contains("pytest"));
    }
}
