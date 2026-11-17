//! Reading requests, writing answers, the limits, and 10,000 random streams.

use super::*;
use crate::api::NodeFacts;
use alloc::{string::ToString, vec};
use moruna_api::{Cloud, Executor, ExecutorKind, NodeHealth};

fn facts() -> NodeFacts {
    NodeFacts {
        version: "0.7.0".to_string(),
        instance: "i-1".to_string(),
        cloud: Cloud::OpenStack,
        cores: 4,
        memory_bytes: 8 << 30,
        executors: vec![Executor {
            kind: ExecutorKind::Cpu,
            slots: 4,
        }],
        boot_ms: 2140,
        health: NodeHealth::Ok,
    }
}

fn read_all(bytes: &[u8]) -> Read {
    Reader::default().read(bytes)
}

fn whole(read: Read) -> (Request, usize) {
    match read {
        Read::Request(request, length) => (request, length),
        other => panic!("not a request: {other:?}"),
    }
}

#[test]
fn a_get_is_read_whole() {
    let text = b"GET /v1/node HTTP/1.1\r\nHost: node\r\nAuthorization: Bearer abc \r\n\r\n";
    let (request, length) = whole(read_all(text));
    assert_eq!(length, text.len());
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, "/v1/node");
    assert!(!request.query);
    assert!(!request.close);
    assert_eq!(
        request.authorization,
        Authorization::One(b"Bearer abc".to_vec())
    );
    assert!(request.body.is_empty());
}

#[test]
fn headers_are_read_without_regard_to_case() {
    let text = b"GET /v1/node?x=1 HTTP/1.1\r\nhOsT: node\r\nconnection: keep-alive, CLOSE\r\nauthorization: a\r\nAUTHORIZATION: b\r\n\r\n";
    let (request, _) = whole(read_all(text));
    assert_eq!(request.path, "/v1/node");
    assert!(request.query);
    assert!(request.close);
    assert_eq!(request.authorization, Authorization::Many);
}

#[test]
fn a_body_is_read_by_its_length() {
    let text = b"POST /mcp HTTP/1.1\r\nHost: node\r\nContent-Length: 5\r\n\r\nhelloGET";
    let (request, length) = whole(read_all(text));
    assert_eq!(request.body, b"hello");
    assert_eq!(length, text.len() - 3);
}

#[test]
fn a_request_arriving_a_byte_at_a_time_is_read_once_whole() {
    let text = b"\r\nPOST /mcp HTTP/1.1\r\nHost: node\r\nContent-Length: 3\r\n\r\nabc";
    let mut reader = Reader::default();
    for end in 0..text.len() {
        assert_eq!(reader.read(&text[..end]), Read::Incomplete, "at {end}");
    }
    let (request, length) = whole(reader.read(text));
    assert_eq!(length, text.len());
    assert_eq!(request.body, b"abc");
    // The reader starts afresh for the next request.
    let next = b"GET /v1/health HTTP/1.1\nHost: node\n\n";
    let (request, _) = whole(reader.read(next));
    assert_eq!(request.path, "/v1/health");
}

/// A request whose head is exactly `size` bytes.
fn head_of(size: usize) -> Vec<u8> {
    let start = b"GET /v1/node HTTP/1.1\r\nHost: node\r\nX-Pad: ";
    let mut text = start.to_vec();
    text.resize(size - 4, b'a');
    text.extend_from_slice(b"\r\n\r\n");
    text
}

#[test]
fn headers_up_to_16_kib_are_read_and_more_are_refused() {
    let text = head_of(HEADER_LIMIT);
    let (_, length) = whole(read_all(&text));
    assert_eq!(length, HEADER_LIMIT);
    let text = head_of(HEADER_LIMIT + 1);
    assert_eq!(
        read_all(&text),
        Read::Refused("the request's headers are over 16 KiB")
    );
    // Refused as soon as 16 KiB have arrived without the head's end.
    let text = vec![b'a'; HEADER_LIMIT];
    assert!(matches!(read_all(&text), Read::Refused(_)));
    assert_eq!(read_all(&text[..HEADER_LIMIT - 1]), Read::Incomplete);
    // Empty lines at the start that fill the limit.
    let text = b"\r\n".repeat(HEADER_LIMIT / 2);
    assert!(matches!(read_all(&text), Read::Refused(_)));
}

