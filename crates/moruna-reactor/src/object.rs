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

use std::collections::HashMap;
use std::future::Future;
use std::ops::Range;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use moruna_kernel::{MorunaError, ObjectMeta, Result};
use object_store::path::Path as OsPath;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

use crate::{AzureConfig, GcsConfig, ObjectStoreConfig, S3Config};

/// The part size of a multipart write (f.4).
pub(crate) const PART_BYTES: usize = 16 * 1024 * 1024;
/// Writes at or below this size are a single `put` (f.4).
pub(crate) const SINGLE_PUT_MAX: usize = 64 * 1024 * 1024;
/// Parts in flight during a multipart write (f.4).
pub(crate) const PARTS_IN_FLIGHT: usize = 4;

/// A boxed future, so the seam below is a trait object without an async-trait macro.
pub(crate) type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// What the reactor asks of an object store (d.2: `get_range`, `put`, multipart, `head`,
/// `list`).
pub(crate) trait ObjectBackend: Send + Sync {
    fn get_range(&self, path: &OsPath, range: Range<u64>) -> BoxFut<object_store::Result<Bytes>>;
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
}

impl StoreBackend {
    pub(crate) fn new(
        store: Arc<dyn ObjectStore>,
        multipart: Option<Arc<dyn object_store::multipart::MultipartStore>>,
    ) -> StoreBackend {
        StoreBackend { store, multipart }
    }
}

impl ObjectBackend for StoreBackend {
    fn get_range(&self, path: &OsPath, range: Range<u64>) -> BoxFut<object_store::Result<Bytes>> {
        let (store, path) = (Arc::clone(&self.store), path.clone());
        Box::pin(async move { store.get_range(&path, range).await })
    }

    fn put(&self, path: &OsPath, payload: PutPayload) -> BoxFut<object_store::Result<()>> {
        let (store, path) = (Arc::clone(&self.store), path.clone());
        Box::pin(async move { store.put(&path, payload).await.map(|_| ()) })
    }

    fn put_multipart(&self, path: &OsPath) -> BoxFut<object_store::Result<Box<dyn MultipartSink>>> {
        let (store, path) = (Arc::clone(&self.store), path.clone());
        Box::pin(async move {
            let upload = store.put_multipart(&path).await?;
            Ok(Box::new(StoreUpload {
                upload: Some(upload),
            }) as Box<dyn MultipartSink>)
        })
    }

    fn head(&self, path: &OsPath) -> BoxFut<object_store::Result<ObjectMeta>> {
        let (store, path) = (Arc::clone(&self.store), path.clone());
        Box::pin(async move { store.head(&path).await.map(|m| meta(&path, &m)) })
    }

    fn list_one_level(
        &self,
        prefix: Option<OsPath>,
    ) -> BoxFut<object_store::Result<(Vec<ObjectMeta>, Vec<OsPath>)>> {
        let store = Arc::clone(&self.store);
        Box::pin(async move {
            let listed = store.list_with_delimiter(prefix.as_ref()).await?;
            let objects = listed
                .objects
                .iter()
                .map(|m| meta(&m.location, m))
                .collect();
            Ok((objects, listed.common_prefixes))
        })
    }

    fn delete(&self, path: &OsPath) -> BoxFut<object_store::Result<()>> {
        let (store, path) = (Arc::clone(&self.store), path.clone());
        Box::pin(async move { store.delete(&path).await })
    }

    fn abort_by_id(&self, path: &OsPath, id: &str) -> Option<BoxFut<object_store::Result<()>>> {
        let multipart = Arc::clone(self.multipart.as_ref()?);
        let (path, id) = (path.clone(), id.to_string());
        Some(Box::pin(async move {
            multipart.abort_multipart(&path, &id).await
        }))
    }
}

struct StoreUpload {
    /// `None` once the upload has been completed or aborted; a later call on it is the
    /// "already finished" programming error, reported and never a panic.
    upload: Option<Box<dyn object_store::MultipartUpload>>,
}

impl MultipartSink for StoreUpload {
    fn part(&mut self, payload: PutPayload) -> BoxFut<object_store::Result<()>> {
        match &mut self.upload {
            Some(upload) => Box::pin(upload.put_part(payload)),
            None => Box::pin(async { Err(finished()) }),
        }
    }

