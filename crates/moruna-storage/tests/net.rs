//! The `moruna-net` connector (system design 3D, item 3.4) end to end: `object_store`'s
//! S3 client over a fake of the network server that proxies to a real local socket, and
//! a small S3-style server behind it.
//!
//! The fake is as strict as the network server: it opens only the hosts it was told the
//! job lists, at 443; it moves no byte before `start_tls`, and takes `start_tls` once.
//! The bytes it carries are plain HTTP: TLS itself is the network server's, tested in
//! `kernel/net/stack/tests/tls.rs`. It counts the sockets open at once, which is what the
//! job's socket bound limits.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use futures::StreamExt;
use moruna_storage::net::{NetConnector, NetSocket, Network};
use object_store::{ObjectStore, ObjectStoreExt, path::Path};

/// What the fake network server saw.
#[derive(Debug, Default)]
struct Seen {
    opened: Vec<(String, u16)>,
    tls: Vec<String>,
    /// Sockets open now, and the most there were at once.
    open: usize,
    most: usize,
}

/// The fake network server: the job's hosts, each proxied to a local address.
#[derive(Debug)]
struct Proxy {
    hosts: HashMap<String, SocketAddr>,
    seen: Arc<Mutex<Seen>>,
    tls_fails: bool,
}

impl Network for Proxy {
    fn open(&self, host: &str, port: u16) -> io::Result<Box<dyn NetSocket>> {
        self.seen.lock().unwrap().opened.push((host.into(), port));
        let to = self
            .hosts
            .get(host)
            .filter(|_| port == 443)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{host}:{port} is not in this job's network"),
                )
            })?;
        let stream = TcpStream::connect(to)?;
        let mut seen = self.seen.lock().unwrap();
        seen.open += 1;
        seen.most = seen.most.max(seen.open);
        Ok(Box::new(Socket {
            stream,
            tls: Mutex::new(false),
            seen: Arc::clone(&self.seen),
            tls_fails: self.tls_fails,
        }))
    }
}

struct Socket {
    stream: TcpStream,
    tls: Mutex<bool>,
    seen: Arc<Mutex<Seen>>,
    tls_fails: bool,
}

impl Socket {
    fn after_tls(&self) -> io::Result<()> {
        if *self.tls.lock().unwrap() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a byte before start_tls",
            ))
        }
    }
}

impl NetSocket for Socket {
    fn start_tls(&self, server_name: &str) -> io::Result<()> {
        if self.tls_fails {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the TLS handshake failed",
            ));
        }
        let mut tls = self.tls.lock().unwrap();
        if *tls {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "TLS twice"));
        }
        *tls = true;
        self.seen.lock().unwrap().tls.push(server_name.into());
        Ok(())
    }
    fn send(&self, bytes: &[u8]) -> io::Result<usize> {
        self.after_tls()?;
        (&self.stream).write(bytes)
    }
    fn receive(&self, buffer: &mut [u8]) -> io::Result<usize> {
        self.after_tls()?;
        (&self.stream).read(buffer)
    }
    fn close(&self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        self.seen.lock().unwrap().open -= 1;
    }
}

/// One request as the S3-style server read it.
#[derive(Debug, Clone)]
struct Request {
    method: String,
    path: String,
    host: String,
    connection: String,
}

/// How the S3-style server ends a connection after an answer.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
enum Close {
    /// It keeps it for the next request.
    #[default]
    Never,
    /// It says `Connection: close` in the answer, and closes.
    Says,
    /// It closes without a word, as a server does with a connection idle too long.
    Quietly,
}

#[derive(Debug, Clone, Copy, Default)]
struct Mode {
    /// Every GET is answered with half its body, and the connection closed.
    short: bool,
    close: Close,
    /// How long it thinks before each answer.
    delay: Duration,
}

