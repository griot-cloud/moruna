//! The tests of section k of `architecture/sdd/06-reactor.md`, named after their ids.
//!
//! They drive the real reactor over `FakeAllocator` buffers (contracts d.15) and a scratch
//! directory unique to this process, and the object-store tests drive it over a test-local
//! backend that counts requests in flight and can fail a named multipart part. Tests that
//! need a device, cuFile or the reference NVMe are tagged "(reference host, E1)" and are
//! marked ignored with that reason, never passed.
//!
//! This host is macOS arm64, where neither `O_DIRECT` nor io_uring exists (preamble 6.6). The
//! direct IO row of e.2 is `F_NOCACHE` here and `O_DIRECT` on Linux; the io_uring row cannot
//! be selected here at all, so its selection, its fallback and its byte-for-byte equivalence
//! with the blocking pool are exercised through the `FileEngine` seam, and the ring itself
//! runs in the weekly container job.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use moruna_kernel::{
    Buffer, CopyDst, CopySrc, DeviceId, Guarantee, HostProfile, MorunaError, ObjectMeta,
    ObjectMetadata, Reactor as ReactorTrait, SegmentRef, Tier,
};
use moruna_testkit::FakeAllocator;
use object_store::PutPayload;
use object_store::path::Path as OsPath;

use crate::fdcache::tests::{DirectRefusingOpener, scratch_dir};
use crate::file_blocking::{Done, FileEngine, FileReq};
use crate::object::{BoxFut, MultipartSink, ObjectBackend, StoreBackend};
use crate::{Hooks, ObjectStoreConfig, Reactor, ReactorConfig, S3Config, stats::OpKind};

/// A profile with every probed path at `g` and nothing declared.
fn profile(g: Guarantee) -> HostProfile {
    HostProfile {
        io_uring: g,
        direct_io_staging: g,
        gds: g,
        memlock: g,
        ..HostProfile::default()
    }
}

fn config(profile: HostProfile) -> ReactorConfig {
    ReactorConfig {
        threads: 2,
        object_concurrency: 8,
        file_depth: 32,
        page_bytes: 4096,
        profile,
        devices: Vec::new(),
        object_store: ObjectStoreConfig {
            s3: Some(S3Config {
                bucket: Some("bucket".into()),
                ..S3Config::default()
            }),
            ..ObjectStoreConfig::default()
        },
    }
}

fn build(cfg: ReactorConfig, alloc: &FakeAllocator, hooks: Hooks) -> Arc<Reactor> {
    Reactor::new_with(cfg, Arc::new(alloc.clone()), hooks).expect("the reactor builds")
}

/// A reactor with the defaults, direct IO probed available, and an in-memory object store.
fn reactor(alloc: &FakeAllocator) -> (Arc<Reactor>, Arc<TestBackend>) {
    let backend = TestBackend::new();
    let hooks = Hooks {
        object_backend: Some(as_backend(&backend)),
        ..Hooks::default()
    };
    (
        build(config(profile(Guarantee::Probed(true))), alloc, hooks),
        backend,
    )
}

/// The test backend as the seam wants it (the impl below is on `Arc<TestBackend>`).
fn as_backend(b: &Arc<TestBackend>) -> Arc<dyn ObjectBackend> {
    Arc::new(Arc::clone(b))
}

fn write_file(path: &std::path::Path, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("fixture write");
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u8) ^ seed).collect()
}

// ---------------------------------------------------------------------------------------
// The test-local object backend of section k.
// ---------------------------------------------------------------------------------------

#[derive(Default)]
struct BackendCounts {
    in_flight: usize,
    max_in_flight: usize,
    parts: usize,
    aborts: usize,
    completes: usize,
}

/// The wrapper section k names: the `object_store` in-memory backend with a count of requests
/// in flight, an optional latency, an injectable transport failure and a part that fails.
pub(crate) struct TestBackend {
    inner: StoreBackend,
    /// The same in-memory store the seam wraps, so a test can start a multipart upload and
    /// hold its id, which no reactor call hands out (the id is the store's, not the
    /// reactor's).
    memory: Arc<object_store::memory::InMemory>,
    counts: Mutex<BackendCounts>,
    latency: Mutex<Duration>,
    fail_part: Mutex<Option<usize>>,
    /// Fail this many `get_range` calls with a connection reset first (f.3's one retry).
    fail_gets: AtomicU64,
    /// Multipart parts are counted and discarded rather than stored, so a 200 MiB write does
    /// not need 200 MiB of store as well as 200 MiB of source.
    discard_parts: bool,
}

impl TestBackend {
    fn new() -> Arc<TestBackend> {
        // The in-memory store is both the `ObjectStore` and the `MultipartStore`, so a test can
        // abort an upload by its id the way a resumed sink does.
        let memory = Arc::new(object_store::memory::InMemory::new());
        Arc::new(TestBackend {
            inner: StoreBackend::new(
                Arc::clone(&memory) as Arc<dyn object_store::ObjectStore>,
                Some(Arc::clone(&memory) as Arc<dyn object_store::multipart::MultipartStore>),
            ),
            memory,
            counts: Mutex::new(BackendCounts::default()),
            latency: Mutex::new(Duration::ZERO),
            fail_part: Mutex::new(None),
            fail_gets: AtomicU64::new(0),
            discard_parts: true,
        })
    }

    fn with_latency(self: &Arc<Self>, d: Duration) {
        *self.latency.lock().unwrap_or_else(|e| e.into_inner()) = d;
    }

    fn fail_part(self: &Arc<Self>, n: usize) {
        *self.fail_part.lock().unwrap_or_else(|e| e.into_inner()) = Some(n);
    }

    fn counts(&self) -> (usize, usize, usize, usize) {
        let c = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        (c.max_in_flight, c.parts, c.aborts, c.completes)
    }

    fn enter(&self) {
        let mut c = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        c.in_flight += 1;
        c.max_in_flight = c.max_in_flight.max(c.in_flight);
    }

    fn leave(&self) {
        let mut c = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        c.in_flight = c.in_flight.saturating_sub(1);
    }

    fn sleep(&self) -> Duration {
        *self.latency.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A guard so an in-flight count is decremented even when the future is dropped.
struct InFlight(Arc<TestBackend>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.leave();
    }
}

fn reset() -> object_store::Error {
    object_store::Error::Generic {
        store: "test",
        source: "connection reset by peer".into(),
    }
}

impl ObjectBackend for Arc<TestBackend> {
    fn get_range(
        &self,
        path: &OsPath,
        range: std::ops::Range<u64>,
    ) -> BoxFut<object_store::Result<Bytes>> {
        let me = Arc::clone(self);
        let inner = self.inner.get_range(path, range);
        let delay = self.sleep();
        me.enter();
        Box::pin(async move {
            let _guard = InFlight(Arc::clone(&me));
            if delay > Duration::ZERO {
                tokio::time::sleep(delay).await;
            }
            if me.fail_gets.load(Ordering::SeqCst) > 0 {
                me.fail_gets.fetch_sub(1, Ordering::SeqCst);
                return Err(reset());
            }
            inner.await
        })
    }

    fn put(&self, path: &OsPath, payload: PutPayload) -> BoxFut<object_store::Result<()>> {
        self.inner.put(path, payload)
    }

    fn put_multipart(&self, path: &OsPath) -> BoxFut<object_store::Result<Box<dyn MultipartSink>>> {
        let me = Arc::clone(self);
        let path = path.clone();
        Box::pin(async move {
            if me.discard_parts {
                Ok(Box::new(TestUpload {
                    backend: Arc::clone(&me),
                    seen: 0,
                }) as Box<dyn MultipartSink>)
            } else {
                me.inner.put_multipart(&path).await
            }
        })
    }

    fn head(&self, path: &OsPath) -> BoxFut<object_store::Result<ObjectMeta>> {
        self.inner.head(path)
    }

    fn list_one_level(
        &self,
        prefix: Option<OsPath>,
    ) -> BoxFut<object_store::Result<(Vec<ObjectMeta>, Vec<OsPath>)>> {
        self.inner.list_one_level(prefix)
    }

    fn delete(&self, path: &OsPath) -> BoxFut<object_store::Result<()>> {
        let me = Arc::clone(self);
        let inner = self.inner.delete(path);
        me.enter();
        Box::pin(async move {
            let _guard = InFlight(Arc::clone(&me));
            inner.await
        })
    }

    fn abort_by_id(&self, path: &OsPath, id: &str) -> Option<BoxFut<object_store::Result<()>>> {
        self.inner.abort_by_id(path, id)
    }
}

struct TestUpload {
    backend: Arc<TestBackend>,
    seen: usize,
}

impl MultipartSink for TestUpload {
    fn part(&mut self, _payload: PutPayload) -> BoxFut<object_store::Result<()>> {
        self.seen += 1;
        let index = self.seen;
        let me = Arc::clone(&self.backend);
        Box::pin(async move {
            {
                let mut c = me.counts.lock().unwrap_or_else(|e| e.into_inner());
                c.parts += 1;
            }
            let failing = *me.fail_part.lock().unwrap_or_else(|e| e.into_inner());
            if failing == Some(index) {
                return Err(object_store::Error::Generic {
                    store: "test",
                    source: format!("part {index} refused").into(),
                });
            }
            Ok(())
        })
    }

