//! SV-T1 (the wire format), SV-T2 (frames) and SV-T8 (the host model of the channel
//! ABI keeps the kernel's rules), SDD 23 k.

use moruna_svc::{
    Channel, DEADLINE_INFINITE, Decoder, Encoder, Error, Handle, Hello, Kind, ObjectType,
    ProtocolId, Refusal, Refused, Signals, Transport, WaitItem, Wire, WireError, frame, limits,
    now, protocol, wait, wait_one,
};

fn round_trip<T: Wire + PartialEq + std::fmt::Debug + Clone>(value: T) {
    let mut e = Encoder::new();
    value.clone().encode(&mut e).expect("encode");
    let (mine, theirs) = Channel::create().expect("channel");
    frame::send(&mine, Kind::Event, 3, 0, e).expect("send");
    let mut f = frame::recv(&theirs).expect("recv").expect("a frame");
    assert_eq!((f.kind, f.ordinal, f.txid), (Kind::Event, 3, 0));
    let mut d = f.decoder();
    assert_eq!(T::decode(&mut d), Ok(value));
    assert_eq!(d.finish(), Ok(()));
}

fn decode<T: Wire>(bytes: &[u8]) -> Result<T, WireError> {
    let mut handles: [Option<Handle>; 0] = [];
    let mut d = Decoder::new(bytes, &mut handles);
    let v = T::decode(&mut d)?;
    d.finish()?;
    Ok(v)
}

#[test]
fn sv_t1_every_type_round_trips() {
    round_trip(7u8);
    round_trip(0xbeefu16);
    round_trip(u32::MAX);
    round_trip(u64::MAX - 1);
    round_trip(-5i8);
    round_trip(-500i16);
    round_trip(i32::MIN);
    round_trip(i64::MIN);
    round_trip(true);
    round_trip(false);
    round_trip(());
    round_trip(String::from("griot ü"));
    round_trip(vec![1u8, 2, 3]);
    round_trip(vec![vec![1u16], vec![]]);
    round_trip(Some(9u64));
    round_trip(None::<u64>);
    round_trip((1u8,));
    round_trip((1u8, 2u16));
    round_trip((1u8, 2u16, 3u32, String::from("four")));
    round_trip(Hello {
        name: String::from("a.b"),
        major: 1,
        minor: 2,
    });
    round_trip(Refused {
        reason: Refusal::MajorMismatch,
        server: ProtocolId::new("x", 3, 4).hello(),
    });
}

#[test]
fn sv_t1_the_decoder_refuses_what_is_not_exactly_one_value() {
    assert_eq!(decode::<u32>(&[1, 2, 3]), Err(WireError::Truncated));
    assert_eq!(decode::<u8>(&[1, 2]), Err(WireError::Trailing));
    assert_eq!(decode::<bool>(&[2]), Err(WireError::Invalid));
    assert_eq!(decode::<Option<u8>>(&[2, 0]), Err(WireError::Invalid));
    assert_eq!(
        decode::<String>(&[2, 0, 0, 0, 0xff, 0xfe]),
        Err(WireError::Invalid)
    );
    assert_eq!(
        decode::<Vec<u8>>(&[0xff, 0xff, 0xff, 0xff]),
        Err(WireError::Invalid)
    );
    assert_eq!(
        decode::<Vec<u8>>(&[4, 0, 0, 0, 1]),
        Err(WireError::Truncated)
    );
    assert_eq!(decode::<Handle>(&[0]).map(drop), Err(WireError::Handle));
    assert_eq!(decode::<Channel>(&[0]).map(drop), Err(WireError::Handle));
    assert_eq!(
        decode::<Hello>(&[1, b'A', 1, 0, 1, 0]),
        Err(WireError::Invalid)
    );
    assert_eq!(
        decode::<Hello>(&[1, 0xff, 1, 0, 1, 0]),
        Err(WireError::Invalid)
    );
    assert_eq!(decode::<Refused>(&[9, 0, 0, 0]), Err(WireError::Invalid));
    // A handle the body names once is taken once; a wrong type is refused.
    let (a, _b) = Channel::create().expect("channel");
    let mut handles = [Some(a.0), Some(Handle::inert())];
    let mut d = Decoder::new(&[0, 0, 1], &mut handles);
    assert!(Channel::decode(&mut d).is_ok());
    assert_eq!(Handle::decode(&mut d).map(drop), Err(WireError::Handle));
    assert_eq!(Channel::decode(&mut d).map(drop), Err(WireError::Handle));
    assert_eq!(d.finish(), Ok(()));
    // A handle left untaken is trailing.
    let mut handles = [Some(Handle::inert())];
    let d = Decoder::new(&[], &mut handles);
    assert_eq!(d.finish(), Err(WireError::Trailing));
}

