//! Finding the cloud (system design, section 1D, step 1; work item 1.4, step 3).
//!
//! Instance data is asked for over HTTP at `169.254.169.254`, Azure's first and then OpenStack's:
//!
//! - **A cloud is named** only by a `200` whose body parses as that cloud's document. OpenStack's
//!   user-data is a second request, once its document has named it.
//! - **Any other answer** moves to the next cloud at once, and round again after the last.
//! - **No address yet:** a request that cannot begin because the node has no address (the
//!   network server has no route) waits for `wan`'s address, as the network server gives it
//!   ([`Network::address`]), and begins again as soon as the address comes (work item 1.11f).
//!   Once the node has had an address, a request with no route is asked again a second later.
//! - **No answer** (a connection refused, timed out, or broken before a whole answer) and a
//!   **`410`** are asked again, a second after that request began.
//! - **At most 5 requests begin in any second.**
//! - **Nothing by 90 s** after `platform` started is fatal.
//!
//! Once OpenStack is named there is no next cloud, so anything but its user-data (`200`) or its
//! lack of any (`404`) is asked again a second later.

use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};
use core::{fmt, net::Ipv4Addr};

use moruna_node_config::{Cloud, Refusal, base64, instance_valid};

use crate::{
    boot::{Fatal, Stage},
    http::{self, Header, HttpError, Request, Response},
    machine::{Clock, Network, StreamError},
};

/// Where a cloud serves instance data.
pub const ADDRESS: Ipv4Addr = Ipv4Addr::new(169, 254, 169, 254);
/// The port it serves it on.
pub const PORT: u16 = 80;
const HOST: &str = "169.254.169.254";

/// A second on the monotonic clock.
pub const SECOND: u64 = 1_000_000_000;
/// How long after `platform`'s start instance data may take.
pub const LIMIT_NS: u64 = 90 * SECOND;
/// How long after a request began it is made again, when it got no answer or a `410`.
pub const RETRY_NS: u64 = SECOND;
/// The most requests that begin in any one second.
pub const REQUESTS_A_SECOND: usize = 5;

/// Azure's instance document; its `compute.userData` is the user-data, in base64.
pub const AZURE: Request<'static> = Request::get(
    HOST,
    "/metadata/instance?api-version=2021-02-01",
    &[("Metadata", "true")],
);
/// OpenStack's instance document.
pub const OPENSTACK: Request<'static> = Request::get(HOST, "/openstack/latest/meta_data.json", &[]);
/// OpenStack's user-data, as the bytes the instance was created with.
pub const OPENSTACK_USER_DATA: Request<'static> =
    Request::get(HOST, "/openstack/latest/user_data", &[]);

/// What instance data says.
#[derive(Clone, PartialEq, Eq)]
pub struct Found {
    /// The cloud that answered.
    pub cloud: Cloud,
    /// The cloud's ID for this machine: Azure's `compute.vmId`, OpenStack's `uuid`.
    pub instance: String,
    /// The `Date` header of the answer that named the cloud, when it has exactly one.
    pub date: Option<String>,
    /// The monotonic clock when that answer had arrived.
    pub read_at_ns: u64,
    /// The instance's user-data, or why there is none to validate.
    pub user_data: Result<Vec<u8>, Refusal>,
}

impl fmt::Debug for Found {
    /// User-data may hold the operator's TLS key, so only its size is printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Found")
            .field("cloud", &self.cloud)
            .field("instance", &self.instance)
            .field("date", &self.date)
            .field("read_at_ns", &self.read_at_ns)
            .field("user_data", &self.user_data.as_ref().map(Vec::len))
            .finish()
    }
}

/// What is being asked.
enum Asking {
    Azure,
    OpenStack,
    /// OpenStack is named, and its user-data is still to come.
    UserData {
        instance: String,
        date: Option<String>,
        read_at_ns: u64,
    },
}

/// What an answer, or the lack of one, leads to.
enum Next {
    Found(Found),
    /// Ask this at once, as far as the pace allows.
    Now(Asking),
    /// Ask this a second after the last request began.
    Later(Asking),
    /// The request could not begin, for no route: ask this as soon as the node has an
    /// address, if it had none yet.
    Routed(Asking),
}

