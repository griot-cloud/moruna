//! The protocol conformance harness (SDD 23, f.5): one suite per protocol, written
//! once, run against any server that claims to implement it. A server passes when
//! every check passes. It is the test that a third party's server is a server of the
//! protocol, and not merely something that answers.
//!
//! A [`Suite`] is the protocol's id, its request ordinals and its cases: for each
//! request, calls with valid arguments and with arguments the protocol refuses, each
//! checking the answer (`protocol!` generates `ID` and `REQUESTS`; the cases are the
//! protocol author's). [`run`] adds the checks every protocol shares, built from the
//! id and the ordinals alone:
//!
//! - the handshake: `Welcome` with the same major and a minor not below the client's;
//! - version refusal: another name, another major, and a request before `Hello` are each
//!   refused with their reason, and the connection closed; an older and a newer minor
//!   of the same major are welcomed;
//! - an unknown request is answered `UnknownMethod`, and the connection stays usable;
//! - every request, sent a body that fills the frame to the channel's bound with
//!   `0xff` (no request's arguments decode from it), is answered `BadMessage`: that is
//!   both the invalid-arguments check at the wire and a frame exactly at the bound;
//! - a frame over the bound is refused at the sender (by the SDK and by the kernel)
//!   and nothing reaches the server;
//! - a handle sent with a request that does not take it is closed by the server;
//! - clients that close mid-request, before or after the handshake, leave the server
//!   serving;
//! - a client that does not read its replies has its connection closed, and the server
//!   goes on serving others.
//!
//! Checks run one after another on fresh connections from a [`Connector`], so one
//! check's failure does not spoil the next.

use alloc::{
    collections::VecDeque,
    format,
    string::{String, ToString},
};

use moruna_abi::{Error, Signals, limits};

use crate::{
    client::{self, CallError, deadline_in},
    frame::{self, Frame, HEADER, Kind, Transport},
    os::{Channel, now, wait_one},
    protocol::{Hello, NAME_MAX, ProtocolId, Refusal, Refused, Status},
    wire::{Encoder, Wire},
};

/// A failed check's reason.
pub type Fail = String;

/// Opens fresh raw channels to the server under test.
pub trait Connector {
    fn connect(&mut self) -> Result<Channel, Fail>;
}

/// Connects through a server's listener (SDD 23, f.4).
pub struct Listener(pub Channel);

impl Connector for Listener {
    fn connect(&mut self) -> Result<Channel, Fail> {
        client::connect(&self.0).map_err(|e| format!("connecting through the listener: {e:?}"))
    }
}

/// A server's answer to one request.
pub enum Answer {
    Reply(Frame),
    Failure(u32),
    Status(Status),
}

impl core::fmt::Debug for Answer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Answer::Reply(r) => write!(
                f,
                "a reply of {} bytes and {} handles",
                r.body_len(),
                r.handle_count()
            ),
            Answer::Failure(code) => write!(f, "failure {code}"),
            Answer::Status(s) => write!(f, "status {s:?}"),
        }
    }
}

impl Answer {
    /// The reply, or why there was none.
    pub fn reply(self) -> Result<Frame, Fail> {
        match self {
            Answer::Reply(f) => Ok(f),
            other => Err(format!("expected a reply, got {other:?}")),
        }
    }

    /// Succeeds if the answer is the protocol's error `code`.
    pub fn failure(self, code: u32) -> Result<(), Fail> {
        match self {
            Answer::Failure(c) if c == code => Ok(()),
            other => Err(format!("expected failure {code}, got {other:?}")),
        }
    }

    /// Succeeds if the answer is the SDK's `status`.
    pub fn status(self, status: Status) -> Result<(), Fail> {
        match self {
            Answer::Status(s) if s == status => Ok(()),
            other => Err(format!("expected status {status:?}, got {other:?}")),
        }
    }
}

/// A connection to the server under test, after a successful handshake, that sends
/// whatever a check asks, well formed or not.
pub struct Probe {
    channel: Channel,
    next_txid: u32,
    events: VecDeque<Frame>,
    timeout: u64,
}