#[test]
fn sv_t1_the_encoder_refuses_what_no_message_holds() {
    let mut e = Encoder::new();
    assert_eq!(
        e.put_len(limits::CHANNEL_MAX_BYTES + 1),
        Err(WireError::TooLarge)
    );
    for _ in 0..limits::CHANNEL_MAX_HANDLES {
        e.put_handle(Handle::inert()).expect("a handle");
    }
    assert_eq!(e.handle_count(), limits::CHANNEL_MAX_HANDLES);
    assert_eq!(e.put_handle(Handle::inert()), Err(WireError::TooLarge));
    assert_eq!(e.body_len(), limits::CHANNEL_MAX_HANDLES);
    let bad = Hello {
        name: String::from("Bad Name"),
        major: 1,
        minor: 0,
    };
    assert_eq!(bad.encode(&mut Encoder::default()), Err(WireError::Invalid));
    assert_eq!(Some(1u8).encode(&mut Encoder::new()), Ok(()));
}

#[test]
fn sv_t1_protocol_names_and_schema_checks() {
    assert!(protocol::valid_name("moruna.example.echo"));
    assert!(protocol::valid_name(&"a".repeat(64)));
    assert!(!protocol::valid_name(&"a".repeat(65)));
    assert!(!protocol::valid_name(""));
    assert!(!protocol::valid_name("Echo"));
    assert!(protocol::ordinals_valid(&[1, 2, 3]));
    assert!(!protocol::ordinals_valid(&[1, 0]));
    assert!(!protocol::ordinals_valid(&[2, 1, 2]));
    assert!(protocol::codes_valid(&[1, 65535]));
    assert!(!protocol::codes_valid(&[0]));
    assert!(!protocol::codes_valid(&[65536]));
    assert!(!protocol::codes_valid(&[4, 4]));
    assert!(protocol::since_valid(1, 1));
    assert!(!protocol::since_valid(2, 1));
    let id = ProtocolId::new("p", 1, 1);
    let mut hello = id.hello();
    assert_eq!(id.accepts(&hello), Ok(()));
    hello.minor = 9;
    assert_eq!(id.accepts(&hello), Ok(()));
    hello.major = 2;
    assert_eq!(id.accepts(&hello), Err(Refusal::MajorMismatch));
    hello.name = String::from("q");
    assert_eq!(id.accepts(&hello), Err(Refusal::UnknownProtocol));
}

#[test]
fn sv_t2_frames_over_the_bound_are_refused_and_bad_frames_are_malformed() {
    let (a, b) = Channel::create().expect("channel");
    let mut e = Encoder::new();
    e.put_raw(&vec![0u8; limits::CHANNEL_MAX_BYTES - frame::HEADER]);
    frame::send(&a, Kind::Request, 1, 1, e).expect("a frame at the bound");
    let mut e = Encoder::new();
    e.put_raw(&vec![0u8; limits::CHANNEL_MAX_BYTES - frame::HEADER + 1]);
    assert_eq!(
        frame::send(&a, Kind::Request, 1, 2, e),
        Err(Transport::TooLarge)
    );
    let f = frame::recv(&b).expect("recv").expect("frame");
    assert_eq!(f.body_len(), limits::CHANNEL_MAX_BYTES - frame::HEADER);
    assert!(frame::recv(&b).expect("recv").is_none());
    for bad in [
        &[1u8, 0, 0][..],
        &[1, 1, 0, 0, 0, 0, 0, 0][..],
        &[99, 0, 0, 0, 0, 0, 0, 0][..],
        &[][..],
    ] {
        a.write_with(bad, &mut []).expect("write");
        assert_eq!(
            frame::recv(&b).map(|f| f.map(|f| f.kind)),
            Err(Transport::Malformed)
        );
    }
    drop(a);
    assert_eq!(frame::recv(&b).map(|_| ()), Err(Transport::PeerClosed));
    assert_eq!(Transport::from(Error::ShouldWait), Transport::Full);
    assert_eq!(Transport::from(Error::Limit), Transport::TooLarge);
    assert_eq!(
        Transport::from(Error::Fault),
        Transport::Kernel(Error::Fault)
    );
}

