//! The data-plane port: the one interface the engine moves batches through, whatever runs
//! its tasks (SDD 29 d.4).
//!
//! On the Moruna kernel the adapter is [`crate::KernelPlane`]: slots, pages moved by page
//! tables, the record checked before the engine sees it. On Linux (KA2) the adapter is the
//! sandbox's channel: a batch offered is written to the task's standard input as an Arrow
//! IPC stream, a record put is read from its standard output, a window granted bounds what
//! is read. The engine above is written against [`DataPlane`] alone and sees the same
//! events from both.

use alloc::{string::String, vec::Vec};

/// A task the plane has opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskId(pub u32);

/// A batch resident in the plane's arena: a checked record, read only from the moment it
/// arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BatchId(pub u64);

/// A port call waiting for the engine's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CallId {
    pub task: TaskId,
    pub token: u64,
}

/// What a task's plane has: how many input and output slots, and whether it may call
/// ports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    pub inputs: u32,
    pub outputs: u32,
    pub port: bool,
}

/// What the engine learns from the plane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A record the task put on its output `output`: checked, and now batch `batch`,
    /// which the engine holds until it drops it.
    Put {
        task: TaskId,
        output: u32,
        batch: BatchId,
        rows: u64,
        bytes: u64,
    },
    /// The task is done with its share of `batch` on input `input`.
    Released {
        task: TaskId,
        input: u32,
        batch: BatchId,
    },
    /// `batch` has no holder left and its memory is back in the arena: what the
    /// scheduler's buffer catalog drops (adaptive-runtime 9.2, release).
    Freed { batch: BatchId },
    /// A record the task put on `output` failed the check; the task has been ended with
    /// `reason`, and the bytes went back to the arena unread by anything else.
    Refused {
        task: TaskId,
        output: u32,
        reason: String,
    },
    /// A port call: the engine answers it with [`DataPlane::answer`].
    Call {
        call: CallId,
        port: u64,
        request: Vec<u8>,
    },
    /// The task has ended and everything it held is back.
    Ended { task: TaskId },
}

/// Why the plane refused an operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlaneError {
    /// No task, slot, batch or call by that name.
    Unknown(&'static str),
    /// The arena cannot give that much within its budget now: wait for a release.
    Budget { asked: u64, free: u64 },
    /// The slot holds as much as it can: wait for an event from it.
    Full,
    /// The task has ended, or the slot was finished.
    Closed,
    /// A bound of the plane: tasks waited on at once, slots of a task.
    Limit(&'static str),
    /// The kernel refused a call, with its error.
    Kernel(moruna_abi::Error),
    /// A record the engine loaded failed the check, with the check's sentence.
    Refused(String),
}

/// What the arena holds now (adaptive-runtime 9.1, the buffer catalog's view of memory).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Residency {
    /// The arena's budget.
    pub budget: u64,
    /// Bytes of memory objects the arena has made.
    pub made: u64,
    /// Bytes held by resident batches.
    pub resident: u64,
    /// Resident batches.
    pub batches: u64,
    /// Shares of resident batches held by tasks.
    pub shares: u64,
    /// Bytes of windows granted and not yet put or returned.
    pub windows: u64,
    /// Bytes in the arena's pool, free for the next window.
    pub pooled: u64,
}

/// The data-plane port (SDD 29 d.4): what the engine does with the batches its tasks read
/// and write, the same on the kernel and on Linux.
pub trait DataPlane {
    /// What the engine needs to start a task with this plane: on the kernel, its slots
    /// and port endpoint, which `process_plane` attaches to the created process.
    type Task;

    /// Opens a task's plane, before the task starts.
    fn open(&mut self, shape: Shape) -> Result<(TaskId, Self::Task), PlaneError>;

    /// Gives `task` a share of `batch` on its input `input`: the same bytes, read only,
    /// with no copy, however many tasks it is offered to (contracts CT-I15).
    fn offer(&mut self, task: TaskId, input: u32, batch: BatchId) -> Result<(), PlaneError>;

    /// No more batches on `task`'s input `input`: the task reads the end once it has
    /// taken what was offered.
    fn finish(&mut self, task: TaskId, input: u32) -> Result<(), PlaneError>;

    /// Grants `task` a window of at least `bytes` on its output `output`, from the arena
    /// and within its budget.
    fn grant(&mut self, task: TaskId, output: u32, bytes: u64) -> Result<(), PlaneError>;

    /// The next event, waiting until `deadline` (nanoseconds on the kernel's clock; zero
    /// polls); `None` when the deadline passed with nothing.
    fn next(&mut self, deadline: u64) -> Result<Option<Event>, PlaneError>;

    /// Answers a port call: the reply's bytes, or the error the call returns.
    fn answer(
        &mut self,
        call: CallId,
        reply: Result<&[u8], moruna_abi::Error>,
    ) -> Result<(), PlaneError>;

    /// The engine's own hold on `batch` ends: once no task holds a share either, its
    /// memory goes back to the arena ([`Event::Freed`]).
    fn drop_batch(&mut self, batch: BatchId) -> Result<(), PlaneError>;

    /// What the arena holds.
    fn residency(&self) -> Residency;
}
