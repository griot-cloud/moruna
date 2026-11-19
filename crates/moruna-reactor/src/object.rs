//! Object storage (06 f.3, f.4): ranged reads into an arena buffer, single and multipart
//! writes from a view, and the metadata calls a source's `plan` needs (d.1).
//!
//! A client is built once per `(scheme, container)` from `ObjectStoreConfig` and cached, so an
//! operation never builds a client; an unknown scheme, or a backend whose configuration is
//! absent, is `Config { name: "object_store" }` at the first operation that names it.
//!
//! The reactor talks to `object_store` through the small [`ObjectBackend`] seam below rather
//! than through `Arc<dyn ObjectStore>` directly, because `ObjectStore` is an `#[async_trait]`
//! trait whose `list` returns a `futures` `BoxStream`: implementing it in a test would need
//! the `futures` crate, which the preamble's dependency table does not carry. The seam is the
//! five calls d.2 names, the adapter over the real store is the only implementation that
//! ships, and the test wrapper of section k implements the same seam.

use moruna_storage::net::NetConnector;
use std::collections::HashMap;
use std::future::Future;
use std::ops::Range;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use moruna_kernel::{MorunaError, ObjectMeta, Result};
use object_store::path::Path as OsPath;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

use crate::credentials::{Guard, Store};
use crate::{AzureConfig, GcsConfig, ObjectStoreConfig, S3Config};

/// The smallest part S3 takes, for every part of a multipart upload but the last: a limit of the
/// protocol, not a tuning (Azure's blocks and Google's are no smaller in practice).
pub const S3_PART_MIN_BYTES: u64 = 5 << 20;
/// The most parts S3 takes in one upload: a limit of the protocol.
pub const S3_PARTS_MAX: u64 = 10_000;

/// What a budget lets the object stores have in flight (system design 3A, item 3.5b): how many
/// requests at once, and how large a part. Derived from the run's in-flight room, never set:
/// the arena charges each request's bytes (a read's body lands in the buffer the drive charged
/// for it, and a write sends from a view of the sink's arena buffer), so what a budget decides is
/// how much of that room the requests share and how many of them share it.
///
/// - `requests` (N) is the room over S3's smallest part, at least one and at most the job's
///   socket budget (3D: `moruna_storage::net::SOCKETS`, the engine's copy of the node's
///   `egress::SOCKETS`), so every request may carry at least a part of S3's least size.
/// - `part_bytes` is the room over N, at least S3's smallest part: each request's share.
///
/// An object's own part ([`ObjectIo::part_for`]) is that share, raised so the object fits in S3's
/// most parts and capped at the object. An object of one part is one `put`; a larger one is a
/// multipart upload with at most N of its parts in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjectIo {
    /// Requests to the stores at once, reads and writes together (N).
    pub requests: usize,
    /// Each request's share of the room: a part's size before an object's own limits.
    pub part_bytes: u64,
}

impl ObjectIo {
    /// The requests and part of a run whose arena has `room` bytes in flight (half the arena,
    /// as the controller counts a byte in flight: once where a worker holds it, once in the
    /// queues behind it) and whose job may hold `sockets` connections at once.
    pub fn for_room(room: u64, sockets: usize) -> ObjectIo {
        let sockets = u64::try_from(sockets.max(1)).unwrap_or(u64::MAX);
        let requests = (room / S3_PART_MIN_BYTES).clamp(1, sockets);
        ObjectIo {
            requests: usize::try_from(requests).unwrap_or(usize::MAX),
            part_bytes: (room / requests).max(S3_PART_MIN_BYTES),
        }
    }

    /// The part an object of `len` bytes is written in: the share, raised to fit the object in
    /// S3's most parts, and no larger than the object.
    pub fn part_for(&self, len: u64) -> u64 {
        self.part_bytes
            .max(len.div_ceil(S3_PARTS_MAX))
            .min(len)
            .max(1)
    }

    /// Whether an object of `len` bytes is one `put`: it is one part.
    pub fn single_put(&self, len: u64) -> bool {
        len <= self.part_for(len)
    }

    /// The parts an object of `len` bytes is written in: one when it is a single put.
    pub fn parts_of(&self, len: u64) -> u64 {
        if self.single_put(len) {
            1
        } else {
            len.div_ceil(self.part_for(len))
        }
    }

    /// The parts of an object of `len` bytes in flight at once: all of them, up to N.
    pub fn parts_in_flight(&self, len: u64) -> usize {
        let parts = usize::try_from(len.div_ceil(self.part_for(len))).unwrap_or(usize::MAX);
        parts.clamp(1, self.requests.max(1))
    }
}

impl Default for ObjectIo {
    /// A reactor given no budget: no room, so one request at once in parts of S3's least size.
    fn default() -> ObjectIo {
        ObjectIo::for_room(0, moruna_storage::net::SOCKETS)
    }
}

/// A boxed future, so the seam below is a trait object without an async-trait macro.
pub(crate) type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// What the reactor asks of an object store (d.2: `get_range`, `put`, multipart, `head`,
/// `list`).
pub(crate) trait ObjectBackend: Send + Sync {
    /// The whole range as one body: for a small object read whole outside the arena (a
    /// manifest, `files`), never for a read into the arena.
    fn get_range(&self, path: &OsPath, range: Range<u64>) -> BoxFut<object_store::Result<Bytes>>;
    /// The range read into `dst`, the arena buffer charged for it, piece by piece as the body
    /// arrives (3A, 3.5b): the read holds outside the arena only the piece on its way in. `dst`
    /// comes back with the outcome, unless a local read's thread took it.
    fn read_range(
        &self,
        path: &OsPath,
        range: Range<u64>,
        dst: moruna_kernel::Buffer,
    ) -> BoxFut<(Option<moruna_kernel::Buffer>, object_store::Result<()>)>;
    fn put(&self, path: &OsPath, payload: PutPayload) -> BoxFut<object_store::Result<()>>;
    fn put_multipart(&self, path: &OsPath) -> BoxFut<object_store::Result<Box<dyn MultipartSink>>>;
    fn head(&self, path: &OsPath) -> BoxFut<object_store::Result<ObjectMeta>>;
    /// The objects and the common prefixes directly under `prefix`.
    fn list_one_level(
        &self,
        prefix: Option<OsPath>,
    ) -> BoxFut<object_store::Result<(Vec<ObjectMeta>, Vec<OsPath>)>>;
    /// Remove one object (d.9, `delete_object`).
    fn delete(&self, path: &OsPath) -> BoxFut<object_store::Result<()>>;
    /// Abandon one multipart upload by its id (d.9, `abort_multipart`). `None` from a backend
    /// that has no multipart uploads at all, which is the local filesystem.
    fn abort_by_id(&self, path: &OsPath, id: &str) -> Option<BoxFut<object_store::Result<()>>>;
}

/// One multipart upload in progress.
pub(crate) trait MultipartSink: Send {
    /// Start one part; the returned future must be polled for the part to move.
    fn part(&mut self, payload: PutPayload) -> BoxFut<object_store::Result<()>>;
    fn complete(&mut self) -> BoxFut<object_store::Result<()>>;
    fn abort(&mut self) -> BoxFut<object_store::Result<()>>;
}

