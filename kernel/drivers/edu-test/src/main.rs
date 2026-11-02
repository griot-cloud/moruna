//! DR-T17 (SDD 25, k): `edu0`, a driver added to the table with no kernel change,
//! found through the directory: its factorial, and its protocol's conformance suite.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{format, string::String};

use devices_protocol::directory;
use edu_protocol::edu;
use moruna_svc::{Channel, Console, conformance, deadline_in, println, start};

moruna_svc::main!(run);

const SECONDS: u64 = 1_000_000_000;

struct Lookup<'a>(&'a mut directory::Client);

impl conformance::Connector for Lookup<'_> {
    fn connect(&mut self) -> Result<Channel, conformance::Fail> {
        self.0
            .lookup(String::from("edu0"))
            .map_err(|e| format!("looking up edu0: {e:?}"))
    }
}

fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    let Some(d) = start::take("directory") else {
        println!(out, "KB3 edu: FAIL: no directory");
        return 1;
    };
    let mut dir = match devices_protocol::open_directory(Channel(d), deadline_in(300 * SECONDS)) {
        Ok(c) => c,
        Err(e) => {
            println!(out, "KB3 edu: FAIL: the directory: {:?}", e);
            return 1;
        }
    };
    dir.core().set_timeout(300 * SECONDS);
    let factorial = dir
        .lookup(String::from("edu0"))
        .map_err(|e| format!("looking up edu0: {e:?}"))
        .and_then(|c| {
            edu::Client::connect(c, deadline_in(60 * SECONDS))
                .map_err(|e| format!("hello to edu0: {e:?}"))
        })
        .and_then(|mut c| {
            c.core().set_timeout(60 * SECONDS);
            c.factorial(10).map_err(|e| format!("factorial: {e:?}"))
        });
    match factorial {
        Ok(3_628_800) => {}
        Ok(other) => {
            println!(out, "KB3 edu: FAIL: 10! came back {}", other);
            return 1;
        }
        Err(e) => {
            println!(out, "KB3 edu: FAIL: {}", e);
            return 1;
        }
    }
    let suite = edu_protocol::suite();
    let summary = conformance::run(&suite, &mut Lookup(&mut dir), &mut |check| {
        if let Err(why) = check.result {
            println!(out, "KB3 conformance: FAIL: {}: {}", check.name, why);
        }
    });
    if !summary.ok() {
        return 1;
    }
    println!(
        out,
        "KB3 edu: ok: edu0, a driver that is a line of the table, computed 10! and passed {} checks of {}",
        summary.passed,
        suite.id.name
    );
    0
}
