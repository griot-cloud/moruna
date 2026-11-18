//! `platform`'s boot, in the order of the system design's section 1D: find the cloud, set the
//! wall clock, validate node config, hand `api` its config, and print the `cloud` and `ready`
//! lines. Every way it cannot go on is one [`Fatal`], which prints as section 1B's
//! `moruna: fatal <stage>: <reason>` line.

use alloc::{
    format,
    string::{String, ToString},
};
use core::{
    fmt::{self, Write},
    net::Ipv4Addr,
};

use moruna_node_config::{Cloud, Handover, NodeConfig, READY, cloud_name};

use crate::{
    date, instance,
    machine::{Clock, Link, LinkError, Network, WallClock},
};

/// The port `api` listens on.
pub const API_PORT: u16 = 443;

/// How long `api` has to answer the hand-over `ready`, from when `platform` begins to hand it
/// over (PM, 2026-11-18).
pub const READY_LIMIT_NS: u64 = 10 * instance::SECOND;

/// The stage a boot stopped in, as the fatal line names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Finding the cloud, its instance data and its `Date`.
    InstanceData,
    /// Validating node config.
    Config,
    /// Handing `api` its config and hearing that it is ready: `platform`'s own stage.
    Platform,
}

impl Stage {
    /// The stage's name on the console.
    pub const fn name(self) -> &'static str {
        match self {
            Stage::InstanceData => "instance-data",
            Stage::Config => "config",
            Stage::Platform => "platform",
        }
    }
}

/// Why `platform` cannot go on. It prints the line and exits with failure, and the node stops.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fatal {
    /// Where it stopped.
    pub stage: Stage,
    /// Why, in words.
    pub reason: String,
}

/// The most characters of a reason a fatal line shows, so the line fits the console's buffer.
const REASON_SHOWN: usize = 300;

impl Fatal {
    /// A fatal error in `stage`.
    pub fn new(stage: Stage, reason: impl Into<String>) -> Fatal {
        Fatal {
            stage,
            reason: reason.into(),
        }
    }
}

impl fmt::Display for Fatal {
    /// The console line. The reason may quote what came from outside the node, so it is shown
    /// as one line of printable ASCII.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "moruna: fatal {}: ", self.stage.name())?;
        for character in self.reason.chars().take(REASON_SHOWN) {
            let shown = character == ' ' || character.is_ascii_graphic();
            f.write_char(if shown { character } else { '?' })?;
        }
        Ok(())
    }
}

/// The line that names the cloud: `moruna: cloud <cloud> instance <id>`.
pub fn cloud_line(cloud: Cloud, instance: &str) -> String {
    format!("moruna: cloud {} instance {instance}", cloud_name(cloud))
}

/// The line that says the node is ready: `moruna: ready api=<address>:443 boot_ms=<n>`.
pub fn ready_line(address: Ipv4Addr, boot_ms: u64) -> String {
    format!("moruna: ready api={address}:{API_PORT} boot_ms={boot_ms}")
}

/// What `platform` is told about its node besides instance data.
#[derive(Clone, Copy, Debug)]
pub struct Inputs {
    /// The monotonic clock when `platform` started: instance data's 90 s count from it.
    pub started_ns: u64,
    /// The node's memory: the machine account's limit, which the supervisor gives `platform` at
    /// start ([`crate::svc::node_memory`]). It bounds `functions.cache_bytes`, and the hand-over
    /// carries it.
    pub memory_bytes: u64,
}

/// Sets the node's wall clock to `unix_seconds` as of `read_at_ns` on the monotonic clock: what it
/// reads now is that time and the time since.
fn set_wall_clock<C: Clock, W: WallClock>(
    wall: &mut W,
    clock: &C,
    unix_seconds: u64,
    read_at_ns: u64,
) -> Result<(), String> {
    let utc_ns = unix_seconds
        .checked_mul(instance::SECOND)
        .ok_or_else(|| String::from("the time is past what the clock holds"))?
        .saturating_add(clock.now().saturating_sub(read_at_ns));
    wall.set(utc_ns)
}

