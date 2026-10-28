//! SV-T3 to SV-T7 (SDD 23, k): a protocol declared with `protocol!`, served by a
//! `Dispatcher` on its own thread and called through its typed client, all on the host
//! model of the channel ABI; and the conformance harness, which the reference server
//! passes and each broken server fails at the check aimed at it.

use std::{
    sync::mpsc,
    thread::{self, JoinHandle},
};

use moruna_svc::{
    CallError, Channel, Context, DEADLINE_INFINITE, Decoder, Dispatcher, Encoder, Error, Handle,
    Handler, Kind, Outcome, ProtocolError, ProtocolId, Refusal, Signals, Status, Transport, Turn,
    Wire, client, conformance, deadline_in, frame, limits, now, wait_one,
};

moruna_svc::protocol! {
    /// The test protocol, version 1.1.
    pub protocol calc {
        name = "moruna.test.calc";
        major = 1;
        minor = 1;
        requests {
            /// The sum.
            1 => fn add(a: u64, b: u64) -> u64;
            /// The quotient, or `DivideByZero`.
            2 => fn div(a: u64, b: u64) -> u64;
            /// Writes `text` to `to`; its length.
            3 => fn deliver(to: Channel, text: Vec<u8>) -> u32;
            /// Emits `n` tick events and one late event, then replies.
            4 => fn count(n: u32) -> ();
            /// The server's version string. Since 1.1.
            5 since 1 => fn version() -> String;
            /// Echoes the bytes, for messages at the bound.
            6 => fn echo(bytes: Vec<u8>) -> Vec<u8>;
        }
        events {
            1 => tick { i: u32 };
            2 since 1 => late {};
        }
        errors {
            1 => DivideByZero;
            2 => PeerGone;
        }
    }
}

moruna_svc::protocol! {
    /// The same protocol at version 1.0: no `version` request, no `late` event.
    pub protocol calc_old {
        name = "moruna.test.calc";
        major = 1;
        minor = 0;
        requests {
            1 => fn add(a: u64, b: u64) -> u64;
            2 => fn div(a: u64, b: u64) -> u64;
            3 => fn deliver(to: Channel, text: Vec<u8>) -> u32;
            4 => fn count(n: u32) -> ();
            6 => fn echo(bytes: Vec<u8>) -> Vec<u8>;
        }
        events {
            1 => tick { i: u32 };
        }
        errors {
            1 => DivideByZero;
            2 => PeerGone;
        }
    }
}

moruna_svc::protocol! {
    /// Another protocol, with no events and no errors.
    pub protocol ping {
        name = "moruna.test.ping";
        major = 2;
        minor = 0;
        requests {
            1 => fn ping(n: (u8, bool, Option<i64>)) -> (u8, bool, Option<i64>);
        }
        events {}
        errors {}
    }
}

struct Calc {
    lates_refused: u32,
}

impl calc::Server for Calc {
    fn add(&mut self, _: &mut Context<'_>, a: u64, b: u64) -> Result<u64, calc::Error> {
        Ok(a.wrapping_add(b))
    }
    fn div(&mut self, _: &mut Context<'_>, a: u64, b: u64) -> Result<u64, calc::Error> {
        a.checked_div(b).ok_or(calc::Error::DivideByZero)
    }
    fn deliver(
        &mut self,
        _: &mut Context<'_>,
        to: Channel,
        text: Vec<u8>,
    ) -> Result<u32, calc::Error> {
        to.write_with(&text, &mut [])
            .map_err(|_| calc::Error::PeerGone)?;
        Ok(text.len() as u32)
    }
    fn count(&mut self, cx: &mut Context<'_>, n: u32) -> Result<(), calc::Error> {
        for i in 0..n {
            calc::Event::tick { i }
                .emit(cx)
                .map_err(|_| calc::Error::PeerGone)?;
        }
        match (calc::Event::late {}).emit(cx) {
            Ok(()) => {}
            Err(Transport::Unsupported) => self.lates_refused += 1,
            Err(_) => return Err(calc::Error::PeerGone),
        }
        Ok(())
    }
    fn version(&mut self, _: &mut Context<'_>) -> Result<String, calc::Error> {
        Ok(String::from("1.1"))
    }
    fn echo(&mut self, _: &mut Context<'_>, bytes: Vec<u8>) -> Result<Vec<u8>, calc::Error> {
        Ok(bytes)
    }
}

