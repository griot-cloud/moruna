//! The reactor's runtime: the tokio runtime and its threads, the submission queues and their
//! drain tasks (06 f.8), the concurrency limits (f.6), and the body of every operation.
//!
//! Every trait method does three things on the caller's thread and nothing else: build the
//! operation record with a `Completion::channel()`, push it onto the queue for its kind, and
//! return the `Completion` (RE-I6). Permit acquisition, path selection, the registry read and
//! every syscall happen here, on a reactor thread or the blocking pool, which is what lets a
//! worker submit from inside `Placement::push` while holding no lock the reactor contends on.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use amoru_kernel::{
    AmoruError, Buffer, BufferView, CompletionSender, CopyDst, CopySrc, IoPaths, ObjectMeta, Result,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

use crate::copy;
use crate::fdcache::{FdCache, Mode};
use crate::file_blocking::{FileEngine, FileReq, Verb};
use crate::object::ObjectLayer;
use crate::paths::{P_DIRECT, P_PINNED, P_URING, Paths};
use crate::segments::Segments;
use crate::stats::{Counters, OpKind};

/// Everything the operations share. The drain tasks hold an `Arc` of this; the tokio runtime
/// itself is held by the public `Reactor`, so dropping the reactor stops the tasks and the
/// tasks then drop this.
pub(crate) struct Inner {
    pub(crate) paths: Paths,
    pub(crate) io_paths: IoPaths,
    pub(crate) page_bytes: usize,
    pub(crate) counters: Counters,
    pub(crate) segments: Segments,
    pub(crate) fds: FdCache,
    pub(crate) objects: ObjectLayer,
    pub(crate) engine: Arc<dyn FileEngine>,
    /// The blocking pool, which is the fallback of the io_uring row of e.2 and is the engine
    /// itself when io_uring was not selected.
    pub(crate) fallback: Arc<dyn FileEngine>,
    pub(crate) cancelled: AtomicBool,
    pub(crate) handle: tokio::runtime::Handle,
    /// The size of the pinned bounce buffer of e.2, allocated once at `new` when the arena is
    /// not pinned and a device exists; zero when this build has no copy engine, in which case
    /// an unpinned arena cannot reach a device at all.
    pub(crate) bounce_bytes: usize,
}

impl Inner {
    pub(crate) fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Resolve a completion, catching a panic in a `then` callback so one bad callback cannot
    /// take a reactor thread down (g).
    pub(crate) fn resolve<T: Send + 'static>(&self, tx: CompletionSender<T>, value: Result<T>) {
        if value.is_err() {
            self.counters.note_error();
        }
        let guarded = std::panic::AssertUnwindSafe(move || tx.resolve(value));
        if std::panic::catch_unwind(guarded).is_err() {
            self.counters.note_error();
            tracing::error!(target: "reactor.op", "a completion callback panicked");
        }
    }
}

/// One file operation waiting for a reactor thread.
pub(crate) struct ReadFileOp {
    pub(crate) path: PathBuf,
    pub(crate) offset: u64,
    pub(crate) dst: Buffer,
    pub(crate) tx: CompletionSender<Buffer>,
}

/// `read_file_opt`, which differs only in what a short read means (d.1).
pub(crate) struct ReadFileOptOp {
    pub(crate) path: PathBuf,
    pub(crate) offset: u64,
    pub(crate) dst: Buffer,
    pub(crate) allow_short: bool,
    pub(crate) tx: CompletionSender<(Buffer, usize)>,
}

/// `write_file`.
pub(crate) struct WriteFileOp {
    pub(crate) path: PathBuf,
    pub(crate) offset: u64,
    pub(crate) src: BufferView,
    pub(crate) tx: CompletionSender<()>,
}

/// `read_object`.
pub(crate) struct ReadObjectOp {
    pub(crate) url: String,
    pub(crate) offset: u64,
    pub(crate) dst: Buffer,
    pub(crate) tx: CompletionSender<Buffer>,
}

/// `write_object`.
pub(crate) struct WriteObjectOp {
    pub(crate) url: String,
    pub(crate) src: BufferView,
    pub(crate) tx: CompletionSender<()>,
}

/// `copy`, already planned on the caller's thread (f.5).
pub(crate) struct CopyOp {
    pub(crate) plan: copy::Plan,
    pub(crate) src: CopySrc,
    pub(crate) dst: CopyDst,
    pub(crate) tx: CompletionSender<Option<Buffer>>,
}

