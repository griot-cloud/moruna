//! `moruna.test.edu` 1.0: the protocol of a driver for QEMU's `edu` test device, a PCI
//! device that inverts what is written to its liveness register and computes
//! factorials. It exists to show a third party's driver added to the table with no
//! kernel change (SDD 25, DR-T17).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::format;

use moruna_svc::{
    Encoder, Wire,
    conformance::{Case, Fail, Probe, Suite},
};

moruna_svc::protocol! {
    /// QEMU's edu device.
    pub protocol edu {
        name = "moruna.test.edu";
        major = 1;
        minor = 0;
        requests {
            /// The device's identification register: `0x010000ed` for edu 1.0.
            1 => fn identify() -> u32;
            /// Writes `value` to the liveness register and reads back its inverse.
            2 => fn liveness(value: u32) -> u32;
            /// The device's factorial of `n`, modulo 2^32.
            3 => fn factorial(n: u32) -> u32;
        }
        events {}
        errors {
            /// The device did not finish the computation in time.
            1 => TimedOut;
        }
    }
}

fn wire<E: core::fmt::Debug>(e: E) -> Fail {
    format!("{e:?}")
}

fn call(p: &mut Probe, ordinal: u16, arg: Option<u32>) -> Result<u32, Fail> {
    let mut e = Encoder::new();
    if let Some(a) = arg {
        a.encode(&mut e).map_err(wire)?;
    }
    let mut reply = p.request(ordinal, e)?.reply()?;
    let mut d = reply.decoder();
    let v = u32::decode(&mut d).map_err(wire)?;
    d.finish().map_err(wire)?;
    Ok(v)
}

fn identify(p: &mut Probe) -> Result<(), Fail> {
    match call(p, 1, None)? {
        id if id & 0xff == 0xed => Ok(()),
        id => Err(format!("identification {id:#x}")),
    }
}

fn liveness(p: &mut Probe) -> Result<(), Fail> {
    for v in [0u32, 0x1234_5678, u32::MAX] {
        if call(p, 2, Some(v))? != !v {
            return Err(format!("liveness of {v:#x} is not its inverse"));
        }
    }
    Ok(())
}

fn factorial(p: &mut Probe) -> Result<(), Fail> {
    for (n, f) in [(0u32, 1u32), (1, 1), (5, 120), (12, 479_001_600)] {
        let got = call(p, 3, Some(n))?;
        if got != f {
            return Err(format!("{n}! is {got}, not {f}"));
        }
    }
    Ok(())
}

const CASES: &[Case] = &[
    Case {
        name: "identify: edu's identification register",
        check: identify,
    },
    Case {
        name: "liveness: the inverse of what was written",
        check: liveness,
    },
    Case {
        name: "factorial: 0!, 1!, 5! and 12!",
        check: factorial,
    },
];

/// The protocol's conformance suite.
pub fn suite() -> Suite {
    Suite {
        id: edu::ID,
        requests: edu::REQUESTS,
        cases: CASES,
        timeout: 10_000_000_000,
    }
}
