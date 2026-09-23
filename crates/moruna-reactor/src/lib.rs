//! Moruna component 6, the IO reactor: where every byte enters and leaves the process and where
//! every move between tiers is issued.
//!
//! Design: `architecture/sdd/06-reactor.md`. The reactor owns the threads that talk to storage
//! and to accelerators so that no worker ever blocks on IO: every trait method of contracts
//! d.9 builds an operation, puts it on a submission queue and returns a `Completion`, and
//! every permit, path decision and syscall happens afterwards on a reactor thread (RE-I2,
//! RE-I6). The only blocking anywhere is `Completion::wait`, which only the scheduler's drives
//! may call.
//!
//! Paths are chosen once, at `new`, from the host profile: a path the host declares `Present`
//! is a guarantee, so a failure on it is an error, and a path discovery only probed may fall
//! back once, producing byte-identical output (G-I7, RE-I3).

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
// Every fallible function here returns the contract's `MorunaError` (contracts d.14), whose size
// is fixed by that crate and is above clippy's 128 byte threshold; the reactor may not box it.
#![allow(clippy::result_large_err)]

mod copy;
mod fdcache;
mod file_blocking;
mod file_uring;
mod gds;
mod object;
mod paths;
mod runtime;
mod segments;
mod stats;
#[cfg(test)]
mod tests;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use moruna_kernel::{
    Allocator, MorunaError, Buffer, BufferView, Completion, CopyDst, CopySrc, DeviceId, HostProfile,
    IoPaths, ObjectMeta, ObjectMetadata, Result,
};

use crate::fdcache::{FdCache, Mode, Opener, SysOpener};
use crate::file_blocking::{BlockingEngine, FileEngine};
use crate::object::{ObjectBackend, ObjectLayer};
use crate::paths::Paths;
use crate::runtime::{
    AbortMultipartOp, CopyOp, DeleteObjectOp, Inner, MetaOp, Queues, ReadFileOp, ReadFileOptOp,
    ReadObjectOp, WriteFileOp, WriteObjectOp,
};
use crate::segments::{SegmentEntry, Segments};
use crate::stats::Counters;

pub use crate::stats::ReactorStats;

/// How long `shutdown` waits for what is in flight before it resolves the rest `Cancelled`
/// (f.7, RE-I7).
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// The pinned bounce buffer of e.2.
const BOUNCE_BYTES: usize = 64 * 1024 * 1024;

/// Everything the reactor is built from (06 d.1).
#[derive(Clone, Debug)]
pub struct ReactorConfig {
    /// `reactor.threads`.
    pub threads: usize,
    /// `reactor.object_concurrency`.
    pub object_concurrency: usize,
    /// `reactor.file_depth`.
    pub file_depth: usize,
    /// `page.bytes`, the alignment direct IO needs.
    pub page_bytes: usize,
    /// The host profile, every field resolved by discovery (DS-I6).
    pub profile: HostProfile,
    /// The devices discovery found.
    pub devices: Vec<DeviceId>,
    /// Credentials and endpoints, from the surface.
    pub object_store: ObjectStoreConfig,
}

impl Default for ReactorConfig {
    /// The defaults of the preamble's configuration table.
    fn default() -> ReactorConfig {
        ReactorConfig {
            threads: 2,
            object_concurrency: 8,
            file_depth: 32,
            page_bytes: 4096,
            profile: HostProfile::default(),
            devices: Vec::new(),
            object_store: ObjectStoreConfig::default(),
        }
    }
}

/// Everything the `object_store` builders need, for every backend a URL may name.
/// A backend whose field is `None` is unavailable: a URL for it is `Config { name:
/// "object_store" }` at the first operation. Values come from the surface's arguments
/// or, when a field is `None` there, from the environment the way the `object_store`
/// crate reads it (`AWS_*`, `GOOGLE_*`, `AZURE_*`); the reactor does not read the
/// environment itself.
#[derive(Clone, Debug, Default)]
pub struct ObjectStoreConfig {
    /// S3 and S3-compatible endpoints.
    pub s3: Option<S3Config>,
    /// Google Cloud Storage.
    pub gcs: Option<GcsConfig>,
    /// Azure Blob Storage.
    pub azure: Option<AzureConfig>,
    /// Root for `file://` URLs; `None` means `file://` URLs are absolute paths.
    pub local_root: Option<std::path::PathBuf>,
    /// Permit plain-HTTP endpoints (MinIO in CI); false in every default.
    pub allow_http: bool,
}

