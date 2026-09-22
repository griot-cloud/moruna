//! The reactor contract, object-store metadata and the copy endpoints (contracts d.9).
//!
//! Submission never blocks the caller: every method returns after enqueueing, and any
//! concurrency limit is waited for on a reactor thread (RE-I6). A worker may therefore issue a
//! reactor operation from inside `Placement::push` or `pop` (placement g) without violating
//! preamble 4.1; what it may not do is `wait`.

use std::path::Path;

use crate::buffer::Buffer;
use crate::completion::Completion;
use crate::tier::SegmentRef;
use crate::view::BufferView;

/// One end of a `copy`. A `Disk` endpoint names a registered staging segment range and
/// is legal only on the GDS rows of the reactor's copy table; everywhere else disk is
/// reached through `read_file` and `write_file`.
pub enum CopySrc {
    /// Bytes a view keeps alive.
    View(BufferView),
    /// A range of a registered staging segment (GDS only).
    Disk(SegmentRef),
}

/// The destination end of a `copy`.
pub enum CopyDst {
    /// A buffer the operation fills and returns.
    Buffer(Buffer),
    /// A range of a registered staging segment (GDS only).
    Disk(SegmentRef),
}

/// Which direct paths a reactor selected at start (for the run report).
#[derive(Clone, Debug, Default)]
pub struct IoPaths {
    /// Direct IO (`O_DIRECT`) in use for aligned file operations.
    pub direct_io: bool,
    /// io_uring in use.
    pub io_uring: bool,
    /// GPUDirect Storage in use.
    pub gds: bool,
    /// The arena is page-locked, so device copies go by the copy engine.
    pub pinned: bool,
    /// RDMA in use; always false without the `rdma` feature.
    pub rdma: bool,
}

/// Object-store metadata for a source's `plan`. Implemented by the reactor; a source
/// holds `Arc<dyn ObjectMetadata>` beside `Arc<dyn Reactor>` (the facade passes the
/// same reactor for both), so a test can supply metadata without a runtime.
pub trait ObjectMetadata: Send + Sync {
    /// Size and validators of one object.
    fn head_object(&self, url: &str) -> Completion<ObjectMeta>;
    /// Every object under a prefix.
    fn list_prefix(&self, url: &str) -> Completion<Vec<ObjectMeta>>;
}

/// What an object store says about one object.
#[derive(Clone, Debug)]
pub struct ObjectMeta {
    /// The object's URL.
    pub url: String,
    /// Size in bytes.
    pub size: u64,
    /// Last modification time in nanoseconds since the epoch, where the store reports one.
    pub last_modified_ns: Option<u64>,
    /// The store's entity tag, where it reports one.
    pub e_tag: Option<String>,
}

/// Every byte movement in the runtime (component 6).
pub trait Reactor: Send + Sync {
    /// Read `dst.len()` bytes from `path` at `offset` into `dst`. Direct IO when the
    /// buffer, offset and length are page-aligned and the host allows; buffered otherwise.
    /// Exactly `dst.len()` bytes or `Io`; see `read_file_opt` for short reads.
    fn read_file(&self, path: &Path, offset: u64, dst: Buffer) -> Completion<Buffer>;
    /// As `read_file`, but a read that ends at end-of-file returns the bytes read.
    fn read_file_opt(
        &self,
        path: &Path,
        offset: u64,
        dst: Buffer,
        allow_short: bool,
    ) -> Completion<(Buffer, usize)>;
    /// Write `src.len()` bytes at `offset`. The view keeps the bytes alive; on error the
    /// caller still holds them.
    fn write_file(&self, path: &Path, offset: u64, src: BufferView) -> Completion<()>;
    /// Ranged object read (S3-compatible, GCS, Azure, file://) into `dst`.
    fn read_object(&self, url: &str, offset: u64, dst: Buffer) -> Completion<Buffer>;
    /// Write an object from a view.
    fn write_object(&self, url: &str, src: BufferView) -> Completion<()>;
    /// DMA between tiers per the reactor's copy table (06 f.5): PinnedHost to and from Device
    /// via the copy engine; Disk to and from Device via GDS when present. Returns the
    /// destination buffer when the destination is a buffer. Never a CPU copy (G-I2).
    fn copy(&self, src: CopySrc, dst: CopyDst) -> Completion<Option<Buffer>>;
    /// Name a staging segment file so `SegmentRef`s can be resolved by `copy`'s Disk
    /// endpoints and so the reactor can cache its descriptor; `unregister_segment`
    /// closes the descriptor so an unlinked file's space is actually released.
    fn register_segment(&self, segment: u32, path: &Path) -> crate::Result<()>;
    /// Close a segment's descriptor.
    fn unregister_segment(&self, segment: u32);
    /// Which direct paths this reactor selected at start (for the run report).
    fn paths(&self) -> IoPaths;
    /// Cancel what can be cancelled; every outstanding completion resolves within the
    /// longest single operation's duration (RE-I7). Idempotent.
    fn shutdown(&self);
}
