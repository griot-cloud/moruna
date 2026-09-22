//! Amoru component 12, the PyO3 module behind the `amoru` Python package.
//!
//! Design: `architecture/sdd/12-python.md`. The module owns argument translation and its range
//! clamping (f.3, PY-I10), the GIL refusal (f.4, PY-I4), the thread roles that make
//! `KeyboardInterrupt` work (f.5, PY-I6), the error to exception mapping (e.2, PY-I2) and the
//! `RunReport` object (PY-I5). It owns no orchestration: `amoru_runtime::Runtime` is the only
//! place components are wired (a).
//!
//! The crate has two halves. Argument translation, the configuration clamping and the sink equals
//! source rule are plain Rust and are tested without an interpreter; the `python` feature adds the
//! module itself, every `#[pyclass]` of which is `frozen` (PY-I7).

#![deny(missing_docs)]
#![deny(unsafe_code)]
// `AmoruError` carries a morsel's features (CT-I10), so every `Result` in the crate has a large
// `Err`. The shape of the error type is the contract's (d.14), not this crate's.
#![allow(clippy::result_large_err)]

pub mod handles;
pub mod size;
pub mod translate;

#[cfg(feature = "python")]
pub mod errors;
#[cfg(feature = "python")]
pub mod inspect;
#[cfg(feature = "python")]
pub mod kernel;
#[cfg(feature = "python")]
pub mod report;
#[cfg(feature = "python")]
pub mod run;
#[cfg(feature = "python")]
pub mod sinks;
#[cfg(feature = "python")]
pub mod sources;

/// The package version, which `amoru.__version__` reports (d.2).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(feature = "python")]
mod module;