struct OldCalc;

impl calc_old::Server for OldCalc {
    fn add(&mut self, _: &mut Context<'_>, a: u64, b: u64) -> Result<u64, calc_old::Error> {
        Ok(a + b)
    }
    fn div(&mut self, _: &mut Context<'_>, a: u64, b: u64) -> Result<u64, calc_old::Error> {
        a.checked_div(b).ok_or(calc_old::Error::DivideByZero)
    }
    fn deliver(
        &mut self,
        _: &mut Context<'_>,
        to: Channel,
        text: Vec<u8>,
    ) -> Result<u32, calc_old::Error> {
        to.write_with(&text, &mut [])
            .map_err(|_| calc_old::Error::PeerGone)?;
        Ok(text.len() as u32)
    }
    fn count(&mut self, cx: &mut Context<'_>, n: u32) -> Result<(), calc_old::Error> {
        for i in 0..n {
            calc_old::Event::tick { i }
                .emit(cx)
                .map_err(|_| calc_old::Error::PeerGone)?;
        }
        Ok(())
    }
    fn echo(&mut self, _: &mut Context<'_>, bytes: Vec<u8>) -> Result<Vec<u8>, calc_old::Error> {
        Ok(bytes)
    }
}

struct Ping;

impl ping::Server for Ping {
    fn ping(
        &mut self,
        _: &mut Context<'_>,
        n: (u8, bool, Option<i64>),
    ) -> Result<(u8, bool, Option<i64>), ping::Error> {
        Ok((n.0.wrapping_add(1), !n.1, n.2.map(|v| -v)))
    }
}

/// A server on its own thread: a dispatcher listening on the channel whose other end
/// is returned, until every channel it holds has closed.
fn serve(
    make: impl FnOnce() -> Dispatcher + Send + 'static,
) -> (Channel, JoinHandle<Result<(), Error>>) {
    let (listener, theirs) = Channel::create().expect("channel");
    let join = thread::spawn(move || {
        let mut d = make();
        d.listen(
            theirs,
            Box::new(|| Box::new(calc::Serve(Calc { lates_refused: 0 })) as Box<dyn Handler>),
        )?;
        d.run()
    });
    (listener, join)
}

fn calc_server() -> (Channel, JoinHandle<Result<(), Error>>) {
    serve(Dispatcher::new)
}

fn connect_calc(listener: &Channel) -> calc::Client {
    let channel = client::connect(listener).expect("connect");
    calc::Client::connect(channel, deadline_in(5_000_000_000)).expect("handshake")
}

#[test]
fn sv_t4_typed_calls_errors_and_handles() {
    let (listener, join) = calc_server();
    let mut c = connect_calc(&listener);
    assert_eq!(c.add(2, 3), Ok(5));
    assert_eq!(c.div(7, 2), Ok(3));
    assert_eq!(
        c.div(1, 0),
        Err(CallError::Remote(calc::Error::DivideByZero))
    );
    assert_eq!(c.version().as_deref(), Ok("1.1"));
    let (kept, sent) = Channel::create().expect("channel");
    assert_eq!(c.deliver(sent, b"hello".to_vec()), Ok(5));
    let mut buf = [0u8; 16];
    assert_eq!(kept.read(&mut buf, &mut []), Ok((5, 0)));
    assert_eq!(&buf[..5], b"hello");
    drop(kept);
    let (_k, gone) = Channel::create().expect("channel");
    drop(_k);
    assert_eq!(
        c.deliver(gone, b"x".to_vec()),
        Err(CallError::Remote(calc::Error::PeerGone))
    );
    assert_eq!(c.core().server_minor(), 1);
    assert_eq!(c.core().protocol(), calc::ID);
    drop(c);
    drop(listener);
    assert_eq!(join.join().expect("server thread"), Ok(()));
}

