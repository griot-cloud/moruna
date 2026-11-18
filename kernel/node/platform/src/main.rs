//! The `platform` process (system design, sections 1D and 2B; work items 1.4 and 2.2c). It takes
//! its start handles, wires the library to the network server and to the supervisor, and runs
//! the boot:
//!
//! - `console`: where its lines go;
//! - `account`: its own account, which the network server charges its sockets to;
//! - `network`: a listener of the network server, granted outbound connections to instance data
//!   and to Azure's WireServer, for the ready report;
//! - `supervisor`: its `platform` bootstrap, whose end the supervisor keeps: first the node's
//!   memory, then, at each start of `api`, `api-started` with the channel to that start.
//!
//! On a fatal error it prints the line and exits with failure, which stops the node. Once the
//! node is ready it stays, holding the hand-over, and gives it again to each new start of `api`
//! (system design 2B).

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{format, string::String};

use moruna_node_platform::{
    boot::{self, Fatal, Inputs, Stage},
    instance,
    svc::{ChannelControl, Monotonic, Wan},
};
use moruna_svc::{Account, Channel, Console, now, println, start};
use netstack_protocol::net;

moruna_svc::main!(run);

/// Boots the node's platform and then serves each new start of `api`; returns only the reason it
/// cannot go on.
fn boot(console: &Console, started_ns: u64) -> Fatal {
    match serve(console, started_ns) {
        Ok(never) => match never {},
        Err(fatal) => fatal,
    }
}

fn serve(console: &Console, started_ns: u64) -> Result<core::convert::Infallible, Fatal> {
    let instance_data = |reason: String| Fatal::new(Stage::InstanceData, reason);
    // Every start handle first, so an image that lacks one says so before anything is asked.
    let account = start::take("account")
        .map(Account)
        .ok_or_else(|| instance_data("platform was started without its account".into()))?;
    let listener = start::take("network")
        .map(Channel)
        .ok_or_else(|| instance_data("platform was started without its network channel".into()))?;
    let own = |reason: &str| Fatal::new(Stage::Platform, reason);
    let mut control = start::take("supervisor")
        .map(|handle| ChannelControl(Channel(handle)))
        .ok_or_else(|| own("platform was started without its bootstrap"))?;
    // The supervisor queues the message before it starts `platform`, so it is not waited for.
    let memory_bytes = control
        .memory(now())
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
        &mut control,
        &mut |line: &str| println!(console, "{}", line),
        Inputs {
            started_ns,
            memory_bytes,
        },
    )?;
    Err(boot::serve(&mut clock, &mut control, &handover))
}

fn run() -> i64 {
    let started_ns = now();
    let Some(console) = start::take("console").map(Console) else {
        return 1;
    };
    println!(console, "{}", boot(&console, started_ns));
    1
}