/// The shipping implementation: the `object_store` crate.
pub(crate) struct StoreBackend {
    store: Arc<dyn ObjectStore>,
    /// The same client again as the crate's `MultipartStore`, which is the only way to reach
    /// an upload that this process did not start: `MultipartUpload` (what `put_multipart`
    /// hands back) can abort only the upload it holds, and it does not survive a crash.
    /// `None` for the local filesystem, which has no multipart uploads.
    multipart: Option<Arc<dyn object_store::multipart::MultipartStore>>,
    /// What the store's errors say of its credentials: every error passes through it.
    guard: Arc<Guard>,
}

impl StoreBackend {
    pub(crate) fn new(
        store: Arc<dyn ObjectStore>,
        multipart: Option<Arc<dyn object_store::multipart::MultipartStore>>,
    ) -> StoreBackend {
        StoreBackend {
            store,
            multipart,
            guard: Arc::default(),
        }
    }

    /// The same backend, its errors read by `guard` (system design 3B).
    pub(crate) fn guarded(mut self, guard: Guard) -> StoreBackend {
        self.guard = Arc::new(guard);
        self
    }
}

/// `result` with its error, if any, read by `guard`.
fn guarded<T>(guard: &Guard, result: object_store::Result<T>) -> object_store::Result<T> {
    result.map_err(|e| guard.error(e))
}

impl ObjectBackend for StoreBackend {
    fn get_range(&self, path: &OsPath, range: Range<u64>) -> BoxFut<object_store::Result<Bytes>> {
        let (store, path, guard) = (
            Arc::clone(&self.store),
            path.clone(),
            Arc::clone(&self.guard),
        );
        Box::pin(async move { guarded(&guard, store.get_range(&path, range).await) })
    }

    fn read_range(
        &self,
        path: &OsPath,
        range: Range<u64>,
        dst: moruna_kernel::Buffer,
    ) -> BoxFut<(Option<moruna_kernel::Buffer>, object_store::Result<()>)> {
        let (store, path, guard) = (
            Arc::clone(&self.store),
            path.clone(),
            Arc::clone(&self.guard),
        );
        Box::pin(async move {
            let (dst, outcome) =
                moruna_storage::body::read_range_into(&*store, &path, range, dst).await;
            (dst, guarded(&guard, outcome))
        })
    }

    fn put(&self, path: &OsPath, payload: PutPayload) -> BoxFut<object_store::Result<()>> {
        let (store, path, guard) = (
            Arc::clone(&self.store),
            path.clone(),
            Arc::clone(&self.guard),
        );
        Box::pin(async move { guarded(&guard, store.put(&path, payload).await.map(|_| ())) })
    }

    fn put_multipart(&self, path: &OsPath) -> BoxFut<object_store::Result<Box<dyn MultipartSink>>> {
        let (store, path, guard) = (
            Arc::clone(&self.store),
            path.clone(),
            Arc::clone(&self.guard),
        );
        Box::pin(async move {
            let upload = guarded(&guard, store.put_multipart(&path).await)?;
            Ok(Box::new(StoreUpload {
                upload: Some(upload),
                guard,
            }) as Box<dyn MultipartSink>)
        })
    }

    fn head(&self, path: &OsPath) -> BoxFut<object_store::Result<ObjectMeta>> {
        let (store, path, guard) = (
            Arc::clone(&self.store),
            path.clone(),
            Arc::clone(&self.guard),
        );
        Box::pin(async move { guarded(&guard, store.head(&path).await.map(|m| meta(&path, &m))) })
    }

    fn list_one_level(
        &self,
        prefix: Option<OsPath>,
    ) -> BoxFut<object_store::Result<(Vec<ObjectMeta>, Vec<OsPath>)>> {
        let (store, guard) = (Arc::clone(&self.store), Arc::clone(&self.guard));
        Box::pin(async move {
            let listed = guarded(&guard, store.list_with_delimiter(prefix.as_ref()).await)?;
            let objects = listed
                .objects
                .iter()
                .map(|m| meta(&m.location, m))
                .collect();
            Ok((objects, listed.common_prefixes))
        })
    }

    fn delete(&self, path: &OsPath) -> BoxFut<object_store::Result<()>> {
        let (store, path, guard) = (
            Arc::clone(&self.store),
            path.clone(),
            Arc::clone(&self.guard),
        );
        Box::pin(async move { guarded(&guard, store.delete(&path).await) })
    }

    fn abort_by_id(&self, path: &OsPath, id: &str) -> Option<BoxFut<object_store::Result<()>>> {
        let multipart = Arc::clone(self.multipart.as_ref()?);
        let (path, id, guard) = (path.clone(), id.to_string(), Arc::clone(&self.guard));
        Some(Box::pin(async move {
            guarded(&guard, multipart.abort_multipart(&path, &id).await)
        }))
    }
}

struct StoreUpload {
    /// `None` once the upload has been completed or aborted; a later call on it is the
    /// "already finished" programming error, reported and never a panic.
    upload: Option<Box<dyn object_store::MultipartUpload>>,
    /// The store's guard: an upload's errors are the store's.
    guard: Arc<Guard>,
}

impl MultipartSink for StoreUpload {
    fn part(&mut self, payload: PutPayload) -> BoxFut<object_store::Result<()>> {
        let guard = Arc::clone(&self.guard);
        match &mut self.upload {
            Some(upload) => {
                let part = upload.put_part(payload);
                Box::pin(async move { guarded(&guard, part.await) })
            }
            None => Box::pin(async { Err(finished()) }),
        }
    }

    fn complete(&mut self) -> BoxFut<object_store::Result<()>> {
        let Some(mut upload) = self.upload.take() else {
            return Box::pin(async { Err(finished()) });
        };
        let guard = Arc::clone(&self.guard);
        Box::pin(async move { guarded(&guard, upload.complete().await.map(|_| ())) })
    }

    fn abort(&mut self) -> BoxFut<object_store::Result<()>> {
        let Some(mut upload) = self.upload.take() else {
            return Box::pin(async { Err(finished()) });
        };
        let guard = Arc::clone(&self.guard);
        Box::pin(async move { guarded(&guard, upload.abort().await) })
    }
}

fn finished() -> object_store::Error {
    object_store::Error::Generic {
        store: "moruna-reactor",
        source: "multipart upload already completed or aborted".into(),
    }
}

fn meta(path: &OsPath, m: &object_store::ObjectMeta) -> ObjectMeta {
    ObjectMeta {
        url: path.to_string(),
        size: m.size,
        last_modified_ns: m
            .last_modified
            .timestamp_nanos_opt()
            .and_then(|n| u64::try_from(n).ok()),
        e_tag: m.e_tag.clone(),
    }
}

/// Which backend a URL names.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum Scheme {
    S3,
    Gcs,
    Azure,
    File,
}

/// A URL resolved to a client and a path inside it.
struct Located {
    client_key: String,
    scheme: Scheme,
    container: String,
    path: OsPath,
}

/// The reactor's object-store side: the configuration, the client cache (f.3) and the
/// operations built on them.
pub(crate) struct ObjectLayer {
    cfg: ObjectStoreConfig,
    clients: Mutex<HashMap<String, Arc<dyn ObjectBackend>>>,
    /// Set by the tests of section k, which drive the same operations over a wrapper that
    /// counts requests in flight and can fail a named part.
    override_backend: Option<Arc<dyn ObjectBackend>>,
    /// What the run's budget lets the stores have in flight (3A, 3.5b).
    pub(crate) budget: ObjectIo,
}

impl ObjectLayer {
    pub(crate) fn new(
        cfg: ObjectStoreConfig,
        override_backend: Option<Arc<dyn ObjectBackend>>,
    ) -> ObjectLayer {
        ObjectLayer {
            cfg,
            clients: Mutex::new(HashMap::new()),
            override_backend,
            budget: ObjectIo::default(),
        }
    }

