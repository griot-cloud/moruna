//! The protocol's conformance suites (SDD 31, k).
//!
//! [`suite`] checks one server through one connection whose policy reaches its
//! loopback (127.0.0.0/8 and ::1, every right but resolving): TCP connections
//! accepted, bytes intact at sizes from one byte to a mebibyte both ways, half-close,
//! backpressure, refusals of closed ports, unknown sockets, wrong kinds, bad
//! addresses and taken endpoints; UDP datagrams; echo; and rights: a narrowed grant
//! reaches only what it names, a wider grant is refused, a policy's socket bound
//! holds, and a connection that goes takes its sockets with it.
//!
//! [`pair_suite`] checks two connections to one server, each whose policy reaches
//! one of two cards on one link (`a` and `b`), so that everything a check sends
//! leaves by one card and arrives at the other: TCP both ways on IPv4 and IPv6
//! (ARP and NDP), half-close and a refused port over the wire, UDP, echo, and a name
//! that resolves through a name server on `b` (the server's resolver must point at
//! `b`'s address) and is then connected to by name.

use alloc::{format, string::String, vec, vec::Vec};

use moruna_svc::{
    Encoder, Error, Kind, ProtocolError, Signals, WaitItem, Wire, WireError,
    conformance::{Answer, Case, Fail, PairCase, PairSuite, Probe as RawProbe, Suite},
    deadline_in, frame, now, wait, wait_one,
};

use crate::{
    Addr, Cidr, Endpoint, Info, NameRule, Opened, PING_MAX, Policy, RECEIVE_MAX, Rule, SEND_MAX,
    SocketState, SocketStatus, Target, dns, net, rights,
};