/// A small S3-style server: GET (whole, or one range) and HEAD of the objects it holds,
/// any number of requests on a connection unless `mode` says otherwise.
fn s3(objects: HashMap<String, Vec<u8>>, mode: Mode) -> (SocketAddr, Arc<Mutex<Vec<Request>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let objects = Arc::new(objects);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let (Ok(stream), objects, log) = (stream, Arc::clone(&objects), Arc::clone(&log))
            else {
                continue;
            };
            thread::spawn(move || while answer(&stream, &objects, &log, mode) {});
        }
    });
    (address, seen)
}

/// Answers one request on `stream`: whether the connection takes another.
fn answer(
    mut stream: &TcpStream,
    objects: &HashMap<String, Vec<u8>>,
    log: &Mutex<Vec<Request>>,
    mode: Mode,
) -> bool {
    let mut head = Vec::new();
    let mut one = [0; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read_exact(&mut one).is_err() {
            return false;
        }
        head.push(one[0]);
    }
    let text = String::from_utf8(head).unwrap();
    let mut lines = text.split("\r\n");
    let mut first = lines.next().unwrap().split(' ');
    let (method, path) = (first.next().unwrap(), first.next().unwrap());
    let header = |name: &str| {
        text.split("\r\n")
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
            })
            .unwrap_or_default()
    };
    log.lock().unwrap().push(Request {
        method: method.into(),
        path: path.into(),
        host: header("host"),
        connection: header("connection"),
    });
    thread::sleep(mode.delay);
    let says = if mode.close == Close::Says {
        "Connection: close\r\n"
    } else {
        ""
    };
    let common = format!("ETag: \"e1\"\r\nLast-Modified: Sun, 06 Nov 1994 08:49:37 GMT\r\n{says}");
    let Some(object) = objects.get(path) else {
        let _ = stream.write_all(
            format!("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n{common}\r\n").as_bytes(),
        );
        return mode.close == Close::Never;
    };
    let range = header("range");
    let (status, body, extra) = match range.strip_prefix("bytes=") {
        Some(r) => {
            let (a, b) = r.split_once('-').unwrap();
            let a: usize = a.parse().unwrap();
            let b: usize = b.parse::<usize>().unwrap().min(object.len() - 1);
            (
                "206 Partial Content",
                &object[a..=b],
                format!("Content-Range: bytes {a}-{b}/{}\r\n", object.len()),
            )
        }
        None => ("200 OK", &object[..], String::new()),
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra}{common}\r\n",
        body.len()
    );
    if stream.write_all(head.as_bytes()).is_err() {
        return false;
    }
    if method == "GET" {
        if mode.short {
            let _ = stream.write_all(&body[..body.len() / 2]);
            return false;
        }
        if stream.write_all(body).is_err() {
            return false;
        }
    }
    mode.close == Close::Never
}