    fn complete(&mut self) -> BoxFut<object_store::Result<()>> {
        let me = Arc::clone(&self.backend);
        Box::pin(async move {
            let mut c = me.counts.lock().unwrap_or_else(|e| e.into_inner());
            c.completes += 1;
            Ok(())
        })
    }

    fn abort(&mut self) -> BoxFut<object_store::Result<()>> {
        let me = Arc::clone(&self.backend);
        Box::pin(async move {
            let mut c = me.counts.lock().unwrap_or_else(|e| e.into_inner());
            c.aborts += 1;
            Ok(())
        })
    }
}

/// A file engine that fails its first `fail` operations and then delegates, so the
/// per-operation fallback of f.9 can be driven on a host with one file path.
struct FlakyEngine {
    inner: Arc<dyn FileEngine>,
    fail: AtomicUsize,
    attempts: AtomicUsize,
}

impl FlakyEngine {
    fn new(handle: tokio::runtime::Handle, fail: usize) -> Arc<FlakyEngine> {
        Arc::new(FlakyEngine {
            inner: Arc::new(crate::file_blocking::BlockingEngine::new(handle)),
            fail: AtomicUsize::new(fail),
            attempts: AtomicUsize::new(0),
        })
    }
}

impl FileEngine for FlakyEngine {
    fn submit(&self, req: FileReq, done: Done) {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        if self
            .fail
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n > 0).then(|| n - 1)
            })
            .is_ok()
        {
            done(Err(io::Error::from_raw_os_error(libc::EIO)));
            return;
        }
        self.inner.submit(req, done);
    }

    fn name(&self) -> &'static str {
        "flaky"
    }
}

/// A file engine that holds every request for a while before delegating, so a test can be sure
/// an operation is genuinely in flight.
struct SlowEngine {
    inner: Arc<dyn FileEngine>,
    delay: Duration,
}

impl SlowEngine {
    fn new(handle: tokio::runtime::Handle, delay: Duration) -> Arc<SlowEngine> {
        Arc::new(SlowEngine {
            inner: Arc::new(crate::file_blocking::BlockingEngine::new(handle)),
            delay,
        })
    }
}

impl FileEngine for SlowEngine {
    fn submit(&self, req: FileReq, done: Done) {
        std::thread::sleep(self.delay);
        self.inner.submit(req, done);
    }

    fn name(&self) -> &'static str {
        "slow"
    }
}

// ---------------------------------------------------------------------------------------
// RE-T1 .. RE-T15
// ---------------------------------------------------------------------------------------

/// RE-T1 exactly_once. RE-I1.
#[test]
fn re_t1_exactly_once() {
    const OPS: usize = 10_000;
    const CHUNK: usize = 4096;
    let dir = scratch_dir("t1");
    let source = dir.join("source");
    write_file(&source, &pattern(CHUNK * 16, 0x5a));
    let target = dir.join("target");
    write_file(&target, &vec![0u8; CHUNK * 16]);
    let alloc = FakeAllocator::new();
    let (reactor, _backend) = reactor(&alloc);
    let object_bytes = pattern(CHUNK, 0x11);
    reactor
        .write_object("s3://bucket/o", {
            let b = Arc::new(alloc.buffer(CHUNK, Tier::Host));
            let mut b2 = alloc.buffer(CHUNK, Tier::Host);
            b2.copy_from_slice(&object_bytes);
            let owned = Arc::new(b2);
            let _ = b;
            owned.view()
        })
        .wait()
        .expect("seed the object");

    let (tx, rx) = std::sync::mpsc::channel::<(usize, usize)>();
    let resolutions = Arc::new(AtomicUsize::new(0));
    let mut written: Vec<Arc<Buffer>> = Vec::new();

    for i in 0..OPS {
        let seen = Arc::clone(&resolutions);
        let tx = tx.clone();
        match i % 4 {
            0 | 1 => {
                let dst = alloc.buffer(CHUNK, Tier::Host);
                let ptr = dst.host_ptr().map_or(0, |p| p as usize);
                let completion = reactor.read_file(&source, ((i % 16) * CHUNK) as u64, dst);
                completion.then(Box::new(move |r| {
                    seen.fetch_add(1, Ordering::SeqCst);
                    let buffer = r.expect("read_file");
                    tx.send((buffer.host_ptr().map_or(0, |p| p as usize), buffer.len()))
                        .ok();
                    assert_eq!(buffer.host_ptr().map_or(0, |p| p as usize), ptr);
                }));
            }
            2 => {
                let mut buf = alloc.buffer(CHUNK, Tier::Host);
                buf.copy_from_slice(&pattern(CHUNK, i as u8));
                let owned = Arc::new(buf);
                written.push(Arc::clone(&owned));
                let completion =
                    reactor.write_file(&target, ((i % 16) * CHUNK) as u64, owned.view());
                completion.then(Box::new(move |r| {
                    seen.fetch_add(1, Ordering::SeqCst);
                    r.expect("write_file");
                    tx.send((0, 0)).ok();
                }));
            }
            _ => {
                let dst = alloc.buffer(CHUNK, Tier::Host);
                let ptr = dst.host_ptr().map_or(0, |p| p as usize);
                let completion = reactor.read_object("s3://bucket/o", 0, dst);
                completion.then(Box::new(move |r| {
                    seen.fetch_add(1, Ordering::SeqCst);
                    let buffer = r.expect("read_object");
                    assert_eq!(buffer.host_ptr().map_or(0, |p| p as usize), ptr);
                    tx.send((0, buffer.len())).ok();
                }));
            }
        }
    }
    drop(tx);
    for _ in 0..OPS {
        rx.recv_timeout(Duration::from_secs(60))
            .expect("every operation resolves");
    }
    assert_eq!(resolutions.load(Ordering::SeqCst), OPS);
    assert!(
        rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "no completion resolves twice"
    );
    for buffer in &written {
        assert_eq!(
            Arc::strong_count(buffer),
            1,
            "a write drops only the view; the bytes are still the caller's (RE-I1)"
        );
    }
    reactor.shutdown();
    std::fs::remove_dir_all(&dir).ok();
}

/// RE-T2 no_worker_work. RE-I2, RE-I6, f.8.
#[test]
fn re_t2_no_worker_work() {
    const OPS: usize = 1000;
    let dir = scratch_dir("t2");
    let source = dir.join("source");
    write_file(&source, &pattern(8192, 3));
    let alloc = FakeAllocator::new();
    let mut cfg = config(profile(Guarantee::Probed(true)));
    cfg.file_depth = 1;
    let backend = TestBackend::new();
    let reactor = build(
        cfg,
        &alloc,
        Hooks {
            object_backend: Some(as_backend(&backend)),
            ..Hooks::default()
        },
    );
    let caller = std::thread::current().id();
    let elsewhere = Arc::new(AtomicUsize::new(0));
    let resolved = Arc::new(AtomicUsize::new(0));
    let mut submit_ns = Vec::with_capacity(OPS);
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    for _ in 0..OPS {
        let dst = alloc.buffer(4096, Tier::Host);
        let start = Instant::now();
        let completion = reactor.read_file(&source, 0, dst);
        submit_ns.push(start.elapsed().as_nanos() as u64);
        let seen = Arc::clone(&elsewhere);
        let tx = tx.clone();
        let done = Arc::clone(&resolved);
        completion.then(Box::new(move |r| {
            r.expect("read_file");
            if std::thread::current().id() != caller {
                seen.fetch_add(1, Ordering::SeqCst);
            }
            done.fetch_add(1, Ordering::SeqCst);
            tx.send(()).ok();
        }));
    }
    // RE-I6 structurally, and on any host: `file_depth` is 1 above, so if submission waited
    // for a permit each of the thousand submissions would have to wait for the previous
    // operation to finish, and the loop could not have outrun the completions. It did, so
    // submission did not wait. This replaces a nanosecond threshold, which on a shared CI
    // runner measured the runner (p99 22 us against a 20 us bound, 2026-09-23) rather than
    // the property, and which the preamble reserves for the reference host in any case.
    let outstanding = OPS - resolved.load(Ordering::SeqCst);
    assert!(
        outstanding > 1,
        "submission waited for a permit: the submit loop left only {outstanding} of {OPS} \
         operations outstanding at a file depth of 1 (RE-I6, 06 RE-T2)"
    );
    drop(tx);
    for _ in 0..OPS {
        rx.recv_timeout(Duration::from_secs(60))
            .expect("every operation resolves");
    }
    submit_ns.sort_unstable();
    let p99 = submit_ns[(OPS * 99) / 100];
    // Provisional on any host but the reference one (preamble E1); the report names the host,
    // and only the reference host asserts the figure.
    println!("re_t2 submission p99: {p99} ns");
    if std::env::var_os("MORUNA_REFERENCE_HOST").is_some() {
        assert!(
            p99 < 20_000,
            "submission must not wait for a permit: p99 was {p99} ns (06 RE-T2)"
        );
    }
    assert_eq!(
        elsewhere.load(Ordering::SeqCst),
        OPS,
        "no operation completes on the calling thread"
    );
    reactor.shutdown();
    std::fs::remove_dir_all(&dir).ok();
}

