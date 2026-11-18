//! The node's minimal HTTP/1.1 client (work item 1.4, step 1), shared by `platform`, which asks
//! instance data in plain HTTP, and `node-admission`, which fetches function images over HTTPS
//! with the network server's TLS (system design 3D, 3E): one `GET` or `POST` a connection, the
//! answer's body by its `Content-Length` or in chunks (`Transfer-Encoding: chunked`, which every
//! HTTP/1.1 client must read; PM, item 1.12). No other transfer coding, no redirects, no
//! keep-alive, and at most [`RESPONSE_MAX`] of answer, head and body together, framing
//! included, unless the caller gives its own limit ([`exchange_within`]).
//!
//! The client is written against [`Stream`], a connection as bytes; [`net`] gives the one through
//! the network server's sockets, by address or by name, with TLS when asked. `no_std`, with
//! `alloc`.
#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

extern crate alloc;
// The tests run on a host, with the network server's own logic over loopback.
#[cfg(test)]
extern crate std;

pub mod net;

use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use core::{fmt, ops::Range};

/// Why a connection was not made, or broke.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamError {
    /// The peer refused the connection.
    Refused,
    /// The connection was not established in time, as the network server judges it.
    TimedOut,
    /// No interface reaches the address: the node has no address yet.
    Unreachable,
    /// The peer reset the connection.
    Reset,
    /// The caller's deadline passed first.
    Deadline,
    /// Anything else, in words.
    Other(String),
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StreamError::Refused => f.write_str("connection refused"),
            StreamError::TimedOut => f.write_str("connection timed out"),
            StreamError::Unreachable => f.write_str("no route to the address"),
            StreamError::Reset => f.write_str("connection reset"),
            StreamError::Deadline => f.write_str("the deadline passed"),
            StreamError::Other(why) => f.write_str(why),
        }
    }
}

/// One TCP connection, as bytes. Dropping it closes it.
pub trait Stream {
    /// Sends some of `bytes`, waiting for room until `deadline` (the monotonic clock, in
    /// nanoseconds): how many were taken, at least one.
    fn write(&mut self, bytes: &[u8], deadline: u64) -> Result<usize, StreamError>;

    /// Receives into `buffer`, waiting for bytes until `deadline`: how many arrived, and 0 once
    /// the peer has finished sending.
    fn read(&mut self, buffer: &mut [u8], deadline: u64) -> Result<usize, StreamError>;
}

/// The most bytes of an answer, head and body together, unless a caller says otherwise
/// ([`exchange_within`]): 1 MiB.
pub const RESPONSE_MAX: usize = 1 << 20;

/// A request: `GET`, or `POST` with a body.
#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
    host: &'a str,
    path: &'a str,
    headers: &'a [(&'a str, &'a str)],
    body: Option<&'a [u8]>,
}

impl<'a> Request<'a> {
    /// `GET path` from `host`, with `headers` besides `Host` and `Connection`.
    pub const fn get(host: &'a str, path: &'a str, headers: &'a [(&'a str, &'a str)]) -> Self {
        Request {
            host,
            path,
            headers,
            body: None,
        }
    }

    /// `POST path` to `host` with `body`, whose length is sent as `Content-Length`.
    pub const fn post(
        host: &'a str,
        path: &'a str,
        headers: &'a [(&'a str, &'a str)],
        body: &'a [u8],
    ) -> Self {
        Request {
            host,
            path,
            headers,
            body: Some(body),
        }
    }

    /// The request's method.
    pub const fn method(&self) -> &'static str {
        match self.body {
            None => "GET",
            Some(_) => "POST",
        }
    }

    /// The request's path.
    pub const fn path(&self) -> &'a str {
        self.path
    }

    /// The request as it goes on the wire. The connection is closed after the answer.
    pub fn encode(&self) -> Vec<u8> {
        let mut wire = Vec::new();
        let mut line = |parts: &[&str]| {
            for part in parts {
                wire.extend_from_slice(part.as_bytes());
            }
            wire.extend_from_slice(b"\r\n");
        };
        line(&[self.method(), " ", self.path, " HTTP/1.1"]);
        line(&["Host: ", self.host]);
        line(&["Connection: close"]);
        for &(name, value) in self.headers {
            line(&[name, ": ", value]);
        }
        if let Some(body) = self.body {
            line(&["Content-Length: ", &body.len().to_string()]);
        }
        line(&[]);
        wire.extend_from_slice(self.body.unwrap_or_default());
        wire
    }
}

