//! The minimal HTTP/1.1 client.

use std::{
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};

use super::recorded;
use crate::{
    http::{Header, HttpError, RESPONSE_MAX, Request, Response, exchange},
    machine::{Stream, StreamError},
};

/// A connection whose server's bytes are written beforehand.
struct Wire {
    /// What the server sends.
    answer: Vec<u8>,
    given: usize,
    /// The most bytes one read gives or one write takes.
    chunk: usize,
    /// What the client sent.
    sent: Vec<u8>,
    /// How the connection ends once the answer is given: closed, or broken.
    end: Option<StreamError>,
    /// The deadline of every call.
    deadlines: Vec<u64>,
}

impl Wire {
    fn new(answer: impl Into<Vec<u8>>) -> Wire {
        Wire {
            answer: answer.into(),
            given: 0,
            chunk: usize::MAX,
            sent: Vec::new(),
            end: None,
            deadlines: Vec::new(),
        }
    }
}

impl Stream for Wire {
    fn write(&mut self, bytes: &[u8], deadline: u64) -> Result<usize, StreamError> {
        self.deadlines.push(deadline);
        let taken = bytes.len().min(self.chunk);
        self.sent.extend_from_slice(&bytes[..taken]);
        Ok(taken)
    }

    fn read(&mut self, buffer: &mut [u8], deadline: u64) -> Result<usize, StreamError> {
        self.deadlines.push(deadline);
        let left = &self.answer[self.given..];
        if left.is_empty() {
            return self.end.clone().map_or(Ok(0), Err);
        }
        let count = left.len().min(buffer.len()).min(self.chunk);
        buffer[..count].copy_from_slice(&left[..count]);
        self.given += count;
        Ok(count)
    }
}

const GET: Request<'static> = Request::get("169.254.169.254", "/latest", &[]);

fn get(answer: impl Into<Vec<u8>>) -> Result<Response, HttpError> {
    exchange(&mut Wire::new(answer), &GET, 90)
}

fn malformed(answer: &str) -> &'static str {
    match get(answer) {
        Err(HttpError::Malformed(what)) => what,
        other => panic!("{answer:?} gave {other:?}"),
    }
}

#[test]
fn a_get_goes_on_the_wire_as_http_1_1() {
    assert_eq!(GET.method(), "GET");
    assert_eq!(GET.path(), "/latest");
    assert_eq!(
        String::from_utf8(GET.encode()).unwrap(),
        "GET /latest HTTP/1.1\r\nHost: 169.254.169.254\r\nConnection: close\r\n\r\n"
    );
    let azure = Request::get(
        "169.254.169.254",
        "/metadata/instance?api-version=2021-02-01",
        &[("Metadata", "true")],
    );
    assert_eq!(
        String::from_utf8(azure.encode()).unwrap(),
        "GET /metadata/instance?api-version=2021-02-01 HTTP/1.1\r\n\
         Host: 169.254.169.254\r\nConnection: close\r\nMetadata: true\r\n\r\n"
    );
}