#[test]
fn sv_t4_events_arrive_before_the_reply_and_are_kept() {
    let (listener, join) = calc_server();
    let mut c = connect_calc(&listener);
    assert_eq!(c.count(3), Ok(()));
    for want in 0..3 {
        match c.next_event(deadline_in(1_000_000_000)) {
            Ok(calc::Event::tick { i }) => assert_eq!(i, want),
            other => panic!("expected tick {want}, got {other:?}"),
        }
    }
    assert!(matches!(
        c.next_event(deadline_in(1_000_000_000)),
        Ok(calc::Event::late {})
    ));
    assert_eq!(
        c.next_event(now() + 20_000_000).map(|_| ()),
        Err(CallError::TimedOut)
    );
    assert_eq!(calc::Event::late {}.ordinal(), 2);
    assert_eq!(calc::Event::late {}.since(), 1);
    drop((c, listener));
    join.join().expect("server").expect("run");
}

#[test]
fn sv_t4_replies_match_their_requests_out_of_order() {
    let (listener, join) = calc_server();
    let channel = client::connect(&listener).expect("connect");
    let mut core =
        moruna_svc::ClientCore::connect(channel, calc::ID, DEADLINE_INFINITE).expect("hello");
    let mut ids = Vec::new();
    for (a, b) in [(1u64, 1u64), (2, 2), (3, 3)] {
        let mut e = Encoder::new();
        a.encode(&mut e).expect("a");
        b.encode(&mut e).expect("b");
        ids.push(core.send(1, e).expect("send"));
    }
    for (txid, want) in ids.iter().rev().zip([6u64, 4, 2]) {
        let mut reply = core
            .reply(*txid, deadline_in(1_000_000_000))
            .expect("reply");
        let mut d = reply.decoder();
        assert_eq!(u64::decode(&mut d), Ok(want));
        d.finish().expect("finish");
    }
    assert_eq!(
        core.reply(99, DEADLINE_INFINITE).map(|_| ()),
        Err(CallError::Malformed)
    );
    core.set_timeout(1_000_000_000);
    let reply = core.call(77, Encoder::new());
    assert_eq!(
        reply.map(|_| ()),
        Err(CallError::Status(Status::UnknownMethod))
    );
    let mut bad = Encoder::new();
    bad.put_raw(&[1, 2, 3]);
    assert_eq!(
        core.call(1, bad).map(|_| ()),
        Err(CallError::Status(Status::BadMessage))
    );
    let channel = core.into_channel();
    drop((channel, listener));
    join.join().expect("server").expect("run");
}

#[test]
fn sv_t3_minor_versions_are_negotiated() {
    // An old server: a new client may not send what the server lacks.
    let (listener, theirs) = Channel::create().expect("channel");
    let join = thread::spawn(move || {
        let mut d = Dispatcher::new();
        d.listen(theirs, calc_old::factory(|| OldCalc))?;
        d.run()
    });
    let mut c = connect_calc(&listener);
    assert_eq!(c.core().server_minor(), 0);
    assert_eq!(c.add(1, 1), Ok(2));
    assert_eq!(c.version(), Err(CallError::Unsupported));
    drop(c);
    // An old client of a new server: the server does not emit what it lacks.
    let (second, theirs) = Channel::create().expect("channel");
    let join2 = thread::spawn(move || {
        let mut d = Dispatcher::new();
        d.serve(theirs, Box::new(calc::Serve(Calc { lates_refused: 0 })))?;
        d.run()
    });
    let mut old = calc_old::Client::connect(second, deadline_in(5_000_000_000)).expect("hello");
    assert_eq!(old.core().server_minor(), 1);
    assert_eq!(old.count(2), Ok(()));
    assert!(matches!(
        old.next_event(deadline_in(1_000_000_000)),
        Ok(calc_old::Event::tick { i: 0 })
    ));
    assert!(matches!(
        old.next_event(deadline_in(1_000_000_000)),
        Ok(calc_old::Event::tick { i: 1 })
    ));
    assert_eq!(
        old.next_event(now() + 50_000_000).map(|_| ()),
        Err(CallError::TimedOut)
    );
    drop(old);
    drop(listener);
    join.join().expect("old server").expect("run");
    join2.join().expect("new server").expect("run");
}

