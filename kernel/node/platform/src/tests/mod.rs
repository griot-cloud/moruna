//! Host tests of the library: the HTTP client, the date parser, finding the cloud over recorded
//! answers with a clock the tests hold, the boot's lines and fatal lines, and the node's own
//! sockets, clock and link on the host's model of the kernel.
//!
//! The recorded answers are the files under `fixtures/`: each is one HTTP answer, its head's
//! lines ending in LF in the file and in CRLF on the wire. They are written from what each
//! cloud's service is documented to answer (nova's metadata API behind neutron's proxy, and
//! Azure's Instance Metadata Service), not captured from a machine in either cloud.
//!
//! Azure's WireServer (item 1.12) is documented only as far as Microsoft's "Create Linux images
//! without a provisioning agent" shows it: a `200` to each request, and the three paths its
//! Python reads in the goal state. The recorded goal state has those, with the values in the
//! shape that page's output shows; the rest of it (the root's name, `Version`, `Machine`, `State`,
//! `Configuration` with its escaped URLs) is an illustration of what a reader must step over, not
//! a documented form.

mod boot;
mod date;
mod http;
mod instance;
mod svc;
mod wireserver;

use core::net::Ipv4Addr;
use std::{
    boxed::Box,
    cell::Cell,
    format,
    rc::Rc,
    string::{String, ToString},
    vec::Vec,
};

use crate::machine::{Clock, Network, Stream, StreamError};

pub(crate) const SECOND: u64 = 1_000_000_000;

/// A clock the tests hold: it moves only when something sleeps on it or a fake takes time.
#[derive(Clone, Default)]
pub(crate) struct FakeClock(Rc<Cell<u64>>);

impl FakeClock {
    pub(crate) fn advance(&self, by: u64) {
        self.0.set(self.0.get() + by);
    }
}

impl Clock for FakeClock {
    fn now(&self) -> u64 {
        self.0.get()
    }

    fn sleep_until(&mut self, at: u64) {
        self.0.set(self.0.get().max(at));
    }
}

/// A fixture as it goes on the wire: the head's line ends made CRLF, the file's last line end
/// dropped. The head's `Content-Length` must be the body's length.
fn wire(file: &str) -> Vec<u8> {
    let (head, body) = file
        .split_once("\n\n")
        .expect("a head, an empty line, a body");
    let body = body
        .strip_suffix('\n')
        .expect("the file ends with a line end");
    let length = head
        .lines()
        .filter_map(|line| line.split_once(": "))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.parse::<usize>().expect("a length"))
        .expect("a Content-Length");
    assert_eq!(length, body.len(), "the fixture's Content-Length");
    format!("{}\r\n\r\n{body}", head.replace('\n', "\r\n")).into_bytes()
}

/// The recorded answers.
pub(crate) mod recorded {
    use super::{Vec, wire};

    /// OpenStack's instance document.
    pub(crate) fn openstack_meta_data() -> Vec<u8> {
        wire(include_str!("../../fixtures/openstack/meta_data.json.http"))
    }

    /// OpenStack's user-data: a node config with one token key.
    pub(crate) fn openstack_user_data() -> Vec<u8> {
        wire(include_str!("../../fixtures/openstack/user_data.http"))
    }

    /// OpenStack's answer for the user-data of an instance created with none.
    pub(crate) fn openstack_no_user_data() -> Vec<u8> {
        wire(include_str!("../../fixtures/openstack/user_data-404.http"))
    }

    /// OpenStack's answer to Azure's path.
    pub(crate) fn openstack_azure_path() -> Vec<u8> {
        wire(include_str!("../../fixtures/openstack/azure-path-404.http"))
    }

    /// Azure's instance document, its `userData` the same node config in base64.
    pub(crate) fn azure_instance() -> Vec<u8> {
        wire(include_str!("../../fixtures/azure/instance.http"))
    }

    /// Azure's answer to a request without `Metadata: true`, as OpenStack's requests are.
    pub(crate) fn azure_no_header() -> Vec<u8> {
        wire(include_str!("../../fixtures/azure/openstack-path-400.http"))
    }

    /// Azure's answer to a path it does not have.
    pub(crate) fn azure_unknown_path() -> Vec<u8> {
        wire(include_str!("../../fixtures/azure/openstack-path-404.http"))
    }

    /// The WireServer's goal state.
    pub(crate) fn goal_state() -> Vec<u8> {
        wire(include_str!("../../fixtures/azure/goalstate.http"))
    }

    /// The WireServer's answer to a `Health` document it takes.
    pub(crate) fn health_taken() -> Vec<u8> {
        wire(include_str!("../../fixtures/azure/health.http"))
    }
}