/// S3 and S3-compatible credentials and endpoints.
#[derive(Clone, Debug, Default)]
pub struct S3Config {
    /// Custom endpoint (MinIO, R2); `None` is AWS.
    pub endpoint: Option<String>,
    /// Region.
    pub region: Option<String>,
    /// Access key id.
    pub access_key_id: Option<String>,
    /// Secret access key.
    pub secret_access_key: Option<String>,
    /// Session token.
    pub session_token: Option<String>,
    /// Default bucket when the URL is a bare key; the URL's bucket wins.
    pub bucket: Option<String>,
}

/// Google Cloud Storage credentials.
#[derive(Clone, Debug, Default)]
pub struct GcsConfig {
    /// Service account file.
    pub service_account_path: Option<std::path::PathBuf>,
    /// Service account key as JSON; one of the two, both set is `Config`.
    pub service_account_json: Option<String>,
    /// Default bucket.
    pub bucket: Option<String>,
}

/// Azure Blob Storage credentials.
#[derive(Clone, Debug, Default)]
pub struct AzureConfig {
    /// Storage account.
    pub account: Option<String>,
    /// Access key.
    pub access_key: Option<String>,
    /// Default container.
    pub container: Option<String>,
}

/// Seams the tests of section k drive: an opener that can refuse `O_DIRECT` for one path
/// (RE-T12), an object backend that counts requests in flight and can fail a part (RE-T6,
/// RE-T9), and a ring factory that can refuse to start (RE-T15).
#[derive(Default)]
pub(crate) struct Hooks {
    pub(crate) opener: Option<Arc<dyn Opener>>,
    pub(crate) object_backend: Option<Arc<dyn ObjectBackend>>,
    /// `Some(false)` makes the io_uring driver refuse to start, as the seccomp shim of RE-T15
    /// does on a host that has one.
    pub(crate) ring_starts: Option<bool>,
    /// Replaces the file engine, so the fallback machinery of f.9 can be driven on a host that
    /// has no io_uring: both Linux file paths are written behind the `FileEngine` trait and
    /// exercised through it here, and for real in the weekly container job.
    pub(crate) engine: Option<Arc<dyn FileEngine>>,
    /// Treat the io_uring row of e.2 as one this build can drive, so its fallback rule can be
    /// exercised where the ring itself cannot exist.
    pub(crate) force_uring: bool,
}

/// `page.bytes`: the configuration's value, or the arena's when the caller left it unset.
fn page_bytes(cfg: &ReactorConfig, alloc: &dyn Allocator) -> usize {
    if cfg.page_bytes == 0 {
        alloc.page_bytes().max(1)
    } else {
        cfg.page_bytes
    }
}

/// The IO reactor (component 6).
pub struct Reactor {
    inner: Arc<Inner>,
    /// The arena, held for the reactor's life because every buffer in flight releases to it.
    _arena: Arc<dyn Allocator>,
    /// The submission queues; `None` after `shutdown`, which is what makes a later call
    /// resolve `Cancelled` and ends the drain tasks.
    queues: RwLock<Option<Queues>>,
    /// The tokio runtime, held here and not in `Inner` so that dropping the reactor stops the
    /// tasks that hold `Inner`.
    rt: Mutex<Option<tokio::runtime::Runtime>>,
    shutting_down: AtomicBool,
}

impl std::fmt::Debug for Reactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reactor")
            .field("paths", &self.inner.io_paths)
            .finish()
    }
}

impl Reactor {
    /// Build the runtime, select the paths (e.2) and, when `alloc.is_pinned()` is false and a
    /// device exists, allocate the pinned bounce buffer. The allocator is the arena behind the
    /// contract; the reactor uses `alloc`, `page_bytes`, `contains`, `tier_of` and `is_pinned`
    /// and nothing arena-specific.
    pub fn new(cfg: ReactorConfig, alloc: Arc<dyn Allocator>) -> Result<Arc<Reactor>> {
        Reactor::new_with(cfg, alloc, Hooks::default())
    }