    /// The same layer, its writes cut in parts as `budget` says.
    pub(crate) fn with_io(mut self, budget: ObjectIo) -> ObjectLayer {
        self.budget = budget;
        self
    }

    /// The client for `url` and the path inside it, built once per `(scheme, container)`.
    pub(crate) fn resolve(&self, url: &str) -> Result<(Arc<dyn ObjectBackend>, OsPath)> {
        self.resolve_for(url, crate::native_runtime())
    }

    fn resolve_for(&self, url: &str, native: bool) -> Result<(Arc<dyn ObjectBackend>, OsPath)> {
        let network = if native {
            moruna_storage::net::installed()
        } else {
            None
        };
        self.resolve_through(url, native, network)
    }

    /// As [`ObjectLayer::resolve`], `native` saying whether the engine runs on a node and
    /// `network` the job's network there, if it has one (system design 3D).
    fn resolve_through(
        &self,
        url: &str,
        native: bool,
        network: Option<NetConnector>,
    ) -> Result<(Arc<dyn ObjectBackend>, OsPath)> {
        let located = self.parse(url)?;
        if let Some(backend) = &self.override_backend {
            return Ok((Arc::clone(backend), located.path));
        }
        // An injected backend already owns its transport; never replace its capability.
        // The personality has no OS socket/connect calls for the generic cloud clients: on a
        // node, S3 and Azure Blob are reached through the job's network, and nothing else is.
        if native && located.scheme != Scheme::File {
            match (&network, located.scheme) {
                (Some(_), Scheme::S3 | Scheme::Azure) => {}
                (None, _) => {
                    return Err(config(
                        "on a node an object URL is reached through the job's network, and this \
                         job has none: its check named no host",
                    ));
                }
                (Some(_), _) => {
                    return Err(config(
                        "on a node the job's network reaches S3 and Azure Blob only, not gs://",
                    ));
                }
            }
        }
        let mut clients = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(client) = clients.get(&located.client_key) {
            return Ok((Arc::clone(client), located.path));
        }
        let client = self.build(&located, network.as_ref())?;
        clients.insert(located.client_key.clone(), Arc::clone(&client));
        Ok((client, located.path))
    }

    fn parse(&self, url: &str) -> Result<Located> {
        let Some((scheme, rest)) = url.split_once("://") else {
            // A bare key, which only the default S3 bucket can answer (d.1).
            let bucket = self
                .cfg
                .s3
                .as_ref()
                .and_then(|s| s.bucket.clone())
                .ok_or_else(|| config("a bare key needs s3.bucket to be set"))?;
            return Ok(Located {
                client_key: format!("s3/{bucket}"),
                scheme: Scheme::S3,
                container: bucket,
                path: OsPath::from(url),
            });
        };
        let scheme = match scheme {
            "s3" | "s3a" => Scheme::S3,
            "gs" => Scheme::Gcs,
            "az" | "abfs" | "azure" => Scheme::Azure,
            "file" => Scheme::File,
            other => return Err(config(&format!("unknown scheme {other}"))),
        };
        if scheme == Scheme::File {
            return Ok(Located {
                client_key: "file".into(),
                scheme,
                container: String::new(),
                path: self.local_path(rest)?,
            });
        }
        let (container, key) = match rest.split_once('/') {
            Some((c, k)) => (c.to_string(), k.to_string()),
            None => (rest.to_string(), String::new()),
        };
        if container.is_empty() {
            return Err(config(&format!("{url} names no bucket or container")));
        }
        Ok(Located {
            client_key: format!("{scheme:?}/{container}"),
            scheme,
            container,
            path: OsPath::from(key.as_str()),
        })
    }

    fn local_path(&self, rest: &str) -> Result<OsPath> {
        match &self.cfg.local_root {
            Some(_) => Ok(OsPath::from(rest.trim_start_matches('/'))),
            None => {
                let absolute = format!("/{}", rest.trim_start_matches('/'));
                OsPath::from_absolute_path(&absolute)
                    .map_err(|e| config(&format!("file url {absolute}: {e}")))
            }
        }
    }

    fn build(
        &self,
        located: &Located,
        network: Option<&NetConnector>,
    ) -> Result<Arc<dyn ObjectBackend>> {
        // Each client is built once and held twice: as the `ObjectStore` every operation uses
        // and, where the backend has multipart uploads, as the `MultipartStore` that
        // `abort_multipart` needs. The local filesystem has no second face.
        let (store, multipart): (
            Arc<dyn ObjectStore>,
            Option<Arc<dyn object_store::multipart::MultipartStore>>,
        ) = match located.scheme {
            Scheme::S3 => {
                let s3 = self
                    .cfg
                    .s3
                    .as_ref()
                    .ok_or_else(|| config("no s3 configuration"))?;
                let client = Arc::new(
                    build_s3(s3, &located.container, self.cfg.allow_http, network)
                        .map_err(|e| self.redacted(Store::S3, e))?,
                );
                (Arc::clone(&client) as Arc<dyn ObjectStore>, Some(client))
            }
            Scheme::Gcs => {
                let gcs = self
                    .cfg
                    .gcs
                    .as_ref()
                    .ok_or_else(|| config("no gcs configuration"))?;
                let client = Arc::new(
                    build_gcs(gcs, &located.container).map_err(|e| self.redacted(Store::Gcs, e))?,
                );
                (Arc::clone(&client) as Arc<dyn ObjectStore>, Some(client))
            }
            Scheme::Azure => {
                let azure = self
                    .cfg
                    .azure
                    .as_ref()
                    .ok_or_else(|| config("no azure configuration"))?;
                let client = Arc::new(
                    build_azure(azure, &located.container, self.cfg.allow_http, network)
                        .map_err(|e| self.redacted(Store::Azure, e))?,
                );
                (Arc::clone(&client) as Arc<dyn ObjectStore>, Some(client))
            }
            Scheme::File => {
                let local = match &self.cfg.local_root {
                    Some(root) => object_store::local::LocalFileSystem::new_with_prefix(root)
                        .map_err(|e| config(&format!("local_root {}: {e}", root.display())))?,
                    None => object_store::local::LocalFileSystem::new(),
                };
                (Arc::new(local), None)
            }
        };
        let guard = match located.scheme {
            Scheme::S3 => Guard::of(&self.cfg, Store::S3),
            Scheme::Gcs => Guard::of(&self.cfg, Store::Gcs),
            Scheme::Azure => Guard::of(&self.cfg, Store::Azure),
            Scheme::File => Guard::default(),
        };
        Ok(Arc::new(StoreBackend::new(store, multipart).guarded(guard)))
    }

    /// A store's build error with no credential's value in it (system design 3B).
    fn redacted(&self, store: Store, error: MorunaError) -> MorunaError {
        match error {
            MorunaError::Config { name, msg } => MorunaError::Config {
                name,
                msg: Guard::of(&self.cfg, store).redact(&msg),
            },
            other => other,
        }
    }
}

