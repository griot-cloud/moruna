//! The established object_store backend, preserving native conditional versions. Network
//! chunks are copied at ingress into the supplied arena allocation, never called DMA.
#![forbid(unsafe_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use bytes::Bytes;
use futures::{StreamExt, TryStreamExt, stream::FuturesUnordered};
use moruna_kernel::{Buffer, BufferView};
use object_store::path::Path;
use object_store::{
    GetOptions, GetRange, ObjectStore, ObjectStoreExt, PutMode, PutPayload, UpdateVersion,
};

use crate::{Backend, Condition, Error, Metadata, Object, Operation, Result, Scope, Version};

/// Bounded bucket upload policy. All sizes are bytes.
#[derive(Clone, Debug)]
pub struct Config {
    /// Explicit owning workspace.
    pub workspace: String,
    /// Explicit home bucket, already configured on the supplied SDK client.
    pub bucket: String,
    /// Parallel upload parts, default 4.
    pub parallel_parts: usize,
    /// Part size, default 16 MiB; at least 5 MiB for cross-provider compatibility.
    pub part_bytes: usize,
    /// Objects above this length use multipart, default 64 MiB.
    pub single_put_bytes: usize,
    /// Maximum payload per SDK GET, default 8 MiB.
    pub range_bytes: usize,
    /// Concurrent GET bodies across this bucket adapter, default 4.
    pub parallel_ranges: usize,
}
impl Config {
    /// Default bounded policy for the required workspace and bucket.
    pub fn new(workspace: String, bucket: String) -> Self {
        Self {
            workspace,
            bucket,
            parallel_parts: 4,
            part_bytes: 16 << 20,
            single_put_bytes: 64 << 20,
            range_bytes: 8 << 20,
            parallel_ranges: 4,
        }
    }
}