#[test]
fn a_post_carries_its_body_and_its_length() {
    let post = Request::post(
        "168.63.129.16",
        "/machine?comp=health",
        &[
            ("x-ms-version", "2012-11-30"),
            ("Content-Type", "text/xml; charset=utf-8"),
        ],
        b"<Health/>",
    );
    assert_eq!(post.method(), "POST");
    assert_eq!(
        String::from_utf8(post.encode()).unwrap(),
        "POST /machine?comp=health HTTP/1.1\r\nHost: 168.63.129.16\r\nConnection: close\r\n\
         x-ms-version: 2012-11-30\r\nContent-Type: text/xml; charset=utf-8\r\n\
         Content-Length: 9\r\n\r\n<Health/>"
    );
    // A POST with nothing in it still says its length.
    let empty = Request::post("168.63.129.16", "/", &[], b"");
    assert!(
        String::from_utf8(empty.encode())
            .unwrap()
            .ends_with("Content-Length: 0\r\n\r\n")
    );
    let mut wire = Wire::new("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
    assert_eq!(exchange(&mut wire, &post, 90).unwrap().body, b"ok");
    assert_eq!(wire.sent, post.encode());
}

#[test]
fn an_answer_is_its_status_headers_and_content_length_body() {
    let answer = "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\ncontent-length:  5 \r\n\
                  Date: Tue, 17 Nov 2026 09:30:00 GMT\r\nX-Empty:\r\n\r\nhello and more";
    // Whatever sizes the bytes arrive in, and however much of a write is taken at once.
    for chunk in [1, 2, 3, 7, 4096, usize::MAX] {
        let mut wire = Wire::new(answer);
        wire.chunk = chunk;
        let response = exchange(&mut wire, &GET, 90).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"hello", "in chunks of {chunk}");
        assert_eq!(wire.sent, GET.encode());
        assert_eq!(response.header("Content-Length"), Header::One("5"));
        assert_eq!(response.header("CONTENT-TYPE"), Header::One("text/plain"));
        assert_eq!(
            response.header("date"),
            Header::One("Tue, 17 Nov 2026 09:30:00 GMT")
        );
        assert_eq!(response.header("x-empty"), Header::One(""));
        assert_eq!(response.header("Server"), Header::Absent);
        // Every call got the caller's deadline.
        assert!(wire.deadlines.iter().all(|deadline| *deadline == 90));
    }
    // What follows the body is not read, when the bytes come one at a time.
    let mut wire = Wire::new(answer);
    wire.chunk = 1;
    exchange(&mut wire, &GET, 90).unwrap();
    assert_eq!(wire.given, answer.len() - " and more".len());
}

#[test]
fn a_header_written_twice_has_no_single_value() {
    let response =
        get("HTTP/1.1 200 OK\r\nDate: a\r\nContent-Length: 0\r\ndate: b\r\n\r\n").unwrap();
    assert_eq!(response.header("Date"), Header::Repeated);
    assert_eq!(response.header("content-length"), Header::One("0"));
}

#[test]
fn the_recorded_answers_are_read() {
    for (answer, status, length) in [
        (recorded::openstack_meta_data(), 200, 427),
        (recorded::openstack_user_data(), 200, 82),
        (recorded::openstack_no_user_data(), 404, 154),
        (recorded::openstack_azure_path(), 404, 154),
        (recorded::azure_instance(), 200, 1683),
        (recorded::azure_no_header(), 400, 66),
        (recorded::azure_unknown_path(), 404, 24),
    ] {
        let response = get(answer).unwrap();
        assert_eq!(response.status, status);
        assert_eq!(response.body.len(), length);
        assert!(matches!(response.header("Date"), Header::One(_)));
    }
}

#[test]
fn statuses_without_a_body_need_no_length() {
    for status in ["100 Continue", "204 No Content", "304 Not Modified"] {
        let response = get(format!("HTTP/1.1 {status}\r\nServer: x\r\n\r\n")).unwrap();
        assert!(response.body.is_empty(), "{status}");
        // And a length they do carry is not a body to wait for.
        let response = get(format!("HTTP/1.1 {status}\r\nContent-Length: 9\r\n\r\n")).unwrap();
        assert!(response.body.is_empty(), "{status}");
    }
    // A status line without a reason, and one with no header at all.
    assert_eq!(
        get("HTTP/1.1 404\r\nContent-Length: 0\r\n\r\n")
            .unwrap()
            .status,
        404
    );
    assert_eq!(get("HTTP/1.1 204 No Content\r\n\r\n").unwrap().status, 204);
}

#[test]
fn a_redirect_is_an_answer_and_is_not_followed() {
    let mut wire = Wire::new(
        "HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/elsewhere\r\n\
         Content-Length: 0\r\n\r\n",
    );
    let response = exchange(&mut wire, &GET, 90).unwrap();
    assert_eq!(response.status, 302);
    assert_eq!(
        response.header("location"),
        Header::One("http://169.254.169.254/elsewhere")
    );
    // One request went out, and it was the one asked for.
    assert_eq!(wire.sent, GET.encode());
}