/// The two `ObjectMetadata` calls, which enqueue like every other operation (d.1).
pub(crate) enum MetaOp {
    /// `head_object`.
    Head {
        url: String,
        tx: CompletionSender<ObjectMeta>,
    },
    /// `list_prefix`.
    List {
        url: String,
        tx: CompletionSender<Vec<ObjectMeta>>,
    },
}

/// The submission queues (f.8), one per operation kind. Held by the public `Reactor` and
/// dropped by `shutdown`, which is what ends the drain tasks.
pub(crate) struct Queues {
    pub(crate) read_file: UnboundedSender<ReadFileOp>,
    pub(crate) read_file_opt: UnboundedSender<ReadFileOptOp>,
    pub(crate) write_file: UnboundedSender<WriteFileOp>,
    pub(crate) read_object: UnboundedSender<ReadObjectOp>,
    pub(crate) write_object: UnboundedSender<WriteObjectOp>,
    pub(crate) copy: UnboundedSender<CopyOp>,
    pub(crate) meta: UnboundedSender<MetaOp>,
}

/// Start one drain task: it takes operations in order, waits for a permit on a reactor thread,
/// and spawns the operation itself (f.6, f.8).
fn drain<T, F, Fut>(inner: &Arc<Inner>, mut rx: UnboundedReceiver<T>, sem: Arc<Semaphore>, run: F)
where
    T: Send + 'static,
    F: Fn(Arc<Inner>, T, OwnedSemaphorePermit) -> Fut + Send + Copy + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let inner = Arc::clone(inner);
    inner.handle.clone().spawn(async move {
        while let Some(op) = rx.recv().await {
            inner.counters.dequeued();
            let Ok(permit) = Arc::clone(&sem).acquire_owned().await else {
                return;
            };
            tokio::spawn(run(Arc::clone(&inner), op, permit));
        }
    });
}

/// Build the queues and their drain tasks.
pub(crate) fn start(inner: &Arc<Inner>, object_concurrency: usize, file_depth: usize) -> Queues {
    let files = Arc::new(Semaphore::new(file_depth.max(1)));
    let objects = Arc::new(Semaphore::new(object_concurrency.max(1)));
    // Copies are limited by the CUDA stream queue and by the placement engine's reservations,
    // not by a semaphore of this component (f.6).
    let copies = Arc::new(Semaphore::new(Semaphore::MAX_PERMITS));

    let (read_file, rx) = unbounded_channel();
    drain(inner, rx, Arc::clone(&files), run_read_file);
    let (read_file_opt, rx) = unbounded_channel();
    drain(inner, rx, Arc::clone(&files), run_read_file_opt);
    let (write_file, rx) = unbounded_channel();
    drain(inner, rx, Arc::clone(&files), run_write_file);
    let (read_object, rx) = unbounded_channel();
    drain(inner, rx, Arc::clone(&objects), run_read_object);
    let (write_object, rx) = unbounded_channel();
    drain(inner, rx, Arc::clone(&objects), run_write_object);
    let (copy_tx, rx) = unbounded_channel();
    drain(inner, rx, copies, run_copy);
    let (meta, rx) = unbounded_channel();
    drain(inner, rx, objects, run_meta);

    Queues {
        read_file,
        read_file_opt,
        write_file,
        read_object,
        write_object,
        copy: copy_tx,
        meta,
    }
}

/// Is this operation one direct IO could take (e.3)?
fn eligible(inner: &Inner, ptr: usize, offset: u64, len: usize) -> bool {
    let page = inner.page_bytes.max(1);
    ptr.is_multiple_of(page) && (offset as usize).is_multiple_of(page) && len.is_multiple_of(page)
}

/// The descriptor for one operation, and whether it is a direct one (e.3, e.4, f.9).
///
/// A registered segment answers from the registry, so a segment has one descriptor for its
/// aligned operations; the piece of a staging record that is not a page multiple (09 f.5) is
/// the one expected buffered operation on such a path, and it is counted without a warn.
fn descriptor(
    inner: &Inner,
    kind: OpKind,
    path: &Path,
    mode: Mode,
    aligned: bool,
) -> Result<(Arc<std::os::fd::OwnedFd>, bool)> {
    if let Some(entry) = inner.segments.by_path(path) {
        if aligned || !entry.direct {
            return Ok((Arc::clone(&entry.fd), entry.direct));
        }
        return Ok((inner.fds.acquire_buffered(path, mode)?, false));
    }
    if inner.paths.direct.on && !aligned {
        inner.counters.warn_misaligned(kind, P_DIRECT);
    }
    inner.fds.acquire(
        path,
        mode,
        inner.paths.direct.on && aligned,
        inner.paths.direct.guaranteed,
        &inner.counters,
    )
}

