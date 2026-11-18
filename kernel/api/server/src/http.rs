//! HTTP/1.1 over one connection's decrypted stream (system design, section 2B): requests read
//! through `httparse`, `Content-Length` bodies only, keep-alive, `Connection: close` honoured,
//! and the limits on headers and bodies. No HTTP/2, compression, cookies or cross-origin
//! headers.

use crate::{
    api::{Admission, Answer, Api, Call, Outcome},
    body::Body,
    jobs::Stream,
};
use alloc::{format, string::String, vec, vec::Vec};
use moruna_api::ErrorCode;

/// The most bytes a request's head may take: its request line, headers and the blank line.
pub const HEADER_LIMIT: usize = 16 * 1024;
/// The most bytes a request's body may take.
pub const BODY_LIMIT: usize = 1024 * 1024;
/// The most bytes of request bodies held at once, across every connection. A request whose body
/// does not fit is answered `busy`, and its connection closes.
pub const BODY_BUDGET: usize = 8 * 1024 * 1024;
/// What a session's input may hold beyond what it asked for: the rest of the last TLS record.
const RECORD_SLACK: usize = 17 * 1024;

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

/// One request's head: everything but its body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Request {
    /// The method, as sent.
    pub method: String,
    /// The path, without its query.
    pub path: String,
    /// The target's query, what follows its `?`, when it carried one.
    pub query: Option<String>,
    /// The `Authorization` header.
    pub authorization: Authorization,
    /// Whether the client asked for the connection to close after this answer.
    pub close: bool,
}

/// What reading the front of the stream gave.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Read {
    /// More bytes are needed.
    Incomplete,
    /// A request's head, which took `length` bytes; a body of `body` bytes follows it.
    Head {
        request: Request,
        length: usize,
        body: usize,
    },
    /// The stream is not HTTP/1.1 as this server reads it, or a request is over a limit. The
    /// answer is `invalid` with this message, then the connection closes.
    Refused(&'static str),
}

/// Reads request heads from the front of a stream, one at a time.
#[derive(Debug, Default)]
pub(crate) struct Reader {
    /// How far the search for the end of the head has looked, so that bytes arriving one at a
    /// time are not searched again and again.
    searched: usize,
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
    /// Reads the request head at the front of `input`. Once a head is read, the caller takes its
    /// bytes off the front before reading again.
    pub(crate) fn read(&mut self, input: &[u8]) -> Read {
        match self.read_head(input) {
            Ok(Some(read)) => {
                self.searched = 0;
                read
            }
            Ok(None) => Read::Incomplete,
            Err(message) => Read::Refused(message),
        }
    }

