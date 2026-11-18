//! The `platform` process (system design, sections 1D and 2B; work items 1.4 and 2.2c). It takes
//! its start handles, wires the library to the network server and to the supervisor, and runs
//! the boot:
//!
//! - `console`: where its lines go;
//! - `account`: its own account, which the network server charges its sockets to;
//! - `network`: a listener of the network server, granted outbound connections to instance data
//!   and to Azure's WireServer, for the ready report;
//! - `supervisor`: its `platform` bootstrap, whose end the supervisor keeps: first the node's
//!   memory, then, at each start of `api`, `api-started` with the channel to that start;
//! - `clock`: `root.clock`, the right to set the node's one wall clock (system design 3D);
//! - `imagefs`: a listener of the image filesystem, whose answer to the `Hello` said through it
//!   at start says the engine's layer is mounted (system design 1B, item 1.6e).
//!
//! On a fatal error it prints the line and exits with failure, which stops the node. Once the
//! node is ready it stays, holding the hand-over, and gives it again to each new start of `api`
//! (system design 2B). Every hour it sets the wall clock again from its cloud's `Date`: a re-read
//! that fails prints `platform: wall clock not refreshed: <reason>` once, and the next that
//! succeeds `platform: wall clock refreshed`; the clock runs on either way.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{format, string::String};

use moruna_node_platform::{
    boot::{self, Fatal, Inputs, Stage},
    instance,
    machine::WallClock,
    svc::{ChannelControl, ImageFs, Monotonic, Wan},
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
    let mut wall = start::take("clock")
        .map(CoreClock)
        .ok_or_else(|| own("platform was started without root.clock"))?;
    // The Hello to imagefs is said now, so that its answer, which says the engine's layer is
    // mounted, is there when the boot looks after api's ready (item 1.6e).
    let mut layer = start::take("imagefs")
        .map(|handle| ImageFs::hello(Channel(handle)))
        .ok_or_else(|| own("platform was started without its link to imagefs"))?;
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
        &mut wall,
        &mut control,
        &mut layer,
        &mut |line: &str| println!(console, "{}", line),
        Inputs {
            started_ns,
            memory_bytes,
        },
    )?;
    Err(boot::serve(
        &mut network,
        &mut clock,
        &mut wall,
        &mut control,
        &handover,
        &mut |line: &str| println!(console, "{}", line),
    ))
}

fn run() -> i64 {
    let started_ns = now();
    let Some(console) = start::take("console").map(Console) else {
        return 1;
    };
    println!(console, "{}", boot(&console, started_ns));
    1
}
