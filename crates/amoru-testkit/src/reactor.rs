//! `FakeReactor`, the `Reactor` fake of contracts d.15.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use amoru_kernel::{
    AmoruError, Buffer, BufferView, Completion, CompletionSender, CopyDst, CopySrc, IoPaths,
    ObjectMeta, ObjectMetadata, Reactor, Result, SegmentRef, Tier,
};

/// Which operation an `OpRecord` records.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub enum OpKind {
    /// `read_file` or `read_file_opt`.
    ReadFile,
    /// `write_file`.
    WriteFile,
    /// `read_object`.
    ReadObject,
    /// `write_object`.
    WriteObject,
    /// `copy` between tiers.
    Copy,
    /// `head_object`.
    HeadObject,
    /// `list_prefix`.
    ListPrefix,
}

/// One operation the reactor was asked for, with the times it was submitted and resolved.
#[derive(Clone, Debug)]
pub struct OpRecord {
    /// Which operation.
    pub kind: OpKind,
    /// The path or URL it named; empty for `copy`.
    pub path_or_url: String,
    /// The byte offset it started at.
    pub offset: u64,
    /// The bytes it moved.
    pub len: u64,
    /// The tier the bytes came from, where there is one.
    pub src_tier: Option<Tier>,
    /// The tier the bytes went to, where there is one.
    pub dst_tier: Option<Tier>,
    /// Nanoseconds from the fake's creation to the submission.
    pub t_submit: u64,
    /// Nanoseconds from the fake's creation to the resolution; `None` while in flight.
    pub t_resolve: Option<u64>,
}

#[derive(Default)]
struct State {
    files: HashMap<String, Vec<u8>>,
    objects: HashMap<String, Vec<u8>>,
    segments: HashMap<u32, std::path::PathBuf>,
    ops: Vec<OpRecord>,
    failures: HashMap<OpKind, u64>,
    in_flight: u64,
}

struct Inner {
    state: Mutex<State>,
    started: Instant,
    latency: Duration,
    cancel_on_shutdown: bool,
    paths: IoPaths,
    shutdown_calls: AtomicU64,
}

/// The reactor as a test sees it: files and objects live in memory, keyed by path or URL, and
/// `read_file` and `write_file` copy to and from a `Vec<u8>` per path (d.15). Submission never
/// blocks: an operation with a latency resolves from a thread of its own (RE-I6).
///
/// Knobs: `with_latency(Duration)`, `fail_next(OpKind, n)`, `cancel_on_shutdown(bool)`,
/// `with_paths(IoPaths)`, and the in-memory files themselves (`with_file`).
/// Observables: `ops()`, `in_flight()`, `shutdown_calls()`, `paths()`.
#[derive(Clone)]
pub struct FakeReactor {
    inner: Arc<Inner>,
}

impl Default for FakeReactor {
    fn default() -> Self {
        FakeReactor::new()
    }
}

impl FakeReactor {
    /// A reactor with no latency, no failures and no direct paths.
    pub fn new() -> FakeReactor {
        FakeReactor {
            inner: Arc::new(Inner {
                state: Mutex::new(State::default()),
                started: Instant::now(),
                latency: Duration::ZERO,
                cancel_on_shutdown: true,
                paths: IoPaths::default(),
                shutdown_calls: AtomicU64::new(0),
            }),
        }
    }

    /// Knob: every operation resolves after this delay, from a thread of its own.
    pub fn with_latency(self, latency: Duration) -> FakeReactor {
        self.rebuild(|b| b.latency = latency)
    }

    /// Knob: fail the next `n` operations of `op` with `Io`.
    pub fn fail_next(self, op: OpKind, n: u64) -> FakeReactor {
        {
            let mut state = self.lock();
            *state.failures.entry(op).or_insert(0) += n;
        }
        self
    }

    /// Knob: whether `shutdown` resolves the operations still in flight with `Cancelled`
    /// (true, the default) or leaves them to resolve normally.
    pub fn cancel_on_shutdown(self, cancel: bool) -> FakeReactor {
        self.rebuild(|b| b.cancel_on_shutdown = cancel)
    }

    /// Knob: what `paths()` reports.
    pub fn with_paths(self, paths: IoPaths) -> FakeReactor {
        self.rebuild(|b| b.paths = paths)
    }

    /// Knob: seed an in-memory file or object with bytes.
    pub fn with_file(self, path: &str, bytes: Vec<u8>) -> FakeReactor {
        {
            let mut state = self.lock();
            state.files.insert(path.to_string(), bytes.clone());
            state.objects.insert(path.to_string(), bytes);
        }
        self
    }

