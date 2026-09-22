//! Amoru component 8, the sinks (`Sink`, `SinkHandle`, `ReorderBuffer`).
//!
//! Design: `architecture/sdd/08-sinks.md`. A sink absorbs morsels and produces durable output:
//! `ParquetSink` to an object store or a local directory, `TensorSink` to safetensors or the
//! aligned binary format of contracts e.4, `ArrowIpcSink` to an Arrow IPC file whose record
//! batches are the page-aligned records of contracts e.7. `ReorderBuffer` wraps any sink and
//! delivers morsels in sequence order within a byte bound, and `SinkHandle` is the one type the
//! scheduler drives, plain or ordered.
//!
//! Writes run on the reactor from views over the payload's own buffers. The single CPU copy a
//! sink is allowed (G-I2) is the Parquet encode, which lands in arena memory and is counted in
//! `SinkStats::encode_bytes` and through `Allocator::note_payload_copy`; `ArrowIpcSink` and
//! `TensorSink` copy nothing at all.
//!
//! This crate contains no `unsafe` (section l).

#![deny(missing_docs)]
#![deny(unsafe_code)]
// Every fallible function here returns the contract's `AmoruError` (contracts d.14), whose
// size is fixed by that crate and is above clippy's 128 byte threshold. This crate may not
// box it: the error type crosses every component boundary and is the contract's to change.
#![allow(clippy::result_large_err)]

mod checkpoint;
mod commit;
mod handle;
mod ipc_sink;
mod parquet_sink;
mod reorder;
mod stats;
mod tensor_sink;

pub use handle::SinkHandle;
pub use ipc_sink::{ArrowIpcSink, ArrowIpcSinkConfig};
pub use parquet_sink::{ParquetSink, ParquetSinkConfig};
pub use reorder::ReorderBuffer;
pub use stats::SinkStats;
pub use tensor_sink::{TensorFormat, TensorSink, TensorSinkConfig};

use amoru_kernel::{Allocator, AmoruError, Payload, Result, Tier};

/// Where a sink is in the state machine of e.1.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Phase {
    /// Built, not yet opened or resumed.
    Created,
    /// Accepting writes.
    Open,
    /// A write failed; `finish` aborts what is open and returns the original error.
    Failed(String),
    /// `finish` ran.
    Finished,
}

impl Phase {
    /// Refuse a call that this state does not allow, naming what is wrong (e.1).
    pub(crate) fn require_open(&self) -> Result<()> {
        match self {
            Phase::Open => Ok(()),
            Phase::Created => Err(AmoruError::Sink("the sink is not open".into())),
            Phase::Failed(msg) => Err(AmoruError::Sink(format!("the sink failed: {msg}"))),
            Phase::Finished => Err(AmoruError::Sink("the sink is finished".into())),
        }
    }

    /// Refuse a second `open` or `resume` (e.1).
    pub(crate) fn require_created(&self) -> Result<()> {
        match self {
            Phase::Created => Ok(()),
            Phase::Open => Err(AmoruError::Sink("the sink is already open".into())),
            Phase::Failed(msg) => Err(AmoruError::Sink(format!("the sink failed: {msg}"))),
            Phase::Finished => Err(AmoruError::Sink("the sink is finished".into())),
        }
    }
}

/// The run's one host tier: `PinnedHost` when the arena is page-locked, `Host` otherwise
/// (contracts e.1). A sink allocates its own buffers in whichever the run has.
pub(crate) fn host_tier(alloc: &dyn Allocator) -> Tier {
    if alloc.is_pinned() {
        Tier::PinnedHost
    } else {
        Tier::Host
    }
}

/// Sinks are host-only (SI-I6): the placement engine demotes a device payload to the host tier
/// before a sink sees one, so a sink that receives one refuses it rather than copying it.
pub(crate) fn require_host(payload: &Payload) -> Result<()> {
    match payload.tier() {
        Tier::Host | Tier::PinnedHost => Ok(()),
        Tier::Device(_) => Err(AmoruError::Sink("device payload".into())),
        Tier::Disk(_) => Err(AmoruError::Sink("disk payload".into())),
        Tier::Remote(_, _) => Err(AmoruError::Unsupported("rdma")),
    }
}

/// `n` rounded up to the next multiple of `to`.
pub(crate) fn round_up(value: u64, to: u64) -> u64 {
    value.div_ceil(to) * to
}

/// The name of file `index` of a rolling sink (e.2, e.3).
pub(crate) fn part_name(index: u32, extension: &str) -> String {
    format!("part-{index:05}.{extension}")
}
