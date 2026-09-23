//! The staging log: the segment files, their format and their lifecycle.

pub mod read;
pub mod segment;
pub mod write;

pub use segment::{Segment, Staging};
