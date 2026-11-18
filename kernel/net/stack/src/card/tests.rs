//! The card link against a card's end of the channel driven by hand (work item 1.3c): an
//! answer that comes while a pump runs is never left filed with the channel empty.

extern crate std;

use alloc::{rc::Rc, string::String, vec, vec::Vec};
use std::cell::RefCell;

use moruna_svc::{
    CallError, Channel, ClientCore, Encoder, Kind, ProtocolError, Wire, deadline_in, frame,
};
use net_protocol::{Frame, Link, SEND_FRAMES_MAX, net};

use super::{Answers, CardLink, Taken};

const SECOND: u64 = 1_000_000_000;

/// An answer the card sends: its kind, request and body.
type Answer = (Kind, u32, Encoder);

/// A client core whose card sends `late` just after a read finds the channel empty: an
/// answer that comes while the link is taking the others.
struct Late {
    core: ClientCore,
    card: Rc<Channel>,
    late: Rc<RefCell<Vec<Answer>>>,
}

impl Answers for Late {
    fn send(&mut self, ordinal: u16, body: Encoder) -> Result<u32, CallError<u32>> {
        self.core.send(ordinal, body)
    }
    fn file_one(&mut self) -> Result<(), CallError<u32>> {
        let read = self.core.file(0);
        if matches!(read, Err(CallError::TimedOut)) {
            for (kind, txid, body) in self.late.borrow_mut().drain(..) {
                frame::send(&self.card, kind, 0, txid, body).expect("the late answer");
            }
        }
        read
    }
    fn take(&mut self, txid: u32) -> Option<Result<frame::Frame, CallError<u32>>> {
        self.core.take_reply(txid)
    }
}

struct Card {
    end: Rc<Channel>,
    late: Rc<RefCell<Vec<Answer>>>,
}

impl Card {
    /// The next request the link sent: its ordinal, id and body.
    fn request(&self) -> (u16, u32, frame::Frame) {
        let f = frame::recv(&self.end).expect("read").expect("a request");
        assert_eq!(f.kind, Kind::Request);
        (f.ordinal, f.txid, f)
    }

    fn answer(&self, kind: Kind, txid: u32, body: Encoder) {
        frame::send(&self.end, kind, 0, txid, body).expect("answer");
    }

    fn nothing_asked(&self) -> bool {
        frame::recv(&self.end).expect("read").is_none()
    }
}

fn body<T: Wire>(value: T) -> Encoder {
    let mut e = Encoder::new();
    value.encode(&mut e).expect("encode");
    e
}

fn frames(n: usize) -> Vec<Frame> {
    (0..n).map(|k| Frame(vec![k as u8; 60])).collect()
}

/// A link over a connection whose card's end the test holds, its receive (id `r`) and
/// link wait (id `l`) asked.
fn open() -> (CardLink<Late>, Card, u32, u32) {
    let (mine, theirs) = Channel::create().expect("channel");
    let mut welcome = Encoder::new();
    net::ID.major.encode(&mut welcome).expect("major");
    0u16.encode(&mut welcome).expect("minor");
    frame::send(&theirs, Kind::Welcome, 0, 0, welcome).expect("welcome");
    let core = ClientCore::connect(mine, net::ID, deadline_in(5 * SECOND)).expect("connect");
    let card = Card {
        end: Rc::new(theirs),
        late: Rc::new(RefCell::new(Vec::new())),
    };
    let hello = frame::recv(&card.end).expect("read").expect("hello");
    assert_eq!(hello.kind, Kind::Hello);
    let late = Late {
        core,
        card: card.end.clone(),
        late: card.late.clone(),
    };
    let mut link = CardLink::new(String::from("net0"), late, 1, 64);
    link.ask().expect("ask");
    let (ordinal, r, mut f) = card.request();
    assert_eq!(ordinal, 6);
    assert_eq!(u32::decode(&mut f.decoder()).expect("max"), 64);
    let (ordinal, l, mut f) = card.request();
    assert_eq!(ordinal, 3);
    assert_eq!(u32::decode(&mut f.decoder()).expect("generation"), 1);
    (link, card, r, l)
}

/// Whether the link's connection has something to read: the server waits on it, so it
/// is woken for it.
fn readable(link: &CardLink<Late>) -> bool {
    link.core.core.channel().needed().expect("needed").is_some()
}

#[test]
fn a_receive_answered_while_the_others_are_taken_is_never_left_behind() {
    let (mut link, card, r, _) = open();
    // A send in flight, answered before the pump.
    let mut tx = vec![vec![7u8; 60]];
    link.flush(&mut tx, 4).expect("flush");
    let (ordinal, s, _) = card.request();
    assert_eq!(ordinal, 4);
    card.answer(Kind::Reply, s, body(1u32));
    // The receive's frames come while the pump runs, once it has read what was there.
    card.late
        .borrow_mut()
        .push((Kind::Reply, r, body(frames(3))));
    let first = link.pump().expect("pump");
    assert_eq!(link.sends(), 0, "the send is taken");
    // Either the frames were taken, or the connection is still readable, so the server
    // is woken for them. Read while another answer was looked for, they would sit filed
    // with the channel empty, and the stack would never have them.
    assert!(
        !first.frames.is_empty() || readable(&link),
        "the receive's answer was read and left filed: nothing wakes the server for it"
    );
    let second = link.pump().expect("pump");
    let got: Vec<Vec<u8>> = first.frames.into_iter().chain(second.frames).collect();
    assert_eq!(got, frames(3).into_iter().map(|f| f.0).collect::<Vec<_>>());
    // Taken, the receive is asked for again.
    let (ordinal, r2, _) = card.request();
    assert_eq!(ordinal, 6);
    assert_ne!(r2, r);
    assert!(card.nothing_asked());
}