/// Production home restricted to an actual configured established cloud SDK type.
/// The caller still owns real endpoint/credential/durability provisioning and proof;
/// this type prevents an arbitrary fixture ObjectStore from attesting bucket home.
pub struct Home {
    store: Arc<dyn ObjectStore>,
}
impl Home {
    /// Explicit S3/S3-compatible client, configured by its owner with real endpoint and
    /// credentials. Construction does not contact a cloud or assert physical proof.
    #[cfg(feature = "s3")]
    pub fn s3(store: object_store::aws::AmazonS3) -> Self {
        Self {
            store: Arc::new(store),
        }
    }
    /// Explicit configured GCS client with application-owned credential/endpoint setup.
    /// Construction performs no network request and asserts no physical/cloud proof.
    #[cfg(feature = "gcs")]
    pub fn gcs(store: object_store::gcp::GoogleCloudStorage) -> Self {
        Self {
            store: Arc::new(store),
        }
    }
}
/// The bucket client, which can use the existing host's HTTP-over-vsock connector or
/// the native network client. Deployment/authentication of that host proxy is independent.
#[derive(Clone)]
pub struct Bucket {
    store: Arc<dyn ObjectStore>,
    cfg: Config,
    ingress: Arc<AtomicU64>,
    stage: Arc<dyn crate::staging::Staging>,
    ranges: Arc<tokio::sync::Semaphore>,
    stats: Arc<StageStats>,
    role: Option<moruna_storage_format::auth::SessionRole>,
}
impl Bucket {
    /// Construct a real typed SDK home, never from an arbitrary fixture trait object.
    pub fn new(home: Home, cfg: Config, stage: Arc<dyn crate::staging::Staging>) -> Result<Self> {
        let mut bucket = Self::unclassified(home.store, cfg, stage)?;
        bucket.role = Some(moruna_storage_format::auth::SessionRole::BucketHome);
        Ok(bucket)
    }
    /// Explicit unclassified SDK adapter for conformance/plugin use. It conveys NO
    /// bucket-home role and native portable-home admission must refuse it.
    pub fn unclassified(
        store: Arc<dyn ObjectStore>,
        cfg: Config,
        stage: Arc<dyn crate::staging::Staging>,
    ) -> Result<Self> {
        if cfg.workspace.is_empty()
            || cfg.bucket.is_empty()
            || cfg.parallel_parts == 0
            || cfg.parallel_parts > 1024
            || cfg.part_bytes < 5 << 20
            || cfg.part_bytes > 1 << 30
            || cfg.single_put_bytes < cfg.part_bytes
            || cfg.range_bytes == 0
            || cfg.range_bytes > 64 << 20
            || cfg.parallel_ranges == 0
            || cfg.parallel_ranges > 1024
        {
            return Err(Error::Invalid("bucket upload configuration"));
        }
        let ranges = Arc::new(tokio::sync::Semaphore::new(cfg.parallel_ranges));
        Ok(Self {
            store,
            cfg,
            ingress: Arc::new(AtomicU64::new(0)),
            stage,
            ranges,
            stats: Arc::new(StageStats::default()),
            role: None,
        })
    }
    /// Bytes actually copied from network/SDK chunks to charged destination buffers.
    pub fn ingress_bytes(&self) -> u64 {
        self.ingress.load(Ordering::Relaxed)
    }
    fn scope(&self, object: &Object) -> Result<()> {
        object.validate()?;
        if object.workspace != self.cfg.workspace || object.bucket != self.cfg.bucket {
            return Err(Error::Denied);
        }
        if object.key.starts_with(".moruna-upload/") {
            return Err(Error::Invalid("reserved upload prefix"));
        }
        Ok(())
    }
    fn path(&self, object: &Object) -> Result<Path> {
        self.scope(object)?;
        let path = Path::parse(&object.key).map_err(|_| Error::Invalid("bucket object path"))?;
        if path.as_ref() != object.key {
            return Err(Error::Invalid("noncanonical bucket object identity"));
        }
        Ok(path)
    }
    async fn multipart(
        &self,
        path: &Path,
        scope: &Scope,
        src: BufferView,
        cancelled: &Cancellation,
    ) -> Result<object_store::PutResult> {
        // Multipart has no portable create-if-absent option. Complete under a random
        // temporary key, then use the backend's atomic copy-if-absent to publish.
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|_| Error::Unavailable("OS entropy"))?;
        let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let temp = Path::from(format!(".moruna-upload/{suffix}"));
        let mut upload = self
            .store
            .put_multipart(&temp)
            .await
            .map_err(|e| error("start multipart", e))?;
        let expected_bytes = src.len() as u64;
        let expected_hash = *blake3::hash(
            src.as_host_slice()
                .ok_or(Error::Invalid("multipart host source"))?,
        )
        .as_bytes();
        let bytes = payload_bytes(src)?;
        let mut failure = drain_parts(
            bytes,
            self.cfg.part_bytes,
            self.cfg.parallel_parts,
            cancelled,
            |payload| upload.put_part(payload),
        )
        .await
        .err();
        if failure.is_none()
            && let Err(error_value) = upload.complete().await
        {
            failure = Some(error("complete multipart", error_value));
        }
        if let Some(primary) = failure {
            return match upload.abort().await {
                Ok(()) => Err(primary),
                Err(e) => Err(Error::UploadAbort {
                    primary: Box::new(primary),
                    abort: Box::new(error("abort multipart", e)),
                }),
            };
        }
        if cancelled.flag.load(Ordering::SeqCst) {
            return match self.store.delete(&temp).await {
                Ok(()) => Err(Error::Cancelled),
                Err(error_value) => Err(Error::Cleanup {
                    primary: Box::new(Error::Cancelled),
                    cleanup: Box::new(error("remove cancelled completed upload", error_value)),
                }),
            };
        }
        let published = self
            .store
            .copy_if_not_exists(&temp, path)
            .await
            .map_err(|e| error("publish multipart", e));
        let removed = self
            .store
            .delete(&temp)
            .await
            .map_err(|e| error("remove multipart temporary object", e));
        match (published, removed) {
            (Err(primary), Err(cleanup)) => Err(Error::Cleanup {
                primary: Box::new(primary),
                cleanup: Box::new(cleanup),
            }),
            (Err(e), Ok(())) | (Ok(()), Err(e)) => Err(e),
            (Ok(()), Ok(())) => {
                let m = self
                    .store
                    .head(path)
                    .await
                    .map_err(|e| error("head completed object", e))?;
                // copy_if_not_exists returns no publication version. A later HEAD may
                // name a concurrent replacement; never acknowledge unrelated bytes.
                self.verify_copy(scope, path, &m, expected_bytes, expected_hash)
                    .await?;
                Ok(object_store::PutResult {
                    e_tag: m.e_tag,
                    version: m.version,
                })
            }
        }
    }
    async fn verify_copy(
        &self,
        scope: &Scope,
        path: &Path,
        meta: &object_store::ObjectMeta,
        bytes: u64,
        expected: [u8; 32],
    ) -> Result<()> {
        if meta.size != bytes {
            return Err(Error::Integrity);
        }
        let wanted = version(meta);
        wanted.validate()?;
        let mut hash = blake3::Hasher::new();
        let mut start = 0;
        while start < bytes {
            let count = (bytes - start).min(self.cfg.range_bytes as u64);
            let end = start.checked_add(count).ok_or(Error::Integrity)?;
            let _permit = self
                .ranges
                .acquire()
                .await
                .map_err(|_| Error::Unavailable("bucket range admission closed"))?;
            let lease = self.stage.reserve(scope, count)?;
            let now = self.stats.current.fetch_add(count, Ordering::SeqCst) + count;
            self.stats.peak.fetch_max(now, Ordering::SeqCst);
            let _charge = Charged {
                _lease: lease,
                stats: self.stats.clone(),
                bytes: count,
            };
            self.stats.requests.fetch_add(1, Ordering::SeqCst);
            let result = self
                .store
                .get_opts(
                    path,
                    GetOptions {
                        if_match: wanted.etag.clone(),
                        version: wanted.generation.clone(),
                        range: Some(GetRange::Bounded(start..end)),
                        ..Default::default()
                    },
                )
                .await
                .map_err(|e| error("verify multipart publication", e))?;
            if version(&result.meta) != wanted
                || result.range != (start..end)
                || result.meta.size != bytes
            {
                return Err(Error::Integrity);
            }
            let mut stream = result.into_stream();
            let mut received = 0u64;
            while let Some(chunk) = stream
                .try_next()
                .await
                .map_err(|e| error("verify multipart publication body", e))?
            {
                received = received
                    .checked_add(chunk.len() as u64)
                    .ok_or(Error::Integrity)?;
                if received > count {
                    return Err(Error::Integrity);
                }
                hash.update(&chunk);
            }
            if received != count {
                return Err(Error::Integrity);
            }
            start = end;
        }
        if hash.finalize().as_bytes() != &expected {
            return Err(Error::Integrity);
        }
        Ok(())
    }
}

