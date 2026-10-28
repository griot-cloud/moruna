//! The echo example's protocol, `moruna.example.echo` 1.1, and its conformance suite:
//! everything a server of it must do, written once, which every implementation runs
//! (SDD 23, f.10).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{format, string::ToString, vec, vec::Vec};

use moruna_svc::{
    Channel, Encoder, ProtocolError, Status, Wire,
    conformance::{BUDGET, Case, Fail, Probe, Suite},
};

moruna_svc::protocol! {
    /// Bytes back, as they came or reversed, and bytes written through a channel the
    /// client sends.
    pub protocol echo {
        name = "moruna.example.echo";
        major = 1;
        minor = 1;
        requests {
            /// The bytes, back; `Empty` for none. Emits `echoed` first.
            1 => fn echo(text: Vec<u8>) -> Vec<u8>;
            /// The bytes, reversed; `Empty` for none.
            2 => fn reverse(text: Vec<u8>) -> Vec<u8>;
            /// Writes the bytes to `to`, an endpoint the client sends; their length.
            /// `PeerGone` if no one holds the other end.
            3 since 1 => fn send_to(to: Channel, text: Vec<u8>) -> u32;
        }
        events {
            /// The bytes echoed on this connection so far.
            1 => echoed { total: u64 };
        }
        errors {
            /// The text is empty.
            1 => Empty;
            /// The channel sent with `send_to` has no one at its other end.
            2 => PeerGone;
        }
    }
}

fn wire<E: core::fmt::Debug>(e: E) -> Fail {
    format!("{e:?}")
}

fn text(bytes: &[u8]) -> Result<Encoder, Fail> {
    let mut e = Encoder::new();
    bytes.to_vec().encode(&mut e).map_err(wire)?;
    Ok(e)
}

fn reply_bytes(probe: &mut Probe, ordinal: u16, body: Encoder) -> Result<Vec<u8>, Fail> {
    let mut reply = probe.request(ordinal, body)?.reply()?;
    let mut d = reply.decoder();
    let bytes = Vec::<u8>::decode(&mut d).map_err(wire)?;
    d.finish().map_err(wire)?;
    Ok(bytes)
}

fn echo_back(p: &mut Probe) -> Result<(), Fail> {
    if reply_bytes(p, 1, text(b"hello")?)? != b"hello" {
        return Err("echo did not give the bytes back".to_string());
    }
    let mut event = p.next_event()?;
    let ordinal = event.ordinal;
    let mut d = event.decoder();
    match echo::Event::decode(ordinal, &mut d) {
        Ok(echo::Event::echoed { total: 5 }) => Ok(()),
        other => Err(format!("expected echoed {{ total: 5 }}, got {other:?}")),
    }
}

fn echo_empty(p: &mut Probe) -> Result<(), Fail> {
    p.request(1, text(b"")?)?.failure(echo::Error::Empty.code())
}

fn echo_at_bound(p: &mut Probe) -> Result<(), Fail> {
    let full = vec![0x5au8; BUDGET - 4];
    if reply_bytes(p, 1, text(&full)?)? == full {
        Ok(())
    } else {
        Err("a text at the bound came back changed".to_string())
    }
}

fn reverse(p: &mut Probe) -> Result<(), Fail> {
    match reply_bytes(p, 2, text(b"abc")?)?.as_slice() {
        b"cba" => Ok(()),
        other => Err(format!("reverse gave {other:?}")),
    }
}

fn reverse_empty(p: &mut Probe) -> Result<(), Fail> {
    p.request(2, text(b"")?)?.failure(echo::Error::Empty.code())
}

fn send_to(p: &mut Probe) -> Result<(), Fail> {
    let (kept, sent) = Channel::create().map_err(wire)?;
    let mut body = Encoder::new();
    sent.encode(&mut body).map_err(wire)?;
    b"through".to_vec().encode(&mut body).map_err(wire)?;
    let mut reply = p.request(3, body)?.reply()?;
    let mut d = reply.decoder();
    match (u32::decode(&mut d), d.finish()) {
        (Ok(7), Ok(())) => {}
        other => return Err(format!("send_to answered {other:?}")),
    }
    let mut buf = [0u8; 16];
    match kept.read(&mut buf, &mut []) {
        Ok((7, 0)) if &buf[..7] == b"through" => Ok(()),
        other => Err(format!("the channel carried {other:?}")),
    }
}

fn send_to_gone(p: &mut Probe) -> Result<(), Fail> {
    let (kept, sent) = Channel::create().map_err(wire)?;
    drop(kept);
    let mut body = Encoder::new();
    sent.encode(&mut body).map_err(wire)?;
    b"x".to_vec().encode(&mut body).map_err(wire)?;
    p.request(3, body)?.failure(echo::Error::PeerGone.code())
}

fn send_to_not_a_channel(p: &mut Probe) -> Result<(), Fail> {
    let mut body = Encoder::new();
    let not_a_channel = moruna_svc::Memory::create(4096).map_err(wire)?;
    body.put_handle(not_a_channel.0).map_err(wire)?;
    b"x".to_vec().encode(&mut body).map_err(wire)?;
    p.request(3, body)?.status(Status::BadMessage)
}

const CASES: &[Case] = &[
    Case {
        name: "echo: the bytes come back, after an echoed event",
        check: echo_back,
    },
    Case {
        name: "echo: an empty text is refused with Empty",
        check: echo_empty,
    },
    Case {
        name: "echo: a text that fills the frame comes back whole",
        check: echo_at_bound,
    },
    Case {
        name: "reverse: the bytes come back reversed",
        check: reverse,
    },
    Case {
        name: "reverse: an empty text is refused with Empty",
        check: reverse_empty,
    },
    Case {
        name: "send_to: the bytes arrive on the channel sent with the request",
        check: send_to,
    },
    Case {
        name: "send_to: a channel no one holds is refused with PeerGone",
        check: send_to_gone,
    },
    Case {
        name: "send_to: a handle that is not a channel is a bad message",
        check: send_to_not_a_channel,
    },
];

/// The protocol's conformance suite.
pub fn suite() -> Suite {
    Suite {
        id: echo::ID,
        requests: echo::REQUESTS,
        cases: CASES,
        timeout: 5_000_000_000,
    }
}
