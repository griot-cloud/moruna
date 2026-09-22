//! Amoru component 1, the contracts crate: every type and trait that crosses a component boundary.
//!
//! Design: `architecture/sdd/01-contracts.md`. The crate has almost no behaviour of its own: the
//! pointer conversions between a table column and a tensor (`payload`), byte accounting
//! (`payload`, `morsel`), the kernel fingerprint (`fingerprint`), and the two shared file formats,
//! the aligned binary format `AMB1` (`amb1`) and the page-aligned Arrow IPC record (`ipc`).
//! Everything else is a signature.
//!
//! It depends on `arrow`, `dlpark`, `thiserror` and `blake3` only (CT-T12); it knows nothing about
//! how any trait is implemented, how the process is threaded, or any policy.

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod amb1;
pub mod buffer;
pub mod completion;
pub mod error;
pub mod fingerprint;
pub mod ids;
pub mod ipc;
pub mod kernel;
pub mod knobs;
pub mod limits;
pub mod morsel;
pub mod payload;
pub mod placement;
pub mod reactor;
pub mod sink;
pub mod source;
pub mod tensor;
pub mod tier;
pub mod trace;
pub mod view;

// The contract is written in terms of `arrow` and `dlpark` types, so every consumer must see
// exactly the versions this crate was built against (preamble 6.2: one arrow in the workspace).
// Re-exporting them here is how a crate that depends on `amoru-kernel` alone, the testkit of
// d.15 among them, names a `RecordBatch` or a DLPack capsule.
pub use arrow;
pub use dlpark;

pub use buffer::{AllocStats, Allocator, ArenaHandle, Buffer};
pub use completion::{Completion, CompletionSender};
pub use error::{AmoruError, ConvertError};
pub use fingerprint::Fingerprint;
pub use ids::{ALIGNMENT, DeviceId, LOCAL_NODE, NodeId, RunId, Seq, SplitId, StageId};
pub use kernel::{
    GilState, InitCtx, Kernel, KernelHints, KernelKind, KernelState, NoState, ResumePolicy,
};
pub use knobs::{
    CancelToken, ErrorPolicy, Knob, KnobSnapshot, Knobs, ProbeResult, Prober, RecordHook,
    SchedulerStats, SizerKind, StageStats, StatsSource,
};
pub use limits::{Device, Guarantee, HostProfile, LimitSource, Limits, Sample, Sampler};
pub use morsel::{Morsel, MorselFeatures, Origin};
pub use payload::{DType, Payload, PayloadKind, PayloadSpec, SourceSchema, TierPref};
pub use placement::{
    CheckpointExtras, Locality, Placement, PlacementStats, QueueStats, ResumePoint, SourceCursor,
    TierBudgets,
};
pub use reactor::{CopyDst, CopySrc, IoPaths, ObjectMeta, ObjectMetadata, Reactor};
pub use sink::{Sink, SinkSummary};
pub use source::{RowRange, Source, Split};
pub use tensor::{DeleterHook, Dlpack, ManagedTensor};
pub use tier::{RemoteRef, SegmentRef, StagingCodec, TIER_COUNT, Tier, TierKind};
pub use trace::{Outcome, TraceRecord, TraceSink, TraceTail};
pub use view::BufferView;

/// `core::result::Result<T, AmoruError>`; every fallible contract returns it.
pub type Result<T> = core::result::Result<T, AmoruError>;

/// A boxed, `Send` future, the shape of every asynchronous contract method (CT-I7). Defined here
/// so no crate needs the `futures` crate to name it.
pub type BoxFuture<'a, T> = core::pin::Pin<Box<dyn core::future::Future<Output = T> + Send + 'a>>;