/// SDK payload reservation accounting; actual network/proxy heap measurements remain a
/// separate proof. `copied_bytes` is observed ingress copies into charged destinations.
#[derive(Clone, Debug, Default)]
pub struct StagingStats {
    /// Currently reserved SDK response payload bytes.
    pub reserved_bytes: u64,
    /// Maximum simultaneously reserved response payload bytes.
    pub peak_reserved_bytes: u64,
    /// Actual SDK chunks copied into caller arena buffers.
    pub copied_bytes: u64,
    /// Bounded range/empty-head requests issued.
    pub requests: u64,
    /// Owned logical reads still draining, including cancelled consumers.
    pub active_reads: u64,
    /// Owned SDK create/conditional writes, including cancellation cleanup.
    pub active_writes: u64,
}
#[derive(Default)]
struct StageStats {
    current: AtomicU64,
    peak: AtomicU64,
    requests: AtomicU64,
    active: AtomicU64,
    writes: AtomicU64,
    detached: tokio::sync::Mutex<Vec<DetachedPublication>>,
    idle: tokio::sync::Notify,
}
struct Charged {
    _lease: Box<dyn crate::staging::Lease>,
    stats: Arc<StageStats>,
    bytes: u64,
}
impl Drop for Charged {
    fn drop(&mut self) {
        self.stats.current.fetch_sub(self.bytes, Ordering::SeqCst);
    }
}
struct ReadOwner(Arc<StageStats>);
impl Drop for ReadOwner {
    fn drop(&mut self) {
        if self.0.active.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}
/// Actual outcome of a publication whose consuming future was dropped.
pub struct DetachedPublication {
    /// Requester ledger scope; no stored opaque credential.
    pub scope: Scope,
    /// Actual target, retained even when cleanup failed before publication.
    pub object: Object,
    /// Actual committed metadata or original diagnosed cancellation/cleanup failure.
    pub result: Result<Metadata>,
}
struct WriteOwner(Arc<StageStats>);
impl Drop for WriteOwner {
    fn drop(&mut self) {
        if self.0.writes.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}
struct Cancellation {
    flag: AtomicBool,
    wake: tokio::sync::Notify,
}
struct CancelOnDrop(Arc<Cancellation>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.flag.store(true, Ordering::SeqCst);
        self.0.wake.notify_waiters();
    }
}
impl Bucket {
    /// Read-body staging reservations and copied-byte counters, never called zero-copy.
    pub fn staging_stats(&self) -> StagingStats {
        StagingStats {
            reserved_bytes: self.stats.current.load(Ordering::SeqCst),
            peak_reserved_bytes: self.stats.peak.load(Ordering::SeqCst),
            copied_bytes: self.ingress_bytes(),
            requests: self.stats.requests.load(Ordering::SeqCst),
            active_reads: self.stats.active.load(Ordering::SeqCst),
            active_writes: self.stats.writes.load(Ordering::SeqCst),
        }
    }
    /// Drain owned reads before releasing their run budget/provider or recycling a node.
    /// A cancelled consumer stops after its current bounded response is consumed; this is
    /// not a claim that the SDK can stop a remote request instantaneously.
    pub async fn wait_idle(&self) {
        loop {
            let notified = self.stats.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.stats.active.load(Ordering::SeqCst) == 0
                && self.stats.writes.load(Ordering::SeqCst) == 0
            {
                return;
            }
            notified.await;
        }
    }
    /// Drain original reads/writes and retrieve this requester's detached publication facts.
    /// The backend owner must inspect failures before releasing budgets or recycling. No
    /// arbitrary run credential is retained in this administrative lifecycle record.
    pub async fn drain(&self, scope: &Scope) -> Result<Vec<DetachedPublication>> {
        scope.validate()?;
        self.wait_idle().await;
        let mut records = self.stats.detached.lock().await;
        let mut owned = Vec::new();
        let mut other = Vec::new();
        for record in records.drain(..) {
            if record.scope == *scope {
                owned.push(record);
            } else {
                other.push(record);
            }
        }
        *records = other;
        Ok(owned)
    }
    fn publication<'a>(
        &'a self,
        scope: &'a Scope,
        object: &'a Object,
        condition: Option<Condition>,
        src: BufferView,
    ) -> Operation<'a, Metadata> {
        let scope = scope.clone();
        let object = object.clone();
        let owned = self.clone();
        Box::pin(async move {
            scope.validate()?;
            if src.as_host_slice().is_none() {
                return Err(Error::Invalid("bucket write needs host view"));
            }
            let runtime = tokio::runtime::Handle::try_current()
                .map_err(|_| Error::Unavailable("SDK write requires Tokio runtime"))?;
            let cancelled = Arc::new(Cancellation {
                flag: AtomicBool::new(false),
                wake: tokio::sync::Notify::new(),
            });
            let _cancel = CancelOnDrop(Arc::clone(&cancelled));
            let (sender, receiver) = tokio::sync::oneshot::channel();
            owned.stats.writes.fetch_add(1, Ordering::SeqCst);
            let owner = WriteOwner(Arc::clone(&owned.stats));
            // JoinHandle drop intentionally detaches: ownership remains in this worker;
            // result delivery failure transfers the actual result to the drain ledger.
            drop(runtime.spawn(async move {
                let _through_original_completion = owner;
                let result = owned
                    .publish_owned(&scope, &object, condition, src, &cancelled)
                    .await;
                if let Err(result) = sender.send(result) {
                    owned.stats.detached.lock().await.push(DetachedPublication {
                        scope,
                        object,
                        result,
                    });
                }
            }));
            receiver
                .await
                .map_err(|_| Error::Unavailable("owned SDK publication worker"))?
        })
    }
    async fn publish_owned(
        &self,
        scope: &Scope,
        object: &Object,
        condition: Option<Condition>,
        src: BufferView,
        cancelled: &Cancellation,
    ) -> Result<Metadata> {
        let path = self.path(object)?;
        if object.key.is_empty() {
            return Err(Error::Invalid("empty object key"));
        }
        if cancelled.flag.load(Ordering::SeqCst) {
            return Err(Error::Cancelled);
        }
        let len = src.len();
        let result = match condition {
            None if len > self.cfg.single_put_bytes => {
                self.multipart(&path, scope, src, cancelled).await?
            }
            condition => {
                let mode = match condition {
                    None | Some(Condition::Absent) => PutMode::Create,
                    Some(Condition::Match(v)) => {
                        v.validate()?;
                        PutMode::Update(UpdateVersion {
                            e_tag: v.etag,
                            version: v.generation,
                        })
                    }
                };
                self.store
                    .put_opts(
                        &path,
                        PutPayload::from_bytes(payload_bytes(src)?),
                        mode.into(),
                    )
                    .await
                    .map_err(|e| error("publish whole object", e))?
            }
        };
        metadata(object, result, len as u64)
    }
    async fn read_owned(
        &self,
        scope: Scope,
        object: Object,
        wanted: Version,
        offset: u64,
        mut dst: Buffer,
        cancelled: Arc<Cancellation>,
    ) -> Result<Buffer> {
        wanted.validate()?;
        if dst.host_ptr().is_none() {
            return Err(Error::Invalid("bucket read needs host buffer"));
        }
        let path = self.path(&object)?;
        let whole_end = offset
            .checked_add(dst.len() as u64)
            .ok_or(Error::Invalid("range overflow"))?;
        let mut total = 0usize;
        loop {
            if cancelled.flag.load(Ordering::SeqCst) {
                return Err(Error::Cancelled);
            }
            let count = (dst.len() - total).min(self.cfg.range_bytes);
            let start = offset
                .checked_add(total as u64)
                .ok_or(Error::Invalid("range overflow"))?;
            let end = start
                .checked_add(count as u64)
                .ok_or(Error::Invalid("range overflow"))?;
            let cancellation = cancelled.wake.notified();
            tokio::pin!(cancellation);
            cancellation.as_mut().enable();
            if cancelled.flag.load(Ordering::SeqCst) {
                return Err(Error::Cancelled);
            }
            let _permit = match futures::future::select(
                Box::pin(Arc::clone(&self.ranges).acquire_owned()),
                cancellation,
            )
            .await
            {
                futures::future::Either::Left((permit, _)) => {
                    permit.map_err(|_| Error::Unavailable("bucket range admission closed"))?
                }
                futures::future::Either::Right(_) => return Err(Error::Cancelled),
            };
            if cancelled.flag.load(Ordering::SeqCst) {
                return Err(Error::Cancelled);
            }
            let lease = self.stage.reserve(&scope, count as u64)?;
            let now = self.stats.current.fetch_add(count as u64, Ordering::SeqCst) + count as u64;
            self.stats.peak.fetch_max(now, Ordering::SeqCst);
            let _charge = Charged {
                _lease: lease,
                stats: Arc::clone(&self.stats),
                bytes: count as u64,
            };
            self.stats.requests.fetch_add(1, Ordering::SeqCst);
            let result = self
                .store
                .get_opts(
                    &path,
                    GetOptions {
                        if_match: wanted.etag.clone(),
                        version: wanted.generation.clone(),
                        range: (count != 0).then_some(GetRange::Bounded(start..end)),
                        head: count == 0,
                        ..Default::default()
                    },
                )
                .await
                .map_err(|e| error("read range", e))?;
            if version(&result.meta) != wanted {
                return Err(Error::Conflict);
            }
            if whole_end > result.meta.size {
                return Err(Error::Invalid("range past object"));
            }
            if count == 0 {
                return Ok(dst);
            }
            if result.range != (start..end) {
                return Err(Error::Integrity);
            }
            let mut stream = result.into_stream();
            let mut copied = 0usize;
            while let Some(chunk) = stream.try_next().await.map_err(|e| error("read body", e))? {
                let end = copied
                    .checked_add(chunk.len())
                    .ok_or(Error::Invalid("body overflow"))?;
                if end > count {
                    return Err(Error::Integrity);
                }
                dst.as_mut()[total + copied..total + end].copy_from_slice(&chunk);
                self.ingress
                    .fetch_add(chunk.len() as u64, Ordering::Relaxed);
                copied = end;
            }
            if copied != count {
                return Err(Error::Integrity);
            }
            total += count;
            if cancelled.flag.load(Ordering::SeqCst) {
                return Err(Error::Cancelled);
            }
            if total == dst.len() {
                return Ok(dst);
            }
        }
    }
}