/// The body bytes one frame can carry.
pub const BUDGET: usize = limits::CHANNEL_MAX_BYTES - HEADER;

impl Probe {
    /// The raw channel.
    pub fn channel(&self) -> &Channel {
        &self.channel
    }

    /// Sends any frame as it is.
    pub fn send_raw(
        &mut self,
        kind: Kind,
        ordinal: u16,
        txid: u32,
        body: Encoder,
    ) -> Result<(), Transport> {
        frame::send(&self.channel, kind, ordinal, txid, body)
    }

    /// Sends request `ordinal` and waits for its answer; events that arrive first are
    /// kept for [`Probe::next_event`].
    pub fn request(&mut self, ordinal: u16, body: Encoder) -> Result<Answer, Fail> {
        let txid = self.next_txid;
        self.next_txid += 1;
        self.send_raw(Kind::Request, ordinal, txid, body)
            .map_err(|e| format!("sending request {ordinal}: {e:?}"))?;
        let deadline = deadline_in(self.timeout);
        loop {
            let mut f = next_frame(&self.channel, deadline)?;
            match f.kind {
                Kind::Event if f.txid == 0 => self.events.push_back(f),
                Kind::Reply if f.txid == txid => return Ok(Answer::Reply(f)),
                Kind::Failure if f.txid == txid => {
                    let mut d = f.decoder();
                    let code =
                        u32::decode(&mut d).map_err(|e| format!("a failure's code: {e:?}"))?;
                    d.finish().map_err(|e| format!("a failure's body: {e:?}"))?;
                    return Ok(match Status::from_code(code) {
                        Some(s) => Answer::Status(s),
                        None if code != 0 && code < Status::FIRST => Answer::Failure(code),
                        None => {
                            return Err(format!(
                                "failure code {code:#x} is neither the protocol's nor a status"
                            ));
                        }
                    });
                }
                other => {
                    return Err(format!(
                        "expected the answer to request {txid}, got {other:?} for {}",
                        f.txid
                    ));
                }
            }
        }
    }

    /// The next event.
    pub fn next_event(&mut self) -> Result<Frame, Fail> {
        if let Some(f) = self.events.pop_front() {
            return Ok(f);
        }
        let f = next_frame(&self.channel, deadline_in(self.timeout))?;
        match f.kind {
            Kind::Event if f.txid == 0 => Ok(f),
            other => Err(format!("expected an event, got {other:?}")),
        }
    }
}

fn next_frame(channel: &Channel, deadline: u64) -> Result<Frame, Fail> {
    loop {
        match frame::recv(channel) {
            Ok(Some(f)) => return Ok(f),
            Ok(None) => {}
            Err(e) => return Err(format!("reading from the server: {e:?}")),
        }
        match wait_one(
            &channel.0,
            Signals::READABLE | Signals::PEER_CLOSED,
            deadline,
        ) {
            Ok(_) => {}
            Err(Error::TimedOut) => return Err("the server did not answer in time".to_string()),
            Err(e) => return Err(format!("waiting for the server: {}", e.name())),
        }
    }
}

/// One protocol-specific check, run on a fresh [`Probe`].
pub struct Case {
    pub name: &'static str,
    pub check: fn(&mut Probe) -> Result<(), Fail>,
}

/// A protocol's conformance suite.
pub struct Suite {
    pub id: ProtocolId,
    /// Every request ordinal the protocol declares.
    pub requests: &'static [u16],
    /// For every request, its valid and invalid calls and what each must answer.
    pub cases: &'static [Case],
    /// How long a check waits for the server, in nanoseconds.
    pub timeout: u64,
}

/// One check's outcome.
pub struct Check<'a> {
    pub name: &'a str,
    pub result: &'a Result<(), Fail>,
}

/// The tally of a run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub passed: u32,
    pub failed: u32,
}

impl Summary {
    pub fn ok(&self) -> bool {
        self.failed == 0 && self.passed > 0
    }
}