impl Asking {
    fn request(&self) -> Request<'static> {
        match self {
            Asking::Azure => AZURE,
            Asking::OpenStack => OPENSTACK,
            Asking::UserData { .. } => OPENSTACK_USER_DATA,
        }
    }

    /// What follows an answer that is not the one asked for.
    fn otherwise(self) -> Next {
        match self {
            Asking::Azure => Next::Now(Asking::OpenStack),
            Asking::OpenStack => Next::Now(Asking::Azure),
            named @ Asking::UserData { .. } => Next::Later(named),
        }
    }

    /// Judges the answer that arrived at `arrived`: what to do next, and what the answer was, for
    /// the fatal line should it be the last.
    fn judge(self, answer: Result<Response, HttpError>, arrived: u64) -> (Next, String) {
        let response = match answer {
            Ok(response) => response,
            Err(error @ HttpError::Stream(StreamError::Unreachable)) => {
                return (Next::Routed(self), error.to_string());
            }
            Err(error @ (HttpError::Stream(_) | HttpError::Closed)) => {
                return (Next::Later(self), error.to_string());
            }
            Err(error) => return (self.otherwise(), error.to_string()),
        };
        let said = format!("answered {}", response.status);
        match (self, response.status) {
            (asking, 410) => (Next::Later(asking), said),
            (Asking::Azure, 200) => match azure(&response, arrived) {
                Some(found) => (Next::Found(found), said),
                None => (
                    Next::Now(Asking::OpenStack),
                    format!("{said}, and its body is not Azure's document"),
                ),
            },
            (Asking::OpenStack, 200) => match openstack(&response) {
                Some(instance) => (
                    Next::Now(Asking::UserData {
                        instance,
                        date: date(&response),
                        read_at_ns: arrived,
                    }),
                    said,
                ),
                None => (
                    Next::Now(Asking::Azure),
                    format!("{said}, and its body is not OpenStack's document"),
                ),
            },
            (
                Asking::UserData {
                    instance,
                    date,
                    read_at_ns,
                },
                status @ (200 | 404),
            ) => (
                Next::Found(Found {
                    cloud: Cloud::OpenStack,
                    instance,
                    date,
                    read_at_ns,
                    user_data: if status == 200 {
                        Ok(response.body)
                    } else {
                        Err(Refusal::Missing)
                    },
                }),
                said,
            ),
            (asking, _) => (asking.otherwise(), said),
        }
    }
}

/// The one `Date` header of an answer.
fn date(response: &Response) -> Option<String> {
    match response.header("date") {
        Header::One(value) => Some(value.to_string()),
        Header::Absent | Header::Repeated => None,
    }
}

/// What Azure's instance document says, or `None` when the body is not that document: JSON with
/// an instance ID at `compute.vmId`. `compute.userData`, when the instance has any, is base64.
fn azure(response: &Response, arrived: u64) -> Option<Found> {
    let document: serde_json::Value = serde_json::from_slice(&response.body).ok()?;
    let compute = document.get("compute")?;
    let instance = compute
        .get("vmId")?
        .as_str()
        .filter(|id| instance_valid(id))?;
    let user_data = match compute.get("userData") {
        None | Some(serde_json::Value::Null) => Err(Refusal::Missing),
        Some(serde_json::Value::String(text)) if text.is_empty() => Err(Refusal::Missing),
        Some(serde_json::Value::String(text)) => {
            base64::decode(text.as_bytes()).ok_or(Refusal::NotBase64)
        }
        Some(_) => return None,
    };
    Some(Found {
        cloud: Cloud::Azure,
        instance: instance.to_string(),
        date: date(response),
        read_at_ns: arrived,
        user_data,
    })
}

/// The instance ID of OpenStack's instance document, or `None` when the body is not that
/// document: JSON with an instance ID at `uuid`.
fn openstack(response: &Response) -> Option<String> {
    let document: serde_json::Value = serde_json::from_slice(&response.body).ok()?;
    let instance = document.get("uuid")?.as_str()?;
    instance_valid(instance).then(|| instance.to_string())
}