    pub(crate) fn new_with(
        cfg: ReactorConfig,
        alloc: Arc<dyn Allocator>,
        hooks: Hooks,
    ) -> Result<Arc<Reactor>> {
        let mut paths = Paths::select(&cfg.profile, alloc.is_pinned());
        if hooks.force_uring {
            paths.uring.on = cfg.profile.io_uring.is_available();
        }
        let rt = start_runtime(cfg.threads, None)?;
        let handle = rt.handle().clone();

        let blocking: Arc<dyn FileEngine> = Arc::new(BlockingEngine::new(handle.clone()));
        let engine = select_file_engine(&mut paths, &cfg, &hooks, &blocking)?;
        select_gds(&mut paths)?;

        let bounce_bytes = if !alloc.is_pinned() && !cfg.devices.is_empty() && paths::CUDA_BUILT {
            BOUNCE_BYTES
        } else {
            0
        };
        let io_paths = paths.to_io_paths();
        tracing::info!(
            target: "reactor.paths",
            direct_io = io_paths.direct_io,
            io_uring = io_paths.io_uring,
            gds = io_paths.gds,
            pinned = io_paths.pinned,
            rdma = io_paths.rdma,
            "io paths selected"
        );
        let opener = hooks.opener.unwrap_or_else(|| Arc::new(SysOpener));
        let inner = Arc::new(Inner {
            paths,
            io_paths,
            page_bytes: page_bytes(&cfg, alloc.as_ref()),
            counters: Counters::default(),
            segments: Segments::default(),
            fds: FdCache::new(opener),
            objects: ObjectLayer::new(cfg.object_store.clone(), hooks.object_backend),
            engine,
            fallback: blocking,
            cancelled: AtomicBool::new(false),
            handle,
            bounce_bytes,
        });
        let queues = runtime::start(&inner, cfg.object_concurrency, cfg.file_depth);
        Ok(Arc::new(Reactor {
            inner,
            _arena: alloc,
            queues: RwLock::new(Some(queues)),
            rt: Mutex::new(Some(rt)),
            shutting_down: AtomicBool::new(false),
        }))
    }

    /// What the reactor has counted so far (j).
    pub fn stats(&self) -> ReactorStats {
        self.inner.counters.snapshot()
    }

    /// True while operations may still be submitted.
    fn open(&self) -> bool {
        !self.inner.cancelled()
    }

    /// Cached descriptors plus registered segments; zero after `shutdown` (RE-T7, f.7).
    #[cfg(test)]
    pub(crate) fn open_descriptors(&self) -> usize {
        self.inner.fds.len() + self.inner.segments.len()
    }

    fn queues(&self) -> std::sync::RwLockReadGuard<'_, Option<Queues>> {
        self.queues.read().unwrap_or_else(|e| e.into_inner())
    }
}

/// The reactor's tokio runtime, and the one place a thread is asked for at start.
///
/// A host that will not give the process a thread (the `ulimit -u` of a loaded build machine,
/// a cgroup's pid limit) is reported as `Io`, so the runtime diagnoses the failure rather than
/// being signalled by it (G-I8) and the caller of `Reactor::new` sees an error.
///
/// `tokio` does not return that failure: its multi-threaded builder panics with "OS can't spawn
/// worker thread" from inside `build`, and a panic on the caller's thread is exactly what G-I8
/// forbids, so it is caught here, at the one place this component asks for a thread, and turned
/// into the error. `stack_bytes` exists so a test can make the first thread refuse to start;
/// nothing else sets it.
fn start_runtime(threads: usize, stack_bytes: Option<usize>) -> Result<tokio::runtime::Runtime> {
    let threads = threads.max(1);
    let built = std::panic::catch_unwind(move || {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder
            .worker_threads(threads)
            .thread_name("moruna-reactor")
            .enable_all();
        if let Some(bytes) = stack_bytes {
            builder.thread_stack_size(bytes);
        }
        builder.build()
    });
    let failure = match built {
        Ok(Ok(rt)) => return Ok(rt),
        Ok(Err(e)) => e.to_string(),
        Err(panic) => panic_message(panic.as_ref()),
    };
    Err(MorunaError::Io {
        op: "reactor_start",
        target: format!("{threads} reactor threads"),
        msg: failure,
    })
}

/// What a caught panic was about, for the message of the error it becomes.
fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        return (*s).to_string();
    }
    if let Some(s) = panic.downcast_ref::<String>() {
        return s.clone();
    }
    "the runtime could not be built".to_string()
}