/// An answer: its status, its headers and its whole body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    /// The status code.
    pub status: u16,
    headers: Vec<(String, String)>,
    /// The body, `Content-Length` bytes of it.
    pub body: Vec<u8>,
}

/// What an answer says under one header name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Header<'a> {
    /// The answer has no such header.
    Absent,
    /// The header's one value, without the blank space around it.
    One(&'a str),
    /// The answer has the header more than once, so it has no single value.
    Repeated,
}

impl Response {
    /// The header `name`, whatever its case.
    pub fn header(&self, name: &str) -> Header<'_> {
        let mut values = self
            .headers
            .iter()
            .filter(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str());
        match (values.next(), values.next()) {
            (None, _) => Header::Absent,
            (Some(value), None) => Header::One(value),
            (Some(_), Some(_)) => Header::Repeated,
        }
    }
}

/// Why a request got no answer this client reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpError {
    /// The connection failed.
    Stream(StreamError),
    /// The server closed the connection before a whole answer.
    Closed,
    /// The answer is over its limit, in bytes: [`RESPONSE_MAX`], or what the caller said.
    TooLarge(usize),
    /// The answer is not HTTP/1.1 as this client reads it; what is wrong with it.
    Malformed(&'static str),
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HttpError::Stream(error) => error.fmt(f),
            HttpError::Closed => f.write_str("the connection closed before a whole answer"),
            HttpError::TooLarge(limit) if limit % (1 << 20) == 0 => {
                write!(f, "the answer is over {} MiB", limit >> 20)
            }
            HttpError::TooLarge(limit) => write!(f, "the answer is over {limit} bytes"),
            HttpError::Malformed(what) => write!(f, "the answer is not HTTP/1.1: {what}"),
        }
    }
}

impl From<StreamError> for HttpError {
    fn from(error: StreamError) -> HttpError {
        HttpError::Stream(error)
    }
}

const HEAD_END: &[u8] = b"\r\n\r\n";

/// The answer as it arrives: what has been read of it, from its connection, by a deadline.
struct Reader<'s, S> {
    stream: &'s mut S,
    deadline: u64,
    bytes: Vec<u8>,
    /// The most bytes of the answer, head and body together.
    limit: usize,
}

impl<S: Stream> Reader<'_, S> {
    /// Reads once more, never past `limit` bytes of answer in all, nor past the answer's limit.
    fn more(&mut self, limit: usize) -> Result<(), HttpError> {
        let limit = limit.min(self.limit);
        if self.bytes.len() >= limit {
            return Err(HttpError::TooLarge(self.limit));
        }
        let mut chunk = [0u8; 4096];
        let room = chunk.len().min(limit - self.bytes.len());
        match self.stream.read(&mut chunk[..room], self.deadline)? {
            0 => Err(HttpError::Closed),
            count => {
                self.bytes.extend_from_slice(&chunk[..count.min(room)]);
                Ok(())
            }
        }
    }

    /// Reads until the answer is `end` bytes long, and no further.
    fn fill(&mut self, end: usize) -> Result<(), HttpError> {
        while self.bytes.len() < end {
            self.more(end)?;
        }
        Ok(())
    }

    /// The line that begins at `at`, without its CRLF; `at` moves past the CRLF.
    fn line(&mut self, at: &mut usize) -> Result<Range<usize>, HttpError> {
        let mut searched = *at;
        loop {
            if let Some(found) = self.bytes[searched..]
                .windows(2)
                .position(|window| window == b"\r\n")
            {
                let line = *at..searched + found;
                *at = searched + found + 2;
                return Ok(line);
            }
            // A CR at the end may be the first half of the next read's CRLF.
            searched = self.bytes.len().saturating_sub(1).max(*at);
            self.more(self.limit)?;
        }
    }
}

/// Sends `request` on a fresh connection and reads its answer, all by `deadline`, at most
/// [`RESPONSE_MAX`] of it.
pub fn exchange<S: Stream>(
    stream: &mut S,
    request: &Request<'_>,
    deadline: u64,
) -> Result<Response, HttpError> {
    exchange_within(stream, request, deadline, RESPONSE_MAX)
}