/// What the recorded goal state names: its incarnation, container and role instance.
pub(crate) const RECORDED_INCARNATION: &str = "1";
pub(crate) const RECORDED_CONTAINER: &str = "0d3f6c58-1b2e-4c8a-9f61-2a7e5b9c4d10";
pub(crate) const RECORDED_ROLE_INSTANCE: &str =
    "a41f2c7e-93b5-4d0a-8e6c-1f7b2d9e3c58._moruna-node-1";

/// The `Health` document that reports the recorded goal state ready, as Microsoft's Python
/// writes it.
pub(crate) fn recorded_health() -> String {
    format!(
        "<Health><GoalStateIncarnation>{RECORDED_INCARNATION}</GoalStateIncarnation><Container>\
         <ContainerId>{RECORDED_CONTAINER}</ContainerId><RoleInstanceList><Role><InstanceId>\
         {RECORDED_ROLE_INSTANCE}</InstanceId><Health><State>Ready</State></Health></Role>\
         </RoleInstanceList></Container></Health>"
    )
}

/// The node config the recorded answers carry.
pub(crate) const RECORDED_CONFIG: &str =
    r#"{"moruna":1,"api":{"token_keys":["AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8="]}}"#;
/// The Unix seconds of the recorded OpenStack answers' `Date`, `Tue, 17 Nov 2026 09:30:00 GMT`.
pub(crate) const RECORDED_OPENSTACK_TIME: u64 = 1_794_907_800;
/// The instance IDs in the recorded documents.
pub(crate) const RECORDED_OPENSTACK_ID: &str = "83679162-1378-4288-a2d4-70e13ec132aa";
pub(crate) const RECORDED_AZURE_ID: &str = "5c08b38e-4d57-4c23-ac45-aca61037f084";

/// An answer made for a test: `status` (as `200 OK`), a `Date`, and `body` with its length.
pub(crate) fn answer(status: &str, body: &[u8]) -> Vec<u8> {
    let mut wire = format!(
        "HTTP/1.1 {status}\r\nDate: Tue, 17 Nov 2026 09:30:00 GMT\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    wire.extend_from_slice(body);
    wire
}

/// A request as the fake server saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Sent {
    pub(crate) to: (Ipv4Addr, u16),
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl Sent {
    fn parse(to: (Ipv4Addr, u16), wire: &[u8]) -> Sent {
        let at = wire
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("a whole request head");
        let head = core::str::from_utf8(&wire[..at]).expect("an ASCII head");
        let mut lines = head.split("\r\n");
        let mut first = lines.next().expect("a request line").split(' ');
        let (method, path, version) = (first.next(), first.next(), first.next());
        assert_eq!(version, Some("HTTP/1.1"));
        assert_eq!(first.next(), None);
        Sent {
            to,
            method: method.expect("a method").to_string(),
            path: path.expect("a path").to_string(),
            headers: lines
                .map(|line| {
                    let (name, value) = line.split_once(": ").expect("a header");
                    (name.to_string(), value.to_string())
                })
                .collect(),
            body: wire[at + 4..].to_vec(),
        }
    }

    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
    }
}

/// What the fake server does with a request.
pub(crate) enum Reply {
    /// It sends these bytes and closes.
    Answer(Vec<u8>),
    /// It sends these bytes, and then the connection is reset.
    Reset(Vec<u8>),
    /// It sends nothing; the caller's deadline passes.
    Stalls,
}

/// What a connection attempt meets: a server, or a failure after so many nanoseconds.
pub(crate) type Meets = Result<(), (StreamError, u64)>;

/// The fake server: what it does with a request that arrives at a time.
type Serve = Box<dyn FnMut(u64, &Sent) -> Reply>;

/// A network with one server at the instance-data address, on the tests' clock.
pub(crate) struct FakeNetwork {
    pub(crate) clock: FakeClock,
    connect: Box<dyn FnMut(u64) -> Meets>,
    serve: Serve,
    /// When each connection was attempted.
    pub(crate) attempts: Vec<u64>,
    /// Each request a server saw, and when its connection was made.
    pub(crate) requests: Vec<(u64, Sent)>,
    /// The node's address on `wan`.
    pub(crate) address: Result<Ipv4Addr, StreamError>,
    /// The most bytes one write takes or one read gives.
    pub(crate) chunk: usize,
}

impl FakeNetwork {
    /// A network whose every connection meets `serve`.
    pub(crate) fn serving(
        clock: &FakeClock,
        serve: impl FnMut(u64, &Sent) -> Reply + 'static,
    ) -> Self {
        FakeNetwork {
            clock: clock.clone(),
            connect: Box::new(|_| Ok(())),
            serve: Box::new(serve),
            attempts: Vec::new(),
            requests: Vec::new(),
            address: Ok(Ipv4Addr::new(10, 0, 2, 15)),
            chunk: 1500,
        }
    }

    /// The same, with `connect` deciding what each connection attempt meets.
    pub(crate) fn connecting(mut self, connect: impl FnMut(u64) -> Meets + 'static) -> Self {
        self.connect = Box::new(connect);
        self
    }