/// A handler that is never asked anything.
struct Idle;

impl Handler for Idle {
    fn protocol(&self) -> ProtocolId {
        calc::ID
    }
    fn handle(&mut self, _: &mut Context<'_>, _: u16, _: &mut Decoder<'_>) -> Outcome {
        Outcome::Status(Status::UnknownMethod)
    }
}

#[test]
fn sv_t3_the_handshake_refuses_with_its_reason() {
    let (listener, join) = calc_server();
    // Another protocol on a calc server.
    let channel = client::connect(&listener).expect("connect");
    match ping::Client::connect(channel, deadline_in(5_000_000_000)) {
        Err(CallError::Refused(r)) => {
            assert_eq!(r.reason, Refusal::UnknownProtocol);
            assert_eq!(r.server, calc::ID.hello());
        }
        other => panic!("expected a refusal, got {:?}", other.map(|_| ())),
    }
    // Another major.
    let channel = client::connect(&listener).expect("connect");
    let other = ProtocolId::new("moruna.test.calc", 2, 0);
    match moruna_svc::ClientCore::connect(channel, other, deadline_in(5_000_000_000)) {
        Err(CallError::Refused(r)) => assert_eq!(r.reason, Refusal::MajorMismatch),
        other => panic!("expected a refusal, got {:?}", other.map(|_| ())),
    }
    // A request first.
    let channel = client::connect(&listener).expect("connect");
    frame::send(&channel, Kind::Request, 1, 1, Encoder::new()).expect("send");
    let mut f = next(&channel);
    assert_eq!(f.kind, Kind::Refuse);
    let mut d = f.decoder();
    assert_eq!(
        moruna_svc::Refused::decode(&mut d).map(|r| r.reason),
        Ok(Refusal::NotHello)
    );
    assert!(wait_one(&channel.0, Signals::PEER_CLOSED, deadline_in(1_000_000_000)).is_ok());
    drop(listener);
    join.join().expect("server").expect("run");
}

fn next(channel: &Channel) -> frame::Frame {
    let deadline = deadline_in(5_000_000_000);
    loop {
        if let Some(f) = frame::recv(channel).expect("recv") {
            return f;
        }
        wait_one(
            &channel.0,
            Signals::READABLE | Signals::PEER_CLOSED,
            deadline,
        )
        .expect("wait");
    }
}

#[test]
fn sv_t5_one_dispatcher_serves_several_protocols_and_listeners() {
    let (calc_listener, calc_theirs) = Channel::create().expect("channel");
    let (ping_listener, ping_theirs) = Channel::create().expect("channel");
    let (direct, direct_theirs) = Channel::create().expect("channel");
    let join = thread::spawn(move || {
        let mut d = Dispatcher::new();
        assert!(d.is_empty());
        d.listen(calc_theirs, calc::factory(|| Calc { lates_refused: 0 }))?;
        d.listen(ping_theirs, ping::factory(|| Ping))?;
        d.serve(direct_theirs, Box::new(ping::Serve(Ping)))?;
        assert_eq!(d.len(), 3);
        d.run()
    });
    let mut c = connect_calc(&calc_listener);
    let mut p = ping::Client::connect(
        client::connect(&ping_listener).expect("connect"),
        DEADLINE_INFINITE,
    )
    .expect("hello");
    let mut q = ping::Client::connect(direct, DEADLINE_INFINITE).expect("hello");
    assert_eq!(p.ping((1, false, Some(5))), Ok((2, true, Some(-5))));
    assert_eq!(q.ping((255, true, None)), Ok((0, false, None)));
    assert_eq!(c.add(40, 2), Ok(42));
    drop((c, p, q, calc_listener, ping_listener));
    join.join().expect("server").expect("run");
}