#[test]
fn sv_t8_the_model_keeps_the_kernels_channel_rules() {
    let (a, b) = Channel::create().expect("channel");
    assert_eq!(a.0.info().map(|(t, _)| t), Ok(ObjectType::Channel));
    assert_eq!(
        Handle::inert().info().map(|(t, _)| t),
        Ok(ObjectType::Memory)
    );
    // Bounds.
    assert_eq!(
        a.write_with(&vec![0; limits::CHANNEL_MAX_BYTES + 1], &mut []),
        Err(Error::Limit)
    );
    let mut many: Vec<Option<Handle>> = (0..=limits::CHANNEL_MAX_HANDLES)
        .map(|_| Some(Handle::inert()))
        .collect();
    assert_eq!(a.write_with(&[], &mut many), Err(Error::Limit));
    assert_eq!(a.write_with(&[], &mut [None]), Err(Error::InvalidArgs));
    // Depth, and WRITABLE with it.
    for i in 0..limits::CHANNEL_DEPTH {
        a.write_with(&[i as u8], &mut []).expect("room");
    }
    assert_eq!(a.write_with(&[0], &mut []), Err(Error::ShouldWait));
    assert_eq!(wait_one(&a.0, Signals::WRITABLE, 0), Err(Error::TimedOut));
    // A whole message or nothing.
    assert_eq!(b.needed(), Ok(Some((1, 0))));
    assert_eq!(b.read(&mut [], &mut []), Err(Error::BufferTooSmall));
    let mut one = [0u8; 1];
    assert_eq!(b.read(&mut one, &mut []), Ok((1, 0)));
    assert!(wait_one(&a.0, Signals::WRITABLE, 0).is_ok());
    // Handles move; a channel's own endpoint never travels through it.
    let (c, d) = Channel::create().expect("channel");
    let mut own = [Some(b.0)];
    assert_eq!(a.write_with(&[], &mut own), Err(Error::InvalidArgs));
    let b = Channel(own[0].take().expect("kept"));
    assert!(b.0.info().is_ok());
    let mut moved = [Some(c.0)];
    let (e, f) = Channel::create().expect("channel");
    e.write_with(b"c", &mut moved).expect("moved");
    assert!(moved[0].is_none());
    let mut got = [None];
    let mut byte = [0u8; 1];
    assert_eq!(f.read(&mut byte, &mut got), Ok((1, 1)));
    // The cycle rule: d's queue carries an endpoint, so d may not travel.
    let (g, h) = Channel::create().expect("channel");
    let c = Channel(got[0].take().expect("c"));
    c.write_with(&[], &mut [Some(h.0)])
        .expect("h into d's queue");
    let mut dd = [Some(d.0)];
    assert_eq!(e.write_with(&[], &mut dd), Err(Error::BadState));
    let d = Channel(dd[0].take().expect("d"));
    let mut back = [None];
    assert_eq!(d.read(&mut [], &mut back), Ok((0, 1)));
    e.write_with(&[], &mut [Some(d.0)])
        .expect("drained, d travels");
    // Closing f drops d, queued to it, which c sees.
    drop(f);
    assert!(wait_one(&c.0, Signals::PEER_CLOSED, now() + 1_000_000_000).is_ok());
    assert_eq!(e.write_with(&[], &mut []), Err(Error::PeerClosed));
    drop((back, g));
    // An empty message is consumed by `needed`.
    let (x, y) = Channel::create().expect("channel");
    x.write_with(&[], &mut []).expect("empty");
    assert_eq!(y.needed(), Ok(Some((0, 0))));
    assert_eq!(y.needed(), Ok(None));
    assert_eq!(y.read(&mut [], &mut []), Err(Error::ShouldWait));
    drop(x);
    assert_eq!(y.needed(), Err(Error::PeerClosed));
    assert_eq!(y.read(&mut [], &mut []), Err(Error::PeerClosed));
    // The inert object is no channel.
    let inert = Channel(Handle::inert());
    assert_eq!(inert.write_with(&[], &mut []), Err(Error::WrongType));
    assert_eq!(inert.read(&mut [], &mut []), Err(Error::WrongType));
    assert_eq!(inert.needed(), Err(Error::WrongType));
    assert_eq!(format!("{:?}", inert.0), "Handle(inert)");
    assert_eq!(format!("{:?}", y.0), "Handle(endpoint 1)");
}

#[test]
fn sv_t8_wait_takes_many_items_and_honours_its_deadline() {
    let (a, b) = Channel::create().expect("channel");
    let (c, d) = Channel::create().expect("channel");
    assert_eq!(wait(&mut [], DEADLINE_INFINITE), Err(Error::InvalidArgs));
    let start = now();
    let mut items = [
        WaitItem::new(&b.0, Signals::READABLE),
        WaitItem::new(&d.0, Signals::READABLE),
    ];
    assert_eq!(wait(&mut items, start + 20_000_000), Err(Error::TimedOut));
    assert!(now() >= start + 20_000_000);
    let writer = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(10));
        c.write_with(b"x", &mut []).expect("write");
        (a, c)
    });
    let mut items = [
        WaitItem::new(&b.0, Signals::READABLE),
        WaitItem::new(&d.0, Signals::READABLE),
    ];
    assert_eq!(wait(&mut items, DEADLINE_INFINITE), Ok(1));
    assert!(!items[0].observed().contains(Signals::READABLE));
    assert!(items[1].observed().contains(Signals::READABLE));
    let (a, c) = writer.join().expect("writer");
    drop((a, c));
    assert!(wait_one(&b.0, Signals::PEER_CLOSED, 0).is_ok());
}