/// Each conformance case owns an explicit host/native test Account. This never
/// substitutes for native quota/charged-origin acceptance.
struct Probe<'a> {
    inner: &'a mut RawProbe,
    account: moruna_svc::Account,
}
impl core::ops::Deref for Probe<'_> {
    type Target = RawProbe;
    fn deref(&self) -> &RawProbe {
        self.inner
    }
}
impl core::ops::DerefMut for Probe<'_> {
    fn deref_mut(&mut self) -> &mut RawProbe {
        self.inner
    }
}
fn bind_account(raw: &mut RawProbe) -> Result<Probe<'_>, Fail> {
    let account = moruna_svc::Account::carve(8 << 20, "net-suite").map_err(wire)?;
    let loan = account
        .0
        .duplicate(
            moruna_svc::Rights::READ | moruna_svc::Rights::WRITE | moruna_svc::Rights::TRANSFER,
        )
        .map_err(wire)?;
    must(
        answered::<()>(raw.request(17, body(|e| loan.encode(e))?)?)?,
        "bind actual account",
    )?;
    Ok(Probe {
        inner: raw,
        account,
    })
}
fn with_account(
    raw: &mut RawProbe,
    check: fn(&mut Probe<'_>) -> Result<(), Fail>,
) -> Result<(), Fail> {
    check(&mut bind_account(raw)?)
}
fn with_pair(
    a: &mut RawProbe,
    b: &mut RawProbe,
    check: for<'account> fn(&mut Probe<'account>, &mut Probe<'account>) -> Result<(), Fail>,
) -> Result<(), Fail> {
    check(&mut bind_account(a)?, &mut bind_account(b)?)
}

/// The name the pair suite resolves through `b`'s name server.
pub const SUITE_NAME: &str = "pair.suite.ke2.test";

const SECOND: u64 = 1_000_000_000;
/// The sizes the TCP checks send, each way.
const SIZES: [usize; 15] = [
    1,
    2,
    3,
    7,
    100,
    536,
    1460,
    1461,
    4096,
    16383,
    65535,
    65536,
    100_000,
    300_000,
    1 << 20,
];
/// Fewer sizes, for the IPv6 and second-family checks.
const FEW_SIZES: [usize; 5] = [1, 1461, 65536, 100_000, 300_000];
/// Where the raw requests' ids start, clear of the probe's own.
const RAW_TXID: u32 = 0x8000_0000;

type Answered<T> = Result<T, net::Error>;

fn wire<E: core::fmt::Debug>(e: E) -> Fail {
    format!("{e:?}")
}

fn body(parts: impl FnOnce(&mut Encoder) -> Result<(), WireError>) -> Result<Encoder, Fail> {
    let mut e = Encoder::new();
    parts(&mut e).map_err(wire)?;
    Ok(e)
}

fn answered<T: Wire>(a: Answer) -> Result<Answered<T>, Fail> {
    match a {
        Answer::Reply(mut f) => {
            let mut d = f.decoder();
            let v =
                T::decode(&mut d).map_err(|e| format!("a reply that does not decode: {e:?}"))?;
            d.finish()
                .map_err(|e| format!("a reply with more in it: {e:?}"))?;
            Ok(Ok(v))
        }
        Answer::Failure(code) => net::Error::from_code(code)
            .map(Err)
            .ok_or_else(|| format!("failure {code}, which the protocol does not declare")),
        Answer::Status(s) => Err(format!("status {s:?}")),
    }
}

fn call<T: Wire>(p: &mut Probe, ordinal: u16, body: Encoder) -> Result<Answered<T>, Fail> {
    answered(p.request(ordinal, body)?)
}

fn must<T>(r: Answered<T>, what: &str) -> Result<T, Fail> {
    r.map_err(|e| format!("{what}: refused {e:?}"))
}

fn refuses<T: core::fmt::Debug>(r: Answered<T>, want: net::Error, what: &str) -> Result<(), Fail> {
    match r {
        Err(e) if e == want => Ok(()),
        other => Err(format!("{what}: expected {want:?}, got {other:?}")),
    }
}

fn info(p: &mut Probe) -> Result<Info, Fail> {
    must(call(p, 1, Encoder::new())?, "info")
}

fn connect(p: &mut Probe, to: Target, from: Option<Addr>) -> Result<Answered<Opened>, Fail> {
    call(
        p,
        2,
        body(|e| {
            to.encode(e)?;
            from.encode(e)
        })?,
    )
}

fn listen(p: &mut Probe, at: Endpoint, backlog: u32) -> Result<Answered<(u32, Endpoint)>, Fail> {
    call(
        p,
        3,
        body(|e| {
            at.encode(e)?;
            backlog.encode(e)
        })?,
    )
}

fn accept(p: &mut Probe, listener: u32) -> Result<Answered<Opened>, Fail> {
    call(p, 4, body(|e| listener.encode(e))?)
}

fn send(p: &mut Probe, socket: u32, bytes: Vec<u8>) -> Result<Answered<u32>, Fail> {
    let encoded = send_body(p, socket, bytes)?;
    call(p, 5, encoded)
}

fn send_body(p: &Probe<'_>, socket: u32, bytes: Vec<u8>) -> Result<Encoder, Fail> {
    let memory =
        moruna_svc::Memory::create_in(bytes.len().max(1) as u64, &p.account.0).map_err(wire)?;
    let pages = (bytes.len().max(1) as u64).div_ceil(4096) * 4096;
    memory
        .map(0, pages, moruna_svc::map::WRITE)
        .and_then(|m| m.write(0, &bytes))
        .map_err(wire)?;
    let loan = moruna_svc::Memory(
        memory
            .0
            .duplicate(
                moruna_svc::Rights::READ | moruna_svc::Rights::MAP | moruna_svc::Rights::TRANSFER,
            )
            .map_err(wire)?,
    );
    body(|e| {
        socket.encode(e)?;
        loan.encode(e)?;
        0u64.encode(e)?;
        (bytes.len() as u32).encode(e)
    })
}

fn receive_body(
    p: &Probe<'_>,
    socket: u32,
    max: u32,
) -> Result<(Encoder, moruna_svc::Memory), Fail> {
    let size = (u64::from(max.max(1)).min(RECEIVE_MAX as u64) + 4095) / 4096 * 4096;
    let memory = moruna_svc::Memory::create_in(size, &p.account.0).map_err(wire)?;
    let loan = moruna_svc::Memory(
        memory
            .0
            .duplicate(
                moruna_svc::Rights::READ
                    | moruna_svc::Rights::WRITE
                    | moruna_svc::Rights::MAP
                    | moruna_svc::Rights::TRANSFER,
            )
            .map_err(wire)?,
    );
    let encoded = body(|e| {
        socket.encode(e)?;
        loan.encode(e)?;
        0u64.encode(e)?;
        max.encode(e)
    })?;
    Ok((encoded, memory))
}
fn received(memory: &moruna_svc::Memory, result: Answered<u32>) -> Result<Answered<Vec<u8>>, Fail> {
    match result {
        Err(e) => Ok(Err(e)),
        Ok(n) => {
            if n as usize > RECEIVE_MAX {
                return Err("oversized receive completion".into());
            }
            if n == 0 {
                return Ok(Ok(Vec::new()));
            }
            let size = (u64::from(n) + 4095) / 4096 * 4096;
            let mapping = memory.map(0, size, moruna_svc::map::READ).map_err(wire)?;
            let mut bytes = vec![0; n as usize];
            mapping.read(0, &mut bytes).map_err(wire)?;
            Ok(Ok(bytes))
        }
    }
}
fn receive(p: &mut Probe, socket: u32, max: u32) -> Result<Answered<Vec<u8>>, Fail> {
    let (body, memory) = receive_body(p, socket, max)?;
    received(&memory, call(p, 6, body)?)
}

fn shutdown(p: &mut Probe, socket: u32) -> Result<Answered<()>, Fail> {
    call(p, 7, body(|e| socket.encode(e))?)
}

fn close(p: &mut Probe, socket: u32) -> Result<Answered<()>, Fail> {
    call(p, 8, body(|e| socket.encode(e))?)
}

fn bind(p: &mut Probe, at: Endpoint) -> Result<Answered<(u32, Endpoint)>, Fail> {
    call(p, 9, body(|e| at.encode(e))?)
}

fn send_to(p: &mut Probe, socket: u32, to: Endpoint, bytes: Vec<u8>) -> Result<Answered<()>, Fail> {
    call(
        p,
        10,
        body(|e| {
            socket.encode(e)?;
            to.encode(e)?;
            bytes.encode(e)
        })?,
    )
}

fn receive_from(p: &mut Probe, socket: u32) -> Result<Answered<(Endpoint, Vec<u8>)>, Fail> {
    call(p, 11, body(|e| socket.encode(e))?)
}

fn resolve_body(name: &str) -> Result<Encoder, Fail> {
    body(|e| String::from(name).encode(e))
}

fn ping(p: &mut Probe, to: Addr, size: u32) -> Result<Answered<u64>, Fail> {
    call(
        p,
        13,
        body(|e| {
            to.encode(e)?;
            size.encode(e)
        })?,
    )
}

fn grant(p: &mut Probe, policy: Policy) -> Result<Answered<moruna_svc::Channel>, Fail> {
    call(p, 14, body(|e| policy.encode(e))?)
}

fn status(p: &mut Probe, socket: u32) -> Result<Answered<SocketStatus>, Fail> {
    call(p, 15, body(|e| socket.encode(e))?)
}

/// A request sent without waiting for its answer, under an id of `next`'s.
fn start(p: &mut Probe, next: &mut u32, ordinal: u16, body: Encoder) -> Result<u32, Fail> {
    let txid = *next;
    *next += 1;
    p.send_raw(Kind::Request, ordinal, txid, body)
        .map_err(|e| format!("sending request {ordinal}: {e:?}"))?;
    Ok(txid)
}

/// The next answer queued on `p`'s connection, if one is.
fn poll_answer(p: &Probe) -> Result<Option<(u32, Answer)>, Fail> {
    let Some(mut f) = frame::recv(p.channel()).map_err(|e| format!("reading the server: {e:?}"))?
    else {
        return Ok(None);
    };
    match f.kind {
        Kind::Reply => Ok(Some((f.txid, Answer::Reply(f)))),
        Kind::Failure => {
            let mut d = f.decoder();
            let code = u32::decode(&mut d).map_err(wire)?;
            d.finish().map_err(wire)?;
            let answer = match moruna_svc::Status::from_code(code) {
                Some(s) => Answer::Status(s),
                None => Answer::Failure(code),
            };
            Ok(Some((f.txid, answer)))
        }
        other => Err(format!("expected an answer, got {other:?}")),
    }
}

/// The answer to the raw request `txid`, waiting until `deadline`.
fn wait_answer(p: &Probe, txid: u32, deadline: u64) -> Result<Answer, Fail> {
    loop {
        if let Some((id, a)) = poll_answer(p)? {
            if id != txid {
                return Err(format!("expected the answer to {txid}, got one to {id}"));
            }
            return Ok(a);
        }
        match wait_one(
            &p.channel().0,
            Signals::READABLE | Signals::PEER_CLOSED,
            deadline,
        ) {
            Ok(_) => {}
            Err(Error::TimedOut) => return Err(format!("request {txid} was not answered in time")),
            Err(e) => return Err(format!("waiting for the server: {}", e.name())),
        }
    }
}

/// Whether `p`'s connection stays quiet for `ns`: a request waits as it should.
fn quiet(p: &Probe, ns: u64) -> Result<bool, Fail> {
    match wait_one(&p.channel().0, Signals::READABLE, deadline_in(ns)) {
        Err(Error::TimedOut) => Ok(true),
        Ok(_) => Ok(false),
        Err(e) => Err(format!("waiting for the server: {}", e.name())),
    }
}

/// `len` bytes of a pattern `seed` picks.
fn pattern(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

/// Sends `data` on socket `tx.1` of `ends[tx.0]` and receives it on socket `rx.1` of
/// `ends[rx.0]`, one send and one receive outstanding at a time, so neither side's
/// buffers bound what a check can send; the bytes must arrive whole and in order.
fn transfer(
    ends: &mut [&mut Probe],
    tx: (usize, u32),
    rx: (usize, u32),
    data: &[u8],
) -> Result<(), Fail> {
    let deadline = deadline_in(120 * SECOND);
    let mut next = RAW_TXID;
    let (mut sent, mut got) = (0usize, Vec::with_capacity(data.len()));
    let mut sending: Option<(u32, usize)> = None;
    let mut receiving: Option<(u32, moruna_svc::Memory)> = None;
    while got.len() < data.len() || sending.is_some() {
        if sending.is_none() && sent < data.len() {
            let n = (data.len() - sent).min(SEND_MAX);
            let encoded = send_body(ends[tx.0], tx.1, data[sent..sent + n].to_vec())?;
            let txid = start(ends[tx.0], &mut next, 5, encoded)?;
            sending = Some((txid, n));
        }
        if receiving.is_none() && got.len() < data.len() {
            let (body, memory) = receive_body(ends[rx.0], rx.1, RECEIVE_MAX as u32)?;
            receiving = Some((start(ends[rx.0], &mut next, 6, body)?, memory));
        }
        let mut items: Vec<WaitItem> = ends
            .iter()
            .map(|p| WaitItem::new(&p.channel().0, Signals::READABLE | Signals::PEER_CLOSED))
            .collect();
        match wait(&mut items, deadline) {
            Ok(_) => {}
            Err(Error::TimedOut) => {
                return Err(format!(
                    "{} of {} bytes arrived ({} sent) in time",
                    got.len(),
                    data.len(),
                    sent
                ));
            }
            Err(e) => return Err(format!("waiting for the server: {}", e.name())),
        }
        for end in ends.iter() {
            while let Some((txid, a)) = poll_answer(end)? {
                if let Some((_, n)) = sending.filter(|(id, _)| *id == txid) {
                    let took = must(answered::<u32>(a)?, "send")? as usize;
                    if took == 0 || took > n {
                        return Err(format!("a send of {n} bytes was answered {took}"));
                    }
                    sent += took;
                    sending = None;
                } else if receiving.as_ref().map(|r| r.0) == Some(txid) {
                    let (_, memory) = receiving.take().ok_or("missing receive loan")?;
                    let bytes = must(received(&memory, answered::<u32>(a)?)?, "receive")?;
                    if bytes.is_empty() {
                        return Err(format!(
                            "the stream ended after {} of {} bytes",
                            got.len(),
                            data.len()
                        ));
                    }
                    if bytes.len() > RECEIVE_MAX || got.len() + bytes.len() > data.len() {
                        return Err(format!(
                            "a receive of {} bytes overran the {} sent",
                            bytes.len(),
                            data.len()
                        ));
                    }
                    got.extend_from_slice(&bytes);
                    receiving = None;
                } else {
                    return Err(format!("an answer to {txid}, which is not outstanding"));
                }
            }
        }
    }
    if got != data {
        let at = got.iter().zip(data).position(|(a, b)| a != b).unwrap_or(0);
        return Err(format!(
            "{} bytes arrived changed, first at byte {at}",
            data.len()
        ));
    }
    Ok(())
}

/// The loopback addresses, checked to be the server's and the policy's.
fn loopback(p: &mut Probe) -> Result<(Info, Addr, Addr), Fail> {
    let i = info(p)?;
    let lo4 = Addr::v4(127, 0, 0, 1);
    let lo6 = Addr(core::net::IpAddr::V6(core::net::Ipv6Addr::LOCALHOST));
    for a in [lo4, lo6] {
        if i.interface_of(&a).is_none() {
            return Err(format!("no interface has {a}"));
        }
        let ep = Endpoint::new(a, 40000);
        let pol = &i.policy;
        if !(pol.may_connect(&ep)
            && pol.may_listen(&ep)
            && pol.may_bind(&ep)
            && pol.may_ping(&a)
            && pol.may_use_source(&a))
        {
            return Err(format!(
                "the connection's policy does not reach {a}, which this suite needs"
            ));
        }
        if !pol.has(rights::GRANT) {
            return Err(String::from(
                "the connection's policy cannot grant, which this suite needs",
            ));
        }
    }
    Ok((i, lo4, lo6))
}

/// A listener on `at`, a connection to it from `from`, and the connection accepted.
fn pair_on(
    client: &mut Probe,
    from: Option<Addr>,
    server: &mut Probe,
    at: Endpoint,
) -> Result<(u32, Opened, Opened), Fail> {
    let (l, bound) = must(listen(server, at, 4)?, "listen")?;
    if bound.addr != at.addr || bound.port == 0 || (at.port != 0 && bound.port != at.port) {
        return Err(format!("a listener on {at} is on {bound}"));
    }
    let c = must(connect(client, Target::Endpoint(bound), from)?, "connect")?;
    let s = must(accept(server, l)?, "accept")?;
    if c.remote != bound || s.local != bound || s.remote != c.local {
        return Err(format!(
            "the connection's ends disagree: connected {} to {}, accepted {} from {}",
            c.local, c.remote, s.local, s.remote
        ));
    }
    if from.is_some_and(|f| f != c.local.addr) {
        return Err(format!("a connection from {from:?} left from {}", c.local));
    }
    Ok((l, c, s))
}

fn info_case(p: &mut Probe) -> Result<(), Fail> {
    let (i, _, _) = loopback(p)?;
    i.policy
        .validate()
        .map_err(|e| format!("the connection's own policy is {e:?}"))?;
    for iface in &i.interfaces {
        if iface.name.is_empty() || iface.mtu < 576 {
            return Err(format!(
                "interface {:?} with an MTU of {}",
                iface.name, iface.mtu
            ));
        }
        if iface.addrs.iter().any(|c| c.prefix > c.addr.bits()) {
            return Err(format!("interface {} has a malformed address", iface.name));
        }
    }
    let lo = i
        .interface_of(&Addr::v4(127, 0, 0, 1))
        .ok_or("no loopback")?;
    if !lo.up || lo.mac != 0 {
        return Err(format!("the loopback is {lo:?}"));
    }
    Ok(())
}

fn tcp_sizes(p: &mut Probe) -> Result<(), Fail> {
    let (_, lo4, lo6) = loopback(p)?;
    for (addr, sizes) in [(lo4, &SIZES[..]), (lo6, &FEW_SIZES[..])] {
        // One connection holds both ends: the probe is the client and the server.
        let (l, bound) = must(listen(p, Endpoint::new(addr, 0), 4)?, "listen")?;
        let c = must(connect(p, Target::Endpoint(bound), Some(addr))?, "connect")?;
        let s = must(accept(p, l)?, "accept")?;
        if c.remote != bound || s.remote != c.local || s.local != bound {
            return Err(format!("the ends disagree: {c:?} and {s:?}"));
        }
        for (n, &size) in sizes.iter().enumerate() {
            let one = pattern(size, n as u32 + 1);
            transfer(&mut [&mut *p], (0, c.socket), (0, s.socket), &one)
                .map_err(|e| format!("{size} bytes from {} to {}: {e}", c.local, s.local))?;
            let back = pattern(size, n as u32 + 101);
            transfer(&mut [&mut *p], (0, s.socket), (0, c.socket), &back)
                .map_err(|e| format!("{size} bytes from {} to {}: {e}", s.local, c.local))?;
        }
        for socket in [c.socket, s.socket, l] {
            must(close(p, socket)?, "close")?;
        }
    }
    Ok(())
}

fn half_close(p: &mut Probe) -> Result<(), Fail> {
    let (_, lo4, _) = loopback(p)?;
    let (l, bound) = must(listen(p, Endpoint::new(lo4, 0), 1)?, "listen")?;
    let c = must(connect(p, Target::Endpoint(bound), None)?, "connect")?;
    let s = must(accept(p, l)?, "accept")?;
    let data = pattern(5000, 7);
    transfer(&mut [&mut *p], (0, c.socket), (0, s.socket), &data)?;
    must(shutdown(p, c.socket)?, "shutdown")?;
    // The peer reads the end of the stream, and goes on reading it.
    for _ in 0..2 {
        let end = must(receive(p, s.socket, 100)?, "receive at the end")?;
        if !end.is_empty() {
            return Err(format!("{} bytes after the end of the stream", end.len()));
        }
    }
    refuses(
        send(p, c.socket, vec![1])?,
        net::Error::NotConnected,
        "a send after shutdown",
    )?;
    let st = must(status(p, s.socket)?, "status")?;
    if st.state != SocketState::CloseWait {
        return Err(format!("the side that read the FIN is {:?}", st.state));
    }
    // The other half is still open.
    let reply = pattern(70_000, 8);
    transfer(&mut [&mut *p], (0, s.socket), (0, c.socket), &reply)?;
    must(close(p, s.socket)?, "close")?;
    let end = must(receive(p, c.socket, 100)?, "receive after the peer closed")?;
    if !end.is_empty() {
        return Err(String::from("bytes after the peer closed"));
    }
    must(close(p, c.socket)?, "close")?;
    refuses(close(p, c.socket)?, net::Error::NoSocket, "a second close")?;
    Ok(())
}

fn backpressure(p: &mut Probe) -> Result<(), Fail> {
    let (_, lo4, _) = loopback(p)?;
    let (l, bound) = must(listen(p, Endpoint::new(lo4, 0), 1)?, "listen")?;
    let c = must(connect(p, Target::Endpoint(bound), None)?, "connect")?;
    let s = must(accept(p, l)?, "accept")?;
    // Send, never reading, until a send waits.
    let mut next = RAW_TXID;
    let mut taken = 0usize;
    let blocked = loop {
        if taken > 64 << 20 {
            return Err(format!("{taken} bytes were taken with nobody reading"));
        }
        let chunk = pattern(SEND_MAX, taken as u32);
        let encoded = send_body(p, c.socket, chunk)?;
        let txid = start(p, &mut next, 5, encoded)?;
        if quiet(p, 500_000_000)? {
            break txid;
        }
        taken += must(
            answered::<u32>(wait_answer(p, txid, deadline_in(SECOND))?)?,
            "send",
        )? as usize;
    };
    if taken == 0 {
        return Err(String::from("the first send waited"));
    }
    // Both queues are full: the sender's send queue and the receiver's window.
    // (Raw requests are outstanding, so the status is asked raw too.)
    let st_txid = start(p, &mut next, 15, body(|e| c.socket.encode(e))?)?;
    let sender: SocketStatus = must(
        answered(wait_answer(p, st_txid, deadline_in(5 * SECOND))?)?,
        "status",
    )?;
    let st_txid = start(p, &mut next, 15, body(|e| s.socket.encode(e))?)?;
    let receiver: SocketStatus = must(
        answered(wait_answer(p, st_txid, deadline_in(5 * SECOND))?)?,
        "status",
    )?;
    // A window advertised in units of its scale may leave a few bytes unused.
    if sender.send_queued == 0 || receiver.receive_queued * 10 < receiver.receive_capacity * 9 {
        return Err(format!(
            "with a send waiting, the sender queues {} of {} and the receiver holds {} of {}",
            sender.send_queued,
            sender.send_capacity,
            receiver.receive_queued,
            receiver.receive_capacity
        ));
    }
    // Reading answers the waiting send; then everything taken arrives.
    let mut got = 0usize;
    let mut waiting = Some(blocked);
    let deadline = deadline_in(60 * SECOND);
    while waiting.is_some() || got < taken {
        let (body, memory) = receive_body(p, s.socket, RECEIVE_MAX as u32)?;
        let r = start(p, &mut next, 6, body)?;
        loop {
            let a = loop {
                if let Some(x) = poll_answer(p)? {
                    break x;
                }
                if now() > deadline {
                    return Err(format!("{got} of {taken} bytes read in time"));
                }
                let _ = wait_one(&p.channel().0, Signals::READABLE, deadline);
            };
            if Some(a.0) == waiting {
                taken += must(answered::<u32>(a.1)?, "the waiting send")? as usize;
                waiting = None;
            } else if a.0 == r {
                let bytes = must(received(&memory, answered::<u32>(a.1)?)?, "receive")?;
                if bytes.is_empty() {
                    return Err(String::from("the stream ended"));
                }
                got += bytes.len();
                break;
            } else {
                return Err(format!("an answer to {}", a.0));
            }
        }
    }
    if got != taken {
        return Err(format!("{got} bytes read of {taken} taken"));
    }
    Ok(())
}

fn refusals(p: &mut Probe) -> Result<(), Fail> {
    let (_, lo4, _) = loopback(p)?;
    // A port nobody listens on is refused by the peer's reset.
    let (l, bound) = must(listen(p, Endpoint::new(lo4, 0), 1)?, "listen")?;
    must(close(p, l)?, "close")?;
    refuses(
        connect(p, Target::Endpoint(bound), None)?,
        net::Error::Refused,
        "a connect to a closed port",
    )?;
    // Unknown sockets, and sockets of the wrong kind.
    refuses(
        receive(p, 999, 1)?,
        net::Error::NoSocket,
        "a receive on no socket",
    )?;
    refuses(
        status(p, 999)?,
        net::Error::NoSocket,
        "the status of no socket",
    )?;
    let (l, bound) = must(listen(p, Endpoint::new(lo4, 0), 2)?, "listen")?;
    refuses(
        send(p, l, vec![1])?,
        net::Error::WrongKind,
        "a send on a listener",
    )?;
    refuses(
        listen(p, bound, 1)?,
        net::Error::AddressInUse,
        "a second listener on one endpoint",
    )?;
    let c = must(connect(p, Target::Endpoint(bound), None)?, "connect")?;
    refuses(
        accept(p, c.socket)?,
        net::Error::WrongKind,
        "an accept on a connection",
    )?;
    let (u, ubound) = must(bind(p, Endpoint::new(lo4, 0))?, "bind")?;
    refuses(
        bind(p, ubound)?,
        net::Error::AddressInUse,
        "a second socket on one endpoint",
    )?;
    refuses(
        receive(p, u, 1)?,
        net::Error::WrongKind,
        "a TCP receive on a UDP socket",
    )?;
    refuses(
        send_to(p, c.socket, ubound, vec![1])?,
        net::Error::WrongKind,
        "a datagram from a TCP socket",
    )?;
    // A second receive while one waits; closing answers the first.
    let mut next = RAW_TXID;
    let (receive_request, _first_memory) = receive_body(p, c.socket, 10)?;
    let first = start(p, &mut next, 6, receive_request)?;
    if !quiet(p, 200_000_000)? {
        return Err(String::from("a receive with nothing to read was answered"));
    }
    refuses(
        receive(p, c.socket, 10)?,
        net::Error::InUse,
        "a second receive",
    )?;
    let close_txid = start(p, &mut next, 8, body(|e| c.socket.encode(e))?)?;
    let (mut closed, mut waited) = (false, false);
    let deadline = deadline_in(5 * SECOND);
    while !(closed && waited) {
        let (id, a) = match poll_answer(p)? {
            Some(x) => x,
            None => {
                wait_one(&p.channel().0, Signals::READABLE, deadline)
                    .map_err(|e| format!("close: {}", e.name()))?;
                continue;
            }
        };
        if id == first {
            refuses(
                answered::<u32>(a)?,
                net::Error::Closed,
                "the receive a close interrupted",
            )?;
            waited = true;
        } else if id == close_txid {
            must(answered::<()>(a)?, "close")?;
            closed = true;
        } else {
            return Err(format!("an answer to {id}"));
        }
    }
    // Addresses that cannot be used.
    let unspecified = Endpoint::new(Addr::v4(0, 0, 0, 0), bound.port);
    refuses(
        connect(p, Target::Endpoint(unspecified), None)?,
        net::Error::BadAddress,
        "a connect to 0.0.0.0",
    )?;
    refuses(
        connect(p, Target::Endpoint(Endpoint::new(lo4, 0)), None)?,
        net::Error::BadAddress,
        "a connect to port 0",
    )?;
    let other = Addr::v4(127, 0, 0, 2);
    let want = if info(p)?.policy.may_use_source(&other) {
        net::Error::BadAddress
    } else {
        net::Error::NotPermitted
    };
    refuses(
        connect(p, Target::Endpoint(bound), Some(other))?,
        want,
        "a connect from an address the server does not have",
    )?;
    refuses(
        listen(p, Endpoint::new(lo4, 0), 0)?,
        net::Error::BadArgument,
        "a backlog of 0",
    )?;
    refuses(
        listen(p, Endpoint::new(lo4, 0), crate::BACKLOG_MAX + 1)?,
        net::Error::BadArgument,
        "a backlog over the bound",
    )?;
    refuses(
        receive(p, l, 0)?,
        net::Error::WrongKind,
        "a receive on a listener",
    )?;
    refuses(
        ping(p, lo4, PING_MAX + 1)?,
        net::Error::BadArgument,
        "a ping over the bound",
    )?;
    Ok(())
}

fn udp(p: &mut Probe) -> Result<(), Fail> {
    let (i, lo4, lo6) = loopback(p)?;
    let mtu = i
        .interface_of(&lo4)
        .map(|x| x.mtu as usize)
        .ok_or("no loopback")?;
    for (addr, header) in [(lo4, 28), (lo6, 48)] {
        let (a, at) = must(bind(p, Endpoint::new(addr, 0))?, "bind")?;
        let (b, bt) = must(bind(p, Endpoint::new(addr, 0))?, "bind")?;
        let largest = (mtu - header).min(crate::DATAGRAM_MAX);
        for (n, size) in [0usize, 1, 100, 1000, 1472, largest]
            .into_iter()
            .enumerate()
        {
            let data = pattern(size, n as u32 + 3);
            must(send_to(p, a, bt, data.clone())?, "send_to")?;
            let (from, got) = must(receive_from(p, b)?, "receive_from")?;
            if from != at || got != data {
                return Err(format!(
                    "{size} bytes from {at} arrived as {} from {from}",
                    got.len()
                ));
            }
        }
        refuses(
            send_to(p, a, bt, vec![0; largest + 1])?,
            net::Error::TooLarge,
            "a datagram over the MTU",
        )?;
        refuses(
            send_to(p, a, Endpoint::new(addr, 0), vec![1])?,
            net::Error::BadAddress,
            "a datagram to port 0",
        )?;
        must(close(p, a)?, "close")?;
        must(close(p, b)?, "close")?;
    }
    Ok(())
}

fn echo(p: &mut Probe) -> Result<(), Fail> {
    let (_, lo4, lo6) = loopback(p)?;
    for a in [lo4, lo6] {
        for size in [0, 56, PING_MAX] {
            must(ping(p, a, size)?, "ping")?;
        }
    }
    Ok(())
}

/// A connection through a listener a grant returned.
fn through(listener: moruna_svc::Channel) -> Result<net::Client, Fail> {
    let channel = moruna_svc::connect(&listener).map_err(wire)?;
    let mut c = net::Client::connect(
        channel,
        deadline_in(10 * SECOND),
        moruna_svc::Account::carve(8 << 20, "net-suite").map_err(wire)?,
    )
    .map_err(wire)?;
    c.core().set_timeout(30 * SECOND);
    Ok(c)
}

fn narrowed_grant(p: &mut Probe) -> Result<(), Fail> {
    let (_, lo4, _) = loopback(p)?;
    let (_, open) = must(listen(p, Endpoint::new(lo4, 0), 4)?, "listen")?;
    let (other, closed_port) = must(listen(p, Endpoint::new(lo4, 0), 4)?, "listen")?;
    let narrow = Policy {
        rights: rights::CONNECT,
        remote: vec![Rule {
            net: Cidr::new(lo4, 32),
            first: open.port,
            last: open.port,
        }],
        local: vec![Rule::any_port(Cidr::new(lo4, 32))],
        names: vec![],
        sockets: 2,
    };
    let mut c = through(must(grant(p, narrow.clone())?, "grant")?)?;
    let got = c.info().map_err(wire)?;
    if got.policy != narrow {
        return Err(format!("the granted connection holds {:?}", got.policy));
    }
    c.open(Target::Endpoint(open), None)
        .map_err(|e| format!("the one permitted connect: {e:?}"))?;
    let denied = |r: Result<_, moruna_svc::CallError<net::Error>>, what: &str| match r {
        Err(moruna_svc::CallError::Remote(net::Error::NotPermitted)) => Ok(()),
        other => Err(format!(
            "{what}: expected NotPermitted, got {:?}",
            other.map(|_| ())
        )),
    };
    denied(
        c.open(Target::Endpoint(closed_port), None).map(|_| ()),
        "a connect to another port",
    )?;
    denied(
        c.open(
            Target::Endpoint(Endpoint::new(Addr::v4(127, 0, 0, 9), open.port)),
            None,
        )
        .map(|_| ()),
        "a connect to another address",
    )?;
    denied(c.listen(Endpoint::new(lo4, 0), 1).map(|_| ()), "a listen")?;
    denied(c.bind(Endpoint::new(lo4, 0)).map(|_| ()), "a bind")?;
    denied(c.ping(lo4, 8).map(|_| ()), "a ping")?;
    denied(c.resolve(String::from(SUITE_NAME)).map(|_| ()), "a resolve")?;
    denied(
        c.open(Target::Name(String::from(SUITE_NAME), open.port), None)
            .map(|_| ()),
        "a connect by name",
    )?;
    denied(
        c.grant(narrow.clone()).map(|_| ()),
        "a grant without the right",
    )?;
    // The policy's socket bound: one is open, a second fits, a third does not.
    c.open(Target::Endpoint(open), None)
        .map_err(|e| format!("a second connect: {e:?}"))?;
    match c.open(Target::Endpoint(open), None) {
        Err(moruna_svc::CallError::Remote(net::Error::TooMany)) => {}
        other => return Err(format!("a third socket of two: {:?}", other.map(|_| ()))),
    }
    must(close(p, other)?, "close")?;
    Ok(())
}

fn wider_grant(p: &mut Probe) -> Result<(), Fail> {
    let (i, _, _) = loopback(p)?;
    let mut more = i.policy.clone();
    more.sockets += 1;
    refuses(
        grant(p, more)?,
        net::Error::NotPermitted,
        "a grant of one more socket",
    )?;
    let mut wider = i.policy.clone();
    wider
        .remote
        .push(Rule::any_port(Cidr::new(Addr::v4(0, 0, 0, 0), 0)));
    wider.remote.push(Rule::any_port(Cidr::new(
        Addr(core::net::IpAddr::V6(core::net::Ipv6Addr::UNSPECIFIED)),
        0,
    )));
    if !i.policy.covers(&wider) {
        refuses(
            grant(p, wider)?,
            net::Error::NotPermitted,
            "a grant reaching every address",
        )?;
    }
    let mut names = i.policy.clone();
    names.names.push(NameRule {
        pattern: String::from("*.test"),
        first: 0,
        last: 65535,
    });
    if !i.policy.covers(&names) {
        refuses(
            grant(p, names)?,
            net::Error::NotPermitted,
            "a grant of a name",
        )?;
    }
    let mut bad = i.policy.clone();
    bad.local.push(Rule {
        net: Cidr::new(Addr::v4(127, 0, 0, 1), 32),
        first: 9,
        last: 8,
    });
    refuses(
        grant(p, bad)?,
        net::Error::BadPolicy,
        "a grant of a reversed port range",
    )?;
    Ok(())
}

fn connection_ends(p: &mut Probe) -> Result<(), Fail> {
    let (i, lo4, _) = loopback(p)?;
    let mut child = i.policy.clone();
    child.rights &= !rights::GRANT;
    child.sockets = child.sockets.min(8);
    let mut c = through(must(grant(p, child)?, "grant")?)?;
    // The other connection listens and connects to this one.
    let (_, its) = c.listen(Endpoint::new(lo4, 0), 2).map_err(wire)?;
    let (l, ours) = must(listen(p, Endpoint::new(lo4, 0), 2)?, "listen")?;
    c.open(Target::Endpoint(ours), None).map_err(wire)?;
    let accepted = must(accept(p, l)?, "accept")?;
    let outbound = must(connect(p, Target::Endpoint(its), None)?, "connect")?;
    drop(c);
    // Its connection is closed by the server: this end reads the end of the stream,
    // and its listener has gone.
    for (what, s) in [
        ("accepted", accepted.socket),
        ("connected", outbound.socket),
    ] {
        match receive(p, s, 16)? {
            Ok(b) if b.is_empty() => {}
            Err(net::Error::Reset) => {}
            other => {
                return Err(format!(
                    "the {what} socket of a connection that went: {other:?}"
                ));
            }
        }
    }
    refuses(
        connect(p, Target::Endpoint(its), None)?,
        net::Error::Refused,
        "a connect to its listener",
    )?;
    Ok(())
}

fn tls_arguments(p: &mut Probe) -> Result<(), Fail> {
    refuses(
        call::<()>(
            p,
            16,
            body(|e| {
                0u32.encode(e)?;
                String::new().encode(e)
            })?,
        )?,
        net::Error::BadAddress,
        "TLS refuses an empty verification name before touching a socket",
    )
}

static CASES: [Case; 11] = [
    Case {
        name: "tls: ordinal 16 refuses an invalid verification name",
        check: |p| with_account(p, tls_arguments),
    },
    Case {
        name: "info: the interfaces, the loopback and the connection's policy",
        check: |p| with_account(p, info_case),
    },
    Case {
        name: "tcp: a connection on loopback, bytes intact from 1 byte to 1 MiB both ways, IPv4 and IPv6",
        check: |p| with_account(p, tcp_sizes),
    },
    Case {
        name: "tcp: half-close: the peer reads to the end, then sends on its open half",
        check: |p| with_account(p, half_close),
    },
    Case {
        name: "tcp: backpressure: a send waits for room and is answered as the peer reads",
        check: |p| with_account(p, backpressure),
    },
    Case {
        name: "refusals: closed ports, unknown sockets, wrong kinds, waits in use, bad addresses and arguments",
        check: |p| with_account(p, refusals),
    },
    Case {
        name: "udp: datagrams on loopback with their senders, up to the MTU, IPv4 and IPv6",
        check: |p| with_account(p, udp),
    },
    Case {
        name: "icmp: echo on loopback, IPv4 and IPv6",
        check: |p| with_account(p, echo),
    },
    Case {
        name: "rights: a narrowed grant reaches only what it names, within its socket bound",
        check: |p| with_account(p, narrowed_grant),
    },
    Case {
        name: "rights: a grant wider than its parent, or malformed, is refused",
        check: |p| with_account(p, wider_grant),
    },
    Case {
        name: "rights: a connection that goes takes its sockets and listeners with it",
        check: |p| with_account(p, connection_ends),
    },
];

/// The suite for one server (SDD 31, k).
pub fn suite() -> Suite {
    Suite {
        id: net::ID,
        requests: net::REQUESTS,
        cases: &CASES,
        timeout: 60 * SECOND,
    }
}

/// The addresses of the card a pair-suite side's policy reaches: an IPv4 and an IPv6
/// address of one interface, both of them the policy's to send from.
fn side(p: &mut Probe) -> Result<(Info, Addr, Addr), Fail> {
    let i = info(p)?;
    for iface in i.interfaces.iter().filter(|x| x.mac != 0) {
        let mine = |v4: bool| {
            iface
                .addrs
                .iter()
                .map(|c| c.addr)
                .find(|a| a.is_v4() == v4 && i.policy.may_use_source(a) && !is_link_local(a))
        };
        if let (Some(a4), Some(a6)) = (mine(true), mine(false)) {
            return Ok((i.clone(), a4, a6));
        }
    }
    Err(String::from(
        "the policy reaches no card with an IPv4 and an IPv6 address",
    ))
}

fn is_link_local(a: &Addr) -> bool {
    match a.0 {
        core::net::IpAddr::V6(v) => v.segments()[0] & 0xffc0 == 0xfe80,
        core::net::IpAddr::V4(v) => v.is_link_local(),
    }
}

fn pair_tcp<'account>(
    a: &mut Probe<'account>,
    b: &mut Probe<'account>,
    family_v4: bool,
    sizes: &[usize],
) -> Result<(), Fail> {
    let (ia, a4, a6) = side(a)?;
    let (_, b4, b6) = side(b)?;
    let (from, to) = if family_v4 { (a4, b4) } else { (a6, b6) };
    if ia.interface_of(&from).map(|x| &x.name) == ia.interface_of(&to).map(|x| &x.name) {
        return Err(format!(
            "{from} and {to} are on one interface, so nothing would cross the link"
        ));
    }
    let (_, c, s) = pair_on(a, Some(from), b, Endpoint::new(to, 0))?;
    for (n, &size) in sizes.iter().enumerate() {
        let one = pattern(size, n as u32 + 11);
        transfer(&mut [&mut *a, &mut *b], (0, c.socket), (1, s.socket), &one)
            .map_err(|e| format!("{size} bytes from {} to {}: {e}", c.local, s.local))?;
        let back = pattern(size, n as u32 + 111);
        transfer(&mut [&mut *a, &mut *b], (1, s.socket), (0, c.socket), &back)
            .map_err(|e| format!("{size} bytes from {} to {}: {e}", s.local, c.local))?;
    }
    Ok(())
}

fn pair_tcp4<'account>(a: &mut Probe<'account>, b: &mut Probe<'account>) -> Result<(), Fail> {
    pair_tcp(a, b, true, &SIZES)
}

