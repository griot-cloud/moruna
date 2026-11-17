//! HTTP/1.1 over one connection's decrypted stream (system design, section 2B): requests read
//! through `httparse`, `Content-Length` bodies only, keep-alive, `Connection: close` honoured,
//! and the limits on headers and bodies. No HTTP/2, compression, cookies or cross-origin
//! headers.

use crate::api::{Answer, Api};
use alloc::{format, string::String, vec, vec::Vec};
use moruna_api::ErrorCode;

/// The most bytes a request's head may take: its request line, headers and the blank line.
pub const HEADER_LIMIT: usize = 16 * 1024;
/// The most bytes a request's body may take.
pub const BODY_LIMIT: usize = 1024 * 1024;
/// The decrypted bytes a session reads ahead to before it stops asking for more: one request at
/// its limits.
const INPUT_LIMIT: usize = HEADER_LIMIT + BODY_LIMIT;

/// What a request's `Authorization` headers held.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Authorization {
    /// None.
    Missing,
    /// One, with this value.
    One(Vec<u8>),
    /// More than one.
    Many,
}

/// One request, read whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Request {
    /// The method, as sent.
    pub method: String,
    /// The path, without its query.
    pub path: String,
    /// Whether the target carried a query (a `?`).
    pub query: bool,
    /// The `Authorization` header.
    pub authorization: Authorization,
    /// Whether the client asked for the connection to close after this answer.
    pub close: bool,
    /// The body.
    pub body: Vec<u8>,
}

/// What reading the front of the stream gave.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Read {
    /// More bytes are needed.
    Incomplete,
    /// A whole request, which took this many bytes.
    Request(Request, usize),
    /// The stream is not HTTP/1.1 as this server reads it, or a request is over a limit. The
    /// answer is `invalid` with this message, then the connection closes.
    Refused(&'static str),
}

/// A request head read whole whose body has not all arrived.
#[derive(Debug)]
struct Head {
    request: Request,
    length: usize,
    body: usize,
}

/// Reads requests from the front of a stream, one at a time.
#[derive(Debug, Default)]
pub(crate) struct Reader {
    /// How far the search for the end of the head has looked, so that bytes arriving one at a
    /// time are not searched again and again.
    searched: usize,
    /// The head of the request whose body is still arriving.
    head: Option<Head>,
}

/// Whether `bytes` holds an empty line: a line feed followed by another, or by a carriage return
/// and another.
fn has_empty_line(bytes: &[u8]) -> bool {
    bytes.windows(2).any(|w| w == b"\n\n") || bytes.windows(3).any(|w| w == b"\n\r\n")
}

/// Whether `name` is `expected`, ignoring ASCII case.
fn named(name: &str, expected: &str) -> bool {
    name.eq_ignore_ascii_case(expected)
}

/// `value` without the spaces and tabs around it.
fn trim(value: &[u8]) -> &[u8] {
    value.trim_ascii()
}

impl Reader {
    /// Reads the request at the front of `input`.
    pub(crate) fn read(&mut self, input: &[u8]) -> Read {
        if self.head.is_none() {
            match self.read_head(input) {
                Ok(Some(head)) => self.head = Some(head),
                Ok(None) => return Read::Incomplete,
                Err(message) => return Read::Refused(message),
            }
        }
        let Some(head) = self.head.as_ref() else {
            return Read::Incomplete;
        };
        let end = head.length + head.body;
        if input.len() < end {
            return Read::Incomplete;
        }
        let Some(Head {
            mut request,
            length,
            ..
        }) = self.head.take()
        else {
            return Read::Incomplete;
        };
        request.body = input[length..end].to_vec();
        self.searched = 0;
        Read::Request(request, end)
    }

    /// The head at the front of `input`, `None` while it has not all arrived, or why it is
    /// refused.
    fn read_head(&mut self, input: &[u8]) -> Result<Option<Head>, &'static str> {
        let window = &input[..input.len().min(HEADER_LIMIT)];
        let from = self.searched.saturating_sub(2);
        if !has_empty_line(&window[from..]) {
            self.searched = window.len();
            return if window.len() == HEADER_LIMIT {
                Err("the request's headers are over 16 KiB")
            } else {
                Ok(None)
            };
        }
        // Every header takes a line, so the lines bound the headers: no count limit beyond the
        // size limit.
        let lines = window.iter().filter(|b| **b == b'\n').count();
        let mut headers = vec![httparse::EMPTY_HEADER; lines];
        let mut parsed = httparse::Request::new(&mut headers);
        let length = match parsed.parse(window) {
            Ok(httparse::Status::Complete(length)) => length,
            // The empty line was one of the empty lines a request may start with.
            Ok(httparse::Status::Partial) => {
                self.searched = window.len();
                return if window.len() == HEADER_LIMIT {
                    Err("the request's headers are over 16 KiB")
                } else {
                    Ok(None)
                };
            }
            Err(_) => return Err("the request is not HTTP/1.1"),
        };
        if parsed.version != Some(1) {
            return Err("only HTTP/1.1 is served");
        }
        let (Some(method), Some(target)) = (parsed.method, parsed.path) else {
            return Err("the request is not HTTP/1.1");
        };
        let mut hosts = 0;
        let mut content_length = None;
        let mut authorization = Authorization::Missing;
        let mut close = false;
        for header in parsed.headers.iter() {
            if named(header.name, "content-length") {
                if content_length.is_some() {
                    return Err("the request has more than one Content-Length");
                }
                content_length = Some(content_length_of(trim(header.value))?);
            } else if named(header.name, "transfer-encoding") {
                return Err("only Content-Length bodies are read");
            } else if named(header.name, "host") {
                hosts += 1;
            } else if named(header.name, "authorization") {
                authorization = match authorization {
                    Authorization::Missing => Authorization::One(trim(header.value).to_vec()),
                    _ => Authorization::Many,
                };
            } else if named(header.name, "connection") {
                close |= header
                    .value
                    .split(|b| *b == b',')
                    .any(|token| trim(token).eq_ignore_ascii_case(b"close"));
            }
        }
        if hosts != 1 {
            return Err("an HTTP/1.1 request carries exactly one Host header");
        }
        let (path, query) = match target.split_once('?') {
            Some((path, _)) => (path, true),
            None => (target, false),
        };
        Ok(Some(Head {
            request: Request {
                method: String::from(method),
                path: String::from(path),
                query,
                authorization,
                close,
                body: Vec::new(),
            },
            length,
            body: content_length.unwrap_or(0),
        }))
    }
}