#[test]
fn bodies_up_to_1_mib_are_read_and_more_are_refused() {
    let at_limit =
        format!("POST /mcp HTTP/1.1\r\nHost: node\r\nContent-Length: {BODY_LIMIT}\r\n\r\n");
    let mut text = at_limit.clone().into_bytes();
    assert_eq!(read_all(&text), Read::Incomplete);
    text.resize(at_limit.len() + BODY_LIMIT, b'x');
    let (request, _) = whole(read_all(&text));
    assert_eq!(request.body.len(), BODY_LIMIT);
    let over = format!(
        "POST /mcp HTTP/1.1\r\nHost: node\r\nContent-Length: {}\r\n\r\n",
        BODY_LIMIT + 1
    );
    assert_eq!(
        read_all(over.as_bytes()),
        Read::Refused("the request's body is over 1 MiB")
    );
    let huge =
        b"POST /mcp HTTP/1.1\r\nHost: node\r\nContent-Length: 99999999999999999999999999\r\n\r\n";
    assert!(matches!(read_all(huge), Read::Refused(_)));
}

#[test]
fn what_this_server_does_not_read_is_refused() {
    for text in [
        &b"GET /v1/node HTTP/1.0\r\nHost: node\r\n\r\n"[..],
        b"GET /v1/node HTTP/1.1\r\n\r\n",
        b"GET /v1/node HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n",
        b"POST /mcp HTTP/1.1\r\nHost: node\r\nTransfer-Encoding: chunked\r\n\r\n",
        b"POST /mcp HTTP/1.1\r\nHost: node\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\n",
        b"POST /mcp HTTP/1.1\r\nHost: node\r\nContent-Length: -1\r\n\r\n",
        b"POST /mcp HTTP/1.1\r\nHost: node\r\nContent-Length: \r\n\r\n",
        b"GET /v1/node HTTP/2\r\nHost: node\r\n\r\n",
        b"\x16\x03\x01\x02\x00\x01\x00\x01\xfc\x03\x03\r\n\r\n",
        b"GET\r\n\r\n",
    ] {
        assert!(
            matches!(read_all(text), Read::Refused(_)),
            "{:?} was not refused",
            String::from_utf8_lossy(text)
        );
    }
}

fn answer(status: u16, body: &[u8]) -> Answer {
    Answer {
        status,
        body: body.to_vec(),
        close: false,
        allow: None,
    }
}

#[test]
fn answers_are_written_as_http_1_1() {
    let mut out = Vec::new();
    write(&answer(200, b"{}"), false, &mut out);
    assert_eq!(
        out,
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}"
    );
    let mut out = Vec::new();
    write(&answer(401, b"{}"), true, &mut out);
    assert_eq!(
        out,
        b"HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 2\r\nWWW-Authenticate: Bearer\r\nConnection: close\r\n\r\n{}"
    );
    let mut out = Vec::new();
    let mut not_allowed = answer(405, b"");
    not_allowed.allow = Some("POST");
    write(&not_allowed, false, &mut out);
    assert_eq!(
        out,
        b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nAllow: POST\r\n\r\n"
    );
    for status in [202, 400, 403, 404, 409, 500, 503] {
        assert!(!reason(status).is_empty());
    }
    assert_eq!(reason(299), "");
}

/// Feeds `bytes` to `session` and takes every answer.
fn exchange(session: &mut Session, api: &mut Api, bytes: &[u8]) -> Vec<u8> {
    session.receive(bytes);
    let mut out = Vec::new();
    while session.serve(api, 1_794_931_200) || !session.pending().is_empty() {
        out.extend_from_slice(session.pending());
        let count = session.pending().len();
        session.take(count);
    }
    out
}

#[test]
fn a_session_answers_pipelined_requests_in_order_and_keeps_the_connection() {
    let mut api = Api::new(facts());
    let mut session = Session::default();
    let out = exchange(
        &mut session,
        &mut api,
        b"GET /v1/health HTTP/1.1\r\nHost: n\r\n\r\nGET /nothing HTTP/1.1\r\nHost: n\r\n\r\nGET /v1/hea",
    );
    let text = String::from_utf8(out).unwrap();
    assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text}");
    assert!(
        text.contains("{\"status\":\"ok\"}HTTP/1.1 404 Not Found\r\n"),
        "{text}"
    );
    assert!(!text.contains("Connection: close"));
    assert!(!session.closing());
    assert!(session.wants_input());
    let out = exchange(
        &mut session,
        &mut api,
        b"lth HTTP/1.1\r\nHost: n\r\nConnection: close\r\n\r\n",
    );
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("Connection: close\r\n"), "{text}");
    assert!(session.closing());
    assert!(!session.wants_input());
    // Nothing after the close is read.
    assert!(
        exchange(
            &mut session,
            &mut api,
            b"GET /v1/health HTTP/1.1\r\nHost: n\r\n\r\n"
        )
        .is_empty()
    );
}

