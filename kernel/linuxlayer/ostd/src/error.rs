// SPDX-License-Identifier: MPL-2.0

/// The error type which is returned from the APIs of this crate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// Invalid arguments provided.
    InvalidArgs,
    /// Insufficient memory available.
    NoMemory,
    /// Page fault occurred.
    PageFault,
    /// Access to a resource is denied.
    AccessDenied,
    /// Input/output error.
    IoError,
    /// Insufficient system resources.
    NotEnoughResources,
    /// Arithmetic Overflow occurred.
    Overflow,
}
