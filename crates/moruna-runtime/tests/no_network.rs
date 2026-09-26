//! H7 (MH 4.6): a run whose object store is reachable only through a Unix socket completes.
//!
//! The test binds a small S3-compatible server to a Unix socket (the operations a Parquet
//! source and sink use: list, head, ranged get, put, delete), puts a Parquet file in it, and
//! runs a job whose source and sink are `s3://` URLs and whose S3 endpoint is
//! `unix://<socket>`. Nothing listens on TCP. On the reference host the same run is repeated
//! inside a network namespace with no interface but `lo`, down (`unshare -rn`), which is the
//! microVM guest's situation (MH 2.1).

#![allow(clippy::result_large_err)]

mod support;

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::{Request, Response, StatusCode};
use moruna_kernel::{CancelToken, Kernel};
use moruna_reactor::{ObjectStoreConfig, S3Config};
use moruna_runtime::{RunSpec, Runtime, SinkSpec, SourceSpec};
use support::{Doubler, Scratch, one_run_at_a_time, write_parquet};

const ROWS: u64 = 50_000;
/// Set in the child that the reference-host variant runs inside `unshare -rn`.
const IN_NAMESPACE: &str = "MORUNA_H7_IN_NAMESPACE";

/// What the server holds, by `bucket/key`, and how many requests it answered.
#[derive(Default)]
struct Store {
    objects: Mutex<BTreeMap<String, Bytes>>,
    requests: AtomicU64,
}

const LAST_MODIFIED_HTTP: &str = "Sat, 26 Sep 2026 00:00:00 GMT";
const LAST_MODIFIED_XML: &str = "2026-09-26T00:00:00.000Z";

fn etag(bytes: &Bytes) -> String {
    format!(
        "\"{:x}-{}\"",
        bytes.len(),
        bytes.first().copied().unwrap_or(0)
    )
}

fn decode(text: &str) -> String {
    let mut out = Vec::new();
    let raw = text.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            b'%' if i + 2 < raw.len() => {
                let hex = std::str::from_utf8(&raw[i + 1..i + 3]).unwrap_or("00");
                out.push(u8::from_str_radix(hex, 16).unwrap_or(b'?'));
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query(req: &Request<hyper::body::Incoming>) -> BTreeMap<String, String> {
    req.uri()
        .query()
        .unwrap_or_default()
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (decode(k), decode(v)),
            None => (decode(pair), String::new()),
        })
        .collect()
}

fn reply(status: StatusCode, body: Bytes) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(body));
    *response.status_mut() = status;
    response
}

fn list(store: &Store, bucket: &str, prefix: &str, delimiter: Option<&str>) -> String {
    let objects = store.objects.lock().unwrap_or_else(|e| e.into_inner());
    let mut contents = String::new();
    let mut prefixes = std::collections::BTreeSet::new();
    let mut count = 0;
    for (name, bytes) in objects.iter() {
        let Some(key) = name.strip_prefix(&format!("{bucket}/")) else {
            continue;
        };
        let Some(rest) = key.strip_prefix(prefix) else {
            continue;
        };
        if let Some(delimiter) = delimiter
            && let Some(at) = rest.find(delimiter)
        {
            prefixes.insert(format!("{prefix}{}", &rest[..at + delimiter.len()]));
            continue;
        }
        count += 1;
        contents.push_str(&format!(
            "<Contents><Key>{key}</Key><LastModified>{LAST_MODIFIED_XML}</LastModified>\
             <ETag>{}</ETag><Size>{}</Size><StorageClass>STANDARD</StorageClass></Contents>",
            etag(bytes).replace('"', "&quot;"),
            bytes.len()
        ));
    }
    let common: String = prefixes
        .iter()
        .map(|p| format!("<CommonPrefixes><Prefix>{p}</Prefix></CommonPrefixes>"))
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
         <Name>{bucket}</Name><Prefix>{prefix}</Prefix><KeyCount>{count}</KeyCount>\
         <MaxKeys>1000</MaxKeys><IsTruncated>false</IsTruncated>{contents}{common}\
         </ListBucketResult>"
    )
}