struct Runner<'a> {
    suite: &'a Suite,
    connector: &'a mut dyn Connector,
    report: &'a mut dyn FnMut(&Check<'_>),
    summary: Summary,
}

impl Runner<'_> {
    fn check(&mut self, name: &str, f: impl FnOnce(&mut Self) -> Result<(), Fail>) {
        let result = f(self);
        if result.is_ok() {
            self.summary.passed += 1;
        } else {
            self.summary.failed += 1;
        }
        (self.report)(&Check {
            name,
            result: &result,
        });
    }

    fn deadline(&self) -> u64 {
        deadline_in(self.suite.timeout)
    }

    /// A raw channel, the handshake not yet made.
    fn raw(&mut self) -> Result<Channel, Fail> {
        self.connector.connect()
    }

    fn hello(&mut self, channel: &Channel, hello: Hello) -> Result<Frame, Fail> {
        let mut body = Encoder::new();
        hello
            .encode(&mut body)
            .map_err(|e| format!("encoding hello: {e:?}"))?;
        frame::send(channel, Kind::Hello, 0, 0, body)
            .map_err(|e| format!("sending hello: {e:?}"))?;
        next_frame(channel, self.deadline())
    }

    /// A connection with a completed handshake.
    fn probe(&mut self) -> Result<Probe, Fail> {
        let channel = self.raw()?;
        let mut answer = self.hello(&channel, self.suite.id.hello())?;
        if answer.kind != Kind::Welcome {
            return Err(format!("the handshake was answered {:?}", answer.kind));
        }
        let mut d = answer.decoder();
        let (major, minor) = (u16::decode(&mut d), u16::decode(&mut d));
        match (major, minor, d.finish()) {
            (Ok(major), Ok(minor), Ok(()))
                if major == self.suite.id.major && minor >= self.suite.id.minor => {}
            _ => return Err("the welcome does not carry the protocol's version".to_string()),
        }
        Ok(Probe {
            channel,
            next_txid: 1,
            events: VecDeque::new(),
            timeout: self.suite.timeout,
        })
    }

    fn closed(&self, channel: &Channel) -> Result<(), Fail> {
        match wait_one(&channel.0, Signals::PEER_CLOSED, self.deadline()) {
            Ok(_) => Ok(()),
            Err(Error::TimedOut) => Err("the server did not close the connection".to_string()),
            Err(e) => Err(format!("waiting for the close: {}", e.name())),
        }
    }

    fn refused(&mut self, hello: Hello, reason: Refusal) -> Result<(), Fail> {
        let channel = self.raw()?;
        let mut answer = self.hello(&channel, hello)?;
        if answer.kind != Kind::Refuse {
            return Err(format!("answered {:?}, not a refusal", answer.kind));
        }
        let mut d = answer.decoder();
        match Refused::decode(&mut d) {
            Ok(r)
                if r.reason == reason
                    && r.server == self.suite.id.hello()
                    && d.finish().is_ok() => {}
            Ok(r) => return Err(format!("refused for {:?} naming {:?}", r.reason, r.server)),
            Err(e) => return Err(format!("a refusal that does not decode: {e:?}")),
        }
        self.closed(&channel)
    }

    /// An ordinal the protocol does not declare.
    fn unknown(&self) -> u16 {
        (1..=u16::MAX)
            .rev()
            .find(|o| !self.suite.requests.contains(o))
            .unwrap_or(0)
    }

    fn still_serving(&mut self) -> Result<(), Fail> {
        let unknown = self.unknown();
        self.probe()?
            .request(unknown, Encoder::new())?
            .status(Status::UnknownMethod)
    }
}

fn filled(len: usize) -> Encoder {
    let mut body = Encoder::new();
    body.put_raw(&alloc::vec![0xffu8; len]);
    body
}