/// As [`exchange`], with at most `limit` bytes of answer, head and body together, framing
/// included: a longer one is [`HttpError::TooLarge`], and no more of it is read.
pub fn exchange_within<S: Stream>(
    stream: &mut S,
    request: &Request<'_>,
    deadline: u64,
    limit: usize,
) -> Result<Response, HttpError> {
    let wire = request.encode();
    let mut sent = 0;
    while sent < wire.len() {
        match stream.write(&wire[sent..], deadline)? {
            0 => return Err(HttpError::Closed),
            taken => sent += taken.min(wire.len() - sent),
        }
    }

    // The head: everything up to the first empty line.
    let mut answer = Reader {
        stream,
        deadline,
        bytes: Vec::new(),
        limit,
    };
    let mut searched = 0;
    let head_end = loop {
        if let Some(at) = answer.bytes[searched..]
            .windows(HEAD_END.len())
            .position(|window| window == HEAD_END)
        {
            break searched + at;
        }
        searched = answer.bytes.len().saturating_sub(HEAD_END.len() - 1);
        answer.more(limit)?;
    };
    let (status, headers) = head(&answer.bytes[..head_end])?;
    let mut response = Response {
        status,
        headers,
        body: Vec::new(),
    };

    let body_start = head_end + HEAD_END.len();
    // A length too long to be a number is over this answer's limit, whatever it is.
    let framing = framing(&response).map_err(|error| match error {
        HttpError::TooLarge(_) => HttpError::TooLarge(limit),
        error => error,
    })?;
    response.body = match framing {
        // Exactly Content-Length bytes. What follows them is not read.
        Framing::Length(length) => {
            let end = body_start
                .checked_add(length)
                .filter(|end| *end <= limit)
                .ok_or(HttpError::TooLarge(limit))?;
            answer.fill(end)?;
            answer.bytes[body_start..end].to_vec()
        }
        Framing::Chunked => chunked(&mut answer, body_start)?,
    };
    Ok(response)
}

/// A head's status and headers. `bytes` ends before the empty line.
fn head(bytes: &[u8]) -> Result<(u16, Vec<(String, String)>), HttpError> {
    let text = printable(bytes).ok_or(HttpError::Malformed(
        "a byte in its head is not printable ASCII",
    ))?;
    let mut lines = text.split("\r\n");
    let status = status(lines.next().unwrap_or_default())?;
    let headers = lines.map(field).collect::<Result<_, _>>()?;
    Ok((status, headers))
}

/// `bytes` as text, when every byte is printable ASCII, a tab, or a line end.
fn printable(bytes: &[u8]) -> Option<&str> {
    let allowed = |byte: &u8| matches!(byte, b'\r' | b'\n' | b'\t' | 0x20..=0x7e);
    core::str::from_utf8(bytes)
        .ok()
        .filter(|_| bytes.iter().all(allowed))
}

/// One header line, or one trailer line: a name that is a token, a colon, and its value
/// without the blank space around it.
fn field(line: &str) -> Result<(String, String), HttpError> {
    let (name, value) = line
        .split_once(':')
        .ok_or(HttpError::Malformed("a header line has no colon"))?;
    // A name is a token, so a folded line (one that begins with blank space) is refused too.
    let token = |byte: u8| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte);
    if name.is_empty() || !name.bytes().all(token) {
        return Err(HttpError::Malformed("a header's name is not a token"));
    }
    if value.contains(['\r', '\n']) {
        return Err(HttpError::Malformed("a line ends without CRLF"));
    }
    Ok((
        name.to_string(),
        value.trim_matches([' ', '\t']).to_string(),
    ))
}

/// The status of `HTTP/1.1 <three digits>`, with or without a reason after it.
fn status(line: &str) -> Result<u16, HttpError> {
    let malformed = HttpError::Malformed("its first line is not an HTTP/1.1 status line");
    let Some(rest) = line.strip_prefix("HTTP/1.1 ") else {
        return Err(malformed);
    };
    let (code, reason) = rest.split_at_checked(3).ok_or(malformed.clone())?;
    let digits = code.bytes().all(|byte| byte.is_ascii_digit());
    let ended = reason.is_empty() || reason.starts_with(' ');
    let lines = !reason.contains(['\r', '\n']);
    match code.parse::<u16>() {
        Ok(status @ 100..=599) if digits && ended && lines => Ok(status),
        _ => Err(malformed),
    }
}