#[test]
fn sv_t5_the_dispatcher_closes_what_breaks_the_protocol() {
    let (listener, join) = calc_server();
    // A reply frame from a client.
    let channel = client::connect(&listener).expect("connect");
    let mut c = calc::Client::connect(channel, DEADLINE_INFINITE).expect("hello");
    frame::send(c.core().channel(), Kind::Reply, 0, 1, Encoder::new()).expect("send");
    assert!(
        wait_one(
            &c.core().channel().0,
            Signals::PEER_CLOSED,
            deadline_in(1_000_000_000)
        )
        .is_ok()
    );
    assert_eq!(c.add(1, 2), Err(CallError::PeerClosed));
    // A request with no id.
    let channel = client::connect(&listener).expect("connect");
    let mut c = calc::Client::connect(channel, DEADLINE_INFINITE).expect("hello");
    frame::send(c.core().channel(), Kind::Request, 1, 0, Encoder::new()).expect("send");
    assert!(
        wait_one(
            &c.core().channel().0,
            Signals::PEER_CLOSED,
            deadline_in(1_000_000_000)
        )
        .is_ok()
    );
    // A frame that is not a frame.
    let channel = client::connect(&listener).expect("connect");
    channel.write_with(&[9, 9, 9], &mut []).expect("write");
    assert!(wait_one(&channel.0, Signals::PEER_CLOSED, deadline_in(1_000_000_000)).is_ok());
    // A listener sent something other than one channel closes.
    listener
        .write_with(&[8, 0, 0, 0, 0, 0, 0, 0], &mut [Some(Handle::inert())])
        .expect("write");
    assert!(
        wait_one(
            &listener.0,
            Signals::PEER_CLOSED,
            deadline_in(1_000_000_000)
        )
        .is_ok()
    );
    drop(c);
    join.join().expect("server").expect("run");
}

#[test]
fn sv_t5_a_full_dispatcher_closes_new_arrivals() {
    let (listener, theirs) = Channel::create().expect("channel");
    let (tx, rx) = mpsc::channel::<()>();
    let join = thread::spawn(move || {
        let mut d = Dispatcher::new();
        d.listen(theirs, calc::factory(|| Calc { lates_refused: 0 }))?;
        let mut held = Vec::new();
        while d.len() < limits::WAIT_MAX_ITEMS {
            let (a, b) = Channel::create()?;
            d.serve(a, Box::new(calc::Serve(Calc { lates_refused: 0 })))?;
            held.push(b);
        }
        let (a, _b) = Channel::create()?;
        assert_eq!(d.serve(a, Box::new(Idle)).map(drop), Err(Error::Limit));
        let (a, _b) = Channel::create()?;
        assert_eq!(
            d.listen(a, calc::factory(|| Calc { lates_refused: 0 })),
            Err(Error::Limit)
        );
        tx.send(()).map_err(|_| Error::BadState)?;
        assert_eq!(d.turn(deadline_in(2_000_000_000)), Ok(Turn::Served));
        drop(held);
        d.run()
    });
    rx.recv().expect("full");
    let channel = client::connect(&listener).expect("connect");
    assert!(wait_one(&channel.0, Signals::PEER_CLOSED, deadline_in(5_000_000_000)).is_ok());
    drop(listener);
    join.join().expect("server").expect("run");
}

#[test]
fn sv_t5_turns_time_out_and_an_empty_dispatcher_is_done() {
    let mut d = Dispatcher::new();
    assert_eq!(d.turn(0), Ok(Turn::Empty));
    assert_eq!(d.run(), Ok(()));
    let (a, _b) = Channel::create().expect("channel");
    d.serve(a, Box::new(calc::Serve(Calc { lates_refused: 0 })))
        .expect("serve");
    assert_eq!(d.turn(now() + 10_000_000), Ok(Turn::TimedOut));
}

