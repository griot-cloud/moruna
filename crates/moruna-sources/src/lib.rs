//! Moruna component 7, the sources (`Source` over Parquet, safetensors, aligned binary and a
//! Python iterator).
//!
//! Design: `architecture/sdd/07-sources.md`. A source turns a dataset into splits with
//! metadata (`plan`), then reads splits into resident payloads on request (`read`). Every
//! payload byte a source returns was allocated from the `&dyn Allocator` the read received,
//! in the tier the read asked for (SO-I3), and every file byte arrives through the
//! `Arc<dyn Reactor>` the source was built with; this crate opens no file of its own for
//! bytes and maps nothing.
//!
//! There is no `unsafe` in this crate (section l): a tensor is a safe view into an arena
//! buffer through `ManagedTensor::from_buffer`, and the decode copy that Parquet needs
//! (the one exception G-I2 grants) goes through `Buffer::into_arrow_buffer`.
//!
//! The module named `parquet` here is this crate's Parquet source (section l names the
//! files); the Parquet crate is spelled `::parquet` throughout.

#![deny(missing_docs)]
#![deny(unsafe_code)]
// `MorunaError` carries a morsel's features (CT-I10), so every `Result` in the crate has a
// large `Err`. The shape of the error type is the contract's (d.14), not this crate's.
#![allow(clippy::result_large_err)]

pub mod parquet;
pub mod stats;
pub mod tensor;

#[cfg(feature = "python")]
pub mod py_iter;

mod util;

pub use crate::parquet::{ParquetSource, ParquetSourceConfig, RowFilter, ScalarValue};
pub use crate::tensor::{TensorSource, TensorSourceConfig};
pub use stats::SourceStats;

#[cfg(feature = "python")]
pub use py_iter::PyIteratorSource;