#[test]
fn every_answer_there_is_taken_in_one_pump_whatever_their_order() {
    let (mut link, card, r, l) = open();
    let mut tx = vec![vec![1u8; 60]; SEND_FRAMES_MAX + 1];
    link.flush(&mut tx, 4).expect("flush");
    let (_, s1, _) = card.request();
    let (_, s2, _) = card.request();
    assert!(tx.is_empty());
    // The sends' answers first, the receive's last, the link's between.
    card.answer(Kind::Reply, s2, body(1u32));
    card.answer(
        Kind::Reply,
        l,
        body(Link {
            up: false,
            speed_mbps: 0,
            generation: 2,
        }),
    );
    card.answer(Kind::Reply, s1, body(SEND_FRAMES_MAX as u32));
    card.answer(Kind::Reply, r, body(frames(2)));
    let taken = link.pump().expect("pump");
    assert_eq!(
        taken,
        Taken {
            frames: frames(2).into_iter().map(|f| f.0).collect(),
            link: Some(false),
        }
    );
    assert_eq!(link.sends(), 0);
    assert!(!readable(&link));
    // The receive and the link's next generation, asked again.
    let (ordinal, _, _) = card.request();
    assert_eq!(ordinal, 6);
    let (ordinal, _, mut f) = card.request();
    assert_eq!(ordinal, 3);
    assert_eq!(u32::decode(&mut f.decoder()).expect("generation"), 2);
    // Nothing more there: a pump takes nothing and asks nothing.
    assert_eq!(link.pump().expect("pump"), Taken::default());
    assert!(card.nothing_asked());
}

#[test]
fn sends_wait_for_room_and_a_frame_too_large_for_a_message_stays() {
    let (mut link, card, _, _) = open();
    let mut tx = vec![vec![2u8; 60]; 2 * SEND_FRAMES_MAX + 3];
    link.flush(&mut tx, 1).expect("flush");
    assert_eq!(link.sends(), 1);
    assert_eq!(tx.len(), SEND_FRAMES_MAX + 3);
    let (_, s, _) = card.request();
    assert!(card.nothing_asked());
    card.answer(Kind::Reply, s, body(SEND_FRAMES_MAX as u32));
    link.pump().expect("pump");
    link.flush(&mut tx, 2).expect("flush");
    assert_eq!(link.sends(), 2);
    assert!(tx.is_empty());
    let mut big = vec![vec![0u8; moruna_svc::limits::CHANNEL_MAX_BYTES]];
    link.flush(&mut big, 8).expect("flush");
    assert_eq!(big.len(), 1, "a frame no message holds is not sent");
}

#[test]
fn a_card_that_answers_wrongly_or_goes_fails_the_link() {
    // A send answered with another count.
    let (mut link, card, _, _) = open();
    link.flush(&mut vec![vec![3u8; 60]], 4).expect("flush");
    let (_, s, _) = card.request();
    card.answer(Kind::Reply, s, body(2u32));
    let e = link.pump().expect_err("a wrong count");
    assert_eq!(e, "a send of 1 frames was answered 2");

    // A receive refused.
    let (mut link, card, r, _) = open();
    card.answer(Kind::Failure, r, body(net::Error::Io.code()));
    assert!(link.pump().expect_err("refused").starts_with("receive: "));

    // A receive whose frames do not decode.
    let (mut link, card, r, _) = open();
    card.answer(Kind::Reply, r, body(7u8));
    assert!(
        link.pump()
            .expect_err("malformed")
            .starts_with("a receive's frames: ")
    );

    // A link wait whose answer does not decode.
    let (mut link, card, _, l) = open();
    card.answer(Kind::Reply, l, body(7u8));
    assert!(link.pump().expect_err("malformed").starts_with("a link: "));

    // A send answer that does not decode.
    let (mut link, card, _, _) = open();
    link.flush(&mut vec![vec![3u8; 60]], 4).expect("flush");
    let (_, s, _) = card.request();
    card.answer(Kind::Reply, s, body(7u8));
    assert!(
        link.pump()
            .expect_err("malformed")
            .starts_with("send answer: ")
    );

    // The card's end gone.
    let (mut link, card, _, _) = open();
    link.core.card = Rc::new(Channel::create().expect("channel").0);
    drop(card);
    assert!(
        link.pump()
            .expect_err("gone")
            .starts_with("the card's answers: ")
    );
    // Nothing more can be asked of it.
    let mut tx = vec![vec![3u8; 60]];
    assert!(
        link.flush(&mut tx, 4)
            .expect_err("gone")
            .starts_with("send: ")
    );
}
