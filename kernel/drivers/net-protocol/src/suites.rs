//! The protocol's conformance suites (SDD 29, k): one card alone ([`suite`]: its
//! information, its link, frames sent, every refusal, receiving and its refusals, the
//! counters), and two cards on one link ([`pair_suite`]: frames of every size out of
//! one arrive at the other intact, a batch arrives in order, and both ways).
//!
//! The frames a suite sends go from the card to its own address with EtherType
//! 0x88b5, which IEEE 802 sets aside for local experiments, so a suite run on a live
//! network addresses no other station.

use alloc::{format, string::String, vec, vec::Vec};

use moruna_svc::{
    Encoder, Error, Kind, ProtocolError, Signals, Wire,
    conformance::{Case, Fail, PairCase, PairSuite, Probe, Suite},
    deadline_in, now, wait_one,
};

use crate::{
    FRAME_MAX, FRAME_MIN, Frame, HEADER, Info, Link, SEND_FRAMES_MAX, Stats, is_group, net, octets,
    offload,
};

/// The EtherType of every frame a suite sends.
pub const ETHERTYPE: u16 = 0x88b5;
/// What a suite's frame's payload starts with, then a sequence number.
const MARK: &[u8; 8] = b"KE1-NET:";

fn wire<E: core::fmt::Debug>(e: E) -> Fail {
    format!("{e:?}")
}

fn decoded<T: Wire>(p: &mut Probe, ordinal: u16, body: Encoder) -> Result<T, Fail> {
    let mut reply = p.request(ordinal, body)?.reply()?;
    let mut d = reply.decoder();
    let value = T::decode(&mut d).map_err(wire)?;
    d.finish().map_err(wire)?;
    Ok(value)
}

fn info_of(p: &mut Probe) -> Result<Info, Fail> {
    decoded(p, 1, Encoder::new())
}

/// A frame of `len` bytes from `src` to `dst`, its payload the mark, `seq` and a
/// pattern.
pub fn frame(dst: u64, src: u64, seq: u32, len: usize) -> Frame {
    let mut f = Vec::with_capacity(len);
    f.extend_from_slice(&octets(dst));
    f.extend_from_slice(&octets(src));
    f.extend_from_slice(&ETHERTYPE.to_be_bytes());
    f.extend_from_slice(MARK);
    f.extend_from_slice(&seq.to_be_bytes());
    while f.len() < len {
        f.push((f.len() as u32 ^ seq) as u8);
    }
    f.truncate(len);
    Frame(f)
}