/// A `Content-Length` value: decimal digits, at most [`BODY_LIMIT`].
fn content_length_of(value: &[u8]) -> Result<usize, &'static str> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return Err("the request's Content-Length is not a number");
    }
    let mut length: usize = 0;
    for digit in value {
        length = length
            .saturating_mul(10)
            .saturating_add(usize::from(digit - b'0'));
    }
    if length > BODY_LIMIT {
        return Err("the request's body is over 1 MiB");
    }
    Ok(length)
}

/// The reason phrase HTTP writes after `status`.
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "",
    }
}

/// Writes `answer` to `out` as an HTTP/1.1 response, saying `Connection: close` when `close`.
pub(crate) fn write(answer: &Answer, close: bool, out: &mut Vec<u8>) {
    let mut head = format!("HTTP/1.1 {} {}\r\n", answer.status, reason(answer.status));
    if !answer.body.is_empty() {
        head.push_str("Content-Type: application/json\r\n");
    }
    head.push_str(&format!("Content-Length: {}\r\n", answer.body.len()));
    if answer.status == 401 {
        head.push_str("WWW-Authenticate: Bearer\r\n");
    }
    if let Some(allow) = answer.allow {
        head.push_str(&format!("Allow: {allow}\r\n"));
    }
    if close {
        head.push_str("Connection: close\r\n");
    }
    head.push_str("\r\n");
    out.extend_from_slice(head.as_bytes());
    out.extend_from_slice(&answer.body);
}

/// One connection's HTTP: the decrypted bytes in, the answers out.
#[derive(Debug, Default)]
pub(crate) struct Session {
    input: Vec<u8>,
    reader: Reader,
    /// Answers not yet taken by TLS; `taken` of them have been.
    output: Vec<u8>,
    taken: usize,
    /// No more requests are read: the connection closes once the output has gone.
    closing: bool,
    /// No more bytes will arrive: once the requests that have are answered, the session closes.
    finished: bool,
    /// The connection is over the connection limit: its first request is answered `busy`.
    refusing: bool,
}

impl Session {
    /// A session for a connection over the connection limit.
    pub(crate) fn refusing() -> Self {
        Self {
            refusing: true,
            ..Self::default()
        }
    }

    /// Whether the session is over the connection limit.
    pub(crate) fn is_refusing(&self) -> bool {
        self.refusing
    }

    /// Whether the session reads more: it is not closing and its input is under one request at
    /// its limits.
    pub(crate) fn wants_input(&self) -> bool {
        !self.closing && self.input.len() < INPUT_LIMIT
    }

    /// Decrypted bytes from the client.
    pub(crate) fn receive(&mut self, bytes: &[u8]) {
        if !self.closing {
            self.input.extend_from_slice(bytes);
        }
    }

    /// The answer bytes TLS has not yet taken.
    pub(crate) fn pending(&self) -> &[u8] {
        &self.output[self.taken..]
    }

    /// TLS took `count` of the pending bytes.
    pub(crate) fn take(&mut self, count: usize) {
        self.taken = (self.taken + count).min(self.output.len());
        if self.taken == self.output.len() {
            self.output.clear();
            self.taken = 0;
        }
    }

    /// Whether the connection closes once its output has gone.
    pub(crate) fn closing(&self) -> bool {
        self.closing
    }

    /// No more bytes will arrive: the requests that have are answered, then the session closes.
    pub(crate) fn finish(&mut self) {
        self.finished = true;
    }

    /// Answers the requests that have arrived, one at a time: the next is read only when the
    /// last answer has all been taken. Says whether anything was answered.
    pub(crate) fn serve(&mut self, api: &mut Api, now: u64) -> bool {
        let mut answered = false;
        while !self.closing && self.output.is_empty() {
            let (answer, close) = match self.reader.read(&self.input) {
                Read::Incomplete => {
                    if self.finished {
                        self.closing = true;
                        self.input = Vec::new();
                        answered = true;
                    }
                    break;
                }
                Read::Request(request, length) => {
                    self.input.drain(..length);
                    if self.refusing {
                        (busy(), true)
                    } else {
                        let answer = api.answer(&request, now);
                        let close = request.close || answer.close;
                        (answer, close)
                    }
                }
                Read::Refused(message) => {
                    if self.refusing {
                        (busy(), true)
                    } else {
                        (Answer::error(ErrorCode::Invalid, message), true)
                    }
                }
            };
            write(&answer, close, &mut self.output);
            if close {
                self.closing = true;
                self.input = Vec::new();
            }
            answered = true;
        }
        answered
    }
}

/// The answer to a request on a connection over the connection limit.
fn busy() -> Answer {
    Answer::error(
        ErrorCode::Busy,
        "the node has 64 open connections; try again later",
    )
}

#[cfg(test)]
mod tests;