    /// Observable: every operation, in submission order.
    pub fn ops(&self) -> Vec<OpRecord> {
        self.lock().ops.clone()
    }

    /// Observable: operations submitted and not yet resolved.
    pub fn in_flight(&self) -> u64 {
        self.lock().in_flight
    }

    /// Observable: how many times `shutdown` was called.
    pub fn shutdown_calls(&self) -> u64 {
        self.inner.shutdown_calls.load(Ordering::SeqCst)
    }

    /// Observable: the bytes an in-memory file holds.
    pub fn file(&self, path: &str) -> Option<Vec<u8>> {
        self.lock().files.get(path).cloned()
    }

    /// Observable: the segment numbers `register_segment` named and has not unregistered.
    pub fn segments(&self) -> Vec<u32> {
        let state = self.lock();
        let mut out: Vec<u32> = state.segments.keys().copied().collect();
        out.sort_unstable();
        out
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn rebuild(self, f: impl FnOnce(&mut Builder)) -> FakeReactor {
        let mut builder = Builder {
            latency: self.inner.latency,
            cancel_on_shutdown: self.inner.cancel_on_shutdown,
            paths: self.inner.paths.clone(),
        };
        f(&mut builder);
        let state = std::mem::take(&mut *self.lock());
        FakeReactor {
            inner: Arc::new(Inner {
                state: Mutex::new(state),
                started: self.inner.started,
                latency: builder.latency,
                cancel_on_shutdown: builder.cancel_on_shutdown,
                paths: builder.paths,
                shutdown_calls: AtomicU64::new(self.shutdown_calls()),
            }),
        }
    }

    fn now(&self) -> u64 {
        self.inner.started.elapsed().as_nanos() as u64
    }

    /// Record a submission and say whether this one is due to fail.
    fn submit(
        &self,
        kind: OpKind,
        path_or_url: &str,
        offset: u64,
        len: u64,
        src_tier: Option<Tier>,
        dst_tier: Option<Tier>,
    ) -> (usize, bool) {
        let t_submit = self.now();
        let mut state = self.lock();
        let failing = match state.failures.get_mut(&kind) {
            Some(left) if *left > 0 => {
                *left -= 1;
                true
            }
            _ => false,
        };
        state.in_flight += 1;
        state.ops.push(OpRecord {
            kind,
            path_or_url: path_or_url.to_string(),
            offset,
            len,
            src_tier,
            dst_tier,
            t_submit,
            t_resolve: None,
        });
        (state.ops.len() - 1, failing)
    }

    /// Resolve an operation, after the configured latency, without blocking the caller.
    fn resolve<T: Send + 'static>(
        &self,
        index: usize,
        sender: CompletionSender<T>,
        value: Result<T>,
    ) {
        let this = self.clone();
        let finish = move || {
            let mut state = this.lock();
            state.in_flight = state.in_flight.saturating_sub(1);
            let at = this.inner.started.elapsed().as_nanos() as u64;
            if let Some(record) = state.ops.get_mut(index) {
                record.t_resolve = Some(at);
            }
            drop(state);
            sender.resolve(value);
        };
        if self.inner.latency.is_zero() {
            finish();
        } else {
            let latency = self.inner.latency;
            std::thread::spawn(move || {
                std::thread::sleep(latency);
                finish();
            });
        }
    }

    fn io_error(op: &'static str, target: &str) -> AmoruError {
        AmoruError::Io {
            op,
            target: target.to_string(),
            msg: "FakeReactor::fail_next".to_string(),
        }
    }

    // `AmoruError` carries a morsel's features (CT-I10), so it is large beside this helper's
    // `usize`; the shape of the error type is the contract's (d.14), not this fake's.
    #[allow(clippy::result_large_err)]
    fn read_into(
        &self,
        key: &str,
        offset: u64,
        dst: &mut Buffer,
        from_objects: bool,
    ) -> Result<usize> {
        let state = self.lock();
        let store = if from_objects {
            &state.objects
        } else {
            &state.files
        };
        let Some(bytes) = store.get(key) else {
            return Err(AmoruError::Io {
                op: if from_objects {
                    "read_object"
                } else {
                    "read_file"
                },
                target: key.to_string(),
                msg: "no such file".to_string(),
            });
        };
        let start = offset as usize;
        let available = bytes.len().saturating_sub(start);
        let taken = available.min(dst.len());
        dst[..taken].copy_from_slice(&bytes[start..start + taken]);
        Ok(taken)
    }
}

