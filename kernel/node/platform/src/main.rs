//! The `platform` process (system design, section 1D; work item 1.4). It takes its start handles,
//! wires the library to the network server and to `api`, and runs the boot:
//!
//! - `console`: where its lines go;
//! - `account`: its own account, which the network server charges its sockets to;
//! - `network`: a listener of the network server, granted outbound connections to instance data;
//! - `api`: the channel the supervisor links to `api`, for the hand-over.
//!
//! On a fatal error it prints the line and exits with failure, which stops the node. Once the
//! node is ready it stays, idle and holding the hand-over: a service that ends stops the node.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{format, string::String};

use moruna_node_config::Handover;
use moruna_node_platform::{
    boot::{self, Fatal, Inputs, Stage},
    instance,
    svc::{ApiLink, Monotonic, Wan},
};
use moruna_svc::{
    Account, Channel, Console, DEADLINE_INFINITE, Signals, now, println, start, wait_one,
};
use netstack_protocol::net;

moruna_svc::main!(run);

/// Boots the node's platform; returns the hand-over `api` took.
fn boot(console: &Console, started_ns: u64) -> Result<Handover, Fatal> {
    let instance_data = |reason: String| Fatal::new(Stage::InstanceData, reason);
    // Every start handle first, so an image that lacks one says so before anything is asked.
    let account = start::take("account")
        .map(Account)
        .ok_or_else(|| instance_data("platform was started without its account".into()))?;
    let listener = start::take("network")
        .map(Channel)
        .ok_or_else(|| instance_data("platform was started without its network channel".into()))?;
    let api = start::take("api").map(Channel).ok_or_else(|| {
        Fatal::new(
            Stage::Platform,
            "platform was started without its channel to api",
        )
    })?;

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

    boot::run(
        &mut network,
        &mut clock,
        &mut ApiLink(api),
        &mut |line: &str| println!(console, "{}", line),
        Inputs {
            started_ns,
            // Open, asked of the PM in item 1.4's report: which figure is the node's memory.
            // Until it is given, a node config that sets `functions.cache_bytes` is refused,
            // because its upper bound (half the node's memory) cannot be checked.
            memory_bytes: None,
        },
    )
}

fn run() -> i64 {
    let started_ns = now();
    let Some(console) = start::take("console").map(Console) else {
        return 1;
    };
    let _handover = match boot(&console, started_ns) {
        Ok(handover) => handover,
        Err(fatal) => {
            println!(console, "{}", fatal);
            return 1;
        }
    };
    // Idle: both ends of this channel are held and neither is written, so the wait does not end.
    // If it ever does, `platform` ends with failure and the supervisor stops the node.
    if let Ok(quiet) = Channel::create() {
        let _ = wait_one(&quiet.0.0, Signals::READABLE, DEADLINE_INFINITE);
    }
    1
}