async fn handle(
    store: Arc<Store>,
    req: Request<hyper::body::Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    store.requests.fetch_add(1, Ordering::Relaxed);
    let path = decode(req.uri().path().trim_start_matches('/'));
    let params = query(&req);
    let (bucket, key) = match path.split_once('/') {
        Some((bucket, key)) => (bucket.to_string(), key.to_string()),
        None => (path.clone(), String::new()),
    };
    let method = req.method().clone();
    let range = req
        .headers()
        .get(hyper::header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = match req.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => return Ok(reply(StatusCode::BAD_REQUEST, Bytes::new())),
    };
    let name = format!("{bucket}/{key}");
    let response = match (method.as_str(), key.is_empty()) {
        ("GET", true) => {
            let xml = list(
                &store,
                &bucket,
                params.get("prefix").map(String::as_str).unwrap_or(""),
                params.get("delimiter").map(String::as_str),
            );
            let mut r = reply(StatusCode::OK, Bytes::from(xml));
            r.headers_mut()
                .insert("content-type", "application/xml".parse().expect("header"));
            r
        }
        ("PUT", false) => {
            let tag = etag(&body);
            store
                .objects
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(name, body);
            let mut r = reply(StatusCode::OK, Bytes::new());
            r.headers_mut().insert("etag", tag.parse().expect("header"));
            r
        }
        ("DELETE", false) => {
            store
                .objects
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&name);
            reply(StatusCode::NO_CONTENT, Bytes::new())
        }
        ("GET" | "HEAD", false) => {
            let found = store
                .objects
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&name)
                .cloned();
            match found {
                None => reply(StatusCode::NOT_FOUND, Bytes::new()),
                Some(bytes) => {
                    let size = bytes.len() as u64;
                    let tag = etag(&bytes);
                    let (status, part, content_range) = match range
                        .as_deref()
                        .and_then(|r| r.strip_prefix("bytes="))
                        .and_then(|r| r.split_once('-'))
                    {
                        Some((start, end)) => {
                            let start: u64 = start.parse().unwrap_or(0);
                            let end: u64 = end.parse().unwrap_or(size - 1).min(size - 1);
                            (
                                StatusCode::PARTIAL_CONTENT,
                                bytes.slice(start as usize..=end as usize),
                                Some(format!("bytes {start}-{end}/{size}")),
                            )
                        }
                        None => (StatusCode::OK, bytes.clone(), None),
                    };
                    let length = part.len();
                    let mut r = if method == hyper::Method::HEAD {
                        reply(status, Bytes::new())
                    } else {
                        reply(status, part)
                    };
                    let headers = r.headers_mut();
                    headers.insert("etag", tag.parse().expect("header"));
                    headers.insert("last-modified", LAST_MODIFIED_HTTP.parse().expect("header"));
                    headers.insert(
                        "content-length",
                        length.to_string().parse().expect("header"),
                    );
                    if let Some(content_range) = content_range {
                        headers.insert("content-range", content_range.parse().expect("header"));
                    }
                    r
                }
            }
        }
        _ => reply(StatusCode::NOT_IMPLEMENTED, Bytes::new()),
    };
    Ok(response)
}

/// The server: a thread with its own runtime, accepting on the socket until the test ends.
struct Server {
    store: Arc<Store>,
    socket: PathBuf,
    _runtime: tokio::runtime::Runtime,
}

impl Server {
    fn start(socket: &Path) -> Server {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("a runtime for the server");
        let store = Arc::new(Store::default());
        let _ = std::fs::remove_file(socket);
        let listener = {
            let _entered = runtime.enter();
            tokio::net::UnixListener::bind(socket).expect("bind the socket")
        };
        let serving = store.clone();
        runtime.spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let store = serving.clone();
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |req| handle(store.clone(), req));
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        Server {
            store,
            socket: socket.to_path_buf(),
            _runtime: runtime,
        }
    }

    fn put(&self, name: &str, bytes: Vec<u8>) {
        self.store
            .objects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_string(), Bytes::from(bytes));
    }

    fn objects(&self) -> BTreeMap<String, Bytes> {
        self.store
            .objects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// Where the socket goes. A Unix socket path is limited to about a hundred bytes, which a
/// target directory in a deep worktree can exceed, so the socket lives in the system's temp
/// directory under a short name.
fn socket_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("m-{label}-{}.sock", std::process::id()))
}

