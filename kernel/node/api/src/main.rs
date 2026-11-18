//! The node's `api` process (system design 1C, 2B and 2C; work items 1.3 and 2.2b).
//!
//! Its start handles are the supervisor's, made afresh at each start (item 2.2c): `console`,
//! its own `account`, `network`, the listener the supervisor mints from the network server's
//! grant `listen-supervisor`, which permits listening on port 443 and nothing else, and
//! `platform`, its channel to `platform`. It connects to the
//! network server and runs the library: `platform`'s hand-over, the API server made from
//! it and its `moruna: tls sha256=` line, and the loop that serves the API on `wan`'s
//! leased address at 443, answering `platform` `ready` once the listener first opens.
//!
//! It never ends by itself. When it cannot go on (a start handle is missing, the
//! hand-over or the operator's TLS identity is refused, or a request to the network
//! server or `platform` fails) it prints `moruna: fatal api: <reason>` and exits with
//! failure (system design 1B). What a dead `api` then means for the node is the
//! supervisor's to decide (1B and 2B).
//!
//! Built with feature `test-exit`, for the `cloud-test` image only, it also exits, with no
//! fatal line, on a request to `/v1/test/exit`, and says at each start that it is that build.

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
    // The `cloud-test` image says what it is at each start of `api` (work item 2.2c).
    #[cfg(feature = "test-exit")]
    println!(
        out,
        "api: cloud-test build, never a release image: exits on {}",
        moruna_api_server::TEST_EXIT_PATH
    );
    let Err(error) = serve(&out);
    // Ended by the test's request: no failure of api's, so no fatal line, which would be
    // taken for the node's cause.
    #[cfg(feature = "test-exit")]
    if error == Error::TestExit {
        println!(out, "api: exits: {}", error);
        return 0;
    }
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
    // The node document's cores: the CPUs the core brought online.
    let cores = u32::try_from(start::cpus()).unwrap_or(u32::MAX);
    moruna_node_api::run(connect(&network, account)?, &platform, cores, &mut |line| {
        println!(out, "{}", line)
    })
}