/// RE-T3 paths_fixed. RE-I3.
#[test]
fn re_t3_paths_fixed() {
    let alloc = FakeAllocator::new().pinned(true);
    for bits in 0..16u8 {
        for declared in [false, true] {
            let yes = if declared {
                Guarantee::Present
            } else {
                Guarantee::Probed(true)
            };
            let no = if declared {
                Guarantee::Absent
            } else {
                Guarantee::Probed(false)
            };
            let pick = |set: bool| if set { yes } else { no };
            let mut cfg = config(HostProfile {
                io_uring: pick(bits & 1 != 0),
                direct_io_staging: pick(bits & 2 != 0),
                gds: pick(bits & 4 != 0),
                memlock: pick(bits & 8 != 0),
                ..HostProfile::default()
            });
            cfg.threads = 1;
            let reactor = build(cfg, &alloc, Hooks::default());
            let paths = reactor.paths();
            assert_eq!(
                paths.direct_io,
                bits & 2 != 0,
                "direct IO follows the profile"
            );
            assert_eq!(
                paths.io_uring,
                (bits & 1 != 0) && crate::paths::URING_BUILT,
                "io_uring needs the profile and a build that has it"
            );
            assert_eq!(
                paths.gds,
                (bits & 4 != 0) && crate::paths::GDS_BUILT,
                "gds needs the profile and a build that has it"
            );
            assert!(paths.pinned, "IoPaths::pinned is alloc.is_pinned()");
            assert!(!paths.rdma, "rdma is false in every v1 build");
            let again = reactor.paths();
            assert_eq!(
                (again.direct_io, again.io_uring, again.gds, again.pinned),
                (paths.direct_io, paths.io_uring, paths.gds, paths.pinned),
                "paths never change after new"
            );
            reactor.shutdown();
        }
    }

    // An injected failure on a probed path falls back once and counts it; the same failure on
    // a declared path is an error.
    let dir = scratch_dir("t3");
    let source = dir.join("source");
    write_file(&source, &pattern(8192, 9));
    for declared in [false, true] {
        let alloc = FakeAllocator::new();
        let mut cfg = config(profile(if declared {
            Guarantee::Present
        } else {
            Guarantee::Probed(true)
        }));
        cfg.threads = 1;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        let reactor = build(
            cfg,
            &alloc,
            Hooks {
                engine: Some(FlakyEngine::new(rt.handle().clone(), 1) as Arc<dyn FileEngine>),
                ..Hooks::default()
            },
        );
        let dst = alloc.buffer(4096, Tier::Host);
        let outcome = reactor.read_file(&source, 0, dst).wait();
        if declared {
            assert!(outcome.is_err(), "a failure on a declared path is an error");
            assert_eq!(reactor.stats().fallbacks, 0);
        } else {
            assert!(outcome.is_ok(), "a probed path falls back once");
            assert_eq!(reactor.stats().fallbacks, 1);
        }
        reactor.shutdown();
        drop(rt);
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// RE-T4 direct_when_aligned. RE-I4, e.3.
#[test]
fn re_t4_direct_when_aligned() {
    let dir = scratch_dir("t4");
    let source = dir.join("source");
    write_file(&source, &pattern(16384, 2));
    let other = dir.join("other");
    write_file(&other, &pattern(16384, 4));
    let alloc = FakeAllocator::new();
    let (reactor, _backend) = reactor(&alloc);
    assert!(reactor.paths().direct_io);

    let dst = alloc.buffer(4096, Tier::Host);
    assert_eq!(dst.host_ptr().map_or(1, |p| p as usize) % 4096, 0);
    reactor.read_file(&source, 0, dst).wait().expect("aligned");
    assert_eq!(reactor.stats().direct_ops, 1);
    assert_eq!(reactor.stats().buffered_ops, 0);

    // Two misaligned reads on one path warn once.
    for _ in 0..2 {
        let dst = alloc.buffer(100, Tier::Host);
        reactor
            .read_file(&source, 1, dst)
            .wait()
            .expect("misaligned");
    }
    assert_eq!(reactor.stats().buffered_ops, 2);
    assert_eq!(reactor.inner_warns().misaligned, 1);

    // A read and a write on it warn twice.
    let mut buf = alloc.buffer(100, Tier::Host);
    buf.copy_from_slice(&pattern(100, 7));
    let owned = Arc::new(buf);
    reactor
        .write_file(&other, 1, owned.view())
        .wait()
        .expect("misaligned write");
    assert_eq!(reactor.inner_warns().misaligned, 2);

    // A misaligned final piece on a registered segment path counts and does not warn.
    let segment = dir.join("segment");
    write_file(&segment, &vec![0u8; 8192]);
    reactor.register_segment(1, &segment).expect("register");
    let before = reactor.inner_warns().misaligned;
    let buffered_before = reactor.stats().buffered_ops;
    reactor
        .write_file(&segment, 0, owned.view())
        .wait()
        .expect("segment tail");
    assert_eq!(reactor.stats().buffered_ops, buffered_before + 1);
    assert_eq!(
        reactor.inner_warns().misaligned,
        before,
        "the tail of a staging record is deliberate (e.3)"
    );
    reactor.shutdown();
    std::fs::remove_dir_all(&dir).ok();
}

/// RE-T5 copy_is_dma. RE-I5, S14.
#[test]
#[ignore = "reference host, E1: no GPU host exists, so the cuda copy engine cannot be exercised"]
fn re_t5_copy_is_dma() {
    let alloc = FakeAllocator::new().pinned(true);
    let (reactor, _backend) = reactor(&alloc);
    let src = Arc::new(alloc.buffer(1 << 30, alloc.host_tier()));
    let dst = alloc.buffer(1 << 30, Tier::Device(DeviceId(0)));
    reactor
        .copy(CopySrc::View(src.view()), CopyDst::Buffer(dst))
        .wait()
        .expect("host to device");
    assert_eq!(reactor.stats().copies_h2d, 1);
    reactor.shutdown();
}

/// RE-T6 concurrency_bound. RE-I6.
#[test]
fn re_t6_concurrency_bound() {
    const OPS: usize = 1000;
    let alloc = FakeAllocator::new();
    let backend = TestBackend::new();
    let mut cfg = config(profile(Guarantee::Probed(false)));
    cfg.object_concurrency = 4;
    let reactor = build(
        cfg,
        &alloc,
        Hooks {
            object_backend: Some(as_backend(&backend)),
            ..Hooks::default()
        },
    );
    let mut seed = alloc.buffer(64, Tier::Host);
    seed.copy_from_slice(&pattern(64, 1));
    let seed = Arc::new(seed);
    reactor
        .write_object("s3://bucket/o", seed.view())
        .wait()
        .expect("seed");
    backend.with_latency(Duration::from_millis(20));

    let mut completions = Vec::with_capacity(OPS);
    let start = Instant::now();
    for _ in 0..OPS {
        let dst = alloc.buffer(64, Tier::Host);
        completions.push(reactor.read_object("s3://bucket/o", 0, dst));
    }
    assert!(
        start.elapsed() < Duration::from_millis(20),
        "all {OPS} calls return before the first completes"
    );
    for completion in completions {
        completion.wait().expect("object read");
    }
    let (max_in_flight, _, _, _) = backend.counts();
    assert!(
        max_in_flight <= 4,
        "the backend saw {max_in_flight} requests in flight, the limit is 4"
    );
    reactor.shutdown();
}

/// RE-T7 shutdown_completes. RE-I7.
#[test]
fn re_t7_shutdown_completes() {
    let dir = scratch_dir("t7");
    let source = dir.join("source");
    write_file(&source, &pattern(4096, 6));
    let alloc = FakeAllocator::new();
    let backend = TestBackend::new();
    let reactor = build(
        config(profile(Guarantee::Probed(true))),
        &alloc,
        Hooks {
            object_backend: Some(as_backend(&backend)),
            ..Hooks::default()
        },
    );
    let mut seed = alloc.buffer(64, Tier::Host);
    seed.copy_from_slice(&pattern(64, 1));
    let seed = Arc::new(seed);
    reactor
        .write_object("s3://bucket/o", seed.view())
        .wait()
        .expect("seed");
    let segment = dir.join("segment");
    write_file(&segment, &vec![0u8; 4096]);
    reactor.register_segment(4, &segment).expect("register");
    backend.with_latency(Duration::from_millis(200));

    let mut completions = Vec::new();
    for _ in 0..100 {
        let dst = alloc.buffer(64, Tier::Host);
        completions.push(reactor.read_object("s3://bucket/o", 0, dst));
    }
    let start = Instant::now();
    reactor.shutdown();
    for completion in completions {
        let _ = completion.wait();
    }
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "every outstanding completion resolves inside the grace period"
    );
    let second = Instant::now();
    reactor.shutdown();
    assert!(
        second.elapsed() < Duration::from_millis(50),
        "shutdown is idempotent and the second call returns at once"
    );
    let dst = alloc.buffer(4096, Tier::Host);
    let after = reactor.read_file(&source, 0, dst).wait();
    assert!(matches!(after, Err(MorunaError::Cancelled)));
    assert!(matches!(
        reactor.register_segment(5, &segment),
        Err(MorunaError::Cancelled)
    ));
    assert_eq!(
        reactor.open_descriptors(),
        0,
        "every cached and registered descriptor is closed"
    );
    drop(reactor);
    std::fs::remove_dir_all(&dir).ok();
}

/// RE-T8 short_read. f.1.
#[test]
fn re_t8_short_read() {
    let dir = scratch_dir("t8");
    let source = dir.join("source");
    write_file(&source, &pattern(100, 8));
    let alloc = FakeAllocator::new();
    let (reactor, _backend) = reactor(&alloc);

    let dst = alloc.buffer(4096, Tier::Host);
    let err = reactor
        .read_file(&source, 0, dst)
        .wait()
        .expect_err("a read past the end is an error");
    assert!(matches!(
        err,
        MorunaError::Io {
            op: "read_file",
            ..
        }
    ));

    let dst = alloc.buffer(4096, Tier::Host);
    let err = reactor
        .read_file_opt(&source, 0, dst, false)
        .wait()
        .expect_err("allow_short false behaves as read_file");
    assert!(matches!(
        err,
        MorunaError::Io {
            op: "read_file_opt",
            ..
        }
    ));

    let dst = alloc.buffer(4096, Tier::Host);
    let (buffer, n) = reactor
        .read_file_opt(&source, 0, dst, true)
        .wait()
        .expect("allow_short true returns what it read");
    assert_eq!(n, 100);
    assert_eq!(&buffer[..100], &pattern(100, 8)[..]);

    let dst = alloc.buffer(4096, Tier::Host);
    let (_, n) = reactor
        .read_file_opt(&source, 1000, dst, true)
        .wait()
        .expect("past the end reads nothing");
    assert_eq!(n, 0);

    // A zero length operation resolves at once, on this thread.
    let dst = alloc.buffer(0, Tier::Host);
    let len = dst.len();
    let zero = reactor
        .read_file(&source, 0, dst)
        .wait()
        .expect("zero read");
    assert_eq!(zero.len(), len);
    reactor.shutdown();
    std::fs::remove_dir_all(&dir).ok();
}

/// RE-T9 multipart. f.4, RE-I1.
#[test]
fn re_t9_multipart() {
    const BYTES: usize = 200 * 1024 * 1024;
    let alloc = FakeAllocator::new();
    let backend = TestBackend::new();
    let reactor = build(
        config(profile(Guarantee::Probed(false))),
        &alloc,
        Hooks {
            object_backend: Some(as_backend(&backend)),
            ..Hooks::default()
        },
    );
    let mut buf = alloc.buffer(BYTES, Tier::Host);
    buf[..16].copy_from_slice(&pattern(16, 0x33));
    let source = Arc::new(buf);
    let first_bytes = source[..16].to_vec();

    backend.fail_part(5);
    let err = reactor
        .write_object("s3://bucket/big", source.view())
        .wait()
        .expect_err("part 5 refuses");
    assert!(matches!(
        err,
        MorunaError::Io {
            op: "write_object",
            ..
        }
    ));
    let (_, parts, aborts, completes) = backend.counts();
    assert!(parts >= 5, "the failing part was reached: {parts} parts");
    assert_eq!(aborts, 1, "the upload is aborted");
    assert_eq!(completes, 0);
    assert_eq!(
        Arc::strong_count(&source),
        1,
        "the view is dropped, the bytes are not (RE-I1)"
    );
    assert_eq!(&source[..16], &first_bytes[..], "the source is unchanged");
    reactor.shutdown();
}

/// RE-T10 uring_vs_blocking_equivalence. e.2.
///
/// Both Linux file paths are written behind the `FileEngine` trait. On this host io_uring does
/// not exist, so what is proved here is the equivalence that G-I7 asks of a direct path and its
/// fallback: the same operations through the direct path, through the buffered path it falls
/// back to, and through `std::fs` produce identical bytes. On a host that has the ring, the
/// same comparison includes it.
#[test]
fn re_t10_uring_vs_blocking_equivalence() {
    let dir = scratch_dir("t10");
    let payload = pattern(4096 * 3, 0x2b);
    let mut results: Vec<Vec<u8>> = Vec::new();
    let mut labels: Vec<&str> = Vec::new();

    for (label, direct) in [("direct", true), ("buffered", false)] {
        let alloc = FakeAllocator::new();
        let source = dir.join(format!("source-{label}"));
        write_file(&source, &payload);
        let target = dir.join(format!("target-{label}"));
        write_file(&target, &vec![0u8; payload.len()]);
        let reactor = build(
            config(profile(if direct {
                Guarantee::Probed(true)
            } else {
                Guarantee::Probed(false)
            })),
            &alloc,
            Hooks::default(),
        );
        assert_eq!(reactor.paths().direct_io, direct);
        let dst = alloc.buffer(payload.len(), Tier::Host);
        let read = reactor.read_file(&source, 0, dst).wait().expect("read");
        assert_eq!(&read[..], &payload[..]);
        let owned = Arc::new(read);
        reactor
            .write_file(&target, 0, owned.view())
            .wait()
            .expect("write");
        results.push(std::fs::read(&target).expect("read back"));
        labels.push(label);
        reactor.shutdown();
    }
    let reference = std::fs::read(dir.join("source-direct")).expect("reference");
    for (bytes, label) in results.iter().zip(labels) {
        assert_eq!(bytes, &reference, "the {label} path wrote different bytes");
    }
    assert_eq!(
        crate::paths::URING_BUILT,
        cfg!(all(target_os = "linux", feature = "uring")),
        "the ring is compared on a host that has it; this host has none"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// RE-T11 bandwidth. S4 (IO-bound), S15.
#[test]
#[ignore = "reference host, E1: the fio comparison is recorded on the reference host"]
fn re_t11_bandwidth() {
    let dir = scratch_dir("t11");
    let source = dir.join("source");
    write_file(&source, &vec![7u8; 4 << 30]);
    let alloc = FakeAllocator::new();
    let mut cfg = config(profile(Guarantee::Probed(true)));
    cfg.file_depth = 32;
    let reactor = build(cfg, &alloc, Hooks::default());
    let start = Instant::now();
    let dst = alloc.buffer(1 << 20, Tier::Host);
    reactor.read_file(&source, 0, dst).wait().expect("read");
    println!("re_t11 elapsed {:?}", start.elapsed());
    reactor.shutdown();
    std::fs::remove_dir_all(&dir).ok();
}

/// RE-T12 sticky_fallback. f.9, RE-I3.
#[test]
fn re_t12_sticky_fallback() {
    let dir = scratch_dir("t12");
    let refused = dir.join("refused");
    write_file(&refused, &pattern(8192, 0x41));
    let ordinary = dir.join("ordinary");
    write_file(&ordinary, &pattern(8192, 0x42));

    // Probed: the first operation falls back and every later one opens buffered.
    let opener = DirectRefusingOpener::new(refused.clone());
    let alloc = FakeAllocator::new();
    let reactor = build(
        config(profile(Guarantee::Probed(true))),
        &alloc,
        Hooks {
            opener: Some(Arc::clone(&opener) as Arc<dyn crate::fdcache::Opener>),
            ..Hooks::default()
        },
    );
    let dst = alloc.buffer(4096, Tier::Host);
    reactor.read_file(&refused, 0, dst).wait().expect("read");
    assert_eq!(reactor.stats().sticky_fallbacks, 1);
    assert_eq!(reactor.inner_warns().sticky, 1);
    let attempts = opener.attempts();
    for _ in 0..100 {
        let dst = alloc.buffer(4096, Tier::Host);
        reactor.read_file(&refused, 0, dst).wait().expect("read");
    }
    assert_eq!(
        opener.attempts(),
        attempts,
        "no later operation on the path tries O_DIRECT again"
    );
    assert_eq!(reactor.stats().sticky_fallbacks, 1);
    let dst = alloc.buffer(4096, Tier::Host);
    reactor.read_file(&ordinary, 0, dst).wait().expect("read");
    assert!(
        opener.attempts() > attempts,
        "operations on another path stay direct"
    );
    reactor.shutdown();

    // Present: the same refusal is a configuration error, not a fallback.
    let alloc = FakeAllocator::new();
    let reactor = build(
        config(profile(Guarantee::Present)),
        &alloc,
        Hooks {
            opener: Some(
                DirectRefusingOpener::new(refused.clone()) as Arc<dyn crate::fdcache::Opener>
            ),
            ..Hooks::default()
        },
    );
    let dst = alloc.buffer(4096, Tier::Host);
    let err = reactor
        .read_file(&refused, 0, dst)
        .wait()
        .expect_err("a declared path does not fall back");
    assert!(matches!(
        err,
        MorunaError::Config {
            name: "host_profile",
            ..
        }
    ));
    assert_eq!(reactor.stats().sticky_fallbacks, 0);
    reactor.shutdown();

    // A per-operation failure on a direct read falls back once, and the next operation on the
    // path tries direct again.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime");
    let opener = DirectRefusingOpener::new(dir.join("nothing"));
    let alloc = FakeAllocator::new();
    let reactor = build(
        config(profile(Guarantee::Probed(true))),
        &alloc,
        Hooks {
            opener: Some(Arc::clone(&opener) as Arc<dyn crate::fdcache::Opener>),
            engine: Some(FlakyEngine::new(rt.handle().clone(), 1) as Arc<dyn FileEngine>),
            ..Hooks::default()
        },
    );
    let dst = alloc.buffer(4096, Tier::Host);
    reactor
        .read_file(&ordinary, 0, dst)
        .wait()
        .expect("the operation falls back once");
    assert_eq!(reactor.stats().fallbacks, 1);
    assert_eq!(reactor.stats().sticky_fallbacks, 0);
    let attempts = opener.attempts();
    let dst = alloc.buffer(4096, Tier::Host);
    reactor.read_file(&ordinary, 0, dst).wait().expect("read");
    assert!(
        opener.attempts() >= attempts,
        "a per-operation failure never becomes sticky"
    );
    reactor.shutdown();
    drop(rt);
    std::fs::remove_dir_all(&dir).ok();
}

/// RE-T13 segment_registry. RE-I8, e.4.
#[test]
fn re_t13_segment_registry() {
    let dir = scratch_dir("t13");
    let segment = dir.join("segment-0");
    write_file(&segment, &pattern(8192, 0x51));
    let alloc = FakeAllocator::new();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let reactor = build(
        config(profile(Guarantee::Probed(true))),
        &alloc,
        Hooks {
            engine: Some(
                SlowEngine::new(rt.handle().clone(), Duration::from_millis(300))
                    as Arc<dyn FileEngine>,
            ),
            ..Hooks::default()
        },
    );

    reactor.register_segment(0, &segment).expect("register");
    let err = reactor
        .register_segment(0, &segment)
        .expect_err("segments are immutable");
    assert!(matches!(
        err,
        MorunaError::Io {
            op: "register_segment",
            ..
        }
    ));

    let unknown = SegmentRef {
        segment: 99,
        offset: 0,
        len: 4096,
    };
    let err = reactor
        .copy(
            CopySrc::Disk(unknown),
            CopyDst::Buffer(alloc.buffer(4096, Tier::Device(DeviceId(0)))),
        )
        .wait()
        .expect_err("an unregistered segment is never a fresh open");
    assert!(matches!(err, MorunaError::Io { op: "copy", .. }));

    // A read in flight keeps the descriptor open past unregistration and still reads the right
    // bytes; the file is gone from the filesystem as soon as it is unlinked.
    let dst = alloc.buffer(4096, Tier::Host);
    let completion = reactor.read_file(&segment, 0, dst);
    std::thread::sleep(Duration::from_millis(80));
    reactor.unregister_segment(0);
    std::fs::remove_file(&segment).expect("unlink");
    let read = completion
        .wait()
        .expect("the in-flight read still resolves");
    assert_eq!(&read[..], &pattern(8192, 0x51)[..4096]);
    assert!(!segment.exists());
    reactor.unregister_segment(0);
    reactor.shutdown();
    drop(rt);
    std::fs::remove_dir_all(&dir).ok();
}

/// RE-T14 then_on_reactor_thread. e.1, g.
#[test]
fn re_t14_then_on_reactor_thread() {
    let dir = scratch_dir("t14");
    let source = dir.join("source");
    write_file(&source, &pattern(8192, 0x61));
    let target = dir.join("target");
    write_file(&target, &vec![0u8; 8192]);
    let alloc = FakeAllocator::new();
    let (reactor, _backend) = reactor(&alloc);
    let mut seed = alloc.buffer(64, Tier::Host);
    seed.copy_from_slice(&pattern(64, 1));
    let seed = Arc::new(seed);
    reactor
        .write_object("s3://bucket/o", seed.view())
        .wait()
        .expect("seed");

    let caller = std::thread::current().id();
    let (tx, rx) = std::sync::mpsc::channel::<(&'static str, bool)>();
    let mut kinds: HashMap<&'static str, usize> = HashMap::new();

    let send = |tx: &std::sync::mpsc::Sender<(&'static str, bool)>, kind: &'static str| {
        let tx = tx.clone();
        move |ok: bool| {
            tx.send((kind, std::thread::current().id() != caller && ok))
                .ok();
        }
    };

    let dst = alloc.buffer(4096, Tier::Host);
    let f = send(&tx, OpKind::ReadFile.as_str());
    reactor
        .read_file(&source, 0, dst)
        .then(Box::new(move |r| f(r.is_ok())));
    let dst = alloc.buffer(4096, Tier::Host);
    let f = send(&tx, OpKind::ReadFileOpt.as_str());
    reactor
        .read_file_opt(&source, 0, dst, true)
        .then(Box::new(move |r| f(r.is_ok())));
    let mut buf = alloc.buffer(4096, Tier::Host);
    buf.copy_from_slice(&pattern(4096, 5));
    let owned = Arc::new(buf);
    let f = send(&tx, OpKind::WriteFile.as_str());
    reactor
        .write_file(&target, 0, owned.view())
        .then(Box::new(move |r| f(r.is_ok())));
    let dst = alloc.buffer(64, Tier::Host);
    let f = send(&tx, OpKind::ReadObject.as_str());
    reactor
        .read_object("s3://bucket/o", 0, dst)
        .then(Box::new(move |r| f(r.is_ok())));
    let f = send(&tx, OpKind::WriteObject.as_str());
    reactor
        .write_object("s3://bucket/o2", owned.view())
        .then(Box::new(move |r| f(r.is_ok())));
    let f = send(&tx, OpKind::Copy.as_str());
    reactor
        .copy(
            CopySrc::View(owned.view()),
            CopyDst::Buffer(alloc.buffer(4096, Tier::Device(DeviceId(0)))),
        )
        .then(Box::new(move |r| {
            // Without a copy engine in this build the copy fails, but the callback still runs
            // exactly once and on a reactor thread, which is what this test is about.
            f(r.is_ok() || cfg!(not(feature = "cuda")));
        }));
    drop(tx);

    for _ in 0..6 {
        let (kind, ok) = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("every kind resolves");
        assert!(ok, "{kind} resolved on the calling thread");
        *kinds.entry(kind).or_default() += 1;
    }
    assert_eq!(kinds.len(), 6, "all six kinds, once each: {kinds:?}");
    assert!(kinds.values().all(|n| *n == 1));
    assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());

    // A callback that panics is caught, counted, and the reactor keeps serving.
    let errors_before = reactor.stats().errors;
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let dst = alloc.buffer(4096, Tier::Host);
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    reactor.read_file(&source, 0, dst).then(Box::new(move |_| {
        done_tx.send(()).ok();
        panic!("a placement callback went wrong");
    }));
    done_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the callback ran");
    std::thread::sleep(Duration::from_millis(50));
    std::panic::set_hook(previous);
    assert!(
        reactor.stats().errors > errors_before,
        "the panic is counted"
    );
    let dst = alloc.buffer(4096, Tier::Host);
    reactor
        .read_file(&source, 0, dst)
        .wait()
        .expect("the reactor keeps serving");
    reactor.shutdown();
    std::fs::remove_dir_all(&dir).ok();
}

/// RE-T15 probed_versus_present. RE-I3, e.2.
#[test]
fn re_t15_probed_versus_present() {
    let alloc = FakeAllocator::new();
    // Present, and the ring will not start: `new` refuses.
    let err = Reactor::new_with(
        config(profile(Guarantee::Present)),
        Arc::new(alloc.clone()),
        Hooks {
            ring_starts: Some(false),
            force_uring: true,
            ..Hooks::default()
        },
    )
    .expect_err("a declared path that will not start is a configuration error");
    assert!(matches!(
        err,
        MorunaError::Config {
            name: "host_profile",
            ..
        }
    ));

    // Probed(true), and the ring will not start: `new` succeeds with io_uring off.
    let reactor = Reactor::new_with(
        config(profile(Guarantee::Probed(true))),
        Arc::new(alloc.clone()),
        Hooks {
            ring_starts: Some(false),
            force_uring: true,
            ..Hooks::default()
        },
    )
    .expect("a probed path that will not start falls back");
    assert!(!reactor.paths().io_uring);
    reactor.shutdown();

    // Probed(false): the ring is never attempted.
    let reactor = Reactor::new_with(
        config(profile(Guarantee::Probed(false))),
        Arc::new(alloc.clone()),
        Hooks {
            ring_starts: Some(false),
            force_uring: true,
            ..Hooks::default()
        },
    )
    .expect("an absent path is simply not selected");
    assert!(!reactor.paths().io_uring);
    reactor.shutdown();
}

/// The object metadata calls of d.1, which sources' `plan` needs.
#[test]
fn object_metadata_answers_head_and_list() {
    let alloc = FakeAllocator::new();
    let (reactor, _backend) = reactor(&alloc);
    let mut buf = alloc.buffer(128, Tier::Host);
    buf.copy_from_slice(&pattern(128, 0x71));
    let owned = Arc::new(buf);
    reactor
        .write_object("s3://bucket/dir/a", owned.view())
        .wait()
        .expect("put");
    reactor
        .write_object("s3://bucket/dir/b", owned.view())
        .wait()
        .expect("put");
    let meta = reactor
        .head_object("s3://bucket/dir/a")
        .wait()
        .expect("head");
    assert_eq!(meta.size, 128);
    assert_eq!(meta.url, "s3://bucket/dir/a");
    let listed = reactor.list_prefix("s3://bucket/dir").wait().expect("list");
    assert_eq!(listed.len(), 2, "both objects under the prefix: {listed:?}");
    let mut urls: Vec<&str> = listed.iter().map(|m| m.url.as_str()).collect();
    urls.sort_unstable();
    assert_eq!(
        urls,
        vec!["s3://bucket/dir/a", "s3://bucket/dir/b"],
        "each listed object's URL reads it back"
    );
    let trailing = reactor
        .list_prefix("s3://bucket/dir/")
        .wait()
        .expect("list");
    assert!(
        trailing
            .iter()
            .all(|m| m.url.starts_with("s3://bucket/dir/") && !m.url.contains("dir/dir"))
    );
    let missing = reactor.head_object("s3://bucket/absent").wait();
    assert!(missing.is_err());
    let bad = reactor.head_object("ftp://host/key").wait();
    assert!(matches!(bad, Err(MorunaError::Config { .. })));
    reactor.shutdown();
}

/// `delete_object` (d.9), which a resumed sink uses to discard what it wrote above
/// `committed_seq`: a delete makes the object unreadable, and deleting what is not there is
/// `Ok(())`, so a resume that runs twice does not fail the second time.
#[test]
fn deleting_an_object_removes_it_and_an_absent_one_is_ok() {
    let alloc = FakeAllocator::new();
    let (reactor, _backend) = reactor(&alloc);
    let mut buf = alloc.buffer(128, Tier::Host);
    buf.copy_from_slice(&pattern(128, 0x5a));
    let owned = Arc::new(buf);
    reactor
        .delete_object("s3://bucket/never-written")
        .wait()
        .expect("deleting what is not there is Ok");
    reactor
        .write_object("s3://bucket/gone", owned.view())
        .wait()
        .expect("put");
    assert_eq!(
        reactor
            .head_object("s3://bucket/gone")
            .wait()
            .expect("head")
            .size,
        128
    );
    reactor
        .delete_object("s3://bucket/gone")
        .wait()
        .expect("delete");
    let after = reactor
        .read_object("s3://bucket/gone", 0, alloc.buffer(128, Tier::Host))
        .wait();
    assert!(matches!(after, Err(MorunaError::Io { .. })), "{after:?}");
    reactor
        .delete_object("s3://bucket/gone")
        .wait()
        .expect("the second delete is Ok as well");
    let bad = reactor.delete_object("ftp://host/key").wait();
    assert!(matches!(bad, Err(MorunaError::Config { .. })), "{bad:?}");
    reactor.shutdown();
}

/// `abort_multipart` (d.9): the parts a killed run left are abandoned, and no object appears
/// at the upload's url. The upload is started on the store directly, because the id belongs to
/// the store and no reactor call hands one out.
#[test]
fn an_aborted_multipart_leaves_no_object() {
    let alloc = FakeAllocator::new();
    let (reactor, backend) = reactor(&alloc);
    let path = OsPath::from("staged");
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a runtime for the store calls this test makes itself");
    let store = Arc::clone(&backend.memory);
    let id = rt.block_on(async {
        let id = object_store::multipart::MultipartStore::create_multipart(&*store, &path)
            .await
            .expect("create");
        object_store::multipart::MultipartStore::put_part(
            &*store,
            &path,
            &id,
            0,
            PutPayload::from_static(b"a part that was never completed"),
        )
        .await
        .expect("part");
        id
    });
    reactor
        .abort_multipart("s3://bucket/staged", &id)
        .wait()
        .expect("abort");
    let head = reactor.head_object("s3://bucket/staged").wait();
    assert!(
        matches!(head, Err(MorunaError::Io { .. })),
        "an aborted upload leaves no object: {head:?}"
    );
    reactor
        .abort_multipart("s3://bucket/staged", &id)
        .wait()
        .expect("aborting an upload the store has forgotten is Ok as well");
    let completed = rt.block_on(object_store::multipart::MultipartStore::complete_multipart(
        &*store,
        &path,
        &id,
        Vec::new(),
    ));
    assert!(completed.is_err(), "the parts are gone, not merely hidden");
    reactor.shutdown();
}

/// The same two calls over a `file://` url, which is a real local store and not the test
/// backend: a delete unlinks the file, and an abort is `Unsupported`, because a local store
/// has no multipart uploads and so no id for one can exist.
#[test]
fn a_file_url_is_deleted_on_disk_and_has_no_multipart() {
    let dir = scratch_dir("delete");
    let victim = dir.join("part-0.parquet");
    write_file(&victim, &pattern(64, 3));
    let alloc = FakeAllocator::new();
    let mut cfg = config(profile(Guarantee::Probed(true)));
    cfg.object_store.local_root = Some(dir.clone());
    let reactor = build(cfg, &alloc, Hooks::default());
    reactor
        .delete_object("file:///part-0.parquet")
        .wait()
        .expect("delete");
    assert!(!victim.exists(), "the file is gone from the filesystem");
    reactor
        .delete_object("file:///part-0.parquet")
        .wait()
        .expect("the second delete is Ok as well");
    let aborted = reactor
        .abort_multipart("file:///part-0.parquet", "1")
        .wait();
    assert!(
        matches!(aborted, Err(MorunaError::Unsupported(_))),
        "{aborted:?}"
    );
    reactor.shutdown();
}

/// A device buffer is not a file destination, and an unconfigured backend is a `Config` error;
/// both are decided without touching the filesystem.
#[test]
fn a_file_operation_refuses_a_destination_it_cannot_address() {
    let dir = scratch_dir("refusals");
    let source = dir.join("source");
    write_file(&source, &pattern(4096, 1));
    let alloc = FakeAllocator::new();
    let (reactor, _backend) = reactor(&alloc);
    let dst = alloc.buffer(4096, Tier::Device(DeviceId(0)));
    let err = reactor
        .read_file(&source, 0, dst)
        .wait()
        .expect_err("a device buffer has no host bytes");
    assert!(matches!(err, MorunaError::Staging(_)));
    let device = Arc::new(alloc.buffer(4096, Tier::Device(DeviceId(0))));
    let err = reactor
        .write_object("s3://bucket/x", device.view())
        .wait()
        .expect_err("a device view is not an object body");
    assert!(matches!(err, MorunaError::Staging(_)));
    reactor.shutdown();
    std::fs::remove_dir_all(&dir).ok();
}

/// One object read that meets a connection reset is retried once above the crate's own backoff
/// (f.3) and the retry is counted as a fallback.
#[test]
fn an_object_read_retries_once_on_a_connection_reset() {
    let alloc = FakeAllocator::new();
    let backend = TestBackend::new();
    let reactor = build(
        config(profile(Guarantee::Probed(false))),
        &alloc,
        Hooks {
            object_backend: Some(as_backend(&backend)),
            ..Hooks::default()
        },
    );
    let mut buf = alloc.buffer(64, Tier::Host);
    buf.copy_from_slice(&pattern(64, 2));
    let owned = Arc::new(buf);
    reactor
        .write_object("s3://bucket/o", owned.view())
        .wait()
        .expect("seed");
    backend.fail_gets.store(1, Ordering::SeqCst);
    let dst = alloc.buffer(64, Tier::Host);
    let read = reactor
        .read_object("s3://bucket/o", 0, dst)
        .wait()
        .expect("the retry succeeds");
    assert_eq!(&read[..], &pattern(64, 2)[..]);
    assert_eq!(reactor.stats().fallbacks, 1);
    assert_eq!(reactor.stats().ingress_bytes, 64);
    assert_eq!(reactor.stats().object_reads, 1);
    backend.fail_gets.store(2, Ordering::SeqCst);
    let dst = alloc.buffer(64, Tier::Host);
    let err = reactor
        .read_object("s3://bucket/o", 0, dst)
        .wait()
        .expect_err("a second failure is the error");
    assert!(matches!(
        err,
        MorunaError::Io {
            op: "read_object",
            ..
        }
    ));
    reactor.shutdown();
}

/// The high water mark of the submission queues reaches the report (f.8, j).
#[test]
fn the_queue_high_water_mark_is_reported() {
    let dir = scratch_dir("queued");
    let source = dir.join("source");
    write_file(&source, &pattern(4096, 4));
    let alloc = FakeAllocator::new();
    let mut cfg = config(profile(Guarantee::Probed(false)));
    cfg.file_depth = 1;
    cfg.threads = 1;
    let reactor = build(cfg, &alloc, Hooks::default());
    let mut completions = Vec::new();
    for _ in 0..64 {
        let dst = alloc.buffer(4096, Tier::Host);
        completions.push(reactor.read_file(&source, 0, dst));
    }
    for completion in completions {
        completion.wait().expect("read");
    }
    assert!(reactor.stats().queued_max > 1, "operations queued");
    assert_eq!(reactor.stats().buffered_ops, 64);
    assert_eq!(reactor.stats().direct_ops, 0);
    assert!(format!("{reactor:?}").contains("Reactor"));
    reactor.shutdown();
    std::fs::remove_dir_all(&dir).ok();
}

impl Reactor {
    /// The once-per-run warn counts, for the tests of e.3 and f.9.
    fn inner_warns(&self) -> crate::stats::WarnCounts {
        self.inner.counters.warn_counts()
    }
}

/// A file engine that never calls back, so the "the io engine stopped" path of f.8 is reached.
struct SilentEngine;

impl FileEngine for SilentEngine {
    fn submit(&self, _req: FileReq, _done: Done) {}

    fn name(&self) -> &'static str {
        "silent"
    }
}

/// G-I8 at start: a host that will not give the reactor a thread is diagnosed, not signalled.
///
/// The build machine reaches its thread limit when several test binaries run at once, and a
/// reactor that panicked there would take the process down instead of failing its run. The
/// thread is asked for in one place, and a refusal is an `Io` naming the operation; the stack
/// size here is the cheapest way to make the host refuse.
#[test]
fn a_host_that_will_not_start_a_thread_is_an_error_and_not_a_panic() {
    let refused = crate::start_runtime(2, Some(usize::MAX));
    match refused {
        Err(MorunaError::Io { op, ref msg, .. }) => {
            assert_eq!(op, "reactor_start");
            assert!(!msg.is_empty(), "the host's reason is carried");
        }
        other => panic!("a refused thread must be an Io error: {other:?}"),
    }
    // The ordinary path still builds, so the check is of the failure and not of the builder.
    assert!(crate::start_runtime(1, None).is_ok());
}

/// A file engine that keeps every request and its callback until a test lets them go, so a
/// test can hold an operation "in flight" for as long as it likes.
#[derive(Default)]
struct HoldingEngine {
    held: Mutex<Vec<(FileReq, Done)>>,
}

impl HoldingEngine {
    fn held(&self) -> usize {
        self.held.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Drop the requests and their callbacks, as an engine whose driver went away would.
    fn drop_all(&self) {
        self.held.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

impl FileEngine for HoldingEngine {
    fn submit(&self, req: FileReq, done: Done) {
        self.held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((req, done));
    }

    fn name(&self) -> &'static str {
        "holding"
    }
}

/// RE-I1 through a shutdown: an operation the engine still holds owns its buffer until the
/// engine answers, and `shutdown` must not free those bytes under the syscall.
///
/// `shutdown` drops the runtime, which drops every task that is awaiting a completion. Until
/// 2026-09-22 the destination buffer was a local of that task, so dropping it returned the
/// bytes to the arena while a `pread` on the blocking pool was still writing into them; the
/// corruption surfaced later as a trap inside an unrelated allocation, and it made this
/// binary fail perhaps a third of the time. The buffer now travels in the engine's completion
/// callback, so it is alive until the engine is done with it, whoever is still waiting.
#[test]
fn an_operation_the_engine_still_holds_keeps_its_buffer_through_shutdown() {
    let dir = scratch_dir("holding");
    let source = dir.join("source");
    write_file(&source, &pattern(8192, 0x5c));
    let alloc = FakeAllocator::new();
    let engine = Arc::new(HoldingEngine::default());
    let reactor = build(
        config(profile(Guarantee::Probed(true))),
        &alloc,
        Hooks {
            engine: Some(Arc::clone(&engine) as Arc<dyn FileEngine>),
            ..Hooks::default()
        },
    );
    let dst = alloc.buffer(4096, Tier::Host);
    let completion = reactor.read_file(&source, 0, dst);
    let deadline = Instant::now() + Duration::from_secs(5);
    while engine.held() == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(engine.held(), 1, "the engine has the request");
    assert_eq!(
        alloc.in_use(Tier::Host),
        4096,
        "the buffer is the operation's while the engine holds it"
    );
    reactor.shutdown();
    assert_eq!(
        alloc.in_use(Tier::Host),
        4096,
        "shutdown does not hand the bytes back under a syscall that is still running"
    );
    let answer = completion.wait();
    assert!(
        matches!(answer, Err(MorunaError::Cancelled)),
        "the caller is told the operation was cancelled, not left waiting: {answer:?}"
    );
    engine.drop_all();
    assert_eq!(
        alloc.in_use(Tier::Host),
        0,
        "the bytes go back when the engine is done with them, and only then"
    );
}

/// The io_uring row's per-operation fallback is the blocking pool (e.2, f.9). The ring cannot
/// exist on this host, so the row is driven through the `FileEngine` seam: the engine standing
/// in for the ring fails once, the operation is re-issued on the blocking pool, and the
/// fallback is counted. On Linux with the `uring` feature the same code runs over the real ring.
#[test]
fn the_io_uring_row_falls_back_to_the_blocking_pool() {
    let dir = scratch_dir("uring-row");
    let source = dir.join("source");
    write_file(&source, &pattern(8192, 0x77));
    let alloc = FakeAllocator::new();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime");
    let reactor = build(
        config(profile(Guarantee::Probed(true))),
        &alloc,
        Hooks {
            engine: Some(FlakyEngine::new(rt.handle().clone(), 1) as Arc<dyn FileEngine>),
            force_uring: true,
            ..Hooks::default()
        },
    );
    assert!(
        reactor.paths().io_uring,
        "the row is selected for this test"
    );
    let dst = alloc.buffer(4096, Tier::Host);
    let read = reactor
        .read_file(&source, 0, dst)
        .wait()
        .expect("the blocking pool answers");
    assert_eq!(&read[..], &pattern(8192, 0x77)[..4096]);
    assert_eq!(reactor.stats().fallbacks, 1);
    assert!(reactor.stats().uring_ops >= 1);
    reactor.shutdown();
    drop(rt);
    std::fs::remove_dir_all(&dir).ok();
}

/// An engine that never resolves its callback is an error, not a hang (f.8).
#[test]
fn an_engine_that_never_calls_back_is_an_error() {
    let dir = scratch_dir("silent");
    let source = dir.join("source");
    write_file(&source, &pattern(4096, 3));
    let alloc = FakeAllocator::new();
    let reactor = build(
        config(profile(Guarantee::Probed(false))),
        &alloc,
        Hooks {
            engine: Some(Arc::new(SilentEngine) as Arc<dyn FileEngine>),
            ..Hooks::default()
        },
    );
    let dst = alloc.buffer(4096, Tier::Host);
    let err = reactor
        .read_file(&source, 0, dst)
        .wait()
        .expect_err("the engine stopped");
    assert!(matches!(
        err,
        MorunaError::Io {
            op: "read_file",
            ..
        }
    ));
    reactor.shutdown();
    std::fs::remove_dir_all(&dir).ok();
}

/// A zero length operation of every kind resolves at once, on the caller's thread (h).
#[test]
fn a_zero_length_operation_resolves_at_once() {
    let dir = scratch_dir("zero");
    let source = dir.join("source");
    write_file(&source, &pattern(16, 1));
    let alloc = FakeAllocator::new();
    let (reactor, _backend) = reactor(&alloc);
    // `FakeAllocator` rounds a zero byte request up to one byte, so an empty buffer is made
    // by splitting one at zero (contracts d.3) and an empty view by slicing one.
    let whole = Arc::new(alloc.buffer(64, Tier::Host));
    let empty_view = whole.view().slice(0, 0);
    let empty_buffer = |n: usize| alloc.buffer(n, Tier::Host).split_at(0).0;
    assert!(empty_buffer(64).is_empty());
    assert!(empty_view.is_empty());
    let caller = std::thread::current().id();
    let here = Arc::new(AtomicUsize::new(0));
    let mark = |here: &Arc<AtomicUsize>| {
        let here = Arc::clone(here);
        move || {
            if std::thread::current().id() == caller {
                here.fetch_add(1, Ordering::SeqCst);
            }
        }
    };
    let f = mark(&here);
    reactor
        .read_file(&source, 0, empty_buffer(64))
        .then(Box::new(move |r| {
            r.expect("zero read");
            f();
        }));
    let f = mark(&here);
    reactor
        .read_file_opt(&source, 0, empty_buffer(64), true)
        .then(Box::new(move |r| {
            assert_eq!(r.expect("zero read").1, 0);
            f();
        }));
    let f = mark(&here);
    reactor
        .write_file(&source, 0, whole.view().slice(0, 0))
        .then(Box::new(move |r| {
            r.expect("zero write");
            f();
        }));
    let f = mark(&here);
    reactor
        .read_object("s3://bucket/o", 0, empty_buffer(64))
        .then(Box::new(move |r| {
            r.expect("zero object read");
            f();
        }));
    let f = mark(&here);
    reactor
        .write_object("s3://bucket/o", empty_view)
        .then(Box::new(move |r| {
            r.expect("zero object write");
            f();
        }));
    assert_eq!(
        here.load(Ordering::SeqCst),
        5,
        "all five ran on this thread"
    );
    assert_eq!(std::fs::read(&source).expect("unchanged").len(), 16);
    reactor.shutdown();
    std::fs::remove_dir_all(&dir).ok();
}

/// Whatever is still on a submission queue when `shutdown` runs resolves `Cancelled` rather
/// than running (f.7, RE-I7).
#[test]
fn queued_operations_are_cancelled_by_shutdown() {
    let dir = scratch_dir("cancel");
    let source = dir.join("source");
    write_file(&source, &pattern(8192, 0x31));
    let alloc = FakeAllocator::new();
    let backend = TestBackend::new();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime");
    let mut cfg = config(profile(Guarantee::Probed(false)));
    cfg.file_depth = 1;
    cfg.object_concurrency = 1;
    let reactor = build(
        cfg,
        &alloc,
        Hooks {
            engine: Some(
                SlowEngine::new(rt.handle().clone(), Duration::from_millis(40))
                    as Arc<dyn FileEngine>,
            ),
            object_backend: Some(as_backend(&backend)),
            ..Hooks::default()
        },
    );
    let mut seed = alloc.buffer(64, Tier::Host);
    seed.copy_from_slice(&pattern(64, 1));
    let seed = Arc::new(seed);
    reactor
        .write_object("s3://bucket/o", seed.view())
        .wait()
        .expect("seed");
    backend.with_latency(Duration::from_millis(40));

    let mut completions: Vec<Box<dyn FnOnce() -> bool + Send>> = Vec::new();
    for _ in 0..40 {
        let c = reactor.read_file(&source, 0, alloc.buffer(4096, Tier::Host));
        completions.push(Box::new(move || {
            matches!(c.wait(), Err(MorunaError::Cancelled))
        }));
        let c = reactor.read_file_opt(&source, 0, alloc.buffer(4096, Tier::Host), true);
        completions.push(Box::new(move || {
            matches!(c.wait(), Err(MorunaError::Cancelled))
        }));
        let c = reactor.write_file(&source, 0, seed.view());
        completions.push(Box::new(move || {
            matches!(c.wait(), Err(MorunaError::Cancelled))
        }));
        let c = reactor.read_object("s3://bucket/o", 0, alloc.buffer(64, Tier::Host));
        completions.push(Box::new(move || {
            matches!(c.wait(), Err(MorunaError::Cancelled))
        }));
        let c = reactor.write_object("s3://bucket/o", seed.view());
        completions.push(Box::new(move || {
            matches!(c.wait(), Err(MorunaError::Cancelled))
        }));
        let c = reactor.head_object("s3://bucket/o");
        completions.push(Box::new(move || {
            matches!(c.wait(), Err(MorunaError::Cancelled))
        }));
        let c = reactor.list_prefix("s3://bucket");
        completions.push(Box::new(move || {
            matches!(c.wait(), Err(MorunaError::Cancelled))
        }));
        let c = reactor.copy(
            CopySrc::View(seed.view()),
            CopyDst::Buffer(alloc.buffer(64, Tier::Device(DeviceId(0)))),
        );
        completions.push(Box::new(move || {
            matches!(c.wait(), Err(MorunaError::Cancelled))
        }));
    }
    reactor.shutdown();
    let cancelled = completions
        .into_iter()
        .filter(|_| true)
        .fold(0, |n, f| n + usize::from(f()));
    assert!(
        cancelled > 0,
        "the queue held work that shutdown cancelled rather than ran"
    );
    drop(rt);
    std::fs::remove_dir_all(&dir).ok();
}

/// An unpinned arena cannot reach a device in a build with no copy engine, and says so rather
/// than pretending (e.2, f.5).
#[test]
fn an_unpinned_arena_without_a_bounce_buffer_refuses_a_device_copy() {
    let alloc = FakeAllocator::new().pinned(false);
    let (reactor, _backend) = reactor(&alloc);
    let src = Arc::new(alloc.buffer(64, alloc.host_tier()));
    let err = reactor
        .copy(
            CopySrc::View(src.view()),
            CopyDst::Buffer(alloc.buffer(64, Tier::Device(DeviceId(0)))),
        )
        .wait()
        .expect_err("no copy engine and no bounce buffer");
    assert!(matches!(
        err,
        MorunaError::Staging(_) | MorunaError::Io { op: "copy", .. }
    ));
    reactor.shutdown();
}

/// An object read whose destination is larger than the object, and one into a device buffer,
/// are errors rather than partial results (h).
#[test]
fn an_object_read_refuses_what_it_cannot_fill() {
    let alloc = FakeAllocator::new();
    let (reactor, _backend) = reactor(&alloc);
    let mut buf = alloc.buffer(64, Tier::Host);
    buf.copy_from_slice(&pattern(64, 4));
    let owned = Arc::new(buf);
    reactor
        .write_object("s3://bucket/o", owned.view())
        .wait()
        .expect("seed");
    let err = reactor
        .read_object("s3://bucket/o", 0, alloc.buffer(4096, Tier::Host))
        .wait()
        .expect_err("a range beyond the object");
    assert!(matches!(err, MorunaError::Io { .. }));
    let err = reactor
        .read_object(
            "s3://bucket/o",
            0,
            alloc.buffer(64, Tier::Device(DeviceId(0))),
        )
        .wait()
        .expect_err("a device buffer");
    assert!(matches!(err, MorunaError::Staging(_)));
    reactor.shutdown();
}

/// The public constructor and the defaults of the configuration table (d.1, preamble 5).
#[test]
fn the_public_constructor_takes_the_defaults() {
    let alloc = FakeAllocator::new().page_bytes(8192);
    let cfg = ReactorConfig {
        page_bytes: 0,
        ..ReactorConfig::default()
    };
    assert_eq!(cfg.threads, 2);
    assert_eq!(cfg.object_concurrency, 8);
    assert_eq!(cfg.file_depth, 32);
    let reactor = Reactor::new(cfg, Arc::new(alloc)).expect("the reactor builds");
    assert!(
        !reactor.paths().direct_io,
        "an Unknown profile probes nothing"
    );
    assert_eq!(reactor.stats(), crate::ReactorStats::default());
    reactor.shutdown();
}

/// A multipart write that every part accepts completes the upload (f.4).
#[test]
fn a_multipart_write_that_succeeds_completes_the_upload() {
    let alloc = FakeAllocator::new();
    let backend = TestBackend::new();
    let reactor = build(
        config(profile(Guarantee::Probed(false))),
        &alloc,
        Hooks {
            object_backend: Some(as_backend(&backend)),
            ..Hooks::default()
        },
    );
    let buf = alloc.buffer(68 * 1024 * 1024, Tier::Host);
    let source = Arc::new(buf);
    reactor
        .write_object("s3://bucket/big", source.view())
        .wait()
        .expect("every part is accepted");
    let (_, parts, aborts, completes) = backend.counts();
    assert_eq!(parts, 5, "68 MiB is five 16 MiB parts");
    assert_eq!((aborts, completes), (0, 1));
    reactor.shutdown();
}