/// An S3 client for `bucket`; on a node, over `network`, the job's (system design 3D).
fn build_s3(
    cfg: &S3Config,
    bucket: &str,
    allow_http: bool,
    network: Option<&NetConnector>,
) -> Result<object_store::aws::AmazonS3> {
    let mut b = object_store::aws::AmazonS3Builder::from_env()
        .with_bucket_name(bucket)
        .with_allow_http(allow_http);
    if let Some(network) = network {
        // The network server carries HTTPS to host names only: the connector refuses anything
        // else at its first request, naming it.
        b = b.with_http_connector(network.clone());
    }
    if let Some(v) = &cfg.endpoint {
        // MH 4.6: a `unix://` or `vsock://` endpoint is a socket the host proxies; the client
        // speaks plain HTTP/1.1 over it, path-style, whatever `allow_http` says, because the
        // socket never leaves the machine. A node has no such socket.
        match crate::socket::parse_endpoint(v).map_err(|error| *error)? {
            Some(_) if network.is_some() => {
                return Err(config(
                    "s3.endpoint: on a node object storage is reached through the job's network, \
                     not a socket",
                ));
            }
            Some(target) => {
                b = b
                    .with_endpoint(crate::socket::SOCKET_ENDPOINT)
                    .with_allow_http(true)
                    .with_virtual_hosted_style_request(false)
                    .with_http_connector(crate::socket::SocketConnector::new(target));
            }
            None => {
                b = b.with_endpoint(v);
                // Virtual-host style: the endpoint is the bucket's own host, used as it is; its
                // host is the one a node lets the job reach either way (system design 3D).
                if cfg.virtual_hosted_style_request {
                    b = b.with_virtual_hosted_style_request(true);
                }
            }
        }
    }
    if let Some(v) = &cfg.region {
        b = b.with_region(v);
    }
    if let Some(v) = &cfg.access_key_id {
        b = b.with_access_key_id(v);
    }
    if let Some(v) = &cfg.secret_access_key {
        b = b.with_secret_access_key(v);
    }
    if let Some(v) = &cfg.session_token {
        b = b.with_token(v);
    }
    b.build().map_err(|e| config(&format!("s3: {e}")))
}

fn build_gcs(cfg: &GcsConfig, bucket: &str) -> Result<object_store::gcp::GoogleCloudStorage> {
    if cfg.service_account_path.is_some() && cfg.service_account_json.is_some() {
        return Err(config("gcs: set service_account_path or _json, not both"));
    }
    let mut b = object_store::gcp::GoogleCloudStorageBuilder::from_env().with_bucket_name(bucket);
    if let Some(v) = &cfg.service_account_path {
        b = b.with_service_account_path(v.display().to_string());
    }
    if let Some(v) = &cfg.service_account_json {
        b = b.with_service_account_key(v);
    }
    b.build().map_err(|e| config(&format!("gcs: {e}")))
}

/// An Azure Blob client for `container`; on a node, over `network`, the job's (system design 3D).
fn build_azure(
    cfg: &AzureConfig,
    container: &str,
    allow_http: bool,
    network: Option<&NetConnector>,
) -> Result<object_store::azure::MicrosoftAzure> {
    let mut b = object_store::azure::MicrosoftAzureBuilder::from_env()
        .with_container_name(container)
        .with_allow_http(allow_http);
    if let Some(network) = network {
        b = b.with_http_connector(network.clone());
    }
    if let Some(v) = &cfg.account {
        b = b.with_account(v);
    }
    if let Some(v) = &cfg.access_key {
        b = b.with_access_key(v);
    }
    if let Some(v) = &cfg.sas_token {
        // The query string Azure gives, with or without its leading `?`.
        b = b.with_config(
            object_store::azure::AzureConfigKey::SasKey,
            v.trim_start_matches('?'),
        );
    }
    b.build().map_err(|e| config(&format!("azure: {e}")))
}

/// A view as the bytes `object_store` writes, with no copy: the `Bytes` owns the view, the
/// view owns whatever keeps the arena region alive, and a failed write drops only the view
/// (RE-I1, f.4).
struct ViewBytes(moruna_kernel::BufferView);

impl AsRef<[u8]> for ViewBytes {
    fn as_ref(&self) -> &[u8] {
        self.0.as_host_slice().unwrap_or(&[])
    }
}

fn payload_of(view: moruna_kernel::BufferView) -> PutPayload {
    PutPayload::from_bytes(Bytes::from_owner(ViewBytes(view)))
}

/// `write_object` (f.4): an object of one part is one `put`; a larger one is a multipart upload in
/// the parts the budget gives it (`ObjectIo`), at most N of them in flight, and an abort of the
/// upload on any failure. Every part is a view of the sink's arena buffer, sent as it is.
pub(crate) async fn write(
    layer: &ObjectLayer,
    url: &str,
    src: &moruna_kernel::BufferView,
) -> Result<()> {
    if src.host_ptr().is_none() {
        return Err(MorunaError::Staging(format!(
            "an object write needs a host view, not {:?}",
            src.tier()
        )));
    }
    let (backend, path) = layer.resolve(url)?;
    let len = src.len();
    let budget = layer.budget;
    if budget.single_put(len as u64) {
        return backend
            .put(&path, payload_of(src.slice(0, len)))
            .await
            .map_err(|e| io(crate::stats::OpKind::WriteObject.as_str(), url, &e));
    }
    let part_bytes = usize::try_from(budget.part_for(len as u64)).unwrap_or(len);
    let parts_in_flight = budget.parts_in_flight(len as u64);
    let mut upload = backend
        .put_multipart(&path)
        .await
        .map_err(|e| io("write_object", url, &e))?;
    let mut in_flight: std::collections::VecDeque<
        tokio::task::JoinHandle<object_store::Result<()>>,
    > = std::collections::VecDeque::new();
    let mut offset = 0usize;
    let mut failure: Option<MorunaError> = None;
    while offset < len && failure.is_none() {
        let n = part_bytes.min(len - offset);
        let part = upload.part(payload_of(src.slice(offset, n)));
        in_flight.push_back(tokio::spawn(part));
        offset += n;
        if in_flight.len() >= parts_in_flight {
            failure = join_one(&mut in_flight, url).await;
        }
    }
    while failure.is_none() && !in_flight.is_empty() {
        failure = join_one(&mut in_flight, url).await;
    }
    for handle in in_flight {
        handle.abort();
    }
    if let Some(e) = failure {
        let _ = upload.abort().await;
        return Err(e);
    }
    upload
        .complete()
        .await
        .map_err(|e| io("write_object", url, &e))
}

async fn join_one(
    in_flight: &mut std::collections::VecDeque<tokio::task::JoinHandle<object_store::Result<()>>>,
    url: &str,
) -> Option<MorunaError> {
    let handle = in_flight.pop_front()?;
    match handle.await {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(io("write_object", url, &e)),
        Err(e) => Some(MorunaError::Io {
            op: "write_object",
            target: url.to_string(),
            msg: e.to_string(),
        }),
    }
}

/// `delete_object` (d.9): remove one object, or one file under a `file://` url.
///
/// An object that is not there is `Ok(())`: a resumed sink deletes what it wrote above
/// `committed_seq`, and a resume that runs twice must not fail the second time.
pub(crate) async fn delete(layer: &ObjectLayer, url: &str) -> Result<()> {
    let (backend, path) = layer.resolve(url)?;
    match backend.delete(&path).await {
        Ok(()) => Ok(()),
        Err(object_store::Error::NotFound { .. }) => Ok(()),
        Err(e) => Err(io(crate::stats::OpKind::DeleteObject.as_str(), url, &e)),
    }
}

