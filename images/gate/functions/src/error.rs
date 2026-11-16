//! What a port refuses.

use alloc::string::String;
use core::fmt;

/// Why a function did not produce a batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The input lacks a column the function reads, or has it in a type it cannot take.
    Schema(String),
    /// A value the Python function raises on: the port refuses the batch as 0.6.6 failed it.
    Value(String),
    /// Buffers that do not hold what they claim (a bitmap or offsets too short).
    Layout(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Schema(m) => write!(f, "schema: {m}"),
            Error::Value(m) => write!(f, "value: {m}"),
            Error::Layout(m) => write!(f, "layout: {m}"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for Error {}

pub type Result<T> = core::result::Result<T, Error>;
