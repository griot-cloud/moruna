//! `ClientCore::file_all` (work item 1.3c): the channel read to its end without waiting,
//! every answer filed for `take_reply`, and an answer that comes after left on the channel,
//! so whoever waits on it is woken for it.

use moruna_svc::{
    CallError, Channel, ClientCore, Encoder, Kind, ProtocolId, Wire, deadline_in, frame,
};

const TEST: ProtocolId = ProtocolId::new("moruna.test.file-all", 1, 0);

fn body(value: u32) -> Encoder {
    let mut e = Encoder::new();
    value.encode(&mut e).expect("encode");
    e
}

/// A client whose server's end the test holds.
fn connected() -> (ClientCore, Channel) {
    let (mine, theirs) = Channel::create().expect("channel");
    let mut welcome = Encoder::new();
    TEST.major.encode(&mut welcome).expect("major");
    TEST.minor.encode(&mut welcome).expect("minor");
    frame::send(&theirs, Kind::Welcome, 0, 0, welcome).expect("welcome");
    let core = ClientCore::connect(mine, TEST, deadline_in(5_000_000_000)).expect("connect");
    frame::recv(&theirs).expect("read").expect("hello");
    (core, theirs)
}

fn value(core: &mut ClientCore, txid: u32) -> Option<u32> {
    core.take_reply(txid)
        .map(|f| u32::decode(&mut f.expect("an answer").decoder()).expect("a value"))
}

fn readable(core: &ClientCore) -> bool {
    core.channel().needed().expect("needed").is_some()
}

#[test]
fn every_answer_there_is_filed_and_one_that_comes_after_keeps_the_channel_readable() {
    let (mut core, server) = connected();
    let ids: Vec<u32> = (0..3)
        .map(|_| core.send(1, Encoder::new()).expect("send"))
        .collect();
    // Nothing yet: nothing filed, nothing to take.
    assert_eq!(core.file_all(), Ok(0));
    assert_eq!(value(&mut core, ids[0]), None);
    // The last two answered, out of order, and an event between them.
    frame::send(&server, Kind::Reply, 1, ids[2], body(30)).expect("answer");
    frame::send(&server, Kind::Event, 9, 0, Encoder::new()).expect("event");
    frame::send(&server, Kind::Reply, 1, ids[1], body(20)).expect("answer");
    assert_eq!(core.file_all(), Ok(3));
    assert!(!readable(&core), "the channel is read to its end");
    assert_eq!(value(&mut core, ids[0]), None);
    assert_eq!(value(&mut core, ids[1]), Some(20));
    assert_eq!(value(&mut core, ids[2]), Some(30));
    assert_eq!(core.next_event(0).expect("the event").ordinal, 9);
    // The first answer comes after the channel was read: it waits on the channel, which
    // is readable, for the next turn's read.
    frame::send(&server, Kind::Reply, 1, ids[0], body(10)).expect("answer");
    assert!(readable(&core));
    assert_eq!(value(&mut core, ids[0]), None);
    assert_eq!(core.file_all(), Ok(1));
    assert_eq!(value(&mut core, ids[0]), Some(10));
}

#[test]
fn a_server_that_has_gone_or_answers_wrongly_is_said() {
    let (mut core, server) = connected();
    core.send(1, Encoder::new()).expect("send");
    // An answer to nothing asked is malformed.
    frame::send(&server, Kind::Reply, 1, 77, body(1)).expect("answer");
    assert_eq!(core.file_all(), Err(CallError::Malformed));
    drop(server);
    assert_eq!(core.file_all(), Err(CallError::PeerClosed));
}
