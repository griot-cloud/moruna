//! What [`crate::serve`] needs of the data plane: a task's half of its rings. On the core it is
//! `moruna_sys::plane::Rings` (`sys`); on a host, the rings in memory (`host`).

use std::panic::RefUnwindSafe;
use std::ptr::NonNull;
use std::sync::Arc;

/// Memory a batch lies in, kept alive for as long as any part of the batch is: the host's
/// stand-in for a region. On the core the region is mapped for the task's whole life, and nothing
/// needs keeping.
pub(crate) trait Mapped: Send + Sync + RefUnwindSafe {}

impl<T: Send + Sync + RefUnwindSafe> Mapped for T {}

/// The cookie of a record that answers no input: one written while the task holds no batch, as
/// [`crate::Function::end`] writes. No input has it, and the engine, finding it on no input the
/// task holds, moves nothing for it and takes the record (`moruna_plane`'s ringed engine).
pub(crate) const NO_INPUT: u64 = u64::MAX;

/// A batch the engine posted: its record, in place.
///
/// `addr` is valid for reads of `len` bytes, and no one writes them, until the lane releases the
/// batch; the memory stays mapped for as long as `keep` lives (or the task does, when `keep` is
/// `None`).
pub(crate) struct Posted {
    pub cookie: u64,
    pub addr: NonNull<u8>,
    pub len: usize,
    pub keep: Option<Arc<dyn Mapped>>,
}

/// A window to write a record in: valid for writes of `len` bytes, by this task alone, until the
/// record in it is posted.
pub(crate) struct Window {
    pub number: u64,
    pub addr: NonNull<u8>,
    pub len: usize,
}

/// Why a task has no lane: the exit code it ends with, and the line its log gets.
#[derive(Debug)]
pub(crate) struct Unattached {
    pub code: i64,
    pub line: String,
}

/// A task's half of its rings.
pub(crate) trait Lane {
    /// The task's name, which every line of its log starts with.
    fn name(&self) -> &str;
    /// The next batch, `None` while there is none; it stays the task's until [`Lane::release`].
    fn batch(&mut self) -> Result<Option<Posted>, String>;
    /// Whether every batch was taken and the engine posts no more.
    fn ended(&mut self) -> bool;
    /// Sleeps until a batch or the end is there.
    fn wait_input(&mut self);
    /// How many bytes each window holds.
    fn window_bytes(&self) -> u64;
    /// A free window, `None` while the engine has given none.
    fn window(&mut self) -> Result<Option<Window>, String>;
    /// Sleeps until a window is free; `false` when none ever will be.
    fn wait_window(&mut self) -> bool;
    /// Posts the record of `len` bytes at the start of window `window`, answering `cookie`;
    /// `false` when the output ring is full.
    fn post(&mut self, window: u64, len: u64, cookie: u64) -> bool;
    /// The oldest batch taken is done: every record posted before answers it.
    fn release(&mut self);
    /// Says the task's exit code on the rings, and returns it.
    fn exit(&mut self, code: i64) -> i64;
    /// A line in the task's log.
    fn log(&mut self, line: &str);
}