    /// The paths asked, in order.
    pub(crate) fn paths(&self) -> Vec<&str> {
        self.requests
            .iter()
            .map(|(_, sent)| sent.path.as_str())
            .collect()
    }
}

impl Network for FakeNetwork {
    type Stream<'a> = FakeStream<'a>;

    fn connect(
        &mut self,
        address: Ipv4Addr,
        port: u16,
        deadline: u64,
    ) -> Result<FakeStream<'_>, StreamError> {
        let now = self.clock.now();
        assert!(now < deadline, "a connection attempted at its deadline");
        self.attempts.push(now);
        if let Err((error, took)) = (self.connect)(now) {
            self.clock.advance(took);
            if self.clock.now() >= deadline {
                self.clock.sleep_until(deadline);
                return Err(StreamError::Deadline);
            }
            return Err(error);
        }
        Ok(FakeStream {
            network: self,
            to: (address, port),
            connected: now,
            written: Vec::new(),
            reply: None,
            given: 0,
        })
    }

    fn address(&mut self, _: u64) -> Result<Ipv4Addr, StreamError> {
        self.address.clone()
    }
}

/// One connection to the fake server.
pub(crate) struct FakeStream<'a> {
    network: &'a mut FakeNetwork,
    to: (Ipv4Addr, u16),
    connected: u64,
    written: Vec<u8>,
    reply: Option<Reply>,
    given: usize,
}

impl Stream for FakeStream<'_> {
    fn write(&mut self, bytes: &[u8], _: u64) -> Result<usize, StreamError> {
        assert!(self.reply.is_none(), "a write after the answer began");
        let taken = bytes.len().min(self.network.chunk);
        self.written.extend_from_slice(&bytes[..taken]);
        Ok(taken)
    }

    fn read(&mut self, buffer: &mut [u8], deadline: u64) -> Result<usize, StreamError> {
        assert!(!buffer.is_empty(), "a read with no room");
        if self.reply.is_none() {
            let sent = Sent::parse(self.to, &self.written);
            self.reply = Some((self.network.serve)(self.network.clock.now(), &sent));
            self.network.requests.push((self.connected, sent));
        }
        let (bytes, reset) = match self.reply.as_ref().expect("a reply") {
            Reply::Answer(bytes) => (bytes, false),
            Reply::Reset(bytes) => (bytes, true),
            Reply::Stalls => {
                self.network.clock.sleep_until(deadline);
                return Err(StreamError::Deadline);
            }
        };
        let left = &bytes[self.given..];
        if left.is_empty() {
            return if reset {
                Err(StreamError::Reset)
            } else {
                Ok(0)
            };
        }
        let count = left.len().min(buffer.len()).min(self.network.chunk);
        buffer[..count].copy_from_slice(&left[..count]);
        self.given += count;
        Ok(count)
    }
}

/// OpenStack's metadata service, from the recorded answers.
pub(crate) fn openstack(sent: &Sent) -> Reply {
    Reply::Answer(match sent.path.as_str() {
        "/openstack/latest/meta_data.json" => recorded::openstack_meta_data(),
        "/openstack/latest/user_data" => recorded::openstack_user_data(),
        _ => recorded::openstack_azure_path(),
    })
}

/// Azure's WireServer, strict about what it is sent: the goal state to a `GET` with the header
/// `x-ms-version: 2012-11-30`, and its taking of the recorded goal state's `Health` document to a
/// `POST` with the same header. Anything else is a `400`, which is this fake's, not a documented
/// answer.
pub(crate) fn wire_server(sent: &Sent) -> Reply {
    assert_eq!(
        sent.to,
        (crate::wireserver::ADDRESS, crate::wireserver::PORT)
    );
    let versioned = sent.header("x-ms-version") == Some("2012-11-30");
    let host = sent.header("Host") == Some("168.63.129.16");
    Reply::Answer(match (sent.method.as_str(), sent.path.as_str()) {
        ("GET", "/machine?comp=goalstate") if versioned && host && sent.body.is_empty() => {
            recorded::goal_state()
        }
        ("POST", "/machine?comp=health")
            if versioned && host && sent.body == recorded_health().as_bytes() =>
        {
            recorded::health_taken()
        }
        _ => answer("400 Bad Request", b""),
    })
}

/// Azure's Instance Metadata Service, from the recorded answers, and its WireServer.
pub(crate) fn azure(sent: &Sent) -> Reply {
    if sent.to.0 == crate::wireserver::ADDRESS {
        return wire_server(sent);
    }
    Reply::Answer(match (sent.header("Metadata"), sent.path.as_str()) {
        (Some("true"), "/metadata/instance?api-version=2021-02-01") => recorded::azure_instance(),
        (Some("true"), _) => recorded::azure_unknown_path(),
        _ => recorded::azure_no_header(),
    })
}