/// `abort_multipart` (d.9): abandon an upload by its id, so a killed run does not leave parts
/// billed forever.
///
/// An upload the store no longer knows is `Ok(())`, for the same reason `delete` is. A
/// `file://` url is `Unsupported`: the local filesystem has no multipart uploads, so an id for
/// one cannot exist and a caller that passes one is asking for something that never happened.
pub(crate) async fn abort_multipart(layer: &ObjectLayer, url: &str, upload_id: &str) -> Result<()> {
    let (backend, path) = layer.resolve(url)?;
    let Some(fut) = backend.abort_by_id(&path, upload_id) else {
        return Err(MorunaError::Unsupported(
            "abort_multipart: this backend has no multipart uploads",
        ));
    };
    match fut.await {
        Ok(()) => Ok(()),
        Err(e) if is_missing_upload(&e) => Ok(()),
        Err(e) => Err(io(crate::stats::OpKind::AbortMultipart.as_str(), url, &e)),
    }
}

/// True for the "this store has never heard of that upload" answers, which an abort treats as
/// success: a resume that runs twice must not fail the second time. S3 answers an unknown
/// upload id with a 404, which the crate reports as `NotFound`; the crate's own in-memory store
/// reports it as a `Generic` naming the upload, so the text is read as well, the way
/// `is_transport_error` below reads it.
fn is_missing_upload(e: &object_store::Error) -> bool {
    if matches!(e, object_store::Error::NotFound { .. }) {
        return true;
    }
    let text = e.to_string().to_ascii_lowercase();
    text.contains("upload") && text.contains("not found")
}

/// The `Config { name: "object_store" }` of d.1.
pub(crate) fn config(msg: &str) -> MorunaError {
    MorunaError::Config {
        name: "object_store",
        msg: msg.to_string(),
    }
}

/// One object-store failure as the contract's `Io`.
pub(crate) fn io(op: &'static str, url: &str, e: &object_store::Error) -> MorunaError {
    MorunaError::Io {
        op,
        target: url.to_string(),
        msg: e.to_string(),
    }
}

/// True for the transport failures f.3 gives one reactor-level retry.
pub(crate) fn is_transport_error(e: &object_store::Error) -> bool {
    let text = e.to_string().to_ascii_lowercase();
    text.contains("connection reset")
        || text.contains("connection closed")
        || text.contains("broken pipe")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer() -> ObjectLayer {
        ObjectLayer::new(
            ObjectStoreConfig {
                s3: Some(S3Config {
                    bucket: Some("default".into()),
                    endpoint: Some("http://127.0.0.1:9000".into()),
                    region: Some("us-east-1".into()),
                    access_key_id: Some("k".into()),
                    secret_access_key: Some("s".into()),
                    ..S3Config::default()
                }),
                ..ObjectStoreConfig::default()
            },
            None,
        )
    }

    #[test]
    fn a_url_names_a_backend_a_container_and_a_key() {
        let l = layer();
        let p = l.parse("s3://bucket/a/b.parquet").expect("parse");
        assert_eq!(p.scheme, Scheme::S3);
        assert_eq!(p.container, "bucket");
        assert_eq!(p.path.to_string(), "a/b.parquet");
        assert_eq!(p.client_key, "S3/bucket");
        let bare = l.parse("a/b.parquet").expect("bare key");
        assert_eq!(bare.container, "default", "the default bucket answers");
        let g = l.parse("gs://bucket/k").expect("parse");
        assert_eq!(g.scheme, Scheme::Gcs);
        let a = l.parse("az://container/k").expect("parse");
        assert_eq!(a.scheme, Scheme::Azure);
        let f = l.parse("file:///tmp/x").expect("parse");
        assert_eq!(f.scheme, Scheme::File);
        assert_eq!(f.path.to_string(), "tmp/x");
    }

    #[test]
    fn an_unknown_scheme_or_a_missing_backend_is_a_config_error() {
        let l = layer();
        assert!(matches!(
            l.parse("ftp://h/k"),
            Err(MorunaError::Config {
                name: "object_store",
                ..
            })
        ));
        assert!(matches!(
            l.parse("s3:///k"),
            Err(MorunaError::Config { .. })
        ));
        let empty = ObjectLayer::new(ObjectStoreConfig::default(), None);
        assert!(matches!(
            empty.parse("bare-key"),
            Err(MorunaError::Config { .. })
        ));
        assert!(matches!(
            empty.resolve("gs://b/k"),
            Err(MorunaError::Config { .. })
        ));
        assert!(matches!(
            empty.resolve("az://c/k"),
            Err(MorunaError::Config { .. })
        ));
        assert!(matches!(
            empty.resolve("s3://b/k"),
            Err(MorunaError::Config { .. })
        ));
        let both = ObjectLayer::new(
            ObjectStoreConfig {
                gcs: Some(GcsConfig {
                    service_account_path: Some("/a".into()),
                    service_account_json: Some("{}".into()),
                    bucket: None,
                }),
                ..ObjectStoreConfig::default()
            },
            None,
        );
        assert!(matches!(
            both.resolve("gs://b/k"),
            Err(MorunaError::Config { .. })
        ));
    }

    /// The job's network as a node's engine has it, for tests: every connection it is asked
    /// for is refused, naming the host and port, as the network server refuses one its policy
    /// does not hold.
    #[derive(Debug, Default)]
    struct Refusing {
        asked: std::sync::Mutex<Vec<(String, u16)>>,
    }

    impl moruna_storage::net::Network for Refusing {
        fn open(
            &self,
            host: &str,
            port: u16,
        ) -> std::io::Result<Box<dyn moruna_storage::net::NetSocket>> {
            self.asked
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((host.to_string(), port));
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("{host}:{port} is not in this job's network"),
            ))
        }
    }

    fn node_layer(endpoint: Option<&str>) -> ObjectLayer {
        ObjectLayer::new(
            ObjectStoreConfig {
                s3: Some(S3Config {
                    bucket: None,
                    endpoint: endpoint.map(String::from),
                    region: Some("us-east-1".into()),
                    access_key_id: Some("AKIA-NODE-TEST".into()),
                    secret_access_key: Some("node-test-secret-value".into()),
                    ..S3Config::default()
                }),
                azure: Some(AzureConfig {
                    account: Some("acct".into()),
                    access_key: Some("a2V5".into()),
                    ..AzureConfig::default()
                }),
                gcs: Some(GcsConfig::default()),
                ..ObjectStoreConfig::default()
            },
            None,
        )
    }

    #[test]
    fn on_a_node_s3_and_azure_go_through_the_job_s_network_and_nothing_else_does() {
        let network = Arc::new(Refusing::default());
        let connector = NetConnector::new(network.clone());
        let l = node_layer(Some("https://objects.example.test"));
        // S3: built over the network, and its requests go there, by the endpoint's name.
        let (s3, path) = l
            .resolve_through("s3://bucket/data/a.bin", true, Some(connector.clone()))
            .expect("an S3 client over the job's network");
        let refused = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(s3.head(&path))
            .unwrap_err()
            .to_string();
        assert!(refused.contains("not in this job's network"), "{refused}");
        assert_eq!(
            network.asked.lock().unwrap().as_slice(),
            [("objects.example.test".to_string(), 443)]
        );
        // Azure Blob too; gs:// is not reached from a node.
        l.resolve_through("az://container/x", true, Some(connector.clone()))
            .expect("an Azure client over the job's network");
        let gcs = l
            .resolve_through("gs://bucket/x", true, Some(connector.clone()))
            .err()
            .unwrap()
            .to_string();
        assert!(gcs.contains("S3 and Azure Blob only"), "{gcs}");
        // No network: the job's check named no host.
        let none = l
            .resolve_through("s3://bucket/x", true, None)
            .err()
            .unwrap()
            .to_string();
        assert!(none.contains("its check named no host"), "{none}");
        // A host's socket endpoint is not a node's.
        let socket = node_layer(Some("unix:///run/s3.sock"))
            .resolve_through("s3://bucket/x", true, Some(connector))
            .err()
            .unwrap()
            .to_string();
        assert!(socket.contains("not a socket"), "{socket}");
        // Off a node, nothing changes: no network, clients as before.
        l.resolve_through("gs://bucket/x", false, None)
            .expect("a host's own GCS client");
    }

    #[test]
    fn a_client_is_built_once_per_scheme_and_container() {
        let l = layer();
        let (first, _) = l.resolve("s3://bucket/a").expect("resolve");
        let (second, _) = l.resolve("s3://bucket/b").expect("resolve");
        assert!(Arc::ptr_eq(&first, &second), "the client is cached (f.3)");
        let (other, _) = l.resolve("s3://elsewhere/a").expect("resolve");
        assert!(!Arc::ptr_eq(&first, &other));
    }

    #[test]
    fn a_local_root_makes_file_urls_relative() {
        let l = ObjectLayer::new(
            ObjectStoreConfig {
                local_root: Some(std::env::temp_dir()),
                ..ObjectStoreConfig::default()
            },
            None,
        );
        let p = l.parse("file://sub/x").expect("parse");
        assert_eq!(p.path.to_string(), "sub/x");
        l.resolve("file://sub/x").expect("a local client builds");
    }

    #[test]
    fn transport_failures_are_the_ones_worth_one_more_try() {
        let reset = object_store::Error::Generic {
            store: "s3",
            source: "connection reset by peer".into(),
        };
        assert!(is_transport_error(&reset));
        let forbidden = object_store::Error::Generic {
            store: "s3",
            source: "403 forbidden".into(),
        };
        assert!(!is_transport_error(&forbidden));
        assert!(matches!(
            io("read_object", "s3://b/k", &forbidden),
            MorunaError::Io {
                op: "read_object",
                ..
            }
        ));
    }

    /// The requests and parts a budget's room gives (3A, 3.5b): the room over S3's least part,
    /// at least one and at most the sockets, each request a share of the room no smaller than
    /// S3's least part.
    #[test]
    fn a_budgets_room_sets_the_requests_and_the_part() {
        const MIB: u64 = 1 << 20;
        // No room: one request, in parts of S3's least size.
        let none = ObjectIo::for_room(0, 64);
        assert_eq!((none.requests, none.part_bytes), (1, S3_PART_MIN_BYTES));
        // Less than a part's room: still one request of a whole part.
        let thin = ObjectIo::for_room(3 * MIB, 64);
        assert_eq!((thin.requests, thin.part_bytes), (1, S3_PART_MIN_BYTES));
        // Twenty parts' room and a little: twenty requests, each its share.
        let room = 20 * S3_PART_MIN_BYTES + 40;
        let some = ObjectIo::for_room(room, 64);
        assert_eq!((some.requests, some.part_bytes), (20, room / 20));
        // More room than the sockets: the sockets bound N, and the parts grow with the room.
        let wide = ObjectIo::for_room(4096 * MIB, 64);
        assert_eq!((wide.requests, wide.part_bytes), (64, 64 * MIB));
        // A socket budget of none is read as one.
        assert_eq!(ObjectIo::for_room(4096 * MIB, 0).requests, 1);
    }

    /// An object's part: the share, raised to fit S3's most parts, capped at the object; one
    /// part is one `put`, and at most N parts are in flight.
    #[test]
    fn an_objects_part_fits_s3s_limits_and_the_object() {
        const MIB: u64 = 1 << 20;
        let io = ObjectIo {
            requests: 4,
            part_bytes: 8 * MIB,
        };
        // Smaller than a part: one put of the whole object.
        assert_eq!(io.part_for(3 * MIB), 3 * MIB);
        assert!(io.single_put(3 * MIB));
        assert!(io.single_put(8 * MIB));
        assert_eq!(io.parts_in_flight(3 * MIB), 1);
        // Larger: parts of the share, at most N of them in flight.
        assert!(!io.single_put(8 * MIB + 1));
        assert_eq!(io.part_for(100 * MIB), 8 * MIB);
        assert_eq!(io.parts_in_flight(100 * MIB), 4);
        assert_eq!(io.parts_in_flight(17 * MIB), 3);
        // So large that the share would pass S3's most parts: the part grows to fit them.
        let huge = 200_000 * MIB;
        assert_eq!(io.part_for(huge), huge.div_ceil(S3_PARTS_MAX));
        assert!(huge.div_ceil(io.part_for(huge)) <= S3_PARTS_MAX);
        // An empty object is one put of nothing.
        assert!(io.single_put(0));
        assert_eq!(io.parts_in_flight(0), 1);
    }
}