#[test]
fn sv_t2_a_request_over_the_bound_is_refused_before_it_is_sent() {
    let (listener, join) = calc_server();
    let mut c = connect_calc(&listener);
    let at_bound = vec![7u8; conformance::BUDGET - 4];
    assert_eq!(c.echo(at_bound.clone()), Ok(at_bound));
    assert_eq!(
        c.echo(vec![7u8; conformance::BUDGET - 3]),
        Err(CallError::TooLarge)
    );
    assert_eq!(c.add(1, 1), Ok(2));
    drop((c, listener));
    join.join().expect("server").expect("run");
}

#[test]
fn sv_t4_a_client_whose_server_is_gone_says_so() {
    let (listener, server) = Channel::create().expect("channel");
    drop(server);
    assert_eq!(
        client::connect(&listener).map(drop),
        Err(CallError::PeerClosed)
    );
    let (mine, theirs) = Channel::create().expect("channel");
    drop(theirs);
    assert_eq!(
        calc::Client::connect(mine, DEADLINE_INFINITE)
            .map(drop)
            .err(),
        Some(CallError::PeerClosed)
    );
    let (mine, _theirs) = Channel::create().expect("channel");
    assert_eq!(
        calc::Client::connect(mine, now() + 10_000_000)
            .map(drop)
            .err(),
        Some(CallError::TimedOut)
    );
    let (mine, theirs) = Channel::create().expect("channel");
    frame::send(&theirs, Kind::Event, 1, 0, Encoder::new()).expect("send");
    assert_eq!(
        calc::Client::connect(mine, DEADLINE_INFINITE)
            .map(drop)
            .err(),
        Some(CallError::Malformed)
    );
}

#[test]
fn sv_t4_typed_errors_round_trip() {
    use moruna_svc::NoError;
    assert_eq!(calc::Error::from_code(1), Some(calc::Error::DivideByZero));
    assert_eq!(calc::Error::PeerGone.code(), 2);
    assert_eq!(calc::Error::from_code(3), None);
    assert_eq!(NoError::from_code(1), None);
    assert_eq!(ping::Error::from_code(1), None);
    assert_eq!(
        CallError::<u32>::Remote(9).typed::<calc::Error>(),
        CallError::Malformed
    );
    for e in [
        CallError::<u32>::PeerClosed,
        CallError::TimedOut,
        CallError::TooLarge,
        CallError::Full,
        CallError::Unsupported,
        CallError::Status(Status::BadMessage),
        CallError::Malformed,
        CallError::Kernel(Error::NoMemory),
    ] {
        let typed = e.clone().typed::<calc::Error>();
        assert_eq!(format!("{e:?}"), format!("{typed:?}"));
    }
    assert_eq!(
        CallError::<u32>::from(Transport::Malformed),
        CallError::Malformed
    );
    assert_eq!(
        CallError::<u32>::from(Transport::Kernel(Error::Fault)),
        CallError::Kernel(Error::Fault)
    );
    assert_eq!(
        CallError::<u32>::from(moruna_svc::WireError::Truncated),
        CallError::Malformed
    );
    assert_eq!(calc::REQUESTS, &[1, 2, 3, 4, 5, 6]);
    assert_eq!(calc::EVENTS, &[1, 2]);
    assert_eq!(Status::from_code(0x1_0003), Some(Status::ReplyTooLarge));
    assert_eq!(Status::from_code(5), None);
    assert_eq!(Refusal::from_code(3), Some(Refusal::NotHello));
    assert_eq!(Refusal::from_code(4), None);
}