fn pair_tcp6<'account>(a: &mut Probe<'account>, b: &mut Probe<'account>) -> Result<(), Fail> {
    pair_tcp(a, b, false, &FEW_SIZES)
}

fn pair_half_close<'account>(a: &mut Probe<'account>, b: &mut Probe<'account>) -> Result<(), Fail> {
    let (_, a4, _) = side(a)?;
    let (_, b4, _) = side(b)?;
    let (l, c, s) = pair_on(a, Some(a4), b, Endpoint::new(b4, 0))?;
    let data = pattern(30_000, 21);
    transfer(&mut [&mut *a, &mut *b], (0, c.socket), (1, s.socket), &data)?;
    must(shutdown(a, c.socket)?, "shutdown")?;
    let end = must(receive(b, s.socket, 100)?, "receive at the end")?;
    if !end.is_empty() {
        return Err(String::from("bytes after a shut-down side's end"));
    }
    let reply = pattern(30_000, 22);
    transfer(
        &mut [&mut *a, &mut *b],
        (1, s.socket),
        (0, c.socket),
        &reply,
    )?;
    must(close(b, s.socket)?, "close")?;
    let end = must(receive(a, c.socket, 100)?, "receive after the peer closed")?;
    if !end.is_empty() {
        return Err(String::from("bytes after the peer closed"));
    }
    // A port nobody listens on is refused across the link.
    must(close(b, l)?, "close")?;
    refuses(
        connect(a, Target::Endpoint(s.local), Some(a4))?,
        net::Error::Refused,
        "a connect across the link to a closed port",
    )
}