struct Builder {
    latency: Duration,
    cancel_on_shutdown: bool,
    paths: IoPaths,
}

impl Reactor for FakeReactor {
    fn read_file(&self, path: &Path, offset: u64, dst: Buffer) -> Completion<Buffer> {
        let key = path.display().to_string();
        let (index, failing) = self.submit(
            OpKind::ReadFile,
            &key,
            offset,
            dst.len() as u64,
            None,
            Some(dst.tier()),
        );
        let (sender, completion) = Completion::channel();
        let mut dst = dst;
        let outcome = if failing {
            Err(FakeReactor::io_error("read_file", &key))
        } else {
            match self.read_into(&key, offset, &mut dst, false) {
                Ok(taken) if taken == dst.len() => Ok(dst),
                Ok(taken) => Err(AmoruError::Io {
                    op: "read_file",
                    target: key.clone(),
                    msg: format!("short read: {taken} of {} bytes", dst.len()),
                }),
                Err(e) => Err(e),
            }
        };
        self.resolve(index, sender, outcome);
        completion
    }

    fn read_file_opt(
        &self,
        path: &Path,
        offset: u64,
        dst: Buffer,
        allow_short: bool,
    ) -> Completion<(Buffer, usize)> {
        let key = path.display().to_string();
        let (index, failing) = self.submit(
            OpKind::ReadFile,
            &key,
            offset,
            dst.len() as u64,
            None,
            Some(dst.tier()),
        );
        let (sender, completion) = Completion::channel();
        let mut dst = dst;
        let outcome = if failing {
            Err(FakeReactor::io_error("read_file", &key))
        } else {
            match self.read_into(&key, offset, &mut dst, false) {
                Ok(taken) if taken == dst.len() || allow_short => Ok((dst, taken)),
                Ok(taken) => Err(AmoruError::Io {
                    op: "read_file",
                    target: key.clone(),
                    msg: format!("short read: {taken} of {} bytes", dst.len()),
                }),
                Err(e) => Err(e),
            }
        };
        self.resolve(index, sender, outcome);
        completion
    }

    fn write_file(&self, path: &Path, offset: u64, src: BufferView) -> Completion<()> {
        let key = path.display().to_string();
        let (index, failing) = self.submit(
            OpKind::WriteFile,
            &key,
            offset,
            src.len() as u64,
            Some(src.tier()),
            None,
        );
        let (sender, completion) = Completion::channel();
        let outcome = if failing {
            Err(FakeReactor::io_error("write_file", &key))
        } else {
            match src.as_host_slice() {
                Some(bytes) => {
                    let mut state = self.lock();
                    let file = state.files.entry(key.clone()).or_default();
                    let end = offset as usize + bytes.len();
                    if file.len() < end {
                        file.resize(end, 0);
                    }
                    file[offset as usize..end].copy_from_slice(bytes);
                    Ok(())
                }
                None => Err(AmoruError::Io {
                    op: "write_file",
                    target: key.clone(),
                    msg: format!("a {:?} view has no host bytes to write", src.tier()),
                }),
            }
        };
        self.resolve(index, sender, outcome);
        completion
    }

    fn read_object(&self, url: &str, offset: u64, dst: Buffer) -> Completion<Buffer> {
        let (index, failing) = self.submit(
            OpKind::ReadObject,
            url,
            offset,
            dst.len() as u64,
            None,
            Some(dst.tier()),
        );
        let (sender, completion) = Completion::channel();
        let mut dst = dst;
        let outcome = if failing {
            Err(FakeReactor::io_error("read_object", url))
        } else {
            match self.read_into(url, offset, &mut dst, true) {
                Ok(_) => Ok(dst),
                Err(e) => Err(e),
            }
        };
        self.resolve(index, sender, outcome);
        completion
    }

    fn write_object(&self, url: &str, src: BufferView) -> Completion<()> {
        let (index, failing) = self.submit(
            OpKind::WriteObject,
            url,
            0,
            src.len() as u64,
            Some(src.tier()),
            None,
        );
        let (sender, completion) = Completion::channel();
        let outcome = if failing {
            Err(FakeReactor::io_error("write_object", url))
        } else {
            match src.as_host_slice() {
                Some(bytes) => {
                    self.lock().objects.insert(url.to_string(), bytes.to_vec());
                    Ok(())
                }
                None => Err(AmoruError::Io {
                    op: "write_object",
                    target: url.to_string(),
                    msg: format!("a {:?} view has no host bytes to write", src.tier()),
                }),
            }
        };
        self.resolve(index, sender, outcome);
        completion
    }