/// How a body follows the head.
enum Framing {
    /// This many bytes.
    Length(usize),
    /// In chunks, to the last chunk and the trailers.
    Chunked,
}

/// How the body of `response` follows its head (RFC 9112, section 6.3).
fn framing(response: &Response) -> Result<Framing, HttpError> {
    // These statuses never have a body, whatever their headers say.
    let bodiless = response.status < 200 || response.status == 204 || response.status == 304;
    match response.header("transfer-encoding") {
        Header::Absent => {}
        Header::Repeated => {
            return Err(HttpError::Malformed(
                "it has more than one Transfer-Encoding",
            ));
        }
        Header::One(coding) if !coding.eq_ignore_ascii_case("chunked") => {
            return Err(HttpError::Malformed(
                "its Transfer-Encoding is not chunked alone",
            ));
        }
        // Both is how a request is smuggled past one reader and not another: refused.
        Header::One(_) if response.header("content-length") != Header::Absent => {
            return Err(HttpError::Malformed(
                "it has both a Transfer-Encoding and a Content-Length",
            ));
        }
        Header::One(_) if bodiless => return Ok(Framing::Length(0)),
        Header::One(_) => return Ok(Framing::Chunked),
    }
    if bodiless {
        return Ok(Framing::Length(0));
    }
    match response.header("content-length") {
        Header::Absent => Err(HttpError::Malformed("it has no Content-Length")),
        Header::Repeated => Err(HttpError::Malformed("it has more than one Content-Length")),
        Header::One(text) if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) => {
            Err(HttpError::Malformed("its Content-Length is not a number"))
        }
        // A length too long to be a number is certainly over the limit.
        Header::One(text) => text
            .parse()
            .map(Framing::Length)
            .map_err(|_| HttpError::TooLarge(usize::MAX)),
    }
}

/// A chunked body that begins at `at` (RFC 9112, section 7.1): each chunk's size in hex, with
/// any extensions after a `;`, which are ignored as the RFC asks; its data and a CRLF; then the
/// last chunk, of size 0, any trailers, which are read and dropped, and an empty line. Framing,
/// data and trailers count against the answer's limit with the head.
fn chunked<S: Stream>(answer: &mut Reader<'_, S>, mut at: usize) -> Result<Vec<u8>, HttpError> {
    let mut body = Vec::new();
    loop {
        let line = answer.line(&mut at)?;
        let size = chunk_size(&answer.bytes[line], answer.limit)?;
        if size == 0 {
            break;
        }
        let end = at
            .checked_add(size)
            .and_then(|data_end| data_end.checked_add(2))
            .filter(|end| *end <= answer.limit)
            .ok_or(HttpError::TooLarge(answer.limit))?;
        answer.fill(end)?;
        if &answer.bytes[end - 2..end] != b"\r\n" {
            return Err(HttpError::Malformed(
                "a chunk's data does not end with CRLF",
            ));
        }
        body.extend_from_slice(&answer.bytes[at..end - 2]);
        at = end;
    }
    loop {
        let line = answer.line(&mut at)?;
        if line.is_empty() {
            return Ok(body);
        }
        let text = printable(&answer.bytes[line]).ok_or(HttpError::Malformed(
            "a byte in a trailer is not printable ASCII",
        ))?;
        field(text)?;
    }
}

/// The size on a chunk's line, at most `limit`: hex digits, then blank space and extensions, if
/// any.
fn chunk_size(line: &[u8], limit: usize) -> Result<usize, HttpError> {
    let text = printable(line).ok_or(HttpError::Malformed(
        "a byte in a chunk's size line is not printable ASCII",
    ))?;
    let size = text.split_once(';').map_or(text, |(size, _)| size);
    let size = size.trim_end_matches([' ', '\t']);
    if size.is_empty() || !size.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(HttpError::Malformed("a chunk's size is not hexadecimal"));
    }
    // Leading zeros say nothing; more than sixteen digits is past any limit.
    let digits = size.trim_start_matches('0');
    if digits.is_empty() {
        return Ok(0);
    }
    match usize::from_str_radix(digits, 16) {
        Ok(size) if digits.len() <= 16 && size <= limit => Ok(size),
        _ => Err(HttpError::TooLarge(limit)),
    }
}

#[cfg(test)]
mod tests;
