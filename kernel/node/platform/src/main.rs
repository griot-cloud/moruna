//! The `platform` process (system design, section 1D; work item 1.4). It takes its start handles,
//! wires the library to the network server and to `api`, and runs the boot:
//!
//! - `console`: where its lines go;
//! - `account`: its own account, which the network server charges its sockets to;
//! - `network`: a listener of the network server, granted outbound connections to instance data;
//! - `api`: the channel the supervisor links to `api`, for the hand-over;
//! - `supervisor`: its `platform` bootstrap, the one message that gives the node's memory;
//! - `clock`: `root.clock`, the right to set the node's one wall clock (system design 3D).
//!
//! On a fatal error it prints the line and exits with failure, which stops the node. Once the
//! node is ready it stays, holding the hand-over, and sets the wall clock again from its cloud's
//! `Date` every hour: a service that ends stops the node. A re-read that fails prints
//! `platform: wall clock not refreshed: <reason>` once, and the next that succeeds
//! `platform: wall clock refreshed`; the clock runs on either way.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{format, string::String};

use moruna_node_config::{Cloud, Handover};
use moruna_node_platform::{
    boot::{self, Fatal, Inputs, RETIME_NS, Refresh, Stage},
    instance,
    machine::{Clock, WallClock},
    svc::{self, ChannelLink, Monotonic, Wan},
};
use moruna_svc::{Account, Channel, Console, Handle, now, println, start};
use netstack_protocol::net;

moruna_svc::main!(run);

/// The core's wall clock, through `root.clock`, which the supervisor gives `platform` alone.
struct CoreClock(Handle);

impl WallClock for CoreClock {
    fn set(&mut self, utc_ns: u64) -> Result<(), String> {
        moruna_svc::clock_set_realtime(&self.0, utc_ns).map_err(|error| String::from(error.name()))
    }
}

/// What `platform` holds once the node is ready.
struct Ready {
    /// The hand-over `api` took.
    _handover: Handover,
    cloud: Cloud,
    network: Wan,
    clock: Monotonic,
    wall: CoreClock,
}

/// Boots the node's platform; returns what it holds once the node is ready.
fn boot(console: &Console, started_ns: u64) -> Result<Ready, Fatal> {
    let instance_data = |reason: String| Fatal::new(Stage::InstanceData, reason);
    // Every start handle first, so an image that lacks one says so before anything is asked.
    let account = start::take("account")
        .map(Account)
        .ok_or_else(|| instance_data("platform was started without its account".into()))?;
    let listener = start::take("network")
        .map(Channel)
        .ok_or_else(|| instance_data("platform was started without its network channel".into()))?;
    let own = |reason: &str| Fatal::new(Stage::Platform, reason);
    let api = start::take("api")
        .map(Channel)
        .ok_or_else(|| own("platform was started without its channel to api"))?;
    let supervisor = start::take("supervisor")
        .map(Channel)
        .ok_or_else(|| own("platform was started without its bootstrap"))?;
    let mut wall = start::take("clock")
        .map(CoreClock)
        .ok_or_else(|| own("platform was started without root.clock"))?;
    // The supervisor queues the message before it starts `platform`, so it is not waited for.
    let memory_bytes = svc::node_memory(supervisor, now())
        .map_err(|error| own(&format!("the node's memory is not known: {error}")))?;

    // The network server, within instance data's limit.
    let deadline = started_ns.saturating_add(instance::LIMIT_NS);
    let client = moruna_svc::connect(&listener)
        .map_err(|error| format!("{error:?}"))
        .and_then(|channel| {
            net::Client::connect(channel, deadline, account).map_err(|error| format!("{error:?}"))
        })
        .map_err(|error| instance_data(format!("the network server did not answer: {error}")))?;
    let mut network =
        Wan::new(client).map_err(|error| instance_data(format!("no memory to lend: {error}")))?;
    let mut clock = Monotonic::new().map_err(|error| {
        instance_data(format!(
            "no channel for the clock's waits: {}",
            error.name()
        ))
    })?;

    let handover = boot::run(
        &mut network,
        &mut clock,
        &mut wall,
        &mut ChannelLink(api),
        &mut |line: &str| println!(console, "{}", line),
        Inputs {
            started_ns,
            memory_bytes,
        },
    )?;
    Ok(Ready {
        cloud: handover.cloud,
        _handover: handover,
        network,
        clock,
        wall,
    })
}

fn run() -> i64 {
    let started_ns = now();
    let Some(console) = start::take("console").map(Console) else {
        return 1;
    };
    let mut ready = match boot(&console, started_ns) {
        Ok(ready) => ready,
        Err(fatal) => {
            println!(console, "{}", fatal);
            return 1;
        }
    };
    // Every hour, the wall clock again from the cloud's `Date`. One that cannot be read leaves
    // the clock running as it was set, and the next hour tries again.
    let mut refresh = Refresh::default();
    let mut next = ready.clock.now().saturating_add(RETIME_NS);
    loop {
        ready.clock.sleep_until(next);
        refresh.run(
            &mut ready.network,
            &mut ready.clock,
            &mut ready.wall,
            ready.cloud,
            &mut |line: &str| println!(console, "{}", line),
        );
        next = next.saturating_add(RETIME_NS);
    }
}
