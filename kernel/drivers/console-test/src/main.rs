//! DR-T12 (SDD 25, k): a client of the console drivers, found through the directory.
//! It writes three lines through `console0` (the boot attaches it to a file the CI
//! step reads), runs the console protocol's conformance suite against it, reads the
//! line the host put on `console1`'s input, and writes that back through `console0`.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{format, string::String, vec::Vec};

use console_protocol::console;
use devices_protocol::directory;
use moruna_svc::{Channel, Console, conformance, deadline_in, println, start};

moruna_svc::main!(run);

const SECONDS: u64 = 1_000_000_000;
/// The line the CI step puts on `console1`'s input.
const HOST_LINE: &[u8] = b"KB3 console input: hello from the host";

struct Lookup<'a>(&'a mut directory::Client, &'static str);

impl conformance::Connector for Lookup<'_> {
    fn connect(&mut self) -> Result<Channel, conformance::Fail> {
        self.0
            .lookup(String::from(self.1))
            .map_err(|e| format!("looking up {}: {e:?}", self.1))
    }
}

fn open(dir: &mut directory::Client, name: &str) -> Result<console::Client, String> {
    let channel = dir
        .lookup(String::from(name))
        .map_err(|e| format!("looking up {name}: {e:?}"))?;
    let mut c = console::Client::connect(channel, deadline_in(60 * SECONDS))
        .map_err(|e| format!("hello to {name}: {e:?}"))?;
    c.core().set_timeout(60 * SECONDS);
    Ok(c)
}

fn lines(dir: &mut directory::Client) -> Result<(), String> {
    let mut c0 = open(dir, "console0")?;
    for n in 1..=3 {
        let line = format!("KB3 console0: line {n} of 3 through the driver\n").into_bytes();
        let len = line.len() as u32;
        match c0.write(line) {
            Ok(written) if written == len => {}
            other => return Err(format!("line {n}: {other:?}")),
        }
    }
    let mut c1 = open(dir, "console1")?;
    let read = c1.read_line().map_err(|e| format!("read_line: {e:?}"))?;
    if read != HOST_LINE {
        return Err(format!(
            "console1 read {:?}",
            core::str::from_utf8(&read).unwrap_or("(not text)")
        ));
    }
    let mut back: Vec<u8> = b"KB3 console1 read: ".to_vec();
    back.extend_from_slice(&read);
    back.push(b'\n');
    c0.write(back)
        .map(drop)
        .map_err(|e| format!("writing back: {e:?}"))
}

fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    let Some(d) = start::take("directory") else {
        println!(out, "KB3 console: FAIL: no directory");
        return 1;
    };
    let mut dir = match devices_protocol::open_directory(Channel(d), deadline_in(300 * SECONDS)) {
        Ok(c) => c,
        Err(e) => {
            println!(out, "KB3 console: FAIL: the directory: {:?}", e);
            return 1;
        }
    };
    dir.core().set_timeout(300 * SECONDS);
    let mut failed = 0;
    match lines(&mut dir) {
        Ok(()) => println!(
            out,
            "KB3 console: ok: three lines written through console0, and the host's line read from console1 and written back"
        ),
        Err(e) => {
            println!(out, "KB3 console: FAIL: {}", e);
            failed += 1;
        }
    }
    let suite = console_protocol::suite();
    let summary = conformance::run(&suite, &mut Lookup(&mut dir, "console0"), &mut |check| {
        if let Err(why) = check.result {
            println!(out, "KB3 conformance: FAIL: {}: {}", check.name, why);
        }
    });
    if summary.ok() {
        println!(
            out,
            "KB3 conformance {}: ok: {} checks passed against console0",
            suite.id.name,
            summary.passed
        );
    } else {
        failed += 1;
    }
    failed
}