/// Run one file request on an engine and wait for its callback.
async fn once(engine: &Arc<dyn FileEngine>, req: FileReq) -> std::io::Result<usize> {
    let (tx, rx) = oneshot::channel();
    engine.submit(
        req,
        Box::new(move |r| {
            let _ = tx.send(r.map_err(|e| e.to_string()));
        }),
    );
    match rx.await {
        Ok(Ok(n)) => Ok(n),
        Ok(Err(msg)) => Err(std::io::Error::other(msg)),
        Err(_) => Err(std::io::Error::other("the io engine stopped")),
    }
}

/// One whole file operation: pick the descriptor, count the path, run it, and take the io_uring
/// row's one per-operation fallback when it is available (f.9).
async fn file_op(
    inner: &Arc<Inner>,
    kind: OpKind,
    path: PathBuf,
    offset: u64,
    ptr: usize,
    len: usize,
    verb: Verb,
) -> Result<usize> {
    let aligned = inner.paths.direct.on && eligible(inner, ptr, offset, len);
    let held = {
        let inner = Arc::clone(inner);
        let owned = path.clone();
        let mode = match verb {
            Verb::Read => Mode::Read,
            Verb::Write => Mode::Write,
        };
        tokio::task::spawn_blocking(move || descriptor(&inner, kind, &owned, mode, aligned))
            .await
            .map_err(|e| join_error(kind, &path, &e))??
    };
    let (fd, direct) = held;
    inner.counters.note_direct(direct);
    let req = |fd: &Arc<std::os::fd::OwnedFd>| FileReq {
        fd: Arc::clone(fd),
        verb,
        ptr,
        len,
        offset,
    };
    if inner.paths.uring.on {
        inner.counters.note_uring();
    }
    let first = once(&inner.engine, req(&fd)).await;
    let Err(e) = first else {
        return first.map_err(|e| crate::fdcache::io_error(kind.as_str(), &path, &e));
    };
    // One per-operation fallback, and only on a path the host merely probed (f.9, RE-I3): the
    // blocking pool for the io_uring row, a buffered descriptor for the direct IO row.
    if inner.paths.uring.on && inner.paths.uring.may_fall_back() {
        inner.counters.note_fallback();
        inner
            .counters
            .warn_fallback(kind, P_URING, &format!("{}: {e}", inner.engine.name()));
        return once(&inner.fallback, req(&fd))
            .await
            .map_err(|e| crate::fdcache::io_error(kind.as_str(), &path, &e));
    }
    if direct && inner.paths.direct.may_fall_back() {
        inner.counters.note_fallback();
        inner.counters.warn_fallback(kind, P_DIRECT, &e.to_string());
        let buffered = {
            let inner = Arc::clone(inner);
            let owned = path.clone();
            let mode = match verb {
                Verb::Read => Mode::Read,
                Verb::Write => Mode::Write,
            };
            tokio::task::spawn_blocking(move || inner.fds.acquire_buffered(&owned, mode))
                .await
                .map_err(|e| join_error(kind, &path, &e))??
        };
        inner.counters.note_direct(false);
        return once(&inner.engine, req(&buffered))
            .await
            .map_err(|e| crate::fdcache::io_error(kind.as_str(), &path, &e));
    }
    Err(crate::fdcache::io_error(kind.as_str(), &path, &e))
}

fn join_error(kind: OpKind, path: &Path, e: &tokio::task::JoinError) -> AmoruError {
    AmoruError::Io {
        op: kind.as_str(),
        target: path.display().to_string(),
        msg: e.to_string(),
    }
}

fn short_read(kind: OpKind, path: &Path, got: usize, want: usize) -> AmoruError {
    AmoruError::Io {
        op: kind.as_str(),
        target: path.display().to_string(),
        msg: format!("short read: {got} of {want} bytes"),
    }
}

/// The host pointer an operation writes into or reads from; a device buffer has none, which is
/// a caller bug and never a runtime condition.
fn host_ptr_of(buffer: &Buffer) -> Result<usize> {
    buffer.host_ptr().map(|p| p as usize).ok_or_else(|| {
        AmoruError::Staging(format!(
            "a file operation needs a host buffer, not {:?}",
            buffer.tier()
        ))
    })
}

fn host_ptr_of_view(view: &BufferView) -> Result<usize> {
    view.host_ptr().map(|p| p as usize).ok_or_else(|| {
        AmoruError::Staging(format!(
            "a file operation needs a host view, not {:?}",
            view.tier()
        ))
    })
}