#[cfg(test)]
mod store_tests {
    use super::*;

    fn store() -> StoreBackend {
        let memory = Arc::new(object_store::memory::InMemory::new());
        StoreBackend::new(
            Arc::clone(&memory) as Arc<dyn ObjectStore>,
            Some(memory as Arc<dyn object_store::multipart::MultipartStore>),
        )
    }

    #[test]
    fn native_object_policy_refuses_os_sockets_but_preserves_file_and_injected_backends() {
        let dir = crate::fdcache::tests::scratch_dir("native-object-policy");
        let layer = ObjectLayer::new(
            ObjectStoreConfig {
                local_root: Some(dir.clone()),
                ..Default::default()
            },
            None,
        );
        let error = layer
            .resolve_for("s3://bucket/key", true)
            .err()
            .expect("native socket path refused before building a client");
        assert!(matches!(
            error,
            MorunaError::Config {
                name: "object_store",
                ..
            }
        ));
        assert!(
            layer.clients.lock().unwrap().is_empty(),
            "no remote client or I/O future was created"
        );
        let host_error = layer
            .resolve_for("s3://bucket/key", false)
            .err()
            .expect("host still requires its normal credentials");
        assert!(host_error.to_string().contains("no s3 configuration"));
        let (file, path) = layer
            .resolve_for("file://actual", true)
            .expect("native local file backend");
        let injected: Arc<dyn ObjectBackend> = Arc::new(store());
        let hooked = ObjectLayer::new(ObjectStoreConfig::default(), Some(injected.clone()));
        let (kept, key) = hooked
            .resolve_for("s3://bucket/key", true)
            .expect("explicit backend retains its own transport");
        assert!(Arc::ptr_eq(&kept, &injected));
        let runtime =
            crate::configure_runtime_for(&mut tokio::runtime::Builder::new_current_thread(), true)
                .build()
                .unwrap();
        runtime.block_on(async {
            file.put(
                &path,
                PutPayload::from_bytes(Bytes::from_static(b"file bytes")),
            )
            .await
            .unwrap();
            assert_eq!(
                &file.get_range(&path, 0..10).await.unwrap()[..],
                b"file bytes"
            );
            kept.put(
                &key,
                PutPayload::from_bytes(Bytes::from_static(b"backend bytes")),
            )
            .await
            .unwrap();
            assert_eq!(
                &kept.get_range(&key, 0..13).await.unwrap()[..],
                b"backend bytes"
            );
        });
        drop(runtime);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The local filesystem has no multipart uploads, so the seam hands back no future for one
    /// and `abort_multipart` is `Unsupported` rather than a silent success.
    #[test]
    fn a_local_store_has_no_multipart_to_abort() {
        let local: Arc<dyn ObjectStore> = Arc::new(object_store::local::LocalFileSystem::new());
        let backend = StoreBackend::new(local, None);
        assert!(backend.abort_by_id(&OsPath::from("x"), "1").is_none());
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    #[test]
    fn a_multipart_upload_runs_through_the_store_and_refuses_a_second_finish() {
        let rt = rt();
        rt.block_on(async {
            let backend = store();
            let path = OsPath::from("multi");
            let mut upload = backend.put_multipart(&path).await.expect("start");
            upload
                .part(PutPayload::from_bytes(Bytes::from(vec![1u8; 8])))
                .await
                .expect("part");
            upload.complete().await.expect("complete");
            assert!(
                upload.complete().await.is_err(),
                "a finished upload refuses a second complete"
            );
            assert!(upload.abort().await.is_err());
            assert!(
                upload
                    .part(PutPayload::from_bytes(Bytes::from(vec![2u8; 8])))
                    .await
                    .is_err()
            );
            let got = backend
                .get_range(&path, 0..8)
                .await
                .expect("the object is there");
            assert_eq!(&got[..], &[1u8; 8]);
            let mut aborted = backend.put_multipart(&path).await.expect("start");
            aborted.abort().await.expect("abort");
        });
    }

    #[test]
    fn head_and_one_level_listing_answer_from_the_store() {
        let rt = rt();
        rt.block_on(async {
            let backend = store();
            backend
                .put(
                    &OsPath::from("dir/a"),
                    PutPayload::from_bytes(Bytes::from(vec![9u8; 4])),
                )
                .await
                .expect("put");
            let meta = backend.head(&OsPath::from("dir/a")).await.expect("head");
            assert_eq!(meta.size, 4);
            assert_eq!(meta.url, "dir/a");
            let (objects, prefixes) = backend
                .list_one_level(Some(OsPath::from("dir")))
                .await
                .expect("list");
            assert_eq!(objects.len(), 1);
            assert!(prefixes.is_empty());
        });
    }

    #[test]
    fn every_backend_builds_from_its_own_configuration() {
        let layer = ObjectLayer::new(
            ObjectStoreConfig {
                s3: Some(S3Config {
                    bucket: Some("b".into()),
                    endpoint: Some("http://127.0.0.1:9000".into()),
                    region: Some("us-east-1".into()),
                    access_key_id: Some("k".into()),
                    secret_access_key: Some("s".into()),
                    session_token: Some("t".into()),
                    virtual_hosted_style_request: false,
                }),
                gcs: Some(GcsConfig {
                    service_account_json: Some(
                        r#"{"gcs_base_url":"http://localhost","disable_oauth":true,"client_email":"","private_key":"","private_key_id":""}"#
                            .into(),
                    ),
                    service_account_path: None,
                    bucket: None,
                }),
                azure: Some(AzureConfig {
                    account: Some("account".into()),
                    access_key: Some("a2V5".into()),
                    container: None,
                    sas_token: None,
                }),
                local_root: None,
                allow_http: true,
                secret_names: Default::default(),
            },
            None,
        );
        layer.resolve("s3://b/k").expect("s3");
        layer.resolve("gs://g/k").expect("gcs");
        layer.resolve("az://c/k").expect("azure");
        layer.resolve("file:///tmp/x").expect("local, absolute");
        let bucket_only = layer.parse("s3://only-a-bucket").expect("no key");
        assert_eq!(bucket_only.container, "only-a-bucket");
        assert_eq!(bucket_only.path.to_string(), "");
        let bad = ObjectLayer::new(
            ObjectStoreConfig {
                gcs: Some(GcsConfig {
                    service_account_path: Some("/does/not/exist".into()),
                    service_account_json: None,
                    bucket: None,
                }),
                ..ObjectStoreConfig::default()
            },
            None,
        );
        assert!(matches!(
            bad.resolve("gs://g/k"),
            Err(MorunaError::Config { .. })
        ));
    }
}

/// S3 and Azure Blob over the job's network with their credentials from the job's secrets
/// (item 3.5): each request goes to its store's own host at 443, and a store that refuses the
/// credentials, even one that says them back, fails with an error naming the secrets, never
/// carrying their values.
#[cfg(test)]
mod secrets_tests {
    use super::*;
    use std::io;
    use std::sync::Condvar;

    /// What each test's credentials are; none may be in any error.
    const KEY_ID: &str = "AKIA-TEST-35-0001";
    const SECRET: &str = "wJalrXUtnFEMI-TEST-35-SECRET";
    const ACCOUNT_KEY: &str = "dGVzdC1hY2NvdW50LWtleS0zNQ==";
    const SIG: &str = "c2lnLTM1LXRlc3Q%3D";

    /// One request as it reached the network: the host and port it was opened to, and its
    /// head (request line and headers).
    #[derive(Clone, Debug)]
    struct Seen {
        host: String,
        port: u16,
        head: String,
    }

    /// A network whose every connection answers its first request with `403 Forbidden` and a
    /// body that says back every credential, as a careless server might.
    #[derive(Debug, Default)]
    struct Forbidding {
        seen: Arc<Mutex<Vec<Seen>>>,
    }

    impl moruna_storage::net::Network for Forbidding {
        fn open(
            &self,
            host: &str,
            port: u16,
        ) -> io::Result<Box<dyn moruna_storage::net::NetSocket>> {
            Ok(Box::new(Answering {
                host: host.to_string(),
                port,
                seen: Arc::clone(&self.seen),
                state: Mutex::default(),
                ready: Condvar::new(),
            }))
        }
    }

    #[derive(Default)]
    struct Exchange {
        request: Vec<u8>,
        answer: Option<Vec<u8>>,
        closed: bool,
    }

    struct Answering {
        host: String,
        port: u16,
        seen: Arc<Mutex<Vec<Seen>>>,
        state: Mutex<Exchange>,
        ready: Condvar,
    }

    impl moruna_storage::net::NetSocket for Answering {
        fn start_tls(&self, server_name: &str) -> io::Result<()> {
            assert_eq!(server_name, self.host, "TLS for the host connected to");
            Ok(())
        }

        fn send(&self, bytes: &[u8]) -> io::Result<usize> {
            let mut state = self.state.lock().unwrap();
            state.request.extend_from_slice(bytes);
            if state.answer.is_none()
                && let Some(end) = state.request.windows(4).position(|w| w == b"\r\n\r\n")
            {
                let head = String::from_utf8_lossy(&state.request[..end]).into_owned();
                self.seen.lock().unwrap().push(Seen {
                    host: self.host.clone(),
                    port: self.port,
                    head,
                });
                let body = format!(
                    "<Error><Code>AccessDenied</Code><AWSAccessKeyId>{KEY_ID}</AWSAccessKeyId>\
                     <Echo>{SECRET} {ACCOUNT_KEY} {SIG}</Echo></Error>"
                );
                state.answer = Some(
                    format!(
                        "HTTP/1.1 403 Forbidden\r\nContent-Type: application/xml\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .into_bytes(),
                );
                self.ready.notify_all();
            }
            Ok(bytes.len())
        }

        fn receive(&self, buffer: &mut [u8]) -> io::Result<usize> {
            let mut state = self.state.lock().unwrap();
            loop {
                if let Some(answer) = state.answer.as_mut()
                    && !answer.is_empty()
                {
                    let n = answer.len().min(buffer.len());
                    buffer[..n].copy_from_slice(&answer[..n]);
                    answer.drain(..n);
                    return Ok(n);
                }
                if state.closed || state.answer.is_some() {
                    return Ok(0);
                }
                state = self.ready.wait(state).unwrap();
            }
        }

        fn close(&self) {
            self.state.lock().unwrap().closed = true;
            self.ready.notify_all();
        }
    }

    fn layer(s3: Option<S3Config>, azure: Option<AzureConfig>) -> ObjectLayer {
        let mut cfg = ObjectStoreConfig {
            s3,
            azure,
            ..ObjectStoreConfig::default()
        };
        for (field, name) in [
            ("object_store.s3.access_key_id", "s3-key"),
            ("object_store.s3.secret_access_key", "s3-secret"),
            ("object_store.azure.access_key", "blob-key"),
            ("object_store.azure.sas_token", "blob-sas"),
        ] {
            cfg.secret_names.insert(field.into(), name.into());
        }
        ObjectLayer::new(cfg, None)
    }

    fn s3(endpoint: &str, virtual_host: bool) -> Option<S3Config> {
        Some(S3Config {
            endpoint: Some(endpoint.into()),
            region: Some("us-east-1".into()),
            access_key_id: Some(KEY_ID.into()),
            secret_access_key: Some(SECRET.into()),
            virtual_hosted_style_request: virtual_host,
            ..S3Config::default()
        })
    }

    /// A range of `url` read through the job's network: the error as the reactor reports it,
    /// and what reached the network.
    fn read(l: &ObjectLayer, url: &str) -> (object_store::Error, MorunaError, Vec<Seen>) {
        let network = Arc::new(Forbidding::default());
        let connector = NetConnector::new(network.clone());
        let (backend, path) = l
            .resolve_through(url, true, Some(connector))
            .expect("a client over the job's network");
        let error = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(backend.get_range(&path, 0..16))
            .expect_err("the store refuses");
        let reported = io("read_object", url, &error);
        let seen = network.seen.lock().unwrap().clone();
        (error, reported, seen)
    }

    /// None of `values`, the store's own credentials, is in `said`.
    fn assert_clean(said: &str, values: &[&str]) {
        for value in values {
            assert!(!said.contains(value), "a credential in: {said}");
        }
    }

    #[test]
    fn s3_path_style_goes_to_the_endpoints_host_and_a_refusal_names_the_secrets() {
        let l = layer(s3("https://objects.example.test", false), None);
        let (error, reported, seen) = read(&l, "s3://in-bucket/taxi/2024-01.parquet");
        assert!(
            matches!(error, object_store::Error::PermissionDenied { .. }),
            "{error}"
        );
        let said = reported.to_string();
        assert_clean(&said, &[KEY_ID, SECRET]);
        assert!(
            said.contains("secret `s3-key` (object_store.s3.access_key_id)")
                && said.contains("secret `s3-secret` (object_store.s3.secret_access_key)"),
            "{said}"
        );
        assert!(
            said.contains("<secret `s3-key`>") && said.contains("<secret `s3-secret`>"),
            "what the store said back is read again: {said}"
        );
        assert_eq!(seen.len(), 1, "a refusal is not retried: {seen:?}");
        assert_eq!(
            (seen[0].host.as_str(), seen[0].port),
            ("objects.example.test", 443)
        );
        assert!(
            seen[0]
                .head
                .starts_with("GET /in-bucket/taxi/2024-01.parquet HTTP/1.1"),
            "{}",
            seen[0].head
        );
        // Signed with the key ID, which went to the store and nowhere else.
        assert!(seen[0].head.contains(KEY_ID), "{}", seen[0].head);
    }

    #[test]
    fn s3_virtual_host_style_goes_to_the_buckets_own_host() {
        let l = layer(s3("https://in-bucket.objects.example.test", true), None);
        let (_, reported, seen) = read(&l, "s3://in-bucket/taxi/2024-01.parquet");
        assert_clean(&reported.to_string(), &[KEY_ID, SECRET]);
        assert_eq!(
            (seen[0].host.as_str(), seen[0].port),
            ("in-bucket.objects.example.test", 443)
        );
        assert!(
            seen[0]
                .head
                .starts_with("GET /taxi/2024-01.parquet HTTP/1.1"),
            "{}",
            seen[0].head
        );
    }

    #[test]
    fn blob_with_an_account_key_goes_to_the_accounts_host_and_names_the_secret() {
        let azure = AzureConfig {
            account: Some("moruna35".into()),
            access_key: Some(ACCOUNT_KEY.into()),
            ..AzureConfig::default()
        };
        let l = layer(None, Some(azure));
        let (_, reported, seen) = read(&l, "az://out/score/part-0.parquet");
        let said = reported.to_string();
        assert_clean(&said, &[ACCOUNT_KEY]);
        assert!(
            said.contains("secret `blob-key` (object_store.azure.access_key)"),
            "{said}"
        );
        assert_eq!(
            (seen[0].host.as_str(), seen[0].port),
            ("moruna35.blob.core.windows.net", 443)
        );
        assert!(
            seen[0].head.starts_with("GET /out/score/part-0.parquet"),
            "{}",
            seen[0].head
        );
        assert!(
            seen[0]
                .head
                .to_ascii_lowercase()
                .contains("authorization: sharedkey moruna35:"),
            "{}",
            seen[0].head
        );
    }

    #[test]
    fn blob_with_a_sas_token_signs_with_it_and_names_the_secret() {
        let azure = AzureConfig {
            account: Some("moruna35".into()),
            sas_token: Some(format!("?sv=2024-11-04&ss=b&srt=co&sp=rl&sig={SIG}")),
            ..AzureConfig::default()
        };
        let l = layer(None, Some(azure));
        let (_, reported, seen) = read(&l, "az://out/score/part-0.parquet");
        let said = reported.to_string();
        assert_clean(&said, &[SIG, "c2lnLTM1LXRlc3Q="]);
        assert!(
            said.contains("secret `blob-sas` (object_store.azure.sas_token)"),
            "{said}"
        );
        assert_eq!(seen[0].host, "moruna35.blob.core.windows.net");
        assert!(seen[0].head.contains("sig="), "{}", seen[0].head);
        assert!(
            !seen[0]
                .head
                .to_ascii_lowercase()
                .contains("authorization: sharedkey"),
            "{}",
            seen[0].head
        );
    }

    #[test]
    fn a_build_error_never_carries_a_credential() {
        // A key that is not base64 does not build; its error names no value.
        let azure = AzureConfig {
            account: Some("moruna35".into()),
            access_key: Some("not base64! NOT-A-KEY-35".into()),
            ..AzureConfig::default()
        };
        let l = layer(None, Some(azure));
        let refused = l
            .resolve_through(
                "az://out/x",
                true,
                Some(NetConnector::new(Arc::new(Forbidding::default()))),
            )
            .err()
            .expect("an access key that is not base64 is refused");
        assert!(!refused.to_string().contains("NOT-A-KEY-35"), "{refused}");
        // `Debug` of the configuration says whether each credential is set, never what it is.
        let shown = format!("{:?}", l.cfg);
        assert!(!shown.contains("NOT-A-KEY-35"), "{shown}");
        assert!(shown.contains("Some(<redacted>)"), "{shown}");
        let shown = format!("{:?}", s3("https://h.test", false).unwrap());
        assert!(
            !shown.contains(KEY_ID) && !shown.contains(SECRET),
            "{shown}"
        );
        let shown = format!(
            "{:?}",
            GcsConfig {
                service_account_json: Some("{\"k\":\"GCS-KEY-35\"}".into()),
                ..GcsConfig::default()
            }
        );
        assert!(!shown.contains("GCS-KEY-35"), "{shown}");
    }
}