fn error(operation: &'static str, source: object_store::Error) -> Error {
    match source {
        object_store::Error::Precondition { .. } | object_store::Error::AlreadyExists { .. } => {
            Error::Conflict
        }
        object_store::Error::NotFound { .. } => Error::Missing,
        source => Error::Bucket {
            operation,
            source: Box::new(source),
        },
    }
}
fn version(meta: &object_store::ObjectMeta) -> Version {
    Version {
        etag: meta.e_tag.clone(),
        generation: meta.version.clone(),
    }
}
fn metadata(object: &Object, result: object_store::PutResult, bytes: u64) -> Result<Metadata> {
    let version = Version {
        etag: result.e_tag,
        generation: result.version,
    };
    version.validate()?;
    Ok(Metadata {
        object: object.clone(),
        version,
        bytes,
    })
}
struct ViewBytes(BufferView);
impl AsRef<[u8]> for ViewBytes {
    fn as_ref(&self) -> &[u8] {
        // Construction below has checked host visibility; an empty view is still valid.
        match self.0.as_host_slice() {
            Some(bytes) => bytes,
            None => &[],
        }
    }
}
fn payload_bytes(view: BufferView) -> Result<Bytes> {
    if view.as_host_slice().is_none() {
        return Err(Error::Invalid("bucket write needs host view"));
    }
    Ok(Bytes::from_owner(ViewBytes(view)))
}