fn store(endpoint: &str, connector: NetConnector) -> impl ObjectStore {
    object_store::aws::AmazonS3Builder::new()
        .with_bucket_name("bucket")
        .with_region("us-east-1")
        .with_access_key_id("TEST-ONLY-ACCESS")
        .with_secret_access_key("TEST-ONLY-SECRET")
        .with_endpoint(endpoint)
        .with_allow_http(true)
        .with_virtual_hosted_style_request(false)
        .with_http_connector(connector)
        .build()
        .unwrap()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

/// An object of `len` bytes, all different nearby.
fn object(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect()
}

fn proxy(host: &str, to: SocketAddr, seen: &Arc<Mutex<Seen>>) -> Proxy {
    Proxy {
        hosts: HashMap::from([(host.to_string(), to)]),
        seen: Arc::clone(seen),
        tls_fails: false,
    }
}

/// A connector over `proxy` holding the job's 64 sockets.
fn connector(proxy: Proxy) -> NetConnector {
    NetConnector::new(Arc::new(proxy))
}

/// Waits up to five seconds for the fake to hold `open` sockets.
fn until_open(seen: &Mutex<Seen>, open: usize) {
    let by = Instant::now() + Duration::from_secs(5);
    while seen.lock().unwrap().open != open {
        assert!(Instant::now() < by, "{:?}", seen.lock().unwrap());
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn an_object_is_read_whole_and_in_ranges_through_the_network_server() {
    let data = object(3 * 1024 * 1024 + 17);
    let (server, requests) = s3(
        HashMap::from([("/bucket/data/a.bin".into(), data.clone())]),
        Mode::default(),
    );
    let seen = Arc::new(Mutex::new(Seen::default()));
    let store = store(
        "https://store.objects.test",
        connector(proxy("store.objects.test", server, &seen)),
    );
    let path = Path::from("data/a.bin");
    runtime().block_on(async {
        let meta = store.head(&path).await.unwrap();
        assert_eq!(meta.size, data.len() as u64);
        let whole = store.get(&path).await.unwrap().bytes().await.unwrap();
        assert!(whole[..] == data[..], "the object changed on the way");
        let ranges = [
            0..1,
            100..200_000,
            1_048_576..3_145_745,
            3_145_728..3_145_745,
        ];
        let parts = store.get_ranges(&path, &ranges).await.unwrap();
        for (range, part) in ranges.iter().zip(parts) {
            assert!(
                part[..] == data[range.start as usize..range.end as usize],
                "{range:?}"
            );
        }
        // A key the store does not hold is the store's answer, not the network's.
        let missing = store.get(&Path::from("data/none.bin")).await.unwrap_err();
        assert!(
            matches!(missing, object_store::Error::NotFound { .. }),
            "{missing}"
        );
    });
    // Every connection went to the store's host, by name, at 443, with TLS for that name;
    // connections were kept, and no request asked the server to close one.
    let seen = seen.lock().unwrap();
    assert!(!seen.opened.is_empty());
    assert!(
        seen.opened
            .iter()
            .all(|(h, p)| h == "store.objects.test" && *p == 443)
    );
    assert_eq!(seen.tls.len(), seen.opened.len());
    assert!(seen.tls.iter().all(|n| n == "store.objects.test"));
    let requests = requests.lock().unwrap();
    assert!(requests.len() > seen.opened.len(), "{requests:?} {seen:?}");
    for r in requests.iter() {
        assert_eq!(r.host, "store.objects.test", "{r:?}");
        assert_eq!(r.connection, "", "{r:?}");
        assert!(r.path.starts_with("/bucket/data/"), "{r:?}");
    }
    assert!(requests.iter().any(|r| r.method == "HEAD"));
}

#[test]
fn sequential_range_reads_open_one_connection() {
    let data = object(1024 * 1024);
    let (server, requests) = s3(
        HashMap::from([("/bucket/k".into(), data.clone())]),
        Mode::default(),
    );
    let seen = Arc::new(Mutex::new(Seen::default()));
    let store = store(
        "https://store.objects.test",
        connector(proxy("store.objects.test", server, &seen)),
    );
    let path = Path::from("k");
    const N: u64 = 32;
    runtime().block_on(async {
        assert_eq!(store.head(&path).await.unwrap().size, data.len() as u64);
        for i in 0..N {
            let range = i * 32_768..(i + 1) * 32_768 - 7;
            let part = store.get_range(&path, range.clone()).await.unwrap();
            assert!(
                part[..] == data[range.start as usize..range.end as usize],
                "{range:?}"
            );
        }
    });
    let seen = seen.lock().unwrap();
    assert_eq!(seen.opened, [("store.objects.test".into(), 443)]);
    assert_eq!(seen.tls, ["store.objects.test"]);
    assert_eq!(seen.most, 1);
    assert_eq!(requests.lock().unwrap().len(), N as usize + 1);
}

#[test]
fn a_connection_the_server_closes_is_dropped_and_reopened() {
    let data = object(100_000);
    for close in [Close::Says, Close::Quietly] {
        let (server, requests) = s3(
            HashMap::from([("/bucket/k".into(), data.clone())]),
            Mode {
                close,
                ..Mode::default()
            },
        );
        let seen = Arc::new(Mutex::new(Seen::default()));
        let store = store(
            "https://store.objects.test",
            connector(proxy("store.objects.test", server, &seen)),
        );
        let path = Path::from("k");
        runtime().block_on(async {
            for i in 0..6_usize {
                // Half at once, half after the connection has had time to close.
                if i >= 3 {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                let range = (i * 10_000) as u64..(i * 10_000 + 5_000) as u64;
                let part = store.get_range(&path, range.clone()).await.unwrap();
                assert!(
                    part[..] == data[range.start as usize..range.end as usize],
                    "{close:?} {range:?}"
                );
            }
        });
        // One connection a request, each closed by the server and none used again.
        assert_eq!(requests.lock().unwrap().len(), 6, "{close:?}");
        assert_eq!(seen.lock().unwrap().opened.len(), 6, "{close:?}");
        until_open(&seen, 0);
    }
}

#[test]
fn the_socket_bound_holds_however_many_requests_run_at_once() {
    let data = object(50_000);
    let (server, requests) = s3(
        HashMap::from([("/bucket/k".into(), data.clone())]),
        Mode {
            delay: Duration::from_millis(20),
            ..Mode::default()
        },
    );
    let seen = Arc::new(Mutex::new(Seen::default()));
    let network = Arc::new(proxy("store.objects.test", server, &seen));
    let store = store(
        "https://store.objects.test",
        NetConnector::with_sockets(network, 3),
    );
    let path = Path::from("k");
    runtime().block_on(async {
        let reads = (0..12_u64).map(|i| {
            let (store, path) = (&store, &path);
            async move { (i, store.get_range(path, i * 1000..i * 1000 + 4000).await) }
        });
        for (i, part) in futures::future::join_all(reads).await {
            let at = i as usize * 1000;
            assert!(part.unwrap()[..] == data[at..at + 4000], "{i}");
        }
    });
    let seen = seen.lock().unwrap();
    assert_eq!(seen.most, 3, "{seen:?}");
    assert_eq!(seen.opened.len(), 3, "{seen:?}");
    assert_eq!(requests.lock().unwrap().len(), 12);
}

#[test]
fn past_the_bound_a_connection_idle_on_another_host_is_freed() {
    let data = object(10_000);
    let (server, requests) = s3(
        HashMap::from([("/bucket/k".into(), data.clone())]),
        Mode::default(),
    );
    let seen = Arc::new(Mutex::new(Seen::default()));
    let network = Proxy {
        hosts: HashMap::from([
            ("a.objects.test".to_string(), server),
            ("b.objects.test".to_string(), server),
        ]),
        seen: Arc::clone(&seen),
        tls_fails: false,
    };
    let connector = NetConnector::with_sockets(Arc::new(network), 1);
    let a = store("https://a.objects.test", connector.clone());
    let b = store("https://b.objects.test", connector);
    let path = Path::from("k");
    runtime().block_on(async {
        for store in [&a, &b, &a, &a, &b] {
            assert!(store.get(&path).await.unwrap().bytes().await.unwrap()[..] == data[..]);
        }
    });
    let seen = seen.lock().unwrap();
    let hosts: Vec<_> = seen.opened.iter().map(|(h, _)| h.as_str()).collect();
    assert_eq!(
        hosts,
        [
            "a.objects.test",
            "b.objects.test",
            "a.objects.test",
            "b.objects.test"
        ]
    );
    assert_eq!(seen.most, 1, "{seen:?}");
    let requests = requests.lock().unwrap();
    let asked: Vec<_> = requests.iter().map(|r| r.host.as_str()).collect();
    assert_eq!(
        asked,
        [
            "a.objects.test",
            "b.objects.test",
            "a.objects.test",
            "a.objects.test",
            "b.objects.test"
        ]
    );
}

#[test]
fn a_body_dropped_before_its_end_ends_its_connection() {
    let data = object(8 * 1024 * 1024);
    let (server, requests) = s3(
        HashMap::from([("/bucket/k".into(), data.clone())]),
        Mode::default(),
    );
    let seen = Arc::new(Mutex::new(Seen::default()));
    let store = store(
        "https://store.objects.test",
        connector(proxy("store.objects.test", server, &seen)),
    );
    let path = Path::from("k");
    let runtime = runtime();
    runtime.block_on(async {
        let mut stream = store.get(&path).await.unwrap().into_stream();
        let first = stream.next().await.unwrap().unwrap();
        assert!(first[..] == data[..first.len()]);
        assert!(first.len() < data.len());
    });
    // The rest was not read: the connection ends, and its socket is freed.
    until_open(&seen, 0);
    runtime.block_on(async {
        assert!(store.get(&path).await.unwrap().bytes().await.unwrap()[..] == data[..]);
    });
    assert_eq!(seen.lock().unwrap().opened.len(), 2);
    assert_eq!(requests.lock().unwrap().len(), 2);
}

#[test]
fn a_host_the_job_does_not_list_is_refused_at_once_naming_it() {
    let (server, requests) = s3(HashMap::new(), Mode::default());
    let seen = Arc::new(Mutex::new(Seen::default()));
    let store = store(
        "https://elsewhere.test",
        connector(proxy("store.objects.test", server, &seen)),
    );
    let started = Instant::now();
    let e = runtime()
        .block_on(store.get(&Path::from("k")))
        .unwrap_err()
        .to_string();
    assert!(e.contains("elsewhere.test"), "{e}");
    assert!(e.contains("not in this job's network"), "{e}");
    // Not retried: one attempt, at once.
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        seen.lock().unwrap().opened,
        [("elsewhere.test".into(), 443)]
    );
    assert!(requests.lock().unwrap().is_empty());
}

#[test]
fn plain_http_and_address_endpoints_are_refused_before_the_network() {
    let (server, _) = s3(HashMap::new(), Mode::default());
    for (endpoint, says) in [
        ("http://store.objects.test", "HTTPS only"),
        ("https://127.0.0.1", "host names only"),
    ] {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let store = store(
            endpoint,
            connector(proxy("store.objects.test", server, &seen)),
        );
        let e = runtime()
            .block_on(store.get(&Path::from("k")))
            .unwrap_err()
            .to_string();
        assert!(e.contains(says), "{endpoint}: {e}");
        assert!(seen.lock().unwrap().opened.is_empty(), "{endpoint}");
    }
}

#[test]
fn a_failed_tls_handshake_is_an_error_and_no_byte_moves() {
    let (server, requests) = s3(
        HashMap::from([("/bucket/k".into(), object(10))]),
        Mode::default(),
    );
    let seen = Arc::new(Mutex::new(Seen::default()));
    let mut network = proxy("store.objects.test", server, &seen);
    network.tls_fails = true;
    let store = store("https://store.objects.test", connector(network));
    let e = runtime()
        .block_on(store.get(&Path::from("k")))
        .unwrap_err()
        .to_string();
    assert!(e.contains("the TLS handshake failed"), "{e}");
    assert!(e.contains("moruna-net://store.objects.test:443"), "{e}");
    // Not retried, nothing reached the server, and the socket was freed.
    assert_eq!(seen.lock().unwrap().opened.len(), 1);
    until_open(&seen, 0);
    thread::sleep(Duration::from_millis(100));
    assert!(requests.lock().unwrap().is_empty());
}

#[test]
fn a_body_cut_short_is_an_error_not_a_short_object() {
    let (server, _) = s3(
        HashMap::from([("/bucket/k".into(), object(100_000))]),
        Mode {
            short: true,
            ..Mode::default()
        },
    );
    let seen = Arc::new(Mutex::new(Seen::default()));
    let store = store(
        "https://store.objects.test",
        connector(proxy("store.objects.test", server, &seen)),
    );
    let read = runtime().block_on(async { store.get(&Path::from("k")).await?.bytes().await });
    assert!(read.is_err(), "{read:?}");
    until_open(&seen, 0);
}