/// The calc protocol's conformance suite: every request, valid and invalid.
fn calc_suite() -> conformance::Suite {
    fn add(p: &mut conformance::Probe) -> Result<(), conformance::Fail> {
        let mut e = Encoder::new();
        2u64.encode(&mut e).map_err(|e| format!("{e:?}"))?;
        3u64.encode(&mut e).map_err(|e| format!("{e:?}"))?;
        let mut reply = p.request(1, e)?.reply()?;
        let mut d = reply.decoder();
        match (u64::decode(&mut d), d.finish()) {
            (Ok(5), Ok(())) => Ok(()),
            other => Err(format!("2 + 3 answered {other:?}")),
        }
    }
    fn div_by_zero(p: &mut conformance::Probe) -> Result<(), conformance::Fail> {
        let mut e = Encoder::new();
        1u64.encode(&mut e).map_err(|e| format!("{e:?}"))?;
        0u64.encode(&mut e).map_err(|e| format!("{e:?}"))?;
        p.request(2, e)?.failure(calc::Error::DivideByZero.code())
    }
    fn deliver(p: &mut conformance::Probe) -> Result<(), conformance::Fail> {
        let (kept, sent) = Channel::create().map_err(|e| e.name().to_string())?;
        let mut e = Encoder::new();
        sent.encode(&mut e).map_err(|e| format!("{e:?}"))?;
        b"hi"
            .to_vec()
            .encode(&mut e)
            .map_err(|e| format!("{e:?}"))?;
        p.request(3, e)?.reply()?;
        let mut buf = [0u8; 4];
        match kept.read(&mut buf, &mut []) {
            Ok((2, 0)) if &buf[..2] == b"hi" => Ok(()),
            other => Err(format!("the delivered text: {other:?}")),
        }
    }
    fn deliver_wrong_handle(p: &mut conformance::Probe) -> Result<(), conformance::Fail> {
        let mut e = Encoder::new();
        e.put_handle(Handle::inert())
            .map_err(|e| format!("{e:?}"))?;
        b"hi"
            .to_vec()
            .encode(&mut e)
            .map_err(|e| format!("{e:?}"))?;
        p.request(3, e)?.status(Status::BadMessage)
    }
    fn count(p: &mut conformance::Probe) -> Result<(), conformance::Fail> {
        let mut e = Encoder::new();
        2u32.encode(&mut e).map_err(|e| format!("{e:?}"))?;
        p.request(4, e)?.reply()?;
        for want in [1u16, 1, 2] {
            let event = p.next_event()?;
            if event.ordinal != want {
                return Err(format!("event {} where {want} was due", event.ordinal));
            }
        }
        Ok(())
    }
    fn echo_at_bound(p: &mut conformance::Probe) -> Result<(), conformance::Fail> {
        let mut e = Encoder::new();
        vec![1u8; conformance::BUDGET - 4]
            .encode(&mut e)
            .map_err(|e| format!("{e:?}"))?;
        let reply = p.request(6, e)?.reply()?;
        if reply.body_len() == conformance::BUDGET {
            Ok(())
        } else {
            Err(format!("a reply of {} bytes", reply.body_len()))
        }
    }
    fn version(p: &mut conformance::Probe) -> Result<(), conformance::Fail> {
        p.request(5, Encoder::new())?.reply().map(drop)
    }
    const CASES: &[conformance::Case] = &[
        conformance::Case {
            name: "add: 2 + 3 is 5",
            check: add,
        },
        conformance::Case {
            name: "div: by zero is DivideByZero",
            check: div_by_zero,
        },
        conformance::Case {
            name: "deliver: the text arrives on the channel sent with it",
            check: deliver,
        },
        conformance::Case {
            name: "deliver: a handle that is not a channel is a bad message",
            check: deliver_wrong_handle,
        },
        conformance::Case {
            name: "count: the events, then the reply",
            check: count,
        },
        conformance::Case {
            name: "echo: a message at the bound and its reply",
            check: echo_at_bound,
        },
        conformance::Case {
            name: "version: answered",
            check: version,
        },
    ];
    conformance::Suite {
        id: calc::ID,
        requests: calc::REQUESTS,
        cases: CASES,
        timeout: 5_000_000_000,
    }
}

/// Each check's name and result, in the order they ran.
type Checks = Vec<(String, Result<(), String>)>;

fn run_suite(listener: Channel) -> (conformance::Summary, Checks) {
    let mut checks = Vec::new();
    let mut connector = conformance::Listener(listener);
    let summary = conformance::run(&calc_suite(), &mut connector, &mut |c| {
        checks.push((c.name.to_string(), c.result.clone()));
    });
    (summary, checks)
}

