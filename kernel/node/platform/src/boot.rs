//! `platform`'s boot, in the order of the system design's section 1D: find the cloud, set the
//! wall clock, validate node config, report ready to Azure on Azure (section 1F), hand `api` its
//! config, and print the `cloud` and `ready` lines. Every way it cannot go on is one [`Fatal`],
//! which prints as section 1B's `moruna: fatal <stage>: <reason>` line.

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
    machine::{Clock, Control, FOREVER, Link, LinkError, Network},
    wireserver,
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

/// Boots: returns the hand-over `api` took, with the node's `boot_ms` from the ready line for
/// `api`'s later starts, having printed the `cloud` and `ready` lines through `console`, or the
/// reason the node must stop.
pub fn run<N: Network, C: Clock, K: Control>(
    network: &mut N,
    clock: &mut C,
    control: &mut K,
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
        memory_bytes: inputs.memory_bytes,
        // None until the node has been ready: this first start of api makes its own.
        boot_ms: None,
    };
    let message = handover
        .encode()
        .map_err(|error| Fatal::new(Stage::Config, format!("node config is too large: {error}")))?;
    // The cloud's line comes before the hand-over, so it is on the console before any line
    // `api` prints once it has its config.
    console(&cloud_line(handover.cloud, &handover.instance));

    // On Azure, the ready report, now that node config is valid (section 1F); without it Azure
    // reboots the machine. Other clouds want none.
    if handover.cloud == Cloud::Azure {
        wireserver::report(network, clock)?;
    }

    // 4. The hand-over, and `api`'s answer, within the limit. The address the ready line
    // names is asked first: the network server answers it at once, and asking it after `api`
    // has answered could find the limit all but spent.
    let deadline = clock.now().saturating_add(READY_LIMIT_NS);
    let address = network.address(deadline).map_err(|error| {
        Fatal::new(
            Stage::Platform,
            format!("the node's address is not known: {error}"),
        )
    })?;
    hand_over(control, &message, deadline)?;

    // 5. Ready. The line's boot_ms is the node's, and every later start of api is handed it
    // (PM, item 2.2c).
    let boot_ms = clock.now() / 1_000_000;
    console(&ready_line(address, boot_ms));
    Ok(Handover {
        boot_ms: Some(boot_ms),
        ..handover
    })
}

/// Hands `message`, the encoded hand-over, to the next start of `api`, whose channel the
/// supervisor sends on `control`, and hears `api` answer `ready`, all by `deadline`.
fn hand_over<K: Control>(control: &mut K, message: &[u8], deadline: u64) -> Result<(), Fatal> {
    let mut link = control.next_api(deadline).map_err(|error| {
        Fatal::new(
            Stage::Platform,
            format!(
                "the supervisor sent no channel to api: {}",
                control_error(error)
            ),
        )
    })?;
    exchange(&mut link, message, deadline)
}

/// A failure of the supervisor's channel, in words: its closing is the supervisor's, not `api`'s.
fn control_error(error: LinkError) -> String {
    match error {
        LinkError::Closed => "the supervisor closed its channel".into(),
        error => error.to_string(),
    }
}

/// Sends `message` on `link` and hears `api` answer `ready`, by `deadline`.
fn exchange<L: Link>(link: &mut L, message: &[u8], deadline: u64) -> Result<(), Fatal> {
    let fatal = |reason: String| Fatal::new(Stage::Platform, reason);
    link.send(message, deadline)
        .map_err(|error| fatal(format!("the hand-over was not sent to api: {error}")))?;
    match link.receive(deadline) {
        Ok(answer) if answer == READY => Ok(()),
        Ok(_) => Err(fatal(
            "api answered the hand-over with something else".into(),
        )),
        Err(LinkError::TimedOut) => {
            let seconds = READY_LIMIT_NS / instance::SECOND;
            Err(fatal(format!(
                "api did not answer ready within {seconds} s"
            )))
        }
        Err(error) => Err(fatal(format!("api did not answer: {error}"))),
    }
}

/// After the boot, hands `handover` again to each new start of `api`, as the supervisor sends
/// that start's channel (system design 2B; PM, item 2.2c), with 1D's 10 s for `ready` each
/// time, counted from when the channel arrives. Nothing reaches the console: the supervisor's
/// restart line and `api`'s new `tls` line say what happened. Returns only the reason it cannot
/// go on.
pub fn serve<C: Clock, K: Control>(clock: &mut C, control: &mut K, handover: &Handover) -> Fatal {
    let message = match handover.encode() {
        Ok(message) => message,
        Err(error) => {
            return Fatal::new(
                Stage::Platform,
                format!("node config is too large: {error}"),
            );
        }
    };
    loop {
        // The wait for a start has no deadline: `api` may never die.
        let mut link = match control.next_api(FOREVER) {
            Ok(link) => link,
            Err(error) => {
                return Fatal::new(
                    Stage::Platform,
                    format!("the supervisor's channel: {}", control_error(error)),
                );
            }
        };
        let deadline = clock.now().saturating_add(READY_LIMIT_NS);
        if let Err(fatal) = exchange(&mut link, &message, deadline) {
            return fatal;
        }
    }
}