async fn run_read_file(inner: Arc<Inner>, op: ReadFileOp, _permit: OwnedSemaphorePermit) {
    let ReadFileOp {
        path,
        offset,
        dst,
        tx,
    } = op;
    if inner.cancelled() {
        inner.resolve(tx, Err(AmoruError::Cancelled));
        return;
    }
    let len = dst.len();
    let result = match host_ptr_of(&dst) {
        Ok(ptr) => {
            file_op(
                &inner,
                OpKind::ReadFile,
                path.clone(),
                offset,
                ptr,
                len,
                Verb::Read,
            )
            .await
        }
        Err(e) => Err(e),
    };
    match result {
        Ok(n) if n == len => inner.resolve(tx, Ok(dst)),
        Ok(n) => inner.resolve(tx, Err(short_read(OpKind::ReadFile, &path, n, len))),
        Err(e) => inner.resolve(tx, Err(e)),
    }
}

async fn run_read_file_opt(inner: Arc<Inner>, op: ReadFileOptOp, _permit: OwnedSemaphorePermit) {
    let ReadFileOptOp {
        path,
        offset,
        dst,
        allow_short,
        tx,
    } = op;
    if inner.cancelled() {
        inner.resolve(tx, Err(AmoruError::Cancelled));
        return;
    }
    let len = dst.len();
    let result = match host_ptr_of(&dst) {
        Ok(ptr) => {
            file_op(
                &inner,
                OpKind::ReadFileOpt,
                path.clone(),
                offset,
                ptr,
                len,
                Verb::Read,
            )
            .await
        }
        Err(e) => Err(e),
    };
    match result {
        Ok(n) if n == len || allow_short => inner.resolve(tx, Ok((dst, n))),
        Ok(n) => inner.resolve(tx, Err(short_read(OpKind::ReadFileOpt, &path, n, len))),
        Err(e) => inner.resolve(tx, Err(e)),
    }
}

async fn run_write_file(inner: Arc<Inner>, op: WriteFileOp, _permit: OwnedSemaphorePermit) {
    let WriteFileOp {
        path,
        offset,
        src,
        tx,
    } = op;
    if inner.cancelled() {
        inner.resolve(tx, Err(AmoruError::Cancelled));
        return;
    }
    let len = src.len();
    let result = match host_ptr_of_view(&src) {
        Ok(ptr) => {
            file_op(
                &inner,
                OpKind::WriteFile,
                path.clone(),
                offset,
                ptr,
                len,
                Verb::Write,
            )
            .await
        }
        Err(e) => Err(e),
    };
    // The view is dropped either way; the bytes it was over are still the caller's (RE-I1).
    drop(src);
    match result {
        Ok(_) => inner.resolve(tx, Ok(())),
        Err(e) => inner.resolve(tx, Err(e)),
    }
}

async fn run_read_object(inner: Arc<Inner>, op: ReadObjectOp, _permit: OwnedSemaphorePermit) {
    let ReadObjectOp {
        url,
        offset,
        dst,
        tx,
    } = op;
    if inner.cancelled() {
        inner.resolve(tx, Err(AmoruError::Cancelled));
        return;
    }
    let mut dst = dst;
    let result = read_object_body(&inner, &url, offset, &mut dst).await;
    match result {
        Ok(()) => inner.resolve(tx, Ok(dst)),
        Err(e) => inner.resolve(tx, Err(e)),
    }
}

async fn read_object_body(
    inner: &Arc<Inner>,
    url: &str,
    offset: u64,
    dst: &mut Buffer,
) -> Result<()> {
    let len = dst.len();
    if dst.host_ptr().is_none() {
        return Err(AmoruError::Staging(format!(
            "an object read needs a host buffer, not {:?}",
            dst.tier()
        )));
    }
    let (backend, path) = inner.objects.resolve(url)?;
    let range = offset..offset + len as u64;
    let mut body = backend.get_range(&path, range.clone()).await;
    if let Err(e) = &body {
        // f.3: one reactor-level retry on a connection reset, above the object_store crate's
        // own backoff.
        if crate::object::is_transport_error(e) {
            inner.counters.note_fallback();
            inner.counters.warn_fallback(
                OpKind::ReadObject,
                crate::paths::P_OBJECT,
                &e.to_string(),
            );
            body = backend.get_range(&path, range).await;
        }
    }
    let bytes = body.map_err(|e| crate::object::io("read_object", url, &e))?;
    if bytes.len() != len {
        return Err(AmoruError::Io {
            op: "read_object",
            target: url.to_string(),
            msg: format!("short read: {} of {len} bytes", bytes.len()),
        });
    }
    // The ingress copy (b): out of the network library's buffer into the arena. It is the one
    // copy on this path and it is counted apart from payload copies (G-I2).
    dst.as_mut().copy_from_slice(&bytes);
    inner.counters.note_object_read(len as u64);
    inner.counters.note_ingress(len as u64);
    Ok(())
}

