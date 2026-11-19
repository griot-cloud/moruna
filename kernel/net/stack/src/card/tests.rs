//! The card link against a card's end of the channel driven by hand (work item 1.3c): an
//! answer that comes while a pump runs is never left filed with the channel empty. And
//! against the protocol's own hub (work item 6.3af): receives enough to take a full queue
//! stay outstanding on a card of 1.1, so a burst leaves nothing queued and drops nothing;
//! one on a card of 1.0, as before.

extern crate std;

use alloc::{rc::Rc, string::String, vec, vec::Vec};
use std::cell::RefCell;

use moruna_svc::{
    CallError, Channel, ClientCore, Encoder, Kind, ProtocolError, Wire, deadline_in, frame,
};
use net_protocol::{Answer as Answered, Frame, Hub, Link, QUEUE_FRAMES, SEND_FRAMES_MAX, net};

use super::{Answers, CardLink, Taken};

const SECOND: u64 = 1_000_000_000;

/// An answer the card sends: its kind, request and body.
type Answer = (Kind, u32, Encoder);

/// A client core whose card sends `late` just after the channel was read to its end: an
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
    fn file_all(&mut self) -> Result<usize, CallError<u32>> {
        let read = self.core.file_all();
        for (kind, txid, body) in self.late.borrow_mut().drain(..) {
            frame::send(&self.card, kind, 0, txid, body).expect("the late answer");
        }
        read
    }
    fn take(&mut self, txid: u32) -> Option<Result<frame::Frame, CallError<u32>>> {
        self.core.take_reply(txid)
    }
    fn minor(&self) -> u16 {
        self.core.server_minor()
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

/// A link over a connection whose card's end the test holds, the card of 1.0, its one
/// receive (id `r`) and link wait (id `l`) asked.
fn open() -> (CardLink<Late>, Card, u32, u32) {
    let (link, card, mut asked) = open_at(0);
    assert_eq!(asked.len(), 1, "a card of 1.0 has one receive asked");
    let l = asked_link(&card);
    (link, card, asked.remove(0), l)
}

/// A link over a connection whose card's end the test holds, the card welcoming it at
/// minor `minor`: the receives it asked (their ids), its link wait not yet read.
fn open_at(minor: u16) -> (CardLink<Late>, Card, Vec<u32>) {
    let (mine, theirs) = Channel::create().expect("channel");
    let mut welcome = Encoder::new();
    net::ID.major.encode(&mut welcome).expect("major");
    minor.encode(&mut welcome).expect("minor");
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
    let mut link = CardLink::new(String::from("net0"), late, 1, 64, MAX_FRAME);
    link.ask().expect("ask");
    let mut asked = Vec::new();
    for _ in 0..link.receives() {
        let (ordinal, r, mut f) = card.request();
        assert_eq!(ordinal, 6);
        assert_eq!(u32::decode(&mut f.decoder()).expect("max"), 64);
        asked.push(r);
    }
    (link, card, asked)
}

/// The link's wait for the link, which it asks after its receives: its id.
fn asked_link(card: &Card) -> u32 {
    let (ordinal, l, mut f) = card.request();
    assert_eq!(ordinal, 3);
    assert_eq!(u32::decode(&mut f.decoder()).expect("generation"), 1);
    l
}

#[test]
fn a_card_of_1_1_has_a_full_queue_of_receives_asked_and_a_card_of_1_0_one() {
    // MTU 1500, 64 frames a receive: 43 of the largest an answer, 1024 a queue, 24.
    let (link, card, asked) = open_at(1);
    assert_eq!(asked.len(), 24);
    assert_eq!(link.receives(), 24);
    asked_link(&card);
    assert!(card.nothing_asked());
    let (link, card, asked) = open_at(0);
    assert_eq!((asked.len(), link.receives()), (1, 1));
    asked_link(&card);
    assert!(card.nothing_asked());
}

#[test]
fn receives_answered_out_of_order_are_taken_and_each_asked_again() {
    let (mut link, card, asked) = open_at(1);
    asked_link(&card);
    // The second answered before the first: both taken, each asked again.
    card.answer(Kind::Reply, asked[1], body(frames(1)));
    let taken = link.pump().expect("pump");
    assert_eq!(taken.frames.len(), 1);
    let (ordinal, _, _) = card.request();
    assert_eq!(ordinal, 6);
    assert!(card.nothing_asked());
    card.answer(Kind::Reply, asked[0], body(frames(2)));
    assert_eq!(link.pump().expect("pump").frames.len(), 2);
    let (ordinal, _, _) = card.request();
    assert_eq!(ordinal, 6);
    assert!(card.nothing_asked());
    assert_eq!(link.receives(), 24);
    assert_eq!((link.counts.answers, link.counts.frames_in), (2, 3));
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
    // What moved, counted (work item 6.3ab): one answer of two frames in, two sends out.
    assert_eq!(
        link.counts,
        crate::meter::CardCounts {
            answers: 1,
            frames_in: 2,
            bytes_in: 120,
            sends: 2,
            frames_out: SEND_FRAMES_MAX as u64 + 1,
            bytes_out: 60 * (SEND_FRAMES_MAX as u64 + 1),
        }
    );
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

/// A card whose receive side is the protocol's own hub, serving the link's requests over a
/// channel the test drives (work item 6.3af): every receive the link asks goes to the hub,
/// and the hub's answers go back on the channel.
struct HubCard {
    end: Channel,
    hub: Hub<u32>,
}

impl HubCard {
    /// Answers every request the link has sent: a receive goes to the hub (answered now,
    /// left waiting or refused), a wait for the link is left waiting.
    fn serve(&mut self) {
        while let Some(mut f) = frame::recv(&self.end).expect("read") {
            if f.ordinal != 6 {
                continue;
            }
            let max = u32::decode(&mut f.decoder()).expect("max");
            match self.hub.receive(1, max, || f.txid) {
                Ok(Answered::Now(frames)) => self.reply(f.txid, frames),
                Ok(Answered::Later) => {}
                Err(e) => frame::send(&self.end, Kind::Failure, 0, f.txid, body(e.code()))
                    .expect("refusal"),
            }
        }
    }

    /// `frames` arrive at the card in one turn of its driver: each is queued, then the
    /// receives waiting are answered.
    fn arrive(&mut self, frames: &[Vec<u8>]) {
        for f in frames {
            self.hub.deliver(f);
        }
        let mut answers = Vec::new();
        self.hub.answers(&mut answers);
        for (txid, frames) in answers {
            self.reply(txid, frames);
        }
    }

    fn reply(&self, txid: u32, frames: Vec<Frame>) {
        frame::send(&self.end, Kind::Reply, 0, txid, body(frames)).expect("reply");
    }
}

/// A link of the network server's default batch to a card of MTU 1500 whose receive side
/// is a hub, its card welcoming the link at protocol minor `minor`.
fn hub_link(minor: u16) -> (CardLink<ClientCore>, HubCard) {
    let (mine, theirs) = Channel::create().expect("channel");
    let mut welcome = Encoder::new();
    net::ID.major.encode(&mut welcome).expect("major");
    minor.encode(&mut welcome).expect("minor");
    frame::send(&theirs, Kind::Welcome, 0, 0, welcome).expect("welcome");
    let core = ClientCore::connect(mine, net::ID, deadline_in(5 * SECOND)).expect("connect");
    let hello = frame::recv(&theirs).expect("read").expect("hello");
    assert_eq!(hello.kind, Kind::Hello);
    // The card's hub: a card of 1.0 serves a client as 1.0 whatever the client says.
    let mut hub = Hub::new(true, 0, MAX_FRAME);
    hub.subscribe(1, minor);
    let batch = crate::config::Settings::default().card_receive_frames as u32;
    let mut link = CardLink::new(String::from("net0"), core, 1, batch, MAX_FRAME);
    link.ask().expect("ask");
    let mut card = HubCard { end: theirs, hub };
    card.serve();
    (link, card)
}

/// `n` frames of the largest size a card of MTU 1500 delivers, numbered from `first`.
fn full_frames(first: u32, n: usize) -> Vec<Vec<u8>> {
    (0..n as u32)
        .map(|k| {
            let mut f = vec![0u8; MAX_FRAME as usize];
            f[..4].copy_from_slice(&(first + k).to_be_bytes());
            f
        })
        .collect()
}

/// The largest frame of a card of MTU 1500.
const MAX_FRAME: u32 = 1514;

/// Two bursts of a full queue of the largest frames, far more than one answer carries
/// (43), at a card of minor `minor`, the network server taking what the card answered
/// after each: answers that left frames queued, frames dropped, and the frames taken.
fn two_bursts(minor: u16) -> (u64, u64, Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let (mut link, mut card) = hub_link(minor);
    let first = full_frames(0, QUEUE_FRAMES);
    let second = full_frames(QUEUE_FRAMES as u32, QUEUE_FRAMES);
    let mut got = Vec::new();
    card.arrive(&first);
    got.extend(link.pump().expect("pump").frames);
    card.serve();
    card.arrive(&second);
    got.extend(link.pump().expect("pump").frames);
    card.serve();
    let want: Vec<Vec<u8>> = first.into_iter().chain(second).collect();
    (card.hub.backlog, card.hub.rx_dropped, got, want)
}

#[test]
fn a_burst_larger_than_one_answer_leaves_nothing_queued_and_drops_nothing() {
    // Work item 6.3af. With one receive outstanding the hub answered 43 frames and queued
    // the rest behind them, and the second burst found the queue nearly full and was
    // dropped: (3, 938, 86). With a full queue's worth outstanding, nothing waits.
    let (backlog, dropped, got, want) = two_bursts(1);
    assert_eq!(
        (backlog, dropped, got.len()),
        (0, 0, want.len()),
        "answers that left frames queued, frames dropped, frames taken"
    );
    assert!(
        got == want,
        "the frames taken are the frames sent, in order"
    );
}

#[test]
fn on_a_card_of_1_0_one_receive_is_outstanding_as_before() {
    // A client of 1.1 and a card of 1.0: one receive, and the burst backlogs and drops
    // exactly as it did before 1.1.
    let (backlog, dropped, got, want) = two_bursts(0);
    assert_eq!((backlog, dropped, got.len()), (3, 938, 86));
    assert!(got[..43] == want[..43], "the frames taken come in order");
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