impl Backend for Bucket {
    fn role(&self) -> Option<moruna_storage_format::auth::SessionRole> {
        // Only a typed `Home` conveys bucket-home; `unclassified` adapters never do.
        self.role
    }

    fn current<'a>(&'a self, scope: &'a Scope, object: &'a Object) -> Operation<'a, Metadata> {
        Box::pin(async move {
            scope.validate()?;
            let meta = self
                .store
                .head(&self.path(object)?)
                .await
                .map_err(|e| error("current object metadata", e))?;
            let version = version(&meta);
            version.validate()?;
            Ok(Metadata {
                object: object.clone(),
                version,
                bytes: meta.size,
            })
        })
    }

    fn head<'a>(
        &'a self,
        scope: &'a Scope,
        object: &'a Object,
        wanted: &'a Version,
    ) -> Operation<'a, Metadata> {
        Box::pin(async move {
            scope.validate()?;
            wanted.validate()?;
            let meta = self
                .store
                .get_opts(
                    &self.path(object)?,
                    GetOptions {
                        head: true,
                        if_match: wanted.etag.clone(),
                        version: wanted.generation.clone(),
                        ..Default::default()
                    },
                )
                .await
                .map_err(|e| error("head object", e))?
                .meta;
            if version(&meta) != *wanted {
                return Err(Error::Conflict);
            }
            Ok(Metadata {
                object: object.clone(),
                version: wanted.clone(),
                bytes: meta.size,
            })
        })
    }

    fn read<'a>(
        &'a self,
        scope: &'a Scope,
        object: &'a Object,
        wanted: &'a Version,
        offset: u64,
        dst: Buffer,
    ) -> Operation<'a, Buffer> {
        let (owned, scope, object, wanted) =
            (self.clone(), scope.clone(), object.clone(), wanted.clone());
        Box::pin(async move {
            let runtime = tokio::runtime::Handle::try_current()
                .map_err(|_| Error::Unavailable("SDK read requires Tokio runtime"))?;
            let cancelled = Arc::new(Cancellation {
                flag: AtomicBool::new(false),
                wake: tokio::sync::Notify::new(),
            });
            let _cancel_on_consumer_drop = CancelOnDrop(Arc::clone(&cancelled));
            owned.stats.active.fetch_add(1, Ordering::SeqCst);
            let owner = ReadOwner(Arc::clone(&owned.stats));
            // JoinHandle drop detaches rather than aborts. The operation retains its
            // reservation/destination and drains the current bounded body before stopping.
            runtime
                .spawn(async move {
                    let _through_owned_completion = owner;
                    owned
                        .read_owned(scope, object, wanted, offset, dst, cancelled)
                        .await
                })
                .await
                .map_err(|_| Error::Unavailable("owned SDK read worker"))?
        })
    }
    fn write<'a>(
        &'a self,
        scope: &'a Scope,
        object: &'a Object,
        src: BufferView,
    ) -> Operation<'a, Metadata> {
        self.publication(scope, object, None, src)
    }
    fn list<'a>(&'a self, scope: &'a Scope, prefix: &'a Object) -> Operation<'a, Vec<Metadata>> {
        Box::pin(async move {
            scope.validate()?;
            self.scope(prefix)?;
            // SDK prefix listing is segment-based and omits the exact prefix key. The
            // storage port uses literal key prefixes like the raw/file adapters, so list
            // the containing directory then filter every exact returned key.
            let parent = prefix
                .key
                .rsplit_once('/')
                .map(|(parent, _)| parent)
                .unwrap_or("");
            let parent = Path::parse(parent).map_err(|_| Error::Invalid("bucket prefix path"))?;
            let mut stream = self.store.list(Some(&parent));
            let mut out = Vec::new();
            while let Some(m) = stream
                .try_next()
                .await
                .map_err(|e| error("list prefix", e))?
            {
                let key = m.location.to_string();
                if key.starts_with(".moruna-upload/") || !key.starts_with(&prefix.key) {
                    continue;
                }
                let version = version(&m);
                version.validate()?;
                out.push(Metadata {
                    object: Object {
                        key,
                        ..prefix.clone()
                    },
                    version,
                    bytes: m.size,
                });
            }
            out.sort_by(|a, b| a.object.key.cmp(&b.object.key));
            Ok(out)
        })
    }
    fn replace<'a>(
        &'a self,
        scope: &'a Scope,
        object: &'a Object,
        condition: Condition,
        src: BufferView,
    ) -> Operation<'a, Metadata> {
        self.publication(scope, object, Some(condition), src)
    }
}

