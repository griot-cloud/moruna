//! `moruna.driver.vsock` 1.0 (SDD 25, d.5): stream sockets between this machine and
//! its host, over virtio-vsock. A connection to the driver owns the streams and
//! listeners it opens; they close with it.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{format, vec::Vec};

use moruna_svc::{
    Encoder, ProtocolError, Wire,
    conformance::{Case, Fail, Probe, Suite},
};

/// The host's address (virtio 1.2, 5.10.4).
pub const HOST_CID: u64 = 2;
/// The most bytes one `recv` answers with, and one `send` takes.
pub const CHUNK_MAX: u32 = 60_000;

moruna_svc::protocol! {
    /// Stream sockets to the host.
    pub protocol vsock {
        name = "moruna.driver.vsock";
        major = 1;
        minor = 0;
        requests {
            /// This machine's context id.
            1 => fn local_cid() -> u64;
            /// Connects to `port` at `cid`; answered with the stream once the peer
            /// accepts, or `Refused`.
            2 => fn connect_to(cid: u64, port: u32) -> u32;
            /// Accepts streams to `port`; `InUse` if another connection listens there.
            3 => fn listen(port: u32) -> ();
            /// The next stream accepted on `port`, with its peer's cid and port;
            /// answered when one arrives.
            4 => fn accept(port: u32) -> (u32, u64, u32);
            /// Sends `data`; answered with its length once it is all queued within the
            /// peer's credit.
            5 => fn send(stream: u32, data: Vec<u8>) -> u32;
            /// Up to `max` received bytes; answered when there are some. Nothing means
            /// the peer will send no more.
            6 => fn recv(stream: u32, max: u32) -> Vec<u8>;
            /// Closes the stream.
            7 => fn close(stream: u32) -> ();
        }
        events {}
        errors {
            /// The peer refused the connection: nothing listens on that port.
            1 => Refused;
            /// The peer reset the stream.
            2 => Reset;
            /// No stream of this connection has this id.
            3 => NoStream;
            /// The port is listened on by another connection, or a `recv` is already
            /// waiting on the stream.
            4 => InUse;
            /// This connection does not listen on the port.
            5 => NotListening;
            /// A send of nothing, or a `recv` of at most zero bytes.
            6 => Empty;
        }
    }
}

fn wire<E: core::fmt::Debug>(e: E) -> Fail {
    format!("{e:?}")
}

fn body(
    values: impl FnOnce(&mut Encoder) -> Result<(), moruna_svc::WireError>,
) -> Result<Encoder, Fail> {
    let mut e = Encoder::new();
    values(&mut e).map_err(wire)?;
    Ok(e)
}

fn local_cid(p: &mut Probe) -> Result<(), Fail> {
    let mut reply = p.request(1, Encoder::new())?.reply()?;
    let mut d = reply.decoder();
    let cid = u64::decode(&mut d).map_err(wire)?;
    d.finish().map_err(wire)?;
    if cid <= HOST_CID || cid >= u32::MAX as u64 {
        return Err(format!("a guest cid of {cid}"));
    }
    Ok(())
}

fn unknown_streams(p: &mut Probe) -> Result<(), Fail> {
    let none = vsock::Error::NoStream.code();
    p.request(
        5,
        body(|e| {
            999u32.encode(e)?;
            b"x".to_vec().encode(e)
        })?,
    )?
    .failure(none)?;
    p.request(
        6,
        body(|e| {
            999u32.encode(e)?;
            10u32.encode(e)
        })?,
    )?
    .failure(none)?;
    p.request(7, body(|e| 999u32.encode(e))?)?.failure(none)
}

fn listen_twice(p: &mut Probe) -> Result<(), Fail> {
    p.request(3, body(|e| 40_001u32.encode(e))?)?.reply()?;
    p.request(3, body(|e| 40_001u32.encode(e))?)?
        .failure(vsock::Error::InUse.code())
}

fn accept_unlistened(p: &mut Probe) -> Result<(), Fail> {
    p.request(4, body(|e| 40_002u32.encode(e))?)?
        .failure(vsock::Error::NotListening.code())
}

const CASES: &[Case] = &[
    Case {
        name: "local_cid: a guest's context id",
        check: local_cid,
    },
    Case {
        name: "send, recv and close: a stream this connection does not have is NoStream",
        check: unknown_streams,
    },
    Case {
        name: "listen: a port already listened on is InUse",
        check: listen_twice,
    },
    Case {
        name: "accept: a port not listened on is NotListening",
        check: accept_unlistened,
    },
];

/// The protocol's conformance suite: what holds whatever the host does.
pub fn suite() -> Suite {
    Suite {
        id: vsock::ID,
        requests: vsock::REQUESTS,
        cases: CASES,
        timeout: 10_000_000_000,
    }
}