fn pair_udp(a: &mut Probe, b: &mut Probe) -> Result<(), Fail> {
    let (ia, a4, a6) = side(a)?;
    let (_, b4, b6) = side(b)?;
    for (from, to, header) in [(a4, b4, 28usize), (a6, b6, 48)] {
        let mtu = ia
            .interface_of(&from)
            .map(|x| x.mtu as usize)
            .ok_or("no interface")?;
        let (sa, at) = must(bind(a, Endpoint::new(from, 0))?, "bind")?;
        let (sb, bt) = must(bind(b, Endpoint::new(to, 0))?, "bind")?;
        for (n, size) in [0usize, 1, 512, mtu - header].into_iter().enumerate() {
            let data = pattern(size, n as u32 + 31);
            must(send_to(a, sa, bt, data.clone())?, "send_to")?;
            let (src, got) = must(receive_from(b, sb)?, "receive_from")?;
            if src != at || got != data {
                return Err(format!(
                    "{size} bytes from {at} arrived as {} from {src}",
                    got.len()
                ));
            }
            must(send_to(b, sb, at, data.clone())?, "send_to")?;
            let (src, got) = must(receive_from(a, sa)?, "receive_from")?;
            if src != bt || got != data {
                return Err(format!(
                    "{size} bytes from {bt} arrived as {} from {src}",
                    got.len()
                ));
            }
        }
        refuses(
            send_to(a, sa, bt, vec![0; mtu - header + 1])?,
            net::Error::TooLarge,
            "a datagram over the MTU",
        )?;
    }
    Ok(())
}

