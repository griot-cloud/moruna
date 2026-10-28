//! `moruna.driver.console` 1.0 (SDD 25, d.4): a console line. A client writes bytes,
//! answered once the device has taken them, and reads lines, each answered when a
//! whole line has arrived.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{format, vec, vec::Vec};

use moruna_svc::{
    Encoder, Error, Kind, ProtocolError, Signals, Wire,
    conformance::{Case, Fail, Probe, Suite},
    deadline_in, wait_one,
};

/// The longest line `read_line` returns; a longer one arrives in pieces.
pub const LINE_MAX: usize = 4096;

moruna_svc::protocol! {
    /// A console line: bytes out, lines in.
    pub protocol console {
        name = "moruna.driver.console";
        major = 1;
        minor = 0;
        requests {
            /// Writes `text`; answered with its length once the device has taken it.
            1 => fn write(text: Vec<u8>) -> u32;
            /// The next line that arrives, without its newline; answered when one has.
            2 => fn read_line() -> Vec<u8>;
        }
        events {}
        errors {
            /// A write of nothing.
            1 => Empty;
        }
    }
}

fn wire<E: core::fmt::Debug>(e: E) -> Fail {
    format!("{e:?}")
}

fn write_bytes(p: &mut Probe, text: &[u8]) -> Result<u32, Fail> {
    let mut e = Encoder::new();
    text.to_vec().encode(&mut e).map_err(wire)?;
    let mut reply = p.request(1, e)?.reply()?;
    let mut d = reply.decoder();
    let n = u32::decode(&mut d).map_err(wire)?;
    d.finish().map_err(wire)?;
    Ok(n)
}

fn write_line(p: &mut Probe) -> Result<(), Fail> {
    let text = b"KB3 console conformance: a line through the driver\n";
    match write_bytes(p, text)? {
        n if n as usize == text.len() => Ok(()),
        n => Err(format!("wrote {n} of {} bytes", text.len())),
    }
}

fn write_pages(p: &mut Probe) -> Result<(), Fail> {
    // Longer than a page, so it goes to the device as a chain.
    let mut text = vec![b'.'; 9000];
    text.push(b'\n');
    match write_bytes(p, &text)? {
        n if n as usize == text.len() => Ok(()),
        n => Err(format!("wrote {n} of {} bytes", text.len())),
    }
}

fn write_empty(p: &mut Probe) -> Result<(), Fail> {
    let mut e = Encoder::new();
    Vec::<u8>::new().encode(&mut e).map_err(wire)?;
    p.request(1, e)?.failure(console::Error::Empty.code())
}

fn read_waits(p: &mut Probe) -> Result<(), Fail> {
    p.send_raw(Kind::Request, 2, 1000, Encoder::new())
        .map_err(wire)?;
    match wait_one(&p.channel().0, Signals::READABLE, deadline_in(200_000_000)) {
        Err(Error::TimedOut) => Ok(()),
        other => Err(format!(
            "read_line was answered with no line to read: {other:?}"
        )),
    }
}

const CASES: &[Case] = &[
    Case {
        name: "write: a line is taken whole",
        check: write_line,
    },
    Case {
        name: "write: a text longer than a page is taken whole",
        check: write_pages,
    },
    Case {
        name: "write: nothing is refused with Empty",
        check: write_empty,
    },
    Case {
        name: "read_line: not answered while no line has arrived",
        check: read_waits,
    },
];

/// The protocol's conformance suite, for a console no input arrives on while it runs.
pub fn suite() -> Suite {
    Suite {
        id: console::ID,
        requests: console::REQUESTS,
        cases: CASES,
        timeout: 10_000_000_000,
    }
}

const _: () = assert!(LINE_MAX <= moruna_svc::limits::CHANNEL_MAX_BYTES - 16);
