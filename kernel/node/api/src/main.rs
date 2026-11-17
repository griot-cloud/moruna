//! The node's `api` process (system design 1C and 2B, work item 1.3).
//!
//! Its start handles are the supervisor's: `console`, its own `account`, and `network`,
//! the listener of the network server's grant `listen-api`, which permits listening on
//! port 443 and nothing else. It connects to the network server, waits for the node's
//! address, opens its listener there on 443 and says so, then accepts each connection
//! and closes it.
//!
//! It never ends by itself. When it cannot go on (a start handle is missing, or a
//! request to the network server fails) it prints `moruna: fatal api: <reason>` and
//! exits with failure (system design 1B). What a dead `api` then means for the node is
//! the supervisor's to decide (1B and 2B).

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use core::convert::Infallible;

use moruna_node_api::{Error, Fatal, await_address, connect, listen, turn};
use moruna_svc::{Account, Channel, Console, println, start};

moruna_svc::main!(run);

fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    let Err(error) = serve(&out);
    println!(out, "{}", Fatal(&error));
    1
}

fn serve(out: &Console) -> Result<Infallible, Error> {
    let network = start::take("network")
        .map(Channel)
        .ok_or(Error::NoSlot("network"))?;
    let account = start::take("account")
        .map(Account)
        .ok_or(Error::NoSlot("account"))?;
    let mut client = connect(&network, account)?;
    let address = await_address(&mut client)?;
    let (listener, at) = listen(&mut client, address)?;
    println!(out, "api: listening on {}", at);
    loop {
        turn(&mut client, listener)?;
    }
}