fn pair_echo(a: &mut Probe, b: &mut Probe) -> Result<(), Fail> {
    let (_, a4, a6) = side(a)?;
    let (_, b4, b6) = side(b)?;
    for to in [b4, b6] {
        must(ping(a, to, 56)?, "ping")?;
    }
    for to in [a4, a6] {
        must(ping(b, to, 1000)?, "ping")?;
    }
    Ok(())
}

/// Serves names on `b`'s port 53 while the request `waiting` on `a` is outstanding:
/// every query for [`SUITE_NAME`] is answered with `addrs`, any other with no such
/// name. Returns that request's answer once it arrives, and closes the socket.
fn serve_names(
    a: &Probe,
    waiting: u32,
    b: &mut Probe,
    at: Endpoint,
    addrs: &[Addr],
) -> Result<Answer, Fail> {
    let (socket, _) = must(bind(b, at)?, "bind on port 53")?;
    let deadline = deadline_in(30 * SECOND);
    let mut next = RAW_TXID;
    let mut asked = start(b, &mut next, 11, body(|e| socket.encode(e))?)?;
    let answer = loop {
        if let Some((id, ans)) = poll_answer(a)? {
            if id != waiting {
                return Err(format!("an answer to {id}"));
            }
            break ans;
        }
        if let Some((id, ans)) = poll_answer(b)? {
            if id != asked {
                return Err(format!("an answer to {id} on b"));
            }
            let (from, q) = must(
                answered::<(Endpoint, Vec<u8>)>(ans)?,
                "receive_from on port 53",
            )?;
            let query =
                dns::parse_query(&q).ok_or("the resolver sent something that is not a query")?;
            let known = query.name == SUITE_NAME;
            let reply = dns::answer(&query, known, if known { addrs } else { &[] }, 30);
            let t = start(
                b,
                &mut next,
                10,
                body(|e| {
                    socket.encode(e)?;
                    from.encode(e)?;
                    reply.encode(e)
                })?,
            )?;
            must(
                answered::<()>(wait_answer(b, t, deadline)?)?,
                "send_to the resolver",
            )?;
            asked = start(b, &mut next, 11, body(|e| socket.encode(e))?)?;
            continue;
        }
        let mut items = [
            WaitItem::new(&a.channel().0, Signals::READABLE | Signals::PEER_CLOSED),
            WaitItem::new(&b.channel().0, Signals::READABLE | Signals::PEER_CLOSED),
        ];
        match wait(&mut items, deadline) {
            Ok(_) => {}
            Err(Error::TimedOut) => {
                return Err(String::from("the request was not answered in time"));
            }
            Err(e) => return Err(format!("waiting: {}", e.name())),
        }
    };
    // The receive still waiting on the socket is answered `Closed` by the close.
    let closing = start(b, &mut next, 8, body(|e| socket.encode(e))?)?;
    let (mut closed, mut ended) = (false, false);
    while !(closed && ended) {
        let (id, ans) = match poll_answer(b)? {
            Some(x) => x,
            None => {
                wait_one(&b.channel().0, Signals::READABLE, deadline)
                    .map_err(|e| format!("closing port 53: {}", e.name()))?;
                continue;
            }
        };
        if id == closing {
            must(answered::<()>(ans)?, "close")?;
            closed = true;
        } else if id == asked {
            // A query that arrived as the socket closed is answered too; either is fine.
            match answered::<(Endpoint, Vec<u8>)>(ans)? {
                Ok(_) | Err(net::Error::Closed) => ended = true,
                Err(e) => return Err(format!("the receive on port 53 ended {e:?}")),
            }
        } else {
            return Err(format!("an answer to {id} on b"));
        }
    }
    Ok(answer)
}