/// The io_uring row of e.2: selected when the profile allows it and this build has it; a
/// driver that refuses to start is a fallback on an available path and a `Config` error on a
/// guaranteed one (RE-I3, RE-T15).
fn select_file_engine(
    paths: &mut Paths,
    cfg: &ReactorConfig,
    hooks: &Hooks,
    blocking: &Arc<dyn FileEngine>,
) -> Result<Arc<dyn FileEngine>> {
    if let Some(engine) = &hooks.engine {
        return Ok(Arc::clone(engine));
    }
    if !paths.uring.on {
        return Ok(Arc::clone(blocking));
    }
    let started = ring(cfg.file_depth, hooks);
    match started {
        Ok(Some(engine)) => Ok(engine),
        Ok(None) | Err(_) if paths.uring.guaranteed => Err(MorunaError::Config {
            name: "host_profile",
            msg: "io_uring is Present but the ring would not start".into(),
        }),
        Ok(None) => {
            paths.disable_uring();
            tracing::info!(target: "reactor.paths", "io_uring was probed but the ring would not start; using the blocking pool");
            Ok(Arc::clone(blocking))
        }
        Err(e) => {
            paths.disable_uring();
            tracing::info!(target: "reactor.paths", error = %e, "io_uring would not start; using the blocking pool");
            Ok(Arc::clone(blocking))
        }
    }
}

/// Start the ring, or say it did not start. `Ok(None)` is "this build or this host has no
/// ring"; `Err` is a ring that exists and refused.
fn ring(_depth: usize, hooks: &Hooks) -> std::io::Result<Option<Arc<dyn FileEngine>>> {
    if hooks.ring_starts == Some(false) {
        return Ok(None);
    }
    #[cfg(all(target_os = "linux", feature = "uring"))]
    {
        let engine = file_uring::UringEngine::new(_depth as u32)?;
        return Ok(Some(Arc::new(engine)));
    }
    #[cfg(not(all(target_os = "linux", feature = "uring")))]
    Ok(None)
}

/// The GDS row of e.2, with the same rule as the ring.
fn select_gds(paths: &mut Paths) -> Result<()> {
    if !paths.gds.on {
        return Ok(());
    }
    if gds::driver_open()? {
        return Ok(());
    }
    if paths.gds.guaranteed {
        return Err(MorunaError::Config {
            name: "host_profile",
            msg: "gds is Present but the cuFile driver would not open".into(),
        });
    }
    paths.disable_gds();
    Ok(())
}

impl ObjectMetadata for Reactor {
    fn head_object(&self, url: &str) -> Completion<ObjectMeta> {
        let (tx, completion) = Completion::channel();
        let op = MetaOp::Head {
            url: url.to_string(),
            tx,
        };
        self.send(|q| &q.meta, op);
        completion
    }

    fn list_prefix(&self, url: &str) -> Completion<Vec<ObjectMeta>> {
        let (tx, completion) = Completion::channel();
        let op = MetaOp::List {
            url: url.to_string(),
            tx,
        };
        self.send(|q| &q.meta, op);
        completion
    }
}

impl Reactor {
    /// Put one operation on its queue (f.8). When the reactor is shut down the operation is
    /// dropped here, and dropping its `CompletionSender` resolves the completion `Cancelled`.
    fn send<T, F>(&self, pick: F, op: T)
    where
        F: FnOnce(&Queues) -> &tokio::sync::mpsc::UnboundedSender<T>,
    {
        let guard = self.queues();
        let Some(queues) = guard.as_ref() else {
            return;
        };
        if !self.open() {
            return;
        }
        self.inner.counters.enqueued();
        if pick(queues).send(op).is_err() {
            self.inner.counters.dequeued();
        }
    }
}

impl moruna_kernel::Reactor for Reactor {
    fn read_file(&self, path: &Path, offset: u64, dst: Buffer) -> Completion<Buffer> {
        if dst.is_empty() {
            return Completion::resolved(Ok(dst));
        }
        let (tx, completion) = Completion::channel();
        self.send(
            |q| &q.read_file,
            ReadFileOp {
                path: path.to_path_buf(),
                offset,
                dst,
                tx,
            },
        );
        completion
    }

    fn read_file_opt(
        &self,
        path: &Path,
        offset: u64,
        dst: Buffer,
        allow_short: bool,
    ) -> Completion<(Buffer, usize)> {
        if dst.is_empty() {
            return Completion::resolved(Ok((dst, 0)));
        }
        let (tx, completion) = Completion::channel();
        self.send(
            |q| &q.read_file_opt,
            ReadFileOptOp {
                path: path.to_path_buf(),
                offset,
                dst,
                allow_short,
                tx,
            },
        );
        completion
    }

    fn write_file(&self, path: &Path, offset: u64, src: BufferView) -> Completion<()> {
        if src.is_empty() {
            return Completion::resolved(Ok(()));
        }
        let (tx, completion) = Completion::channel();
        self.send(
            |q| &q.write_file,
            WriteFileOp {
                path: path.to_path_buf(),
                offset,
                src,
                tx,
            },
        );
        completion
    }

