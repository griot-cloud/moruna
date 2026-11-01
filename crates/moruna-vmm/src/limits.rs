//! Native artifact resource admission limits, independent of retired Linux guest configuration.
pub const MIN_MEMORY_BYTES: u64 = 256 << 20;
pub const MEMORY_ALIGN_BYTES: u64 = 2 << 20;
pub const MAX_MEMORY_BYTES: u64 = 1 << 40;
pub const MAX_VCPUS: u32 = 64;