    fn copy(&self, src: CopySrc, dst: CopyDst) -> Completion<Option<Buffer>> {
        let (src_tier, len) = match &src {
            CopySrc::View(view) => (Some(view.tier()), view.len() as u64),
            CopySrc::Disk(seg) => (Some(Tier::Disk(*seg)), seg.len),
        };
        let dst_tier = match &dst {
            CopyDst::Buffer(buffer) => Some(buffer.tier()),
            CopyDst::Disk(seg) => Some(Tier::Disk(*seg)),
        };
        let (index, failing) = self.submit(OpKind::Copy, "", 0, len, src_tier, dst_tier);
        let (sender, completion) = Completion::channel();
        let outcome = if failing {
            Err(FakeReactor::io_error("copy", ""))
        } else {
            match (src, dst) {
                (CopySrc::View(view), CopyDst::Buffer(mut buffer)) => {
                    match (view.as_host_slice(), buffer.host_ptr()) {
                        (Some(bytes), Some(_)) => {
                            let taken = bytes.len().min(buffer.len());
                            buffer[..taken].copy_from_slice(&bytes[..taken]);
                            Ok(Some(buffer))
                        }
                        // A device endpoint moves no bytes in a fake: the tiers are tags.
                        _ => Ok(Some(buffer)),
                    }
                }
                (CopySrc::View(_), CopyDst::Disk(_)) => Ok(None),
                (CopySrc::Disk(_), CopyDst::Buffer(buffer)) => Ok(Some(buffer)),
                (CopySrc::Disk(_), CopyDst::Disk(_)) => Ok(None),
            }
        };
        self.resolve(index, sender, outcome);
        completion
    }

    fn register_segment(&self, segment: u32, path: &Path) -> Result<()> {
        self.lock().segments.insert(segment, path.to_path_buf());
        Ok(())
    }

    fn unregister_segment(&self, segment: u32) {
        self.lock().segments.remove(&segment);
    }

    fn paths(&self) -> IoPaths {
        self.inner.paths.clone()
    }

    fn shutdown(&self) {
        self.inner.shutdown_calls.fetch_add(1, Ordering::SeqCst);
        if self.inner.cancel_on_shutdown {
            // Every completion still in flight resolves within the longest operation's
            // duration (RE-I7); here that is the latency the knob set.
            let mut state = self.lock();
            state.in_flight = 0;
        }
    }
}

impl ObjectMetadata for FakeReactor {
    fn head_object(&self, url: &str) -> Completion<ObjectMeta> {
        let (index, failing) = self.submit(OpKind::HeadObject, url, 0, 0, None, None);
        let (sender, completion) = Completion::channel();
        let outcome = if failing {
            Err(FakeReactor::io_error("head_object", url))
        } else {
            match self.lock().objects.get(url) {
                Some(bytes) => Ok(ObjectMeta {
                    url: url.to_string(),
                    size: bytes.len() as u64,
                    last_modified_ns: None,
                    e_tag: None,
                }),
                None => Err(AmoruError::Io {
                    op: "head_object",
                    target: url.to_string(),
                    msg: "no such object".to_string(),
                }),
            }
        };
        self.resolve(index, sender, outcome);
        completion
    }

    fn list_prefix(&self, url: &str) -> Completion<Vec<ObjectMeta>> {
        let (index, failing) = self.submit(OpKind::ListPrefix, url, 0, 0, None, None);
        let (sender, completion) = Completion::channel();
        let outcome = if failing {
            Err(FakeReactor::io_error("list_prefix", url))
        } else {
            let state = self.lock();
            let mut found: Vec<ObjectMeta> = state
                .objects
                .iter()
                .filter(|(key, _)| key.starts_with(url))
                .map(|(key, bytes)| ObjectMeta {
                    url: key.clone(),
                    size: bytes.len() as u64,
                    last_modified_ns: None,
                    e_tag: None,
                })
                .collect();
            found.sort_by(|a, b| a.url.cmp(&b.url));
            Ok(found)
        };
        self.resolve(index, sender, outcome);
        completion
    }
}

/// A segment reference over an in-memory file, for a test that wants a `Disk` endpoint.
pub fn segment_ref(segment: u32, offset: u64, len: u64) -> SegmentRef {
    SegmentRef {
        segment,
        offset,
        len,
    }
}