    fn read_object(&self, url: &str, offset: u64, dst: Buffer) -> Completion<Buffer> {
        if dst.is_empty() {
            return Completion::resolved(Ok(dst));
        }
        let (tx, completion) = Completion::channel();
        self.send(
            |q| &q.read_object,
            ReadObjectOp {
                url: url.to_string(),
                offset,
                dst,
                tx,
            },
        );
        completion
    }

    fn write_object(&self, url: &str, src: BufferView) -> Completion<()> {
        if src.is_empty() {
            return Completion::resolved(Ok(()));
        }
        let (tx, completion) = Completion::channel();
        self.send(
            |q| &q.write_object,
            WriteObjectOp {
                url: url.to_string(),
                src,
                tx,
            },
        );
        completion
    }

    fn delete_object(&self, url: &str) -> Completion<()> {
        let (tx, completion) = Completion::channel();
        self.send(
            |q| &q.delete_object,
            DeleteObjectOp {
                url: url.to_string(),
                tx,
            },
        );
        completion
    }

    fn abort_multipart(&self, url: &str, upload_id: &str) -> Completion<()> {
        let (tx, completion) = Completion::channel();
        self.send(
            |q| &q.abort_multipart,
            AbortMultipartOp {
                url: url.to_string(),
                upload_id: upload_id.to_string(),
                tx,
            },
        );
        completion
    }

    fn copy(&self, src: CopySrc, dst: CopyDst) -> Completion<Option<Buffer>> {
        // Every pair contracts e.1 calls illegal, and every `Remote` endpoint, is decided on
        // this thread, before anything is submitted (f.5).
        let plan = match copy::plan(
            &src,
            &dst,
            self.inner.paths.gds.on,
            self.inner.paths.arena_pinned,
        ) {
            Ok(plan) => plan,
            Err(e) => {
                self.inner.counters.note_error();
                return Completion::resolved(Err(e));
            }
        };
        let (tx, completion) = Completion::channel();
        self.send(|q| &q.copy, CopyOp { plan, src, dst, tx });
        completion
    }

    fn register_segment(&self, segment: u32, path: &Path) -> Result<()> {
        if !self.open() {
            return Err(MorunaError::Cancelled);
        }
        let want_direct = self.inner.paths.direct.on;
        let (fd, direct) = self.inner.fds.acquire(
            path,
            Mode::Write,
            want_direct,
            self.inner.paths.direct.guaranteed,
            &self.inner.counters,
        )?;
        let gds_handle = if self.inner.paths.gds.on {
            gds::register(&fd, path)?
        } else {
            None
        };
        self.inner.segments.register(
            segment,
            SegmentEntry {
                path: path.to_path_buf(),
                fd,
                direct,
                gds_handle,
            },
        )?;
        self.inner.counters.segment_registered();
        tracing::debug!(target: "reactor.segment", segment, path = %path.display(), direct, "registered");
        Ok(())
    }

    fn unregister_segment(&self, segment: u32) {
        let Some(path) = self.inner.segments.unregister(segment) else {
            return;
        };
        // The registry's reference is gone; an operation still holding the descriptor keeps
        // the file open until it resolves, and no later operation can find it (RE-I8).
        self.inner.fds.forget(&path);
        self.inner.counters.segment_unregistered();
        tracing::debug!(target: "reactor.segment", segment, path = %path.display(), "unregistered");
    }

    fn paths(&self) -> IoPaths {
        self.inner.io_paths.clone()
    }

    fn shutdown(&self) {
        if self.shutting_down.swap(true, Ordering::SeqCst) {
            return;
        }
        // Order matters: mark cancelled first, so an operation the drain task has already
        // taken resolves `Cancelled` rather than running, then drop the queues, which ends the
        // drain tasks and drops whatever is still queued (each dropped sender resolves its
        // completion `Cancelled`, contracts d.9).
        self.inner.cancelled.store(true, Ordering::SeqCst);
        let queues = self
            .queues
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        drop(queues);
        let rt = self.rt.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(rt) = rt {
            rt.shutdown_timeout(SHUTDOWN_GRACE);
        }
        self.inner.fds.clear();
        self.inner.segments.clear();
    }
}

impl Drop for Reactor {
    fn drop(&mut self) {
        moruna_kernel::Reactor::shutdown(self);
    }
}
