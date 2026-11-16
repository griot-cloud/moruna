//! The seven benchmark functions of Moruna 0.6.6, ported to Rust for the 0.7 gate
//! (`architecture/arrow-core-0.7.md` section 9).
//!
//! Each port computes what the Python function in `bench/vm/workloads.py` or
//! `bench/vm/ooc_workloads.py` computes, value for value: the same rows, columns, types, nulls
//! and order, with Python's and NumPy's integer and float rules (`pyfloat`). The Python
//! functions are the oracle (`bench/vm/gate_oracle.py`); `images/gate/README.md` records each
//! port's equivalence.
//!
//! The crate has two layers:
//!
//! - the core, `no_std` with `alloc`, over column slices ([`cols`]): what a native task on the
//!   data plane links (`images/gate/record`, `images/gate/tasks`);
//! - with the `arrow` feature, the functions as Arrow `RecordBatch` in, `RecordBatch` out on the
//!   workspace's arrow-rs ([`batch`]), and the gate's canonical hash ([`canon`]).
//!
//! Untrusted input never panics a function: a value or schema the Python function would raise
//! on, or that the port cannot take, is an [`Error`].
//!
//! Floating point goes through `libm` (musl's functions in Rust), never the platform's libm, so
//! a port computes the same bits in the VM, in the baseline and on any host.

#![no_std]
// One `unsafe` block, the call into the GEMM (`gemm::matmul`), behind its own checks.
#![deny(unsafe_code)]

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

pub mod cols;
pub mod error;
pub mod fineweb;
pub mod gemm;
pub mod geom;
pub mod nyc;
pub mod overture;
pub mod pyfloat;
pub mod score_weights;

#[cfg(feature = "arrow")]
pub mod batch;
#[cfg(feature = "arrow")]
pub mod canon;

pub use error::Error;

/// The jobs the gate runs, by the names `bench/vm/runner.py` gives them.
pub const JOBS: [&str; 7] = [
    "score",
    "occupancy",
    "enrich",
    "audit",
    "ov_shape",
    "ov_attrs",
    "fw_minhash",
];