#[test]
fn an_answer_that_is_not_http_1_1_is_refused() {
    let first = "its first line is not an HTTP/1.1 status line";
    for line in [
        "HTTP/1.0 200 OK",
        "HTTP/2 200",
        "http/1.1 200 OK",
        "HTTP/1.1  200 OK",
        "HTTP/1.1 20 OK",
        "HTTP/1.1 2000 OK",
        "HTTP/1.1 200OK",
        "HTTP/1.1 +20 OK",
        "HTTP/1.1 099 Low",
        "HTTP/1.1 600 High",
        "HTTP/1.1 2\u{e9} OK",
        "HTTP/1.1",
        "HTTP/1.1 ",
        "",
        "200 OK",
        "HTTP/1.1 200 OK\nContent-Length: 0",
        "HTTP/1.1 200 O\rK",
    ] {
        let answer = format!("{line}\r\nContent-Length: 0\r\n\r\n");
        if line.is_ascii() {
            assert_eq!(malformed(&answer), first, "{line:?}");
        } else {
            assert_eq!(
                malformed(&answer),
                "a byte in its head is not printable ASCII"
            );
        }
    }
    for (headers, what) in [
        ("Server: x", "it has no Content-Length"),
        (
            "Content-Length: 1\r\nContent-Length: 1",
            "it has more than one Content-Length",
        ),
        ("Content-Length: abc", "its Content-Length is not a number"),
        ("Content-Length:", "its Content-Length is not a number"),
        ("Content-Length: -1", "its Content-Length is not a number"),
        ("Content-Length: +1", "its Content-Length is not a number"),
        ("Content-Length: 1 2", "its Content-Length is not a number"),
        ("Content-Length: 0x10", "its Content-Length is not a number"),
        (
            "Transfer-Encoding: gzip",
            "its Transfer-Encoding is not chunked alone",
        ),
        (
            "Transfer-Encoding: gzip, chunked",
            "its Transfer-Encoding is not chunked alone",
        ),
        (
            "Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked",
            "it has more than one Transfer-Encoding",
        ),
        (
            "Content-Length: 5\r\ntransfer-encoding: chunked",
            "it has both a Transfer-Encoding and a Content-Length",
        ),
        (
            "Transfer-Encoding: identity",
            "its Transfer-Encoding is not chunked alone",
        ),
        ("Content-Length 5", "a header line has no colon"),
        ("Content Length: 5", "a header's name is not a token"),
        (": 5", "a header's name is not a token"),
        ("Content-Length: 5\r\n folded", "a header line has no colon"),
        (
            "X-Folded: a\r\n\tb: c\r\nContent-Length: 5",
            "a header's name is not a token",
        ),
        ("X: a\nContent-Length: 0", "a line ends without CRLF"),
        ("X: a\rb\r\nContent-Length: 0", "a line ends without CRLF"),
        ("X\r: a", "a header's name is not a token"),
        ("X: caf\u{e9}", "a byte in its head is not printable ASCII"),
        ("X: a\u{0}b", "a byte in its head is not printable ASCII"),
    ] {
        let answer = format!("HTTP/1.1 200 OK\r\n{headers}\r\n\r\nhello");
        assert_eq!(malformed(&answer), what, "{headers:?}");
    }
}

#[test]
fn an_answer_cut_short_is_no_answer() {
    for answer in [
        "",
        "HTTP/1.1 200 OK",
        "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n",
        "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n",
        "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhell",
        // Line ends that are never CRLF never end the head.
        "HTTP/1.1 200 OK\nContent-Length: 0\n\n",
    ] {
        assert_eq!(get(answer), Err(HttpError::Closed), "{answer:?}");
    }
    // A connection that breaks is told apart from one that closes.
    let mut wire = Wire::new("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhe");
    wire.end = Some(StreamError::Reset);
    assert_eq!(
        exchange(&mut wire, &GET, 90),
        Err(HttpError::Stream(StreamError::Reset))
    );
    let mut wire = Wire::new("HTTP/1.1 200");
    wire.end = Some(StreamError::Deadline);
    assert_eq!(
        exchange(&mut wire, &GET, 90),
        Err(HttpError::Stream(StreamError::Deadline))
    );
}

/// A connection that cannot be written to.
struct Unwritable(Result<usize, StreamError>);