    fn complete(&mut self) -> BoxFut<object_store::Result<()>> {
        let Some(mut upload) = self.upload.take() else {
            return Box::pin(async { Err(finished()) });
        };
        Box::pin(async move { upload.complete().await.map(|_| ()) })
    }

    fn abort(&mut self) -> BoxFut<object_store::Result<()>> {
        let Some(mut upload) = self.upload.take() else {
            return Box::pin(async { Err(finished()) });
        };
        Box::pin(async move { upload.abort().await })
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
        }
    }

    /// The client for `url` and the path inside it, built once per `(scheme, container)`.
    pub(crate) fn resolve(&self, url: &str) -> Result<(Arc<dyn ObjectBackend>, OsPath)> {
        let located = self.parse(url)?;
        if let Some(backend) = &self.override_backend {
            return Ok((Arc::clone(backend), located.path));
        }
        let mut clients = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(client) = clients.get(&located.client_key) {
            return Ok((Arc::clone(client), located.path));
        }
        let client = self.build(&located)?;
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

    fn build(&self, located: &Located) -> Result<Arc<dyn ObjectBackend>> {
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
                let client = Arc::new(build_s3(s3, &located.container, self.cfg.allow_http)?);
                (Arc::clone(&client) as Arc<dyn ObjectStore>, Some(client))
            }
            Scheme::Gcs => {
                let gcs = self
                    .cfg
                    .gcs
                    .as_ref()
                    .ok_or_else(|| config("no gcs configuration"))?;
                let client = Arc::new(build_gcs(gcs, &located.container)?);
                (Arc::clone(&client) as Arc<dyn ObjectStore>, Some(client))
            }
            Scheme::Azure => {
                let azure = self
                    .cfg
                    .azure
                    .as_ref()
                    .ok_or_else(|| config("no azure configuration"))?;
                let client = Arc::new(build_azure(azure, &located.container, self.cfg.allow_http)?);
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
        Ok(Arc::new(StoreBackend::new(store, multipart)))
    }
}

fn build_s3(cfg: &S3Config, bucket: &str, allow_http: bool) -> Result<object_store::aws::AmazonS3> {
    let mut b = object_store::aws::AmazonS3Builder::from_env()
        .with_bucket_name(bucket)
        .with_allow_http(allow_http);
    if let Some(v) = &cfg.endpoint {
        // MH 4.6: a `unix://` or `vsock://` endpoint is a socket the host proxies; the client
        // speaks plain HTTP/1.1 over it, path-style, whatever `allow_http` says, because the
        // socket never leaves the machine.
        match crate::socket::parse_endpoint(v)? {
            Some(target) => {
                b = b
                    .with_endpoint(crate::socket::SOCKET_ENDPOINT)
                    .with_allow_http(true)
                    .with_virtual_hosted_style_request(false)
                    .with_http_connector(crate::socket::SocketConnector::new(target));
            }
            None => b = b.with_endpoint(v),
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

fn build_azure(
    cfg: &AzureConfig,
    container: &str,
    allow_http: bool,
) -> Result<object_store::azure::MicrosoftAzure> {
    let mut b = object_store::azure::MicrosoftAzureBuilder::from_env()
        .with_container_name(container)
        .with_allow_http(allow_http);
    if let Some(v) = &cfg.account {
        b = b.with_account(v);
    }
    if let Some(v) = &cfg.access_key {
        b = b.with_access_key(v);
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

/// `write_object` (f.4): one `put` up to 64 MiB, multipart with 16 MiB parts above it, at most
/// four parts in flight, and an abort of the upload on any failure.
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
    if len <= SINGLE_PUT_MAX {
        return backend
            .put(&path, payload_of(src.slice(0, len)))
            .await
            .map_err(|e| io(crate::stats::OpKind::WriteObject.as_str(), url, &e));
    }
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
        let n = PART_BYTES.min(len - offset);
        let part = upload.part(payload_of(src.slice(offset, n)));
        in_flight.push_back(tokio::spawn(part));
        offset += n;
        if in_flight.len() >= PARTS_IN_FLIGHT {
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
                    session_token: None,
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
                }),
                local_root: None,
                allow_http: true,
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