#[test]
fn a_refused_stream_is_answered_invalid_and_closed() {
    let mut api = Api::new(facts());
    let mut session = Session::default();
    let out = exchange(&mut session, &mut api, b"BREW /pot HTCPCP/1.0\r\n\r\n");
    let text = String::from_utf8(out).unwrap();
    assert!(text.starts_with("HTTP/1.1 400 Bad Request\r\n"), "{text}");
    assert!(text.contains("Connection: close\r\n"));
    assert!(text.ends_with(
        "{\"error\":{\"code\":\"invalid\",\"message\":\"the request is not HTTP/1.1\"}}"
    ));
    assert!(session.closing());
}

#[test]
fn a_session_over_the_connection_limit_answers_busy_and_closes() {
    let mut api = Api::new(facts());
    for bytes in [
        &b"GET /v1/health HTTP/1.1\r\nHost: n\r\n\r\n"[..],
        b"not http\r\n\r\n",
    ] {
        let mut session = Session::refusing();
        assert!(session.is_refusing());
        let text = String::from_utf8(exchange(&mut session, &mut api, bytes)).unwrap();
        assert!(
            text.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
            "{text}"
        );
        assert!(text.contains("Connection: close\r\n"));
        assert!(text.contains("\"code\":\"busy\""));
        assert!(session.closing());
    }
}

#[test]
fn a_finished_session_answers_what_arrived_then_closes() {
    let mut api = Api::new(facts());
    let mut session = Session::default();
    session.finish();
    let text = String::from_utf8(exchange(
        &mut session,
        &mut api,
        b"GET /v1/health HTTP/1.1\r\nHost: n\r\n\r\nGET /v1/heal",
    ))
    .unwrap();
    assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(!text.contains("Connection: close"));
    assert!(session.closing());
}

/// A fixed-seed generator for the random streams (xorshift64*).
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Pieces that make a random stream look enough like HTTP to reach past the request line.
const PIECES: &[&[u8]] = &[
    b"GET ",
    b"POST ",
    b"/v1/node",
    b"/v1/health",
    b"/v1/node/audit",
    b"/mcp",
    b"?",
    b" HTTP/1.1",
    b" HTTP/1.0",
    b"\r\n",
    b"\n",
    b"\r\n\r\n",
    b"Host: n\r\n",
    b"Content-Length: ",
    b"Transfer-Encoding: chunked\r\n",
    b"Connection: close\r\n",
    b"Authorization: Bearer ",
    b"0",
    b"7",
    b"99999999999",
    b": ",
    b" ",
    b"\x00",
    b"\xff",
];

#[test]
fn ten_thousand_random_streams_never_panic() {
    let mut random = Random(0x6d6f_7275_6e61_2e32);
    let mut api = Api::new(facts());
    for _ in 0..10_000 {
        let mut stream = Vec::new();
        let length = random.below(600);
        while stream.len() < length {
            if random.below(3) == 0 {
                let byte = random.next() as u8;
                stream.push(byte);
            } else {
                stream.extend_from_slice(PIECES[random.below(PIECES.len())]);
            }
        }
        let mut session = if random.below(8) == 0 {
            Session::refusing()
        } else {
            Session::default()
        };
        let mut out = Vec::new();
        let mut at = 0;
        while at < stream.len() {
            let end = (at + 1 + random.below(64)).min(stream.len());
            out.extend(exchange(&mut session, &mut api, &stream[at..end]));
            at = end;
        }
        session.finish();
        out.extend(exchange(&mut session, &mut api, &[]));
        assert!(session.closing());
        // Every answer is HTTP/1.1, with a status from 2A's codes or 200.
        let mut rest = &out[..];
        while !rest.is_empty() {
            assert!(
                rest.starts_with(b"HTTP/1.1 "),
                "{:?}",
                String::from_utf8_lossy(rest)
            );
            let mut headers = [httparse::EMPTY_HEADER; 8];
            let mut response = httparse::Response::new(&mut headers);
            let httparse::Status::Complete(head) = response.parse(rest).unwrap() else {
                panic!("a partial answer");
            };
            assert!(matches!(
                response.code,
                Some(200 | 400 | 401 | 403 | 404 | 405 | 409 | 500 | 503)
            ));
            let length: usize = response
                .headers
                .iter()
                .find(|h| h.name == "Content-Length")
                .map(|h| core::str::from_utf8(h.value).unwrap().parse().unwrap())
                .unwrap();
            rest = &rest[head + length..];
        }
    }
}
