//! The server's memory under the limits: 80 connections, each sending a full 1 MiB body with no
//! token, then one valid `node` call; then 80 sending 1 MiB bodies with a valid token, half of
//! each at first, so that eight hold the whole body budget at once; then a job's records filling
//! the buffer, with a stalled reader, documents waiting, and a page of them polled over MCP. A
//! counting allocator measures what the server alone holds at its peak; it must stay under
//! 32 MiB, half of `api`'s 64 MiB account.
//!
//! The test is alone in its own binary, so that nothing else allocates while it measures.
mod common;

use common::{
    Client, End, Harness, INSTANCE, Pipe, TestClock,
    engine::{Engine, RUN},
    facts, pinned,
    signer::Signer,
};
use moruna_api::EVENTS_PAGE_BYTES;
use moruna_api_server::{BODY_BUDGET, BODY_LIMIT, Config, Identity, OPEN_LIMIT, Server, TokenKey};
use rustls::{ClientConnection, pki_types::ServerName};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::{Cell, RefCell},
    rc::Rc,
    sync::atomic::{AtomicUsize, Ordering},
};

/// What the server's allocations hold now, and at most.
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Set while the server runs: what is allocated then is the server's.
    static SERVER: Cell<bool> = const { Cell::new(false) };
}

/// The marker of an allocation that is not the server's.
const NOT_COUNTED: usize = usize::MAX;

/// The system allocator, with a word before each allocation saying whether it is the server's,
/// and how large, so that it is counted off when freed, whoever frees it.
struct Counting;

impl Counting {
    /// The room before each allocation: one word, at the allocation's alignment.
    fn prefix(layout: Layout) -> usize {
        layout.align().max(size_of::<usize>())
    }
}

// SAFETY: every pointer handed out is the system's, offset by a prefix that keeps its
// alignment; `dealloc` undoes the same offset with the same layout.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let prefix = Self::prefix(layout);
        let Ok(outer) = Layout::from_size_align(layout.size() + prefix, prefix) else {
            return std::ptr::null_mut();
        };
        // SAFETY: `outer` has a non-zero size.
        let base = unsafe { System.alloc(outer) };
        if base.is_null() {
            return base;
        }
        let counted = SERVER.try_with(Cell::get).unwrap_or(false);
        // SAFETY: `prefix` is within the allocation, and the word before the returned pointer
        // is aligned, since `prefix` is a multiple of the word's size.
        unsafe {
            let pointer = base.add(prefix);
            let mark = if counted { layout.size() } else { NOT_COUNTED };
            pointer.cast::<usize>().sub(1).write(mark);
            if counted {
                let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
                PEAK.fetch_max(live, Ordering::Relaxed);
            }
            pointer
        }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        let prefix = Self::prefix(layout);
        // SAFETY: `pointer` came from `alloc` with this layout, so the word before it is its
        // mark, and the system's allocation starts `prefix` before it.
        unsafe {
            let mark = pointer.cast::<usize>().sub(1).read();
            if mark != NOT_COUNTED {
                LIVE.fetch_sub(mark, Ordering::Relaxed);
            }
            let outer = Layout::from_size_align_unchecked(layout.size() + prefix, prefix);
            System.dealloc(pointer.sub(prefix), outer);
        }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Runs `f` as the server: what it allocates is counted.
fn as_server<R>(f: impl FnOnce() -> R) -> R {
    SERVER.with(|s| s.set(true));
    let result = f();
    SERVER.with(|s| s.set(false));
    result
}

/// Connects a client, the server's side of it counted.
fn connect(harness: &mut Harness) -> usize {
    let pipe = Rc::new(RefCell::new(Pipe::default()));
    as_server(|| harness.server.accept(End(pipe.clone())));
    let mut tls = ClientConnection::new(
        pinned(&harness.fingerprint),
        ServerName::try_from("node").unwrap(),
    )
    .unwrap();
    tls.set_buffer_limit(None);
    harness.clients.push(Client {
        tls,
        pipe,
        received: Vec::new(),
        error: None,
        closed_by_server: false,
    });
    harness.clients.len() - 1
}