fn pair_names(a: &mut Probe, b: &mut Probe) -> Result<(), Fail> {
    let (ia, a4, _) = side(a)?;
    let (_, b4, b6) = side(b)?;
    if !ia.dns.contains(&b4) {
        return Err(format!(
            "the resolver's servers are {:?}, not {b4}, which this suite needs",
            ia.dns
        ));
    }
    if !ia.policy.may_resolve(SUITE_NAME) {
        return Err(format!(
            "a's policy may not resolve {SUITE_NAME}, which this suite needs"
        ));
    }
    let names = Endpoint::new(b4, 53);
    let mut next = RAW_TXID;
    let r = start(a, &mut next, 12, resolve_body(SUITE_NAME)?)?;
    let mut got: Vec<Addr> = must(
        answered(serve_names(a, r, b, names, &[b4, b6])?)?,
        "resolve",
    )?;
    got.sort();
    let mut want = vec![b4, b6];
    want.sort();
    if got != want {
        return Err(format!("{SUITE_NAME} resolved to {got:?}, not {want:?}"));
    }
    let r = start(a, &mut next, 12, resolve_body("absent.suite.ke2.test")?)?;
    refuses(
        answered::<Vec<Addr>>(serve_names(a, r, b, names, &[])?)?,
        net::Error::NotFound,
        "a name nobody has",
    )?;
    // A connect by name: resolved, then connected.
    let (l, at) = must(listen(b, Endpoint::new(b4, 0), 1)?, "listen")?;
    if ia.policy.may_connect_name(SUITE_NAME, at.port) || ia.policy.may_connect(&at) {
        let c = start(
            a,
            &mut next,
            2,
            body(|e| {
                Target::Name(String::from(SUITE_NAME), at.port).encode(e)?;
                Some(a4).encode(e)
            })?,
        )?;
        let opened: Opened = must(
            answered(serve_names(a, c, b, names, &[b4])?)?,
            "connect by name",
        )?;
        let s = must(accept(b, l)?, "accept")?;
        if opened.remote != at || s.remote != opened.local {
            return Err(format!(
                "a connect by name opened {opened:?}, accepted {s:?}"
            ));
        }
    } else {
        return Err(format!(
            "a's policy may not connect to {SUITE_NAME}, which this suite needs"
        ));
    }
    Ok(())
}

static PAIR_CASES: [PairCase; 6] = [
    PairCase {
        name: "tcp across the link: a to b on IPv4 (ARP), bytes intact from 1 byte to 1 MiB both ways",
        check: |a, b| with_pair(a, b, pair_tcp4),
    },
    PairCase {
        name: "tcp across the link on IPv6 (NDP), both ways",
        check: |a, b| with_pair(a, b, pair_tcp6),
    },
    PairCase {
        name: "tcp across the link: half-close, and a closed port refused by the peer",
        check: |a, b| with_pair(a, b, pair_half_close),
    },
    PairCase {
        name: "udp across the link, both ways and both families, up to the MTU",
        check: |a, b| with_pair(a, b, pair_udp),
    },
    PairCase {
        name: "icmp across the link: each side's echo answered by the other, both families",
        check: |a, b| with_pair(a, b, pair_echo),
    },
    PairCase {
        name: "names across the link: resolved through b's name server, a name nobody has, a connect by name",
        check: |a, b| with_pair(a, b, pair_names),
    },
];

/// The suite for two cards on one link (SDD 31, k).
pub fn pair_suite() -> PairSuite {
    PairSuite {
        id: net::ID,
        cases: &PAIR_CASES,
        timeout: 60 * SECOND,
    }
}
