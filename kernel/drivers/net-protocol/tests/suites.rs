//! NT-T5 (SDD 29, k): the protocol's suites pass against a model of two cards on one
//! link served by `net_protocol::Server`, the server every driver runs, by one
//! dispatcher on the host's model of the channel ABI; and fail against cards that
//! break the protocol: one that sends what it should refuse, one that loses frames.

use std::{cell::RefCell, rc::Rc, thread};

use moruna_svc::{
    Channel, Context, Decoder, Dispatcher, Handler, Outcome, ProtocolId, Wire, conformance,
};
use net_protocol::{Frame, Info, Ring, Server, net};

#[derive(Clone, Copy, PartialEq)]
enum Flaw {
    None,
    /// Answers every send at once, refusing nothing and sending nothing.
    Lax,
    /// Takes every frame and sends none.
    Lossy,
}

struct Card {
    server: Server,
    flaw: Flaw,
}

/// A card's ring: what it puts arrives at its peer, unless it is lossy.
struct Link<'a> {
    peer: &'a RefCell<Card>,
    lossy: bool,
}

impl Ring for Link<'_> {
    fn put(&mut self, frame: &[u8]) -> Result<bool, String> {
        if !self.lossy {
            self.peer.borrow_mut().server.hub.deliver(frame);
        }
        Ok(true)
    }
    fn flush(&mut self) -> Result<(), String> {
        if !self.lossy {
            self.peer.borrow_mut().server.delivered();
        }
        Ok(())
    }
}

struct Conn {
    me: Rc<RefCell<Card>>,
    peer: Rc<RefCell<Card>>,
    id: Option<u64>,
}

impl Handler for Conn {
    fn protocol(&self) -> ProtocolId {
        net::ID
    }

    fn handle(&mut self, cx: &mut Context<'_>, ordinal: u16, d: &mut Decoder<'_>) -> Outcome {
        self.id = Some(cx.connection());
        let flaw = self.me.borrow().flaw;
        if flaw == Flaw::Lax && ordinal == 4 {
            return match Vec::<Frame>::decode(d) {
                Ok(frames) => Outcome::reply(frames.len() as u32),
                Err(_) => Outcome::Status(moruna_svc::Status::BadMessage),
            };
        }
        let outcome = {
            let mut me = self.me.borrow_mut();
            let mut link = Link {
                peer: &self.peer,
                lossy: flaw == Flaw::Lossy,
            };
            me.server.handle(&mut link, cx, ordinal, d)
        };
        for card in [&self.me, &self.peer] {
            for (pending, answer) in std::mem::take(&mut card.borrow_mut().server.ready) {
                cx.complete(pending, answer);
            }
        }
        outcome
    }

    fn closed(&mut self) {
        if let Some(id) = self.id {
            self.me.borrow_mut().server.closed(id);
        }
    }
}

/// Two cards joined by a link, `listeners` listeners each, on a thread of their own.
fn cards(flaw: Flaw, listeners: usize) -> (Vec<Channel>, Vec<Channel>, thread::JoinHandle<()>) {
    let (mut ours_a, mut ours_b, mut theirs_a, mut theirs_b) = (vec![], vec![], vec![], vec![]);
    for _ in 0..listeners {
        let (o, t) = Channel::create().expect("channel");
        ours_a.push(o);
        theirs_a.push(t);
        let (o, t) = Channel::create().expect("channel");
        ours_b.push(o);
        theirs_b.push(t);
    }
    let join = thread::spawn(move || {
        let card = |mac: u64| {
            Rc::new(RefCell::new(Card {
                server: Server::new(
                    Info {
                        mac,
                        mtu: 1500,
                        max_frame: 1514,
                        offloads: 0,
                        queues: 1,
                    },
                    true,
                    0,
                ),
                flaw,
            }))
        };
        let (a, b) = (card(0x5254_0000_0001), card(0x5254_0000_0002));
        let mut d = Dispatcher::new();
        for (listener, (me, peer)) in theirs_a
            .into_iter()
            .map(|l| (l, (a.clone(), b.clone())))
            .chain(theirs_b.into_iter().map(|l| (l, (b.clone(), a.clone()))))
        {
            d.listen(
                listener,
                Box::new(move || {
                    Box::new(Conn {
                        me: me.clone(),
                        peer: peer.clone(),
                        id: None,
                    }) as Box<dyn Handler>
                }),
            )
            .expect("listen");
        }
        d.run().expect("serve");
    });
    (ours_a, ours_b, join)
}

fn failures(summary: &conformance::Summary, names: &[(String, Result<(), String>)]) -> Vec<String> {
    let failed: Vec<String> = names
        .iter()
        .filter(|(_, r)| r.is_err())
        .map(|(n, r)| format!("{n}: {r:?}"))
        .collect();
    assert_eq!(failed.len(), summary.failed as usize);
    failed
}

#[test]
fn nt_t5_the_suites_pass_against_two_joined_cards() {
    let (mut a, mut b, join) = cards(Flaw::None, 2);
    for listener in [a.remove(0), b.remove(0)] {
        let mut checks = Vec::new();
        let summary = conformance::run(
            &net_protocol::suite(),
            &mut conformance::Listener(listener),
            &mut |c| checks.push((c.name.to_string(), c.result.clone())),
        );
        assert!(summary.ok(), "{:?}", failures(&summary, &checks));
        assert!(summary.passed >= 16, "{summary:?}");
    }
    let mut checks = Vec::new();
    let summary = conformance::run_pair(
        &net_protocol::pair_suite(),
        &mut conformance::Listener(a.remove(0)),
        &mut conformance::Listener(b.remove(0)),
        &mut |c| checks.push((c.name.to_string(), c.result.clone())),
    );
    assert!(summary.ok(), "{:?}", failures(&summary, &checks));
    assert_eq!(summary.passed, 3);
    join.join().expect("cards");
}

#[test]
fn nt_t5_a_card_that_refuses_nothing_or_loses_frames_fails() {
    let (mut a, b, join) = cards(Flaw::Lax, 1);
    let mut checks = Vec::new();
    let summary = conformance::run(
        &net_protocol::suite(),
        &mut conformance::Listener(a.remove(0)),
        &mut |c| checks.push((c.name.to_string(), c.result.clone())),
    );
    let failed = failures(&summary, &checks);
    // It refuses nothing, and counts nothing it sent.
    assert_eq!(failed.len(), 2, "{failed:?}");
    assert!(
        failed.iter().any(|f| f.contains("refused whole")),
        "{failed:?}"
    );
    assert!(failed.iter().any(|f| f.contains("counted")), "{failed:?}");
    drop(b);
    join.join().expect("cards");

    let (mut a, mut b, join) = cards(Flaw::Lossy, 1);
    // A receive the card never answers ends its case at the suite's timeout.
    let mut short = net_protocol::pair_suite();
    short.timeout = 300_000_000;
    let mut checks = Vec::new();
    let summary = conformance::run_pair(
        &short,
        &mut conformance::Listener(a.remove(0)),
        &mut conformance::Listener(b.remove(0)),
        &mut |c| checks.push((c.name.to_string(), c.result.clone())),
    );
    assert_eq!(summary.passed, 0);
    assert_eq!(failures(&summary, &checks).len(), 3);
    join.join().expect("cards");
}
