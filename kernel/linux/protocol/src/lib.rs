//! `moruna.linux` 1.0 (SDD 28, d.3): runs an unmodified Linux program as a process on
//! the Moruna microkernel, from the image the personality server's image filesystem
//! serves, and answers when it has ended: how, with what code, what it wrote to its
//! standard output and error, and which calls it was refused.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;

use moruna_svc::{Decoder, Encoder, Wire, WireError};

/// The most bytes of a program's output an answer carries; the rest is counted.
pub const OUTPUT_MAX: usize = 16 * 1024;
/// The most bytes of path, arguments and environment a request carries.
pub const REQUEST_MAX: usize = 48 * 1024;

/// Bytes, carried as one run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bytes(pub Vec<u8>);

impl Wire for Bytes {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        e.put_len(self.0.len())?;
        e.put_raw(&self.0);
        Ok(())
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        let n = d.take_len()?;
        Ok(Bytes(d.take_raw(n)?.to_vec()))
    }
}

impl From<&[u8]> for Bytes {
    fn from(b: &[u8]) -> Bytes {
        Bytes(b.to_vec())
    }
}

/// How a program ended.
pub mod how {
    /// It called `exit_group`, or its last thread `exit`: `code` is its code.
    pub const EXITED: u8 = 1;
    /// A signal it did not handle ended it: `code` is the signal.
    pub const SIGNALED: u8 = 2;
    /// It faulted, or was killed: the kernel's console line says which.
    pub const FAULTED: u8 = 3;
}

/// What a program did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ended {
    pub how: u8,
    pub code: i64,
    /// Its standard output and error, interleaved as written, up to `OUTPUT_MAX`.
    pub output: Bytes,
    /// Output bytes past `OUTPUT_MAX`.
    pub dropped: u64,
    /// Every call refused with `ENOSYS`: number and count.
    pub refused: Vec<(u64, u64)>,
    /// The calls the server answered.
    pub served: u64,
}

impl Wire for Ended {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        self.how.encode(e)?;
        self.code.encode(e)?;
        self.output.encode(e)?;
        self.dropped.encode(e)?;
        e.put_len(self.refused.len())?;
        for (n, c) in self.refused {
            n.encode(e)?;
            c.encode(e)?;
        }
        self.served.encode(e)
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        let how = u8::decode(d)?;
        let code = i64::decode(d)?;
        let output = Bytes::decode(d)?;
        let dropped = u64::decode(d)?;
        let n = d.take_len()?;
        let mut refused = Vec::new();
        for _ in 0..n {
            refused.push((u64::decode(d)?, u64::decode(d)?));
        }
        Ok(Ended {
            how,
            code,
            output,
            dropped,
            refused,
            served: u64::decode(d)?,
        })
    }
}

moruna_svc::protocol! {
    /// Linux programs as processes.
    pub protocol linux {
        name = "moruna.linux";
        major = 1;
        minor = 0;
        requests {
            /// Runs the program at `path` (absolute, in the image) with `argv` and
            /// `env`, in its own process and scratch directory; answers when it ends.
            1 => fn run(path: Bytes, argv: Vec<Bytes>, env: Vec<Bytes>) -> Ended;
        }
        events {}
        errors {
            /// No file at the path.
            1 => NotFound;
            /// Not a program this personality loads: the reason is on the console.
            2 => NotExecutable;
            /// The kernel refused to make or start the process.
            3 => NoProcess;
            /// The path, arguments and environment pass `REQUEST_MAX`, or hold NUL.
            4 => BadRequest;
            /// The server runs as many programs as it can at once.
            5 => Busy;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lx_t9_ended_round_trips() {
        let ended = Ended {
            how: how::EXITED,
            code: -3,
            output: Bytes(b"hello\n".to_vec()),
            dropped: 2,
            refused: alloc::vec![(41, 1), (334, 2)],
            served: 77,
        };
        let mut e = Encoder::new();
        ended.clone().encode(&mut e).unwrap();
        let (mine, theirs) = moruna_svc::Channel::create().unwrap();
        moruna_svc::frame::send(&mine, moruna_svc::Kind::Event, 1, 0, e).unwrap();
        let mut f = moruna_svc::frame::recv(&theirs).unwrap().unwrap();
        let mut d = f.decoder();
        assert_eq!(Ended::decode(&mut d).unwrap(), ended);
        assert_eq!(d.finish(), Ok(()));
        assert_eq!(Bytes::from(&b"x"[..]), Bytes(b"x".to_vec()));
    }
}