    /// The head at the front of `input`, `None` while it has not all arrived, or why it is
    /// refused.
    fn read_head(&mut self, input: &[u8]) -> Result<Option<Read>, &'static str> {
        let window = &input[..input.len().min(HEADER_LIMIT)];
        let from = self.searched.min(window.len()).saturating_sub(2);
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
            Some((path, query)) => (path, Some(String::from(query))),
            None => (target, None),
        };
        Ok(Some(Read::Head {
            request: Request {
                method: String::from(method),
                path: String::from(path),
                query,
                authorization,
                close,
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

/// Writes the head of `answer` to `out` as an HTTP/1.1 response's, saying `Connection: close`
/// when `close`. Its body follows a piece at a time ([`Session`]).
pub(crate) fn head(answer: &Answer, close: bool, out: &mut Vec<u8>) {
    let mut head = format!("HTTP/1.1 {} {}\r\n", answer.status, reason(answer.status));
    let length = answer.body.len();
    if length > 0 {
        head.push_str("Content-Type: application/json\r\n");
    }
    head.push_str(&format!("Content-Length: {length}\r\n"));
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
}

/// The bytes of request bodies the server may still hold, shared by every connection.
#[derive(Debug)]
pub(crate) struct Budget {
    free: usize,
}

impl Budget {
    pub(crate) fn new(size: usize) -> Self {
        Self { free: size }
    }

    /// Takes `size` bytes, when they are free.
    fn take(&mut self, size: usize) -> bool {
        match self.free.checked_sub(size) {
            Some(free) => {
                self.free = free;
                true
            }
            None => false,
        }
    }

    /// Gives back `size` bytes.
    fn give(&mut self, size: usize) {
        self.free += size;
    }

    /// The bytes free.
    #[cfg(test)]
    pub(crate) fn free(&self) -> usize {
        self.free
    }
}

/// A call whose head was admitted and whose body is arriving.
#[derive(Debug)]
struct Pending {
    call: Call,
    /// The body's length, which the session holds from the budget.
    length: usize,
    /// The client asked for the connection to close after this answer.
    close: bool,
}

/// A call whose answer comes later: the engine's verdict on a document.
#[derive(Debug)]
struct Waiting {
    ticket: u64,
    /// The client asked for the connection to close after this answer.
    close: bool,
}

/// The most bytes of an answer's body written into a session's output at a time: the next piece
/// is written when TLS has taken it all.
const BODY_PIECE: usize = 32 * 1024;

/// The head of a stream of Server-Sent Events. The stream has no length: it ends when the
/// connection closes.
const STREAM_HEAD: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";

/// One connection's HTTP: the decrypted bytes in, the answers out. A request's head is read and
/// its token checked before any of its body: a body is read only for a call that takes one and
/// is allowed, and only when the body budget holds it.
#[derive(Debug, Default)]
pub(crate) struct Session {
    input: Vec<u8>,
    reader: Reader,
    /// The call whose body is arriving.
    pending: Option<Pending>,
    /// The call whose answer comes later.
    waiting: Option<Waiting>,
    /// The stream of events being sent.
    stream: Option<Stream>,
    /// The body of the answer being sent, written into `output` a piece at a time as TLS takes
    /// it, and whether the connection closes after it.
    body: Option<Body>,
    close_after: bool,
    /// The bytes this session holds from the body budget.
    reserved: usize,
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

    /// How many more decrypted bytes the session wants now: the rest of a head, up to its limit,
    /// or the rest of an admitted body. None once it is closing.
    pub(crate) fn wanted(&self) -> usize {
        if self.closing || self.waiting.is_some() || self.stream.is_some() || self.body.is_some() {
            return 0;
        }
        let target = self.pending.as_ref().map_or(HEADER_LIMIT, |p| p.length);
        target.saturating_sub(self.input.len())
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
            self.output = Vec::new();
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

    /// Gives back what the session holds from the body budget: when a body has been answered,
    /// or the connection is gone.
    pub(crate) fn release(&mut self, budget: &mut Budget) {
        budget.give(self.reserved);
        self.reserved = 0;
    }

    /// The connection is gone: what the session holds from the budget goes back, and an answer
    /// it waited for is no longer wanted.
    pub(crate) fn end(&mut self, api: &mut Api, budget: &mut Budget) {
        self.release(budget);
        if let Some(waiting) = self.waiting.take() {
            api.abandon(waiting.ticket);
        }
        self.stream = None;
        self.body = None;
    }

    /// What becomes of a call: its answer now, its answer later, or a stream of events.
    fn settle(&mut self, outcome: Outcome, close: bool, budget: &mut Budget) {
        match outcome {
            Outcome::Answer(answer) => {
                self.release(budget);
                let close = close || answer.close;
                self.reply(answer, close);
            }
            // The body's share of the budget is held until the answer comes: the document
            // waits with it.
            Outcome::Wait(ticket) => self.waiting = Some(Waiting { ticket, close }),
            Outcome::Stream(stream) => {
                self.release(budget);
                self.output.extend_from_slice(STREAM_HEAD);
                self.stream = Some(stream);
            }
        }
    }

    /// Queues `answer`'s head and the first piece of its body; the rest of the body follows as
    /// TLS takes what is queued. The session closes after it when `close`.
    fn reply(&mut self, answer: Answer, close: bool) {
        head(&answer, close, &mut self.output);
        let mut body = answer.body;
        if body.fill(&mut self.output, BODY_PIECE) {
            if close {
                self.close();
            }
        } else {
            self.body = Some(body);
            self.close_after = close;
        }
    }

    /// Reads no more: the input goes, and the connection closes once its output has gone.
    fn close(&mut self) {
        self.closing = true;
        self.pending = None;
        self.input = Vec::new();
    }

    /// Answers the requests that have arrived, one at a time: the next is read only when the
    /// last answer has all been taken. Says whether anything was answered.
    pub(crate) fn serve(&mut self, api: &mut Api, budget: &mut Budget, now: u64) -> bool {
        let mut answered = false;
        while !self.closing && self.output.is_empty() {
            if let Some(body) = &mut self.body {
                if body.fill(&mut self.output, BODY_PIECE) {
                    self.body = None;
                    if self.close_after {
                        self.close();
                    }
                }
                answered = true;
                break;
            }
            if let Some(waiting) = &self.waiting {
                let Some(answer) = api.ready(waiting.ticket) else {
                    break;
                };
                let close = waiting.close;
                self.waiting = None;
                self.settle(Outcome::Answer(answer), close, budget);
                answered = true;
                continue;
            }
            if let Some(stream) = &mut self.stream {
                if api.fill(stream, &mut self.output) {
                    self.stream = None;
                    self.close();
                    answered = true;
                }
                answered |= !self.output.is_empty();
                break;
            }
            if let Some(length) = self.pending.as_ref().map(|p| p.length) {
                if self.input.len() < length {
                    if self.finished {
                        self.release(budget);
                        self.close();
                        answered = true;
                    }
                    break;
                }
                let Some(pending) = self.pending.take() else {
                    break;
                };
                let outcome = api.complete(pending.call, &self.input[..length], now);
                self.input.drain(..length);
                // Keep no room a body needed once it has been read.
                self.input.shrink_to(2 * HEADER_LIMIT);
                self.settle(outcome, pending.close, budget);
                answered = true;
                continue;
            }
            match self.reader.read(&self.input) {
                Read::Incomplete => {
                    if self.finished {
                        self.close();
                        answered = true;
                    }
                    break;
                }
                Read::Refused(message) => {
                    let answer = if self.refusing {
                        busy()
                    } else {
                        Answer::error(ErrorCode::Invalid, message)
                    };
                    self.reply(answer, true);
                }
                Read::Head {
                    request,
                    length,
                    body,
                } => {
                    self.input.drain(..length);
                    if self.refusing {
                        self.reply(busy(), true);
                    } else {
                        match api.admit(&request, body, now) {
                            // Decided on the head: a body that follows is never read, so the
                            // stream cannot go on past it.
                            Admission::Now(outcome) => {
                                self.settle(outcome, request.close || body > 0, budget);
                            }
                            Admission::Body(call) if budget.take(body) => {
                                self.reserved = body;
                                self.input.reserve_exact(
                                    (body + RECORD_SLACK).saturating_sub(self.input.len()),
                                );
                                self.pending = Some(Pending {
                                    call,
                                    length: body,
                                    close: request.close,
                                });
                            }
                            Admission::Body(_) => self.reply(budget_spent(), true),
                        }
                    }
                }
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

/// The answer to a request whose body the body budget cannot hold now.
fn budget_spent() -> Answer {
    Answer::error(
        ErrorCode::Busy,
        "the node holds as many request bodies as it can; try again later",
    )
}

#[cfg(test)]
mod tests;