// One admission/drain authority shared by the SDK path and controlled completion tests.
// Every already-created part future reaches its original result before this returns.
async fn drain_parts(
    bytes: Bytes,
    part_bytes: usize,
    parallel: usize,
    cancelled: &Cancellation,
    mut submit: impl FnMut(PutPayload) -> object_store::UploadPart,
) -> Result<()> {
    let mut parts = FuturesUnordered::new();
    let mut offset = 0;
    let mut failure = None;
    while offset < bytes.len() || !parts.is_empty() {
        if cancelled.flag.load(Ordering::SeqCst) && failure.is_none() {
            failure = Some(Error::Cancelled);
        }
        while failure.is_none() && offset < bytes.len() && parts.len() < parallel {
            let end = offset.saturating_add(part_bytes).min(bytes.len());
            parts.push(submit(PutPayload::from_bytes(bytes.slice(offset..end))));
            offset = end;
        }
        if parts.is_empty() {
            break;
        }
        if let Some(Err(source)) = parts.next().await {
            let next = error("upload part", source);
            failure = Some(match failure.take() {
                None => next,
                Some(first) => Error::Concurrent {
                    first: Box::new(first),
                    next: Box::new(next),
                },
            });
        }
    }
    if cancelled.flag.load(Ordering::SeqCst) && failure.is_none() {
        failure = Some(Error::Cancelled);
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(test)]
mod publication_tests {
    use super::*;
    use moruna_kernel::{Allocator, Tier};
    use moruna_testkit::FakeAllocator;
    use std::collections::VecDeque;
    struct VerificationLease;
    impl crate::staging::Lease for VerificationLease {}
    struct VerificationBudget(Scope);
    impl crate::staging::Staging for VerificationBudget {
        fn reserve(&self, scope: &Scope, bytes: u64) -> Result<Box<dyn crate::staging::Lease>> {
            if scope != &self.0 || bytes > 3 {
                return Err(Error::Denied);
            }
            Ok(Box::new(VerificationLease))
        }
    }
    #[test]
    fn multipart_readback_never_acknowledges_a_concurrent_replacement() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let scope = Scope {
                    workspace: "requester".into(),
                    run: "attempt".into(),
                };
                let store = Arc::new(object_store::memory::InMemory::new());
                let path = Path::from("actual-object");
                store
                    .put(
                        &path,
                        PutPayload::from_bytes(Bytes::from_static(b"original")),
                    )
                    .await
                    .unwrap();
                let original = store.head(&path).await.unwrap();
                let mut config = Config::new("owner".into(), "home".into());
                config.range_bytes = 3;
                let bucket = Bucket::unclassified(
                    store.clone(),
                    config,
                    Arc::new(VerificationBudget(scope.clone())),
                )
                .unwrap();
                let expected = *blake3::hash(b"original").as_bytes();
                bucket
                    .verify_copy(&scope, &path, &original, 8, expected)
                    .await
                    .unwrap();
                assert_eq!(bucket.staging_stats().reserved_bytes, 0);
                assert!(bucket.staging_stats().peak_reserved_bytes <= 3);
                assert_eq!(bucket.ingress_bytes(), 0); // hashing SDK bytes is not an arena copy
                store
                    .put(
                        &path,
                        PutPayload::from_bytes(Bytes::from_static(b"replaced")),
                    )
                    .await
                    .unwrap();
                assert!(
                    bucket
                        .verify_copy(&scope, &path, &original, 8, expected)
                        .await
                        .is_err()
                );
                let replacement = store.head(&path).await.unwrap();
                assert!(matches!(
                    bucket
                        .verify_copy(&scope, &path, &replacement, 8, expected)
                        .await,
                    Err(Error::Integrity)
                ));
                assert_eq!(bucket.staging_stats().reserved_bytes, 0);
            });
    }
    #[test]
    fn cancelled_multipart_drains_every_original_part_before_abort_admission() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let allocator = FakeAllocator::new();
                let mut buffer = allocator.alloc(10, Tier::Host).unwrap();
                buffer.as_mut().fill(9);
                let payload = payload_bytes(Arc::new(buffer).view()).unwrap();
                let cancel = Arc::new(Cancellation {
                    flag: AtomicBool::new(false),
                    wake: tokio::sync::Notify::new(),
                });
                let admitted = Arc::new(AtomicU64::new(0));
                let changed = Arc::new(tokio::sync::Notify::new());
                let abort_admitted = Arc::new(AtomicBool::new(false));
                let (first, first_gate) = tokio::sync::oneshot::channel::<()>();
                let (second, second_gate) = tokio::sync::oneshot::channel::<()>();
                let gates = VecDeque::from([first_gate, second_gate]);
                let task_cancel = cancel.clone();
                let task_admitted = admitted.clone();
                let task_changed = changed.clone();
                let task_abort = abort_admitted.clone();
                let worker = tokio::spawn(async move {
                    let mut gates = gates;
                    let result = drain_parts(payload, 3, 2, &task_cancel, move |payload| {
                        let gate = gates
                            .pop_front()
                            .expect("only two admitted parts before cancellation");
                        task_admitted.fetch_add(1, Ordering::SeqCst);
                        task_changed.notify_one();
                        Box::pin(async move {
                            let _through_original_completion = payload;
                            gate.await.map_err(|_| object_store::Error::Generic {
                                store: "TEST-ONLY part completion",
                                source: Box::new(std::io::Error::other(
                                    "fixture completion closed",
                                )),
                            })?;
                            Ok(())
                        })
                    })
                    .await;
                    // Production multipart calls abort only after this shared drain helper.
                    task_abort.store(true, Ordering::SeqCst);
                    result
                });
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    while admitted.load(Ordering::SeqCst) < 2 {
                        changed.notified().await;
                    }
                })
                .await
                .unwrap();
                cancel.flag.store(true, Ordering::SeqCst);
                assert!(!abort_admitted.load(Ordering::SeqCst));
                assert_eq!(allocator.in_use(Tier::Host), 10);
                first.send(()).unwrap();
                tokio::task::yield_now().await;
                assert!(!abort_admitted.load(Ordering::SeqCst));
                assert_eq!(admitted.load(Ordering::SeqCst), 2);
                assert_eq!(allocator.in_use(Tier::Host), 10);
                second.send(()).unwrap();
                assert!(matches!(worker.await.unwrap(), Err(Error::Cancelled)));
                assert!(abort_admitted.load(Ordering::SeqCst));
                assert_eq!(allocator.in_use(Tier::Host), 0);
            });
    }
}