/// Runs `suite` against the server `connector` reaches, reporting each check as it
/// finishes.
pub fn run(
    suite: &Suite,
    connector: &mut dyn Connector,
    report: &mut dyn FnMut(&Check<'_>),
) -> Summary {
    let mut r = Runner {
        suite,
        connector,
        report,
        summary: Summary::default(),
    };
    let id = suite.id;

    r.check("handshake: welcome with the protocol's version", |r| {
        r.probe().map(drop)
    });
    r.check("handshake: another protocol is refused", |r| {
        let mut name = String::from(id.name);
        if name.len() < NAME_MAX {
            name.push('x');
        } else {
            name.pop();
        }
        r.refused(
            Hello {
                name,
                major: id.major,
                minor: id.minor,
            },
            Refusal::UnknownProtocol,
        )
    });
    r.check("handshake: another major version is refused", |r| {
        r.refused(
            Hello {
                name: String::from(id.name),
                major: id.major.wrapping_add(1),
                minor: id.minor,
            },
            Refusal::MajorMismatch,
        )
    });
    for (what, minor) in [("an older", 0), ("a newer", id.minor.saturating_add(1))] {
        r.check(
            &format!("handshake: {what} minor version of the same major is welcomed"),
            |r| {
                let channel = r.raw()?;
                let answer = r.hello(
                    &channel,
                    Hello {
                        name: String::from(id.name),
                        major: id.major,
                        minor,
                    },
                )?;
                match answer.kind {
                    Kind::Welcome => Ok(()),
                    kind => Err(format!("answered {kind:?}")),
                }
            },
        );
    }
    r.check("handshake: a request before hello is refused", |r| {
        let channel = r.raw()?;
        frame::send(&channel, Kind::Request, r.unknown(), 1, Encoder::new())
            .map_err(|e| format!("{e:?}"))?;
        let mut answer = next_frame(&channel, r.deadline())?;
        let kind = answer.kind;
        let mut d = answer.decoder();
        match (kind, Refused::decode(&mut d)) {
            (Kind::Refuse, Ok(refused)) if refused.reason == Refusal::NotHello => {
                r.closed(&channel)
            }
            (kind, _) => Err(format!("answered {kind:?}")),
        }
    });
    r.check(
        "an unknown request is answered unknown method, and the connection serves on",
        |r| {
            let unknown = r.unknown();
            let mut probe = r.probe()?;
            probe
                .request(unknown, Encoder::new())?
                .status(Status::UnknownMethod)?;
            probe
                .request(unknown, Encoder::new())?
                .status(Status::UnknownMethod)
        },
    );
    for &ordinal in suite.requests {
        let name = format!(
            "request {ordinal}: a body that fills the frame to the bound and decodes as nothing is a bad message"
        );
        r.check(&name, |r| {
            let mut probe = r.probe()?;
            probe
                .request(ordinal, filled(BUDGET))?
                .status(Status::BadMessage)
        });
    }
    r.check(
        "a frame over the bound is refused at the sender and never reaches the server",
        |r| {
            let probe = r.probe()?;
            match frame::send(
                probe.channel(),
                Kind::Request,
                r.unknown(),
                7,
                filled(BUDGET + 1),
            ) {
                Err(Transport::TooLarge) => {}
                other => return Err(format!("the SDK sent an oversized frame: {other:?}")),
            }
            let mut too_many = Encoder::new();
            for _ in 0..limits::CHANNEL_MAX_HANDLES {
                let (a, _b) = Channel::create().map_err(|e| e.name().to_string())?;
                too_many.put_handle(a.0).map_err(|e| format!("{e:?}"))?;
            }
            let (extra, _peer) = Channel::create().map_err(|e| e.name().to_string())?;
            if too_many.put_handle(extra.0).is_ok() {
                return Err("the SDK took more handles than a message carries".to_string());
            }
            let bytes = alloc::vec![0u8; limits::CHANNEL_MAX_BYTES + 1];
            match probe.channel().write_with(&bytes, &mut []) {
                Err(Error::Limit) => {}
                other => return Err(format!("the kernel took an oversized message: {other:?}")),
            }
            match frame::recv(probe.channel()) {
                Ok(None) => {}
                Ok(Some(f)) => {
                    return Err(format!(
                        "the server answered a frame it never got: {:?}",
                        f.kind
                    ));
                }
                Err(e) => return Err(format!("{e:?}")),
            }
            r.still_serving()
        },
    );
    r.check(
        "handle transfer: a handle sent with a request that does not take it is closed",
        |r| {
            let unknown = r.unknown();
            let mut probe = r.probe()?;
            let (kept, sent) = Channel::create().map_err(|e| e.name().to_string())?;
            let mut body = Encoder::new();
            body.put_handle(sent.0).map_err(|e| format!("{e:?}"))?;
            probe
                .request(unknown, body)?
                .status(Status::UnknownMethod)?;
            r.closed(&kept)
                .map_err(|_| "the server kept a handle it did not take".to_string())?;
            let requests = r.suite.requests;
            for &ordinal in requests {
                let (kept, sent) = Channel::create().map_err(|e| e.name().to_string())?;
                let mut body = filled(16);
                body.put_handle(sent.0).map_err(|e| format!("{e:?}"))?;
                probe.request(ordinal, body)?.status(Status::BadMessage)?;
                r.closed(&kept).map_err(|_| {
                    format!("request {ordinal}: the server kept a handle from a bad message")
                })?;
            }
            Ok(())
        },
    );
    r.check(
        "clients that disappear mid-request leave the server serving",
        |r| {
            for i in 0..8u16 {
                let channel = r.raw()?;
                if i % 2 == 0 {
                    // Gone after hello, before the welcome is read.
                    let mut body = Encoder::new();
                    id.hello().encode(&mut body).map_err(|e| format!("{e:?}"))?;
                    frame::send(&channel, Kind::Hello, 0, 0, body).map_err(|e| format!("{e:?}"))?;
                } else {
                    // Gone with requests unanswered, one of them carrying a handle; the
                    // raw channel above is a client gone before its hello.
                    let probe = r.probe()?;
                    frame::send(
                        probe.channel(),
                        Kind::Request,
                        r.unknown(),
                        1,
                        Encoder::new(),
                    )
                    .map_err(|e| format!("{e:?}"))?;
                    if let Some(&ordinal) = r.suite.requests.first() {
                        let (_kept, sent) = Channel::create().map_err(|e| e.name().to_string())?;
                        let mut body = filled(8);
                        body.put_handle(sent.0).map_err(|e| format!("{e:?}"))?;
                        frame::send(probe.channel(), Kind::Request, ordinal, 2, body)
                            .map_err(|e| format!("{e:?}"))?;
                    }
                }
                drop(channel);
            }
            r.still_serving()
        },
    );
    r.check(
        "a client that does not read has its connection closed, and the server serves on",
        |r| {
            let probe = r.probe()?;
            let unknown = r.unknown();
            let deadline = r.deadline();
            let mut sent = 0u32;
            loop {
                match frame::send(
                    probe.channel(),
                    Kind::Request,
                    unknown,
                    sent + 1,
                    Encoder::new(),
                ) {
                    Ok(()) => sent += 1,
                    Err(Transport::Full) => {}
                    Err(Transport::PeerClosed) => break,
                    Err(e) => return Err(format!("{e:?}")),
                }
                if sent as usize > 4 * limits::CHANNEL_DEPTH || now() > deadline {
                    return Err(format!(
                        "the server still holds the connection after {sent} unread replies"
                    ));
                }
                if sent as usize >= limits::CHANNEL_DEPTH {
                    match wait_one(
                        &probe.channel().0,
                        Signals::WRITABLE | Signals::PEER_CLOSED,
                        deadline,
                    ) {
                        Ok(s) if s.contains(Signals::PEER_CLOSED) => break,
                        Ok(_) => {}
                        Err(e) => return Err(format!("waiting on the server: {}", e.name())),
                    }
                }
            }
            r.still_serving()
        },
    );
    for case in suite.cases {
        r.check(case.name, |r| {
            let mut probe = r.probe()?;
            (case.check)(&mut probe)
        });
    }
    r.summary
}

/// A typed client's error, for a case that calls through a `Client`.
pub fn call_error<E: core::fmt::Debug>(e: CallError<E>) -> Fail {
    format!("{e:?}")
}