/// When requests may begin: never a sixth within a second of the fifth before it.
#[derive(Default)]
struct Pace {
    /// When each of the last requests began, as a ring.
    begun: [u64; REQUESTS_A_SECOND],
    made: usize,
}

impl Pace {
    /// Waits until a request may begin, which is not before `earliest`, and says when it did; or
    /// waits for `deadline` and says `None`, when no request may begin before it.
    fn begin<C: Clock>(&mut self, clock: &mut C, earliest: u64, deadline: u64) -> Option<u64> {
        let slot = self.made % REQUESTS_A_SECOND;
        let mut at = clock.now().max(earliest);
        if self.made >= REQUESTS_A_SECOND {
            at = at.max(self.begun[slot].saturating_add(SECOND));
        }
        clock.sleep_until(at.min(deadline));
        let now = clock.now();
        if now >= deadline {
            return None;
        }
        self.begun[slot] = now;
        self.made += 1;
        Some(now)
    }
}

/// One request on a connection of its own.
fn ask<N: Network>(
    network: &mut N,
    request: &Request<'_>,
    deadline: u64,
) -> Result<Response, HttpError> {
    let mut stream = network.connect(ADDRESS, PORT, deadline)?;
    http::exchange(&mut stream, request, deadline)
}

/// Finds the cloud and the instance's user-data, within [`LIMIT_NS`] of `started`.
pub fn find<N: Network, C: Clock>(
    network: &mut N,
    clock: &mut C,
    started: u64,
) -> Result<Found, Fatal> {
    let deadline = started.saturating_add(LIMIT_NS);
    let mut pace = Pace::default();
    let mut asking = Asking::Azure;
    let mut earliest = started;
    // Whether the node has had an address: before it has, a request with no route waits for
    // one rather than for the next second.
    let mut addressed = false;
    let mut last = String::from("no request was made");
    loop {
        let Some(begun) = pace.begin(clock, earliest, deadline) else {
            return Err(Fatal::new(
                Stage::InstanceData,
                format!("nothing by {} s: {last}", LIMIT_NS / SECOND),
            ));
        };
        let request = asking.request();
        let answer = ask(network, &request, deadline);
        let (next, said) = asking.judge(answer, clock.now());
        last = format!("{} {}: {said}", request.method(), request.path());
        asking = match next {
            Next::Found(found) => return Ok(found),
            Next::Now(asking) => asking,
            Next::Later(asking) => {
                earliest = begun.saturating_add(RETRY_NS);
                asking
            }
            Next::Routed(asking) if addressed => {
                earliest = begun.saturating_add(RETRY_NS);
                asking
            }
            Next::Routed(asking) => {
                match network.address(deadline) {
                    // The address has come: the request begins again at once.
                    Ok(_) => {
                        addressed = true;
                        earliest = clock.now();
                    }
                    // Past the limit, the next turn says so; anything else is asked again a
                    // second later.
                    Err(error) => {
                        earliest = begun.saturating_add(RETRY_NS);
                        last = format!("{last}, and the node's address: {error}");
                    }
                }
                asking
            }
        };
    }
}

/// The `Date` of `cloud`'s instance document now, and the monotonic clock when it arrived: one
/// request, which must be answered `200` with exactly one `Date` header before `deadline`. For
/// setting the wall clock again once the node is ready (system design 3D).
pub fn date_of<N: Network, C: Clock>(
    network: &mut N,
    clock: &C,
    cloud: Cloud,
    deadline: u64,
) -> Result<(String, u64), String> {
    let request = match cloud {
        Cloud::Azure => AZURE,
        Cloud::OpenStack => OPENSTACK,
        Cloud::None => return Err("a node with no cloud has no instance data".into()),
    };
    let asked = |what: String| format!("{} {}: {what}", request.method(), request.path());
    let response = ask(network, &request, deadline).map_err(|error| asked(error.to_string()))?;
    let arrived = clock.now();
    if response.status != 200 {
        return Err(asked(format!("answered {}", response.status)));
    }
    let date =
        date(&response).ok_or_else(|| asked("its answer has no single Date header".into()))?;
    Ok((date, arrived))
}