async fn run_write_object(inner: Arc<Inner>, op: WriteObjectOp, _permit: OwnedSemaphorePermit) {
    let WriteObjectOp { url, src, tx } = op;
    if inner.cancelled() {
        inner.resolve(tx, Err(AmoruError::Cancelled));
        return;
    }
    let result = crate::object::write(&inner.objects, &url, &src).await;
    drop(src);
    inner.resolve(tx, result);
}

async fn run_copy(inner: Arc<Inner>, op: CopyOp, _permit: OwnedSemaphorePermit) {
    let CopyOp { plan, src, dst, tx } = op;
    if inner.cancelled() {
        inner.resolve(tx, Err(AmoruError::Cancelled));
        return;
    }
    let bytes = match &src {
        CopySrc::View(v) => v.len() as u64,
        CopySrc::Disk(seg) => seg.len,
    };
    let entry = match &src {
        CopySrc::Disk(seg) => match inner.segments.get(seg.segment) {
            Some(entry) => Some(entry),
            None => {
                inner.resolve(tx, Err(crate::segments::not_registered(seg.segment)));
                return;
            }
        },
        CopySrc::View(_) => None,
    };
    let handle = entry.as_ref().and_then(|e| e.gds_handle.as_ref());
    let bouncing = matches!(
        plan,
        copy::Plan::HostToDevice { bounce: true, .. }
            | copy::Plan::DeviceToHost { bounce: true, .. }
    );
    if bouncing && inner.bounce_bytes == 0 {
        inner.resolve(
            tx,
            Err(AmoruError::Staging(
                "an unpinned arena needs the pinned bounce buffer, which this build has not".into(),
            )),
        );
        return;
    }
    let result = copy::execute(plan, src, dst, handle);
    if result.is_ok() {
        inner.counters.note_copy(plan.direction(), bytes);
        if bouncing {
            // The bounce is a counted fallback, in pieces of the bounce buffer's size (e.2).
            inner.counters.note_fallback();
            inner.counters.note_bounce(bytes);
            inner
                .counters
                .warn_fallback(OpKind::Copy, P_PINNED, "the arena is not page locked");
        }
    }
    inner.resolve(tx, result);
}

async fn run_meta(inner: Arc<Inner>, op: MetaOp, _permit: OwnedSemaphorePermit) {
    match op {
        MetaOp::Head { url, tx } => {
            if inner.cancelled() {
                inner.resolve(tx, Err(AmoruError::Cancelled));
                return;
            }
            let result = match inner.objects.resolve(&url) {
                Ok((backend, path)) => backend
                    .head(&path)
                    .await
                    .map(|mut m| {
                        m.url = url.clone();
                        m
                    })
                    .map_err(|e| crate::object::io("head_object", &url, &e)),
                Err(e) => Err(e),
            };
            inner.resolve(tx, result);
        }
        MetaOp::List { url, tx } => {
            if inner.cancelled() {
                inner.resolve(tx, Err(AmoruError::Cancelled));
                return;
            }
            let result = list_prefix(&inner, &url).await;
            inner.resolve(tx, result);
        }
    }
}

/// Every object under a prefix, walked one level at a time because the crate's recursive
/// listing is a `futures` stream and the dependency table carries no `futures` (object.rs).
async fn list_prefix(inner: &Arc<Inner>, url: &str) -> Result<Vec<ObjectMeta>> {
    let (backend, path) = inner.objects.resolve(url)?;
    let prefix = url.trim_end_matches('/');
    let mut out = Vec::new();
    let mut todo = vec![Some(path)];
    while let Some(next) = todo.pop() {
        let (objects, prefixes) = backend
            .list_one_level(next)
            .await
            .map_err(|e| crate::object::io("list_prefix", url, &e))?;
        for mut m in objects {
            m.url = format!("{}/{}", prefix.trim_end_matches(&m.url), m.url);
            out.push(m);
        }
        todo.extend(prefixes.into_iter().map(Some));
    }
    Ok(out)
}
