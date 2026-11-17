// SPDX-License-Identifier: MPL-2.0

//! B2's booted check of a task's filesystems and descriptors: asks the `linux` server on
//! the slot `server` to run `taskcheck` with an account and a scratch account carved out of
//! this process's own, and prints `B2 task: ok: ...` or `B2 task: FAIL: ...`.

use alloc::{format, string::String, vec, vec::Vec};

use linux_protocol::{Bytes, Grant, linux};
use linuxlayer_check::{LIMIT, PROGRAM, SCRATCH, argv, verdict};
use moruna_svc::{Account, Channel, Console, deadline_in, println, start};

moruna_svc::main!(run);

const SECONDS: u64 = 1_000_000_000;

fn b(s: &str) -> Bytes {
    Bytes(s.as_bytes().to_vec())
}

fn carve(limit: u64, name: &str) -> Result<Account, String> {
    Account::carve(limit, name).map_err(|e| format!("an account of {limit} bytes: {}", e.name()))
}

fn check(server: &Channel) -> Result<String, String> {
    let channel = moruna_svc::connect(server).map_err(|e| format!("connecting: {e:?}"))?;
    let mut client = linux::Client::connect(channel, deadline_in(600 * SECONDS))
        .map_err(|e| format!("hello: {e:?}"))?;
    client.core().set_timeout(600 * SECONDS);
    let grant = Grant {
        handles: Vec::new(),
        account: carve(LIMIT, "linuxlayer-check.task")?.0,
        scratch: Some(carve(SCRATCH, "linuxlayer-check.scratch")?.0),
    };
    let ended = client
        .run_granted(
            b(PROGRAM),
            argv().iter().map(|a| b(a)).collect(),
            vec![b("HOME=/tmp")],
            grant,
        )
        .map_err(|e| format!("run_granted {PROGRAM}: {e:?}"))?;
    verdict(&ended)
}

fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    let Some(server) = start::take("server").map(Channel) else {
        println!(out, "B2 task: FAIL: no `server` slot");
        return 1;
    };
    match check(&server) {
        Ok(line) => {
            println!(out, "B2 task: ok: {}", line);
            0
        }
        Err(why) => {
            println!(out, "B2 task: FAIL: {}", why);
            1
        }
    }
}