/// The sequence number of a suite's frame, if `f` is one.
fn seq_of(f: &[u8]) -> Option<u32> {
    let ours = f.get(12..14) == Some(&ETHERTYPE.to_be_bytes()[..])
        && f.get(HEADER..HEADER + MARK.len()) == Some(&MARK[..]);
    let s = f.get(HEADER + MARK.len()..HEADER + MARK.len() + 4)?;
    ours.then(|| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

fn send_body(frames: Vec<Frame>) -> Result<Encoder, Fail> {
    let mut e = Encoder::new();
    frames.encode(&mut e).map_err(wire)?;
    Ok(e)
}

fn send(p: &mut Probe, frames: Vec<Frame>) -> Result<u32, Fail> {
    decoded(p, 4, send_body(frames)?)
}

fn u32_body(v: u32) -> Result<Encoder, Fail> {
    let mut e = Encoder::new();
    v.encode(&mut e).map_err(wire)?;
    Ok(e)
}

/// Not answered within 200 ms: the answer waits for something that does not come.
fn unanswered(p: &mut Probe, ordinal: u16, txid: u32, body: Encoder) -> Result<(), Fail> {
    p.send_raw(Kind::Request, ordinal, txid, body)
        .map_err(wire)?;
    match wait_one(&p.channel().0, Signals::READABLE, deadline_in(200_000_000)) {
        Err(Error::TimedOut) => Ok(()),
        other => Err(format!("request {ordinal} was answered at once: {other:?}")),
    }
}

fn info(p: &mut Probe) -> Result<(), Fail> {
    let i = info_of(p)?;
    if i.mac == 0 || i.mac >> 48 != 0 || is_group(i.mac) {
        return Err(format!("the address {:#014x} is not a station's", i.mac));
    }
    let largest = (FRAME_MAX - HEADER) as u32;
    if !(68..=largest).contains(&i.mtu) || i.max_frame != i.mtu + HEADER as u32 {
        return Err(format!(
            "an MTU of {} with frames of {}",
            i.mtu, i.max_frame
        ));
    }
    if i.offloads & !offload::KNOWN != 0 {
        return Err(format!(
            "offloads {:#x} this version does not define",
            i.offloads
        ));
    }
    if i.queues == 0 {
        return Err(String::from("no queues"));
    }
    Ok(())
}

fn link(p: &mut Probe) -> Result<(), Fail> {
    let now_: Link = decoded(p, 2, Encoder::new())?;
    let stale: Link = decoded(p, 3, u32_body(now_.generation.wrapping_add(7))?)?;
    if stale.generation != now_.generation {
        return Err(format!(
            "link_changed from another generation answered {stale:?}, link {now_:?}"
        ));
    }
    unanswered(p, 3, 1000, u32_body(now_.generation)?)
}

fn sends(p: &mut Probe) -> Result<(), Fail> {
    let i = info_of(p)?;
    for len in [FRAME_MIN, 60, i.max_frame as usize] {
        match send(p, vec![frame(i.mac, i.mac, 0, len)])? {
            1 => {}
            n => return Err(format!("a send of one frame of {len} bytes answered {n}")),
        }
    }
    let batch = (0..SEND_FRAMES_MAX as u32)
        .map(|s| frame(i.mac, i.mac, s, 64))
        .collect();
    match send(p, batch)? {
        n if n as usize == SEND_FRAMES_MAX => Ok(()),
        n => Err(format!("a send of {SEND_FRAMES_MAX} frames answered {n}")),
    }
}

fn refusals(p: &mut Probe) -> Result<(), Fail> {
    let i = info_of(p)?;
    let refused = |p: &mut Probe, frames: Vec<Frame>, e: net::Error| -> Result<(), Fail> {
        p.request(4, send_body(frames)?)?.failure(e.code())
    };
    refused(
        p,
        vec![frame(i.mac, i.mac, 0, FRAME_MIN - 1)],
        net::Error::TooShort,
    )?;
    refused(p, vec![Frame(Vec::new())], net::Error::TooShort)?;
    refused(
        p,
        vec![frame(i.mac, i.mac, 0, i.max_frame as usize + 1)],
        net::Error::TooLong,
    )?;
    refused(
        p,
        vec![frame(i.mac, 0xffff_ffff_ffff, 0, 60)],
        net::Error::BadSource,
    )?;
    refused(
        p,
        vec![frame(i.mac, i.mac | 1 << 40, 0, 60)],
        net::Error::BadSource,
    )?;
    refused(p, Vec::new(), net::Error::Empty)?;
    let many = (0..=SEND_FRAMES_MAX as u32)
        .map(|s| frame(i.mac, i.mac, s, 60))
        .collect();
    refused(p, many, net::Error::TooMany)?;
    // One bad frame refuses the batch it is in.
    refused(
        p,
        vec![frame(i.mac, i.mac, 0, 60), frame(i.mac, i.mac, 1, 3)],
        net::Error::TooShort,
    )
}

fn receiving(p: &mut Probe) -> Result<(), Fail> {
    p.request(6, u32_body(8)?)?
        .failure(net::Error::NotSubscribed.code())?;
    decoded::<()>(p, 5, Encoder::new())?;
    decoded::<()>(p, 5, Encoder::new())?;
    unanswered(p, 6, 1000, u32_body(8)?)?;
    p.request(6, u32_body(8)?)?
        .failure(net::Error::InUse.code())
}

fn counters(p: &mut Probe) -> Result<(), Fail> {
    let i = info_of(p)?;
    let before: Stats = decoded(p, 7, Encoder::new())?;
    send(p, vec![frame(i.mac, i.mac, 0, 100)])?;
    let after: Stats = decoded(p, 7, Encoder::new())?;
    if after.tx_frames < before.tx_frames + 1 || after.tx_bytes < before.tx_bytes + 100 {
        return Err(format!(
            "a frame sent left the counters at {after:?}, from {before:?}"
        ));
    }
    if after.rx_dropped != 0 {
        return Err(format!(
            "a connection that never subscribed dropped {}",
            after.rx_dropped
        ));
    }
    Ok(())
}

const CASES: &[Case] = &[
    Case {
        name: "info: a station's address, an MTU and its frame size, known offloads, queues",
        check: info,
    },
    Case {
        name: "link: answered; link_changed answers at once for another generation and waits for this one",
        check: link,
    },
    Case {
        name: "send: frames of the least, an ordinary and the largest size, and a full batch",
        check: sends,
    },
    Case {
        name: "send: short, long, group-source, empty and oversized batches are refused whole",
        check: refusals,
    },
    Case {
        name: "receive: refused before subscribe; waits for a frame; a second waiting receive is InUse",
        check: receiving,
    },
    Case {
        name: "stats: a frame sent is counted",
        check: counters,
    },
];

/// The suite for one card, on a link where no frame arrives for it while the suite
/// runs.
pub fn suite() -> Suite {
    Suite {
        id: net::ID,
        requests: net::REQUESTS,
        cases: CASES,
        timeout: 30_000_000_000,
    }
}

/// Receives on `p` until frames with sequence numbers `0 .. n` of the suite's have
/// arrived, or the suite's time is up; frames that are not the suite's are skipped.
fn collect(p: &mut Probe, n: usize) -> Result<Vec<Vec<u8>>, Fail> {
    let deadline = now().saturating_add(20_000_000_000);
    let mut got: Vec<Vec<u8>> = Vec::new();
    while got.len() < n {
        if now() > deadline {
            return Err(format!("{} of {n} frames arrived", got.len()));
        }
        let frames: Vec<Frame> = decoded(p, 6, u32_body(64)?)?;
        got.extend(
            frames
                .into_iter()
                .map(|f| f.0)
                .filter(|f| seq_of(f).is_some()),
        );
    }
    Ok(got)
}

/// A frame as sent, as a receiver may see it: the same bytes, a frame shorter than 60
/// bytes perhaps padded to 60 with zeros.
fn same(sent: &[u8], got: &[u8]) -> bool {
    got.len() >= sent.len()
        && got[..sent.len()] == *sent
        && (got.len() == sent.len() || (sent.len() < 60 && got.len() == 60))
        && got[sent.len()..].iter().all(|b| *b == 0)
}

fn one_way(a: &mut Probe, b: &mut Probe, lens: &[usize]) -> Result<(), Fail> {
    let (ia, ib) = (info_of(a)?, info_of(b)?);
    decoded::<()>(b, 5, Encoder::new())?;
    let max = ia.max_frame.min(ib.max_frame) as usize;
    let lens: Vec<usize> = lens.iter().map(|l| (*l).min(max)).collect();
    let frames: Vec<Frame> = lens
        .iter()
        .enumerate()
        .map(|(s, len)| frame(ib.mac, ia.mac, s as u32, (*len).max(HEADER + 12)))
        .collect();
    let sent: Vec<Vec<u8>> = frames.iter().map(|f| f.0.clone()).collect();
    for f in frames {
        send(a, vec![f])?;
    }
    let got = collect(b, sent.len())?;
    for (s, (want, have)) in sent.iter().zip(&got).enumerate() {
        if seq_of(have) != Some(s as u32) {
            return Err(format!("frame {s} arrived as {:?}", seq_of(have)));
        }
        if !same(want, have) {
            return Err(format!(
                "frame {s} of {} bytes arrived as {} bytes, not intact",
                want.len(),
                have.len()
            ));
        }
    }
    Ok(())
}

fn sizes(a: &mut Probe, b: &mut Probe) -> Result<(), Fail> {
    one_way(a, b, &[26, 60, 61, 1000, 1514, usize::MAX])
}

fn reverse(a: &mut Probe, b: &mut Probe) -> Result<(), Fail> {
    one_way(b, a, &[60, 1514, usize::MAX])
}

fn in_order(a: &mut Probe, b: &mut Probe) -> Result<(), Fail> {
    let (ia, ib) = (info_of(a)?, info_of(b)?);
    decoded::<()>(b, 5, Encoder::new())?;
    let batch: Vec<Frame> = (0..SEND_FRAMES_MAX as u32)
        .map(|s| frame(ib.mac, ia.mac, s, 64 + (s as usize % 7) * 100))
        .collect();
    send(a, batch)?;
    let got = collect(b, SEND_FRAMES_MAX)?;
    let order: Vec<Option<u32>> = got.iter().map(|f| seq_of(f)).collect();
    let want: Vec<Option<u32>> = (0..SEND_FRAMES_MAX as u32).map(Some).collect();
    if order != want {
        return Err(format!("the batch arrived in the order {order:?}"));
    }
    let after: Stats = decoded(b, 7, Encoder::new())?;
    if after.rx_dropped != 0 {
        return Err(format!("the receiver dropped {} frames", after.rx_dropped));
    }
    Ok(())
}

const PAIR_CASES: &[PairCase] = &[
    PairCase {
        name: "frames of every size from the least to the largest out of one card arrive at the other intact",
        check: sizes,
    },
    PairCase {
        name: "a batch of frames arrives in the order it was sent, none dropped",
        check: in_order,
    },
    PairCase {
        name: "frames arrive the other way too",
        check: reverse,
    },
];

/// The suite for two cards on one link, each frame out of one arriving at the other
/// and nothing else arriving while it runs.
pub fn pair_suite() -> PairSuite {
    PairSuite {
        id: net::ID,
        cases: PAIR_CASES,
        timeout: 30_000_000_000,
    }
}