/// Boots: returns the hand-over `api` took, having set the wall clock through `wall` and printed
/// the `cloud` and `ready` lines through `console`, or the reason the node must stop.
pub fn run<N: Network, C: Clock, W: WallClock, L: Link>(
    network: &mut N,
    clock: &mut C,
    wall: &mut W,
    link: &mut L,
    console: &mut dyn FnMut(&str),
    inputs: Inputs,
) -> Result<Handover, Fatal> {
    // 1. The cloud, from instance data.
    let found = instance::find(network, clock, inputs.started_ns)?;

    // 2. The wall clock, from the Date header of the answer that named the cloud.
    let wall_clock = match &found.date {
        None => Err("the answer that named the cloud has no single Date header"),
        Some(date) => date::parse(date)
            .ok_or("the Date header of the answer that named the cloud is not an IMF-fixdate"),
    }
    .map_err(|reason| Fatal::new(Stage::InstanceData, reason))?;
    // It is the node's one wall clock (system design 3D): the network server's TLS reads it.
    set_wall_clock(wall, clock, wall_clock, found.read_at_ns).map_err(|reason| {
        Fatal::new(
            Stage::InstanceData,
            format!("the wall clock was not set from the Date header: {reason}"),
        )
    })?;

    // 3. Node config: the instance's user-data.
    let config = found
        .user_data
        .and_then(|user_data| NodeConfig::validate(&user_data, inputs.memory_bytes))
        .map_err(|refusal| Fatal::new(Stage::Config, refusal.to_string()))?;
    let handover = Handover {
        config,
        instance: found.instance,
        cloud: found.cloud,
        wall_clock,
        read_at_ns: found.read_at_ns,
        memory_bytes: inputs.memory_bytes,
    };
    let message = handover
        .encode()
        .map_err(|error| Fatal::new(Stage::Config, format!("node config is too large: {error}")))?;
    // The cloud's line comes before the hand-over, so it is on the console before any line
    // `api` prints once it has its config.
    console(&cloud_line(handover.cloud, &handover.instance));

    // 4. The hand-over, and `api`'s answer, within the limit. The address the ready line
    // names is asked first: the network server answers it at once, and asking it after `api`
    // has answered could find the limit all but spent.
    let fatal = |reason: String| Fatal::new(Stage::Platform, reason);
    let deadline = clock.now().saturating_add(READY_LIMIT_NS);
    let address = network
        .address(deadline)
        .map_err(|error| fatal(format!("the node's address is not known: {error}")))?;
    link.send(&message, deadline)
        .map_err(|error| fatal(format!("the hand-over was not sent to api: {error}")))?;
    match link.receive(deadline) {
        Ok(answer) if answer == READY => {}
        Ok(_) => {
            return Err(fatal(
                "api answered the hand-over with something else".into(),
            ));
        }
        Err(LinkError::TimedOut) => {
            let seconds = READY_LIMIT_NS / instance::SECOND;
            return Err(fatal(format!(
                "api did not answer ready within {seconds} s"
            )));
        }
        Err(error) => return Err(fatal(format!("api did not answer: {error}"))),
    }

    // 5. Ready.
    console(&ready_line(address, clock.now() / 1_000_000));
    Ok(handover)
}

/// How often `platform` sets the wall clock again once the node is ready (system design 3D): S3
/// and Blob sign requests with it, and refuse any more than 15 minutes from their own.
pub const RETIME_NS: u64 = 3600 * instance::SECOND;

/// How long the request that reads the `Date` again may take.
pub const RETIME_LIMIT_NS: u64 = 10 * instance::SECOND;

/// Sets the wall clock again from the `Date` of `cloud`'s instance document: one request, within
/// [`RETIME_LIMIT_NS`]; returns the Unix seconds it was set to. When it cannot, the clock runs on
/// as it was last set and the reason is returned: the node goes on either way.
pub fn retime<N: Network, C: Clock, W: WallClock>(
    network: &mut N,
    clock: &mut C,
    wall: &mut W,
    cloud: Cloud,
) -> Result<u64, String> {
    let deadline = clock.now().saturating_add(RETIME_LIMIT_NS);
    let (date, read_at_ns) = instance::date_of(network, clock, cloud, deadline)?;
    let unix_seconds =
        date::parse(&date).ok_or_else(|| String::from("the Date header is not an IMF-fixdate"))?;
    set_wall_clock(wall, clock, unix_seconds, read_at_ns)?;
    Ok(unix_seconds)
}