/// Polls the server, counted, and pumps the clients until nothing moves.
fn settle(harness: &mut Harness) {
    settle_but(harness, usize::MAX);
}

/// As [`settle`], with client `stalled` reading nothing: what the server sends it stays where
/// the server put it.
fn settle_but(harness: &mut Harness, stalled: usize) {
    let mut quiet = 0;
    for _ in 0..100_000 {
        as_server(|| harness.server.poll());
        let mut moved = false;
        for (index, client) in harness.clients.iter_mut().enumerate() {
            if index != stalled {
                moved |= client.pump();
            }
        }
        quiet = if moved { 0 } else { quiet + 1 };
        if quiet == 4 {
            return;
        }
    }
    panic!("the exchange never settled");
}

#[test]
fn eighty_connections_with_1_mib_bodies_and_no_token_stay_under_32_mib() {
    let signer = Signer::new();
    let identity = Identity::generate(INSTANCE, common::WALL).unwrap();
    let fingerprint = identity.fingerprint();
    let clock = TestClock::new();
    let engine = Engine::new();
    let server = as_server(|| {
        Server::with_engine(
            Config {
                identity,
                facts: facts(),
                token_keys: vec![TokenKey::new(signer.public())],
            },
            clock.clone(),
            Box::new(engine.clone()),
        )
        .unwrap()
    });
    let mut harness = Harness {
        server,
        clock,
        fingerprint,
        clients: Vec::new(),
        signer,
        engine,
    };

    // 80 connections at once, each sending a whole 1 MiB body with no token.
    let mut request = format!(
        "POST /mcp HTTP/1.1\r\nHost: node\r\nContent-Type: application/json\r\nContent-Length: {BODY_LIMIT}\r\n\r\n"
    )
    .into_bytes();
    request.resize(request.len() + BODY_LIMIT, b' ');
    let clients: Vec<usize> = (0..OPEN_LIMIT).map(|_| connect(&mut harness)).collect();
    for &client in &clients {
        harness.clients[client].send(&request);
    }
    settle(&mut harness);
    for &client in &clients {
        let status = harness.clients[client].response().unwrap().status;
        assert!(status == 401 || status == 503, "{status}");
        assert!(harness.clients[client].server_closed());
    }

    // Then one valid node call, answered.
    let client = connect(&mut harness);
    let token = harness.token(&["observe"]);
    harness.clients[client].send(
        format!(
            "GET /v1/node HTTP/1.1\r\nHost: node\r\n{}\r\n",
            Harness::bearer(&token)
        )
        .as_bytes(),
    );
    settle(&mut harness);
    let response = harness.clients[client].response().unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.json()["instance"], INSTANCE);

    let peak = PEAK.load(Ordering::Relaxed);
    report("80 bodies with no token, then a node call", peak);
    // The scratch buffer alone is 16 KiB: the counter counts.
    assert!(peak > 16 * 1024, "peak {peak} bytes");
    assert!(peak < 32 * 1024 * 1024, "peak {peak} bytes");

    // The same 80, now with a valid token and half of each body sent: eight hold the whole
    // body budget, and the rest are answered busy.
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    let head = format!(
        "POST /mcp HTTP/1.1\r\nHost: node\r\n{}Content-Length: {BODY_LIMIT}\r\n\r\n",
        Harness::bearer(&token)
    );
    let body = {
        let start = r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{"pad":""#;
        let end = r#""}}"#;
        let mut body = start.as_bytes().to_vec();
        body.resize(BODY_LIMIT - end.len(), b'x');
        body.extend_from_slice(end.as_bytes());
        body
    };
    let holders: Vec<usize> = (1..OPEN_LIMIT)
        .map(|_| {
            let client = connect(&mut harness);
            harness.clients[client].send(head.as_bytes());
            harness.clients[client].send(&body[..BODY_LIMIT / 2]);
            client
        })
        .collect();
    settle(&mut harness);
    let held: Vec<usize> = holders
        .iter()
        .copied()
        .filter(|&c| harness.clients[c].response().is_none())
        .collect();
    assert_eq!(held.len(), BODY_BUDGET / BODY_LIMIT);
    for &client in &held {
        harness.clients[client].send(&body[BODY_LIMIT / 2..]);
    }
    settle(&mut harness);
    for &client in &held {
        assert_eq!(harness.clients[client].response().unwrap().status, 200);
    }
    let peak = PEAK.load(Ordering::Relaxed);
    report("80 bodies with a token, eight held at once", peak);
    assert!(peak < 32 * 1024 * 1024, "peak {peak} bytes");

    // A job runs and tells far more records than the node's buffer keeps, while a reader of
    // its events has stopped reading, and eight documents of 1 MiB wait for the engine's check.
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    let submit = harness.token(&["submit"]);
    let job = r#"{"v1":{"moruna_spec":1,"source":{"kind":"rows"},"sink":{"kind":"count"}}}"#;
    let submitter = connect(&mut harness);
    harness.clients[submitter].send(&Harness::post("/v1/jobs", &submit, job));
    settle(&mut harness);
    harness.engine.attempt_started();
    settle(&mut harness);
    assert_eq!(harness.clients[submitter].response().unwrap().status, 201);
    let reader = connect(&mut harness);
    harness.clients[reader].send(
        format!(
            "GET /v1/jobs/{RUN}/events HTTP/1.1\r\nHost: node\r\n{}\r\n",
            Harness::bearer(&token)
        )
        .as_bytes(),
    );
    settle(&mut harness);
    assert!(
        harness.clients[reader]
            .received
            .starts_with(b"HTTP/1.1 200 OK\r\n")
    );
    // From here the reader takes nothing more, and the server may hold 4 KiB for it.
    harness.clients[reader].pipe.borrow_mut().capacity = Some(4096);
    harness.engine.0.borrow_mut().hold_checks = true;
    let document = {
        let start = r#"{"v1":{"pad":""#;
        let mut document = start.as_bytes().to_vec();
        document.resize(BODY_LIMIT - 3, b'x');
        document.extend_from_slice(br#""}}"#);
        String::from_utf8(document).unwrap()
    };
    let waiting: Vec<usize> = (0..BODY_BUDGET / BODY_LIMIT)
        .map(|_| {
            let client = connect(&mut harness);
            harness.clients[client].send(&Harness::post("/v1/jobs/validate", &submit, &document));
            client
        })
        .collect();
    settle_but(&mut harness, reader);
    let line = "a line of a task's output, as long as such lines are. ".repeat(4);
    for batch in 0..60 {
        for _ in 0..1_000 {
            harness.engine.log("source", &line);
        }
        harness.engine.heartbeat(batch * 1_000);
        settle_but(&mut harness, reader);
    }
    let held = harness.clients[reader].pipe.borrow().to_client.len();
    assert!(held <= 4096, "the stalled reader's pipe holds {held} bytes");
    let peak = PEAK.load(Ordering::Relaxed);
    report(
        "a job telling 60,000 records, a reader that stopped, eight documents waiting",
        peak,
    );
    assert!(
        peak > 8 * 1024 * 1024,
        "the buffer and the documents were held: {peak}"
    );
    assert!(peak < 32 * 1024 * 1024, "peak {peak} bytes");

    // The documents are checked. With the buffer still full and the reader still stopped, an
    // agent polls the events over MCP: a page of 1,000 records.
    harness.engine.0.borrow_mut().hold_checks = false;
    settle_but(&mut harness, reader);
    for &client in &waiting {
        assert_eq!(harness.clients[client].response().unwrap().status, 200);
    }
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    let poller = connect(&mut harness);
    let call = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"moruna_jobs_events","arguments":{{"id":"{RUN}","from":0}}}}}}"#
    );
    harness.clients[poller].send(&Harness::post("/mcp", &token, &call));
    settle_but(&mut harness, reader);
    let page = harness.clients[poller].response().unwrap().json();
    let records = page["result"]["structuredContent"]["records"]
        .as_array()
        .unwrap();
    assert_eq!(records.len(), 1_000);
    let peak = PEAK.load(Ordering::Relaxed);
    report(
        "the buffer full, a reader that stopped, an MCP page of 1,000 records",
        peak,
    );
    assert!(peak < 32 * 1024 * 1024, "peak {peak} bytes");

    // The job ends, and the reader, reading again, is sent the rest from the oldest record the
    // buffer still holds, then the report and the exit.
    harness.engine.finish(r#"{"rows_out":60000}"#, 0, None);
    harness.clients[reader].pipe.borrow_mut().capacity = None;
    settle(&mut harness);
    let stream = String::from_utf8(std::mem::take(&mut harness.clients[reader].received)).unwrap();
    assert!(
        stream.ends_with("\"diagnostic\":null}\n\n"),
        "it reached the exit"
    );
    assert!(stream.contains("event: report\n"));
    let ids: Vec<u64> = stream
        .lines()
        .filter_map(|line| line.strip_prefix("id: "))
        .map(|id| id.parse().unwrap())
        .collect();
    assert_eq!(ids.last(), Some(&60_001), "the last record");
    assert!(
        ids.windows(2).any(|pair| pair[1] > pair[0] + 1),
        "with a gap where it fell behind"
    );
    assert!(harness.clients[reader].server_closed());
    // The reader's connection is gone. Its pipe, written while the server ran, stands for the
    // network server's buffer of the connection, not the server's: let it go, as the network
    // server would.
    harness.clients[reader].pipe = Rc::new(RefCell::new(Pipe::default()));

    // A second job tells records of 12 KiB until they fill the buffer: 1,000 of them would be
    // all of its 12 MiB. Polled over MCP, a page stops at 1 MiB, and the server stays under
    // 32 MiB.
    let second = "22222222222222222222222222222222";
    harness.engine.run_as(second);
    let submitter = connect(&mut harness);
    harness.clients[submitter].send(&Harness::post("/v1/jobs", &submit, job));
    settle(&mut harness);
    harness.engine.attempt_started();
    settle(&mut harness);
    assert_eq!(harness.clients[submitter].response().unwrap().status, 201);
    let large = "x".repeat(12 * 1024);
    for _ in 0..1_100 {
        harness.engine.log("source", &large);
    }
    settle(&mut harness);
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    let poller = connect(&mut harness);
    let call = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"moruna_jobs_events","arguments":{{"id":"{second}","from":0}}}}}}"#
    );
    harness.clients[poller].send(&Harness::post("/mcp", &token, &call));
    settle(&mut harness);
    let page = harness.clients[poller].response().unwrap().json();
    let records = page["result"]["structuredContent"]["records"]
        .as_array()
        .unwrap();
    let bytes: usize = records
        .iter()
        .map(|record| record["data"].to_string().len())
        .sum();
    assert!(
        bytes <= EVENTS_PAGE_BYTES && bytes > EVENTS_PAGE_BYTES - 13 * 1024,
        "{bytes}"
    );
    assert!((80..90).contains(&records.len()), "{}", records.len());
    let peak = PEAK.load(Ordering::Relaxed);
    report(
        "the buffer full of records of 12 KiB, an MCP page of them",
        peak,
    );
    assert!(peak < 32 * 1024 * 1024, "peak {peak} bytes");
}

fn report(case: &str, peak: usize) {
    eprintln!(
        "{case}: peak held by the server {peak} bytes ({:.2} MiB)",
        peak as f64 / (1024.0 * 1024.0)
    );
}
