//! The minimal HTTP/1.1 client (work item 1.4, step 1): one `GET` or `POST` a connection, the
//! answer's body by its `Content-Length`. No chunked transfer, no redirects, no keep-alive, and at
//! most 1 MiB of answer, head and body together. Instance data needs nothing more.

use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use core::fmt;

use crate::machine::{Stream, StreamError};

/// The most bytes of an answer, head and body together: 1 MiB.
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
    /// The answer is over [`RESPONSE_MAX`].
    TooLarge,
    /// The answer is not HTTP/1.1 as this client reads it; what is wrong with it.
    Malformed(&'static str),
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HttpError::Stream(error) => error.fmt(f),
            HttpError::Closed => f.write_str("the connection closed before a whole answer"),
            HttpError::TooLarge => f.write_str("the answer is over 1 MiB"),
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

/// Sends `request` on a fresh connection and reads its answer, all by `deadline`.
pub fn exchange<S: Stream>(
    stream: &mut S,
    request: &Request<'_>,
    deadline: u64,
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
    let mut answer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut searched = 0;
    let head_end = loop {
        if let Some(at) = answer[searched..]
            .windows(HEAD_END.len())
            .position(|window| window == HEAD_END)
        {
            break searched + at;
        }
        searched = answer.len().saturating_sub(HEAD_END.len() - 1);
        if answer.len() == RESPONSE_MAX {
            return Err(HttpError::TooLarge);
        }
        let room = chunk.len().min(RESPONSE_MAX - answer.len());
        match stream.read(&mut chunk[..room], deadline)? {
            0 => return Err(HttpError::Closed),
            count => answer.extend_from_slice(&chunk[..count.min(room)]),
        }
    };
    let (status, headers) = head(&answer[..head_end])?;
    let mut response = Response {
        status,
        headers,
        body: Vec::new(),
    };

    // The body: exactly Content-Length bytes. What follows them is not read.
    let body_start = head_end + HEAD_END.len();
    let end = body_start
        .checked_add(length(&response)?)
        .filter(|end| *end <= RESPONSE_MAX)
        .ok_or(HttpError::TooLarge)?;
    while answer.len() < end {
        let room = chunk.len().min(end - answer.len());
        match stream.read(&mut chunk[..room], deadline)? {
            0 => return Err(HttpError::Closed),
            count => answer.extend_from_slice(&chunk[..count.min(room)]),
        }
    }
    response.body = answer[body_start..end].to_vec();
    Ok(response)
}

/// A head's status and headers. `bytes` ends before the empty line.
fn head(bytes: &[u8]) -> Result<(u16, Vec<(String, String)>), HttpError> {
    let printable = |byte: &u8| matches!(byte, b'\r' | b'\n' | b'\t' | 0x20..=0x7e);
    let text = core::str::from_utf8(bytes)
        .ok()
        .filter(|_| bytes.iter().all(printable))
        .ok_or(HttpError::Malformed(
            "a byte in its head is not printable ASCII",
        ))?;
    let mut lines = text.split("\r\n");
    let status = status(lines.next().unwrap_or_default())?;
    let mut headers = Vec::new();
    for line in lines {
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
        headers.push((
            name.to_string(),
            value.trim_matches([' ', '\t']).to_string(),
        ));
    }
    Ok((status, headers))
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

/// How many bytes of body follow the head.
fn length(response: &Response) -> Result<usize, HttpError> {
    if response.header("transfer-encoding") != Header::Absent {
        return Err(HttpError::Malformed(
            "it has a Transfer-Encoding, and only Content-Length bodies are read",
        ));
    }
    // These statuses never have a body, whatever their headers say.
    if response.status < 200 || response.status == 204 || response.status == 304 {
        return Ok(0);
    }
    match response.header("content-length") {
        Header::Absent => Err(HttpError::Malformed("it has no Content-Length")),
        Header::Repeated => Err(HttpError::Malformed("it has more than one Content-Length")),
        Header::One(text) if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) => {
            Err(HttpError::Malformed("its Content-Length is not a number"))
        }
        // A length too long to be a number is certainly over the limit.
        Header::One(text) => text.parse().map_err(|_| HttpError::TooLarge),
    }
}