impl Stream for Unwritable {
    fn write(&mut self, _: &[u8], _: u64) -> Result<usize, StreamError> {
        self.0.clone()
    }

    fn read(&mut self, _: &mut [u8], _: u64) -> Result<usize, StreamError> {
        panic!("an answer was read before the request was sent");
    }
}

#[test]
fn a_request_that_cannot_be_sent_is_no_answer() {
    assert_eq!(
        exchange(&mut Unwritable(Ok(0)), &GET, 90),
        Err(HttpError::Closed)
    );
    assert_eq!(
        exchange(&mut Unwritable(Err(StreamError::Reset)), &GET, 90),
        Err(HttpError::Stream(StreamError::Reset))
    );
}

#[test]
fn an_answer_is_at_most_1_mib() {
    assert_eq!(RESPONSE_MAX, 1 << 20);
    let head = "HTTP/1.1 200 OK\r\nContent-Length: ";
    let with_body = |body: usize| {
        let mut answer = format!("{head}{body}\r\n\r\n").into_bytes();
        let head = answer.len();
        answer.resize(head + body, b'x');
        (answer, head)
    };
    // Exactly 1 MiB, head and body together, is read.
    let (_, head_bytes) = with_body(1_000_000);
    let (answer, _) = with_body(RESPONSE_MAX - head_bytes);
    assert_eq!(answer.len(), RESPONSE_MAX);
    let mut wire = Wire::new(answer);
    wire.chunk = 65536;
    let response = exchange(&mut wire, &GET, 90).unwrap();
    assert_eq!(response.body.len(), RESPONSE_MAX - head_bytes);
    // One byte more is refused from its head alone: the body is not read.
    let (answer, head_bytes) = with_body(RESPONSE_MAX - head_bytes + 1);
    let mut wire = Wire::new(answer);
    wire.chunk = 64;
    assert_eq!(exchange(&mut wire, &GET, 90), Err(HttpError::TooLarge));
    assert!(wire.given <= head_bytes + 64);
    // A length too long to be a number.
    assert_eq!(
        get(format!("{head}{}\r\n\r\n", "9".repeat(40))),
        Err(HttpError::TooLarge)
    );
    // A head that never ends.
    let mut endless = b"HTTP/1.1 200 OK\r\n".to_vec();
    endless.resize(RESPONSE_MAX + 4096, b'x');
    let mut wire = Wire::new(endless);
    assert_eq!(exchange(&mut wire, &GET, 90), Err(HttpError::TooLarge));
    assert_eq!(wire.given, RESPONSE_MAX);
}

const CHUNKED: &str = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";

/// An answer in chunks: `body` after a head that says so.
fn chunked(body: impl AsRef<[u8]>) -> Result<Response, HttpError> {
    let mut answer = CHUNKED.as_bytes().to_vec();
    answer.extend_from_slice(body.as_ref());
    get(answer)
}

fn chunked_malformed(body: impl AsRef<[u8]>) -> &'static str {
    match chunked(body.as_ref()) {
        Err(HttpError::Malformed(what)) => what,
        other => panic!(
            "{:?} gave {other:?}",
            String::from_utf8_lossy(body.as_ref())
        ),
    }
}

