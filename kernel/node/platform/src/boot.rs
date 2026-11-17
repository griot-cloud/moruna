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
use moruna_svc::DEADLINE_INFINITE;

use crate::{
    date, instance,
    machine::{Clock, Link, LinkError, Network},
};

/// The port `api` listens on.
pub const API_PORT: u16 = 443;

/// The stage a boot stopped in, as the fatal line names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Finding the cloud, its instance data and its `Date`.
    InstanceData,
    /// Validating node config.
    Config,
    /// Handing `api` its config and hearing that it is ready.
    Ready,
}

impl Stage {
    /// The stage's name on the console.
    pub const fn name(self) -> &'static str {
        match self {
            Stage::InstanceData => "instance-data",
            Stage::Config => "config",
            Stage::Ready => "ready",
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
    /// The node's memory, which bounds `functions.cache_bytes`; `None` when it is not known.
    pub memory_bytes: Option<u64>,
    /// How long `api` may take to answer the hand-over; `None` for no limit.
    pub ready_limit_ns: Option<u64>,
}

/// Boots: returns the hand-over `api` took, having printed the `cloud` and `ready` lines through
/// `console`, or the reason the node must stop.
pub fn run<N: Network, C: Clock, L: Link>(
    network: &mut N,
    clock: &mut C,
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
    };
    let message = handover
        .encode()
        .map_err(|error| Fatal::new(Stage::Config, format!("node config is too large: {error}")))?;
    // The cloud's line comes before the hand-over, so it is on the console before any line
    // `api` prints once it has its config.
    console(&cloud_line(handover.cloud, &handover.instance));

    // 4. The hand-over, and `api`'s answer.
    let ready = |reason: String| Fatal::new(Stage::Ready, reason);
    let deadline = match inputs.ready_limit_ns {
        None => DEADLINE_INFINITE,
        Some(limit) => clock.now().saturating_add(limit),
    };
    link.send(&message, deadline)
        .map_err(|error| ready(format!("the hand-over was not sent to api: {error}")))?;
    match link.receive(deadline) {
        Ok(answer) if answer == READY => {}
        Ok(_) => {
            return Err(ready(
                "api answered the hand-over with something else".into(),
            ));
        }
        Err(LinkError::TimedOut) => {
            let seconds = inputs.ready_limit_ns.unwrap_or_default() / instance::SECOND;
            return Err(ready(format!("api did not answer within {seconds} s")));
        }
        Err(error) => return Err(ready(format!("api did not answer: {error}"))),
    }

    // 5. Ready.
    let address = network
        .address(deadline)
        .map_err(|error| ready(format!("the node's address is not known: {error}")))?;
    console(&ready_line(address, clock.now() / 1_000_000));
    Ok(handover)
}
