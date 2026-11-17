//! The node's `api` process (system design 1C and 2B, work item 1.3).
//!
//! Its start handles are the supervisor's: `console`, its own `account`, `network`, the
//! listener of the network server's grant `listen-api`, which permits listening on port
//! 443 and nothing else, and `platform`, its link to `platform`. It connects to the
//! network server, takes `platform`'s hand-over, and runs the service: a listener on
//! `wan`'s leased address at 443, which follows the lease, closing each connection it
//! accepts. Once the listener first opens it answers `platform` `ready`. It says when it
//! starts and stops listening.
//!
//! It never ends by itself. When it cannot go on (a start handle is missing, the
//! hand-over is refused, or a request to the network server or `platform` fails) it
//! prints `moruna: fatal api: <reason>` and
//! exits with failure (system design 1B). What a dead `api` then means for the node is
//! the supervisor's to decide (1B and 2B).

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use core::convert::Infallible;

use moruna_node_api::{Error, Fatal, connect};
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
    let platform = start::take("platform")
        .map(Channel)
        .ok_or(Error::NoSlot("platform"))?;
    moruna_node_api::run(connect(&network, account)?, &platform, &mut |line| {
        println!(out, "{}", line)
    })
}