/// The job: every row of `s3://bucket/in/` doubled into `s3://bucket/out`, the object store
/// behind `unix://<socket>`.
fn run_through(server: &Server, scratch: &Scratch) -> moruna_runtime::RunReport {
    let kernels: Vec<Arc<dyn Kernel>> = vec![Arc::new(Doubler::new())];
    let mut spec = RunSpec::new(
        SourceSpec::Build(Box::new(|ctx| {
            moruna_sources::ParquetSource::with_allocator(
                moruna_sources::ParquetSourceConfig {
                    urls: vec!["s3://bucket/in/".to_string()],
                    ..Default::default()
                },
                ctx.reactor.clone(),
                ctx.object_metadata()?,
                ctx.alloc.clone(),
            )
            .map(|source| Arc::new(source) as Arc<dyn moruna_kernel::Source>)
        })),
        kernels,
        SinkSpec::Build(Box::new(|ctx| {
            moruna_sinks::ParquetSink::new(
                moruna_sinks::ParquetSinkConfig {
                    url: "s3://bucket/out".to_string(),
                    row_group_bytes: 1 << 20,
                    file_bytes: 4 << 20,
                    ..Default::default()
                },
                ctx.reactor.clone(),
                ctx.alloc.clone(),
            )
            .map(|sink| Box::new(sink.with_run_id(ctx.run_id)) as Box<dyn moruna_kernel::Sink>)
        })),
    );
    spec.budget = Some(512 << 20);
    spec.cpu = Some(2.0);
    spec.staging_dir = Some(scratch.path().join("staging"));
    spec.profiles_dir = Some(scratch.path().join("profiles"));
    spec.object_store = ObjectStoreConfig {
        s3: Some(S3Config {
            endpoint: Some(format!("unix://{}", server.socket.display())),
            region: Some("us-east-1".into()),
            access_key_id: Some("moruna".into()),
            secret_access_key: Some("moruna-secret".into()),
            ..S3Config::default()
        }),
        ..ObjectStoreConfig::default()
    };
    std::fs::create_dir_all(scratch.path().join("staging")).expect("staging");
    match Runtime::run(spec, CancelToken::new()) {
        Ok(report) => report,
        Err(error) => panic!("the run through the socket did not complete: {error}"),
    }
}

fn h7(label: &str) {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new(label);
    let input = scratch.path().join("in.parquet");
    write_parquet(&input, ROWS, 5);
    let server = Server::start(&socket_path(label));
    server.put(
        "bucket/in/data.parquet",
        std::fs::read(&input).expect("the input"),
    );

    let report = run_through(&server, &scratch);
    assert_eq!(report.exit, moruna_runtime::ExitReason::Completed);
    assert!(
        server.store.requests.load(Ordering::Relaxed) > 5,
        "the run listed, sized, read and wrote through the socket"
    );

    let objects = server.objects();
    assert!(
        objects.keys().any(|k| k.starts_with("bucket/out/part-")),
        "the sink's files are in the store: {:?}",
        objects.keys()
    );
    let mut rows = 0u64;
    let mut doubled = true;
    for (name, bytes) in objects
        .iter()
        .filter(|(n, _)| n.starts_with("bucket/out/") && n.ends_with(".parquet"))
    {
        let reader =
            parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(bytes.clone())
                .unwrap_or_else(|e| panic!("{name} is Parquet: {e}"))
                .build()
                .expect("a batch reader");
        for batch in reader {
            let batch = batch.expect("a batch");
            rows += batch.num_rows() as u64;
            let ids = batch
                .column_by_name("id")
                .and_then(|c| {
                    c.as_any()
                        .downcast_ref::<arrow::array::Int64Array>()
                        .cloned()
                })
                .expect("id");
            let values = batch
                .column_by_name("value")
                .and_then(|c| {
                    c.as_any()
                        .downcast_ref::<arrow::array::Int64Array>()
                        .cloned()
                })
                .expect("value");
            doubled &= (0..batch.num_rows()).all(|i| values.value(i) == ids.value(i) * 4);
        }
    }
    assert_eq!(rows, ROWS, "every row reached the sink through the socket");
    assert!(doubled, "and was doubled on the way");
    let _ = std::fs::remove_file(&server.socket);
}

/// H7 on any host: the object store is a Unix socket and nothing else.
#[test]
fn h7_parquet_through_a_unix_socket() {
    if std::env::var(IN_NAMESPACE).is_ok() {
        // The reference-host variant's child: the same run, inside the namespace.
        h7("h7-ns");
        return;
    }
    h7("h7");
}

/// H7 on the reference host: the same run inside a network namespace with no usable interface,
/// so a TCP connection anywhere would fail and the socket is the only way out (MH 2.1, H7).
#[test]
#[ignore = "reference host, E1: needs Linux with unprivileged user and network namespaces (unshare -rn)"]
fn h7_parquet_through_a_unix_socket_under_unshare() {
    if !cfg!(target_os = "linux") {
        panic!("network namespaces are a Linux facility");
    }
    let exe = std::env::current_exe().expect("this test binary");
    let status = std::process::Command::new("unshare")
        .args(["-rn", "--"])
        .arg(exe)
        .args([
            "--exact",
            "h7_parquet_through_a_unix_socket",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .env(IN_NAMESPACE, "1")
        .status()
        .expect("unshare runs");
    assert!(status.success(), "the run inside the namespace: {status:?}");
}