#[test]
fn a_chunked_answer_is_read_whole() {
    let body = "4\r\nWiki\r\n6\r\npedia \r\nE\r\nin \r\n\r\nchunks.\r\n0\r\n\r\n";
    for chunk in [1, 2, 3, 7, 4096, usize::MAX] {
        let mut wire = Wire::new(format!("{CHUNKED}{body}"));
        wire.chunk = chunk;
        let response = exchange(&mut wire, &GET, 90).unwrap();
        // The data, CRLFs inside it included, and nothing of the framing.
        assert_eq!(
            response.body, b"Wikipedia in \r\n\r\nchunks.",
            "in reads of {chunk}"
        );
        assert_eq!(response.header("transfer-encoding"), Header::One("chunked"));
        assert_eq!(response.status, 200);
    }
    // The coding's name in any case; sizes in hex of either case, with leading zeros.
    let response = get(
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: Chunked\r\n\r\n000A\r\n0123456789\r\nb\r\n\
         abcdefghijk\r\n00\r\n\r\n",
    )
    .unwrap();
    assert_eq!(response.body, b"0123456789abcdefghijk");
    // No data at all.
    assert_eq!(chunked("0\r\n\r\n").unwrap().body, b"");
    // What follows the empty line after the last chunk is not the body's.
    assert_eq!(
        chunked("1\r\nx\r\n0\r\n\r\nHTTP/1.1 200 OK\r\n")
            .unwrap()
            .body,
        b"x"
    );
    // A status that has no body has none, chunked or not.
    let response = get("HTTP/1.1 204 No Content\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
    assert_eq!((response.status, response.body.len()), (204, 0));
    // A chunk's data is any bytes.
    let mut binary = b"3\r\n".to_vec();
    binary.extend_from_slice(&[0, 0xff, b'\n']);
    binary.extend_from_slice(b"\r\n0\r\n\r\n");
    assert_eq!(chunked(binary).unwrap().body, [0, 0xff, b'\n']);
}

#[test]
fn chunk_extensions_are_read_past() {
    // Ignored, as RFC 9112 asks of a recipient that does not know them: with or without a
    // value, a quoted value with a `;` in it, blank space before them, and on the last chunk.
    let body = "5;name=value\r\nhello\r\n1 ; a ; b=\"q;uoted\" \r\n!\r\n2\t;x\r\n:)\r\n\
                0;last=1\r\n\r\n";
    assert_eq!(chunked(body).unwrap().body, b"hello!:)");
}

#[test]
fn trailers_are_read_and_dropped() {
    let body = "5\r\nhello\r\n0\r\nExpires: Wed, 21 Oct 2026 07:28:00 GMT\r\nX-Sum:abc\r\n\r\n";
    let response = chunked(body).unwrap();
    assert_eq!(response.body, b"hello");
    // A trailer is not one of the answer's headers.
    assert_eq!(response.header("expires"), Header::Absent);
    // A trailer is a header line, or the answer is refused.
    for (trailer, what) in [
        ("no colon", "a header line has no colon"),
        ("Bad Name: x", "a header's name is not a token"),
        (" folded: x", "a header's name is not a token"),
        ("X: caf\u{e9}", "a byte in a trailer is not printable ASCII"),
        ("X: a\u{0}b", "a byte in a trailer is not printable ASCII"),
    ] {
        assert_eq!(
            chunked_malformed(format!("0\r\n{trailer}\r\n\r\n")),
            what,
            "{trailer:?}"
        );
    }
}

#[test]
fn a_bad_chunk_size_is_refused() {
    for size in ["", "g", "-1", "+1", "0x4", " 4", "4 4", ";x", "4,5"] {
        assert_eq!(
            chunked_malformed(format!("{size}\r\nhell\r\n0\r\n\r\n")),
            "a chunk's size is not hexadecimal",
            "{size:?}"
        );
    }
    assert_eq!(
        chunked_malformed("4\u{e9}\r\nhell\r\n0\r\n\r\n"),
        "a byte in a chunk's size line is not printable ASCII"
    );
    assert_eq!(
        chunked_malformed("4\u{0}\r\nhell\r\n0\r\n\r\n"),
        "a byte in a chunk's size line is not printable ASCII"
    );
    // Sizes over the limit, however they are written, before any of their data is read.
    for size in [
        "100001",
        "fffffffff",
        "ffffffffffffffffffff",
        "7fffffffffffffff",
    ] {
        let mut wire = Wire::new(format!("{CHUNKED}{size}\r\n{}", "x".repeat(5000)));
        assert_eq!(
            exchange(&mut wire, &GET, 90),
            Err(HttpError::TooLarge),
            "{size}"
        );
    }
    // Leading zeros are not digits that count.
    assert_eq!(
        chunked("00000000000000000004\r\nhell\r\n0\r\n\r\n")
            .unwrap()
            .body,
        b"hell"
    );
    // Data that runs past its size.
    assert_eq!(
        chunked_malformed("4\r\nhello\r\n0\r\n\r\n"),
        "a chunk's data does not end with CRLF"
    );
    assert_eq!(
        chunked_malformed("4\r\nhel\r\n0\r\n\r\n"),
        "a chunk's data does not end with CRLF"
    );
}

#[test]
fn a_chunked_answer_cut_short_is_no_answer() {
    for body in [
        "",
        "4",
        "4\r",
        "4\r\nhel",
        "4\r\nhell",
        "4\r\nhell\r\n",
        "4\r\nhell\r\n0",
        "4\r\nhell\r\n0\r\n",
        "4\r\nhell\r\n0\r\nX: y\r\n",
    ] {
        assert_eq!(chunked(body), Err(HttpError::Closed), "{body:?}");
    }
    let mut wire = Wire::new(format!("{CHUNKED}4\r\nhe"));
    wire.end = Some(StreamError::Reset);
    assert_eq!(
        exchange(&mut wire, &GET, 90),
        Err(HttpError::Stream(StreamError::Reset))
    );
}

#[test]
fn a_chunked_answer_is_at_most_1_mib_framing_included() {
    // One chunk that makes the whole answer exactly 1 MiB is read.
    let size_line = |size: usize| format!("{size:x}\r\n");
    let fixed = CHUNKED.len() + size_line(0x10000).len() + "\r\n0\r\n\r\n".len();
    let size = RESPONSE_MAX - fixed;
    let whole = |size: usize| {
        let mut answer = format!("{CHUNKED}{}", size_line(size)).into_bytes();
        answer.resize(answer.len() + size, b'x');
        answer.extend_from_slice(b"\r\n0\r\n\r\n");
        answer
    };
    let answer = whole(size);
    assert_eq!(answer.len(), RESPONSE_MAX);
    let mut wire = Wire::new(answer);
    wire.chunk = 65536;
    assert_eq!(exchange(&mut wire, &GET, 90).unwrap().body.len(), size);
    // One byte more is refused: the answer is over 1 MiB.
    let mut wire = Wire::new(whole(size + 1));
    wire.chunk = 65536;
    assert_eq!(exchange(&mut wire, &GET, 90), Err(HttpError::TooLarge));
    // Many small chunks count their framing too: 1 MiB of 1-byte chunks is three times that.
    let mut many = CHUNKED.as_bytes().to_vec();
    for _ in 0..RESPONSE_MAX / 6 + 1 {
        many.extend_from_slice(b"1\r\nx\r\n");
    }
    many.extend_from_slice(b"0\r\n\r\n");
    let mut wire = Wire::new(many);
    wire.chunk = 65536;
    assert_eq!(exchange(&mut wire, &GET, 90), Err(HttpError::TooLarge));
    assert_eq!(wire.given, RESPONSE_MAX);
    // A size line, or a trailer, that never ends.
    for body in [
        "4".to_string(),
        format!("0\r\nX: {}", "y".repeat(RESPONSE_MAX)),
    ] {
        let mut wire = Wire::new(format!("{CHUNKED}{body}{}", "z".repeat(RESPONSE_MAX)));
        wire.chunk = 65536;
        assert_eq!(exchange(&mut wire, &GET, 90), Err(HttpError::TooLarge));
        assert_eq!(wire.given, RESPONSE_MAX);
    }
}

#[test]
fn errors_say_what_they_are() {
    let lines = vec![
        HttpError::Stream(StreamError::Refused).to_string(),
        HttpError::Stream(StreamError::TimedOut).to_string(),
        HttpError::Stream(StreamError::Unreachable).to_string(),
        HttpError::Stream(StreamError::Reset).to_string(),
        HttpError::Stream(StreamError::Deadline).to_string(),
        HttpError::Stream(StreamError::Other("something else".into())).to_string(),
        HttpError::Closed.to_string(),
        HttpError::TooLarge.to_string(),
        HttpError::Malformed("it has no Content-Length").to_string(),
    ];
    assert_eq!(
        lines,
        [
            "connection refused",
            "connection timed out",
            "no route to the address",
            "connection reset",
            "the deadline passed",
            "something else",
            "the connection closed before a whole answer",
            "the answer is over 1 MiB",
            "the answer is not HTTP/1.1: it has no Content-Length",
        ]
    );
}