#[test]
fn sv_t7_the_reference_server_passes_its_suite() {
    let (listener, join) = calc_server();
    let (summary, checks) = run_suite(listener);
    for (name, result) in &checks {
        assert!(result.is_ok(), "{name}: {result:?}");
    }
    assert!(summary.ok(), "{summary:?}");
    assert!(summary.passed >= 20, "{summary:?}");
    join.join().expect("server").expect("run");
}

/// A server that answers everything with an empty reply: no unknown methods, no bad
/// messages.
struct Lax;

impl Handler for Lax {
    fn protocol(&self) -> ProtocolId {
        calc::ID
    }
    fn handle(&mut self, _: &mut Context<'_>, _: u16, _: &mut Decoder<'_>) -> Outcome {
        Outcome::Reply(Encoder::new())
    }
}

/// A server that keeps every handle it is sent.
struct Hoarder(Vec<Handle>);

impl Handler for Hoarder {
    fn protocol(&self) -> ProtocolId {
        calc::ID
    }
    fn handle(&mut self, _: &mut Context<'_>, ordinal: u16, d: &mut Decoder<'_>) -> Outcome {
        while d.remaining() > 0 {
            if let Ok(h) = d.take_handle() {
                self.0.push(h);
            }
        }
        if calc::REQUESTS.contains(&ordinal) {
            Outcome::Status(Status::BadMessage)
        } else {
            Outcome::Status(Status::UnknownMethod)
        }
    }
}

fn broken(make: fn() -> Box<dyn Handler>) -> Vec<String> {
    let (listener, theirs) = Channel::create().expect("channel");
    let join = thread::spawn(move || {
        let mut d = Dispatcher::new();
        d.listen(theirs, Box::new(make))?;
        d.run()
    });
    let (summary, checks) = run_suite(listener);
    join.join().expect("server").expect("run");
    assert!(!summary.ok());
    checks
        .into_iter()
        .filter(|(_, r)| r.is_err())
        .map(|(n, _)| n)
        .collect()
}

#[test]
fn sv_t7_a_lax_server_fails_the_checks_of_unknown_and_malformed_requests() {
    let failed = broken(|| Box::new(Lax));
    assert!(
        failed.iter().any(|n| n.starts_with("an unknown request")),
        "{failed:?}"
    );
    assert!(
        failed.iter().any(|n| n.contains("decodes as nothing")),
        "{failed:?}"
    );
    assert!(failed.iter().any(|n| n.starts_with("add:")), "{failed:?}");
    assert!(
        !failed.iter().any(|n| n.starts_with("handshake")),
        "{failed:?}"
    );
}

#[test]
fn sv_t7_a_server_that_keeps_handles_fails_the_handle_check() {
    let failed = broken(|| Box::new(Hoarder(Vec::new())));
    assert!(
        failed.iter().any(|n| n.starts_with("handle transfer")),
        "{failed:?}"
    );
}

#[test]
fn sv_t7_a_server_of_another_protocol_fails_the_handshake() {
    let (listener, theirs) = Channel::create().expect("channel");
    let join = thread::spawn(move || {
        let mut d = Dispatcher::new();
        d.listen(theirs, ping::factory(|| Ping))?;
        d.run()
    });
    let (summary, checks) = run_suite(listener);
    join.join().expect("server").expect("run");
    assert!(!summary.ok());
    assert!(
        checks
            .iter()
            .any(|(n, r)| n.starts_with("handshake: welcome") && r.is_err())
    );
    assert_eq!(
        conformance::call_error(CallError::<u32>::TimedOut),
        "TimedOut"
    );
}

#[test]
fn sv_t7_a_server_that_never_answers_times_out() {
    let (listener, theirs) = Channel::create().expect("channel");
    let suite = conformance::Suite {
        timeout: 20_000_000,
        ..calc_suite()
    };
    let mut connector = conformance::Listener(listener);
    let mut failures = 0;
    let summary = conformance::run(&suite, &mut connector, &mut |c| {
        failures += u32::from(c.result.is_err());
    });
    assert_eq!(summary.passed, 0);
    assert_eq!(summary.failed, failures);
    drop(theirs);
}
