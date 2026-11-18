//! Host tests of the `netvsc` process (work item 1.11b): its driver against
//! `moruna-netvsc`'s fake netvsp behind `moruna-vmbus`'s in-memory channel, with clients
//! that reach it through `moruna.driver.net` on the SDK's model of channels.
//!
//! - `serve`: what it serves: the card's address, MTU and link, frames out and in, sends
//!   that wait for the host, the link's changes, what the host sends wrong, and a channel
//!   the host takes back.
//! - `suites`: `net-protocol`'s conformance suite against one card, and its pair suite
//!   against two cards whose hosts are joined, each frame out of one in at the other.
//!
//! The world is the process's loop on the host: each turn the host's queued packets go on
//! the ring (on a node the host writes them and `vmbus` wakes the driver), the driver
//! services the card, its answers go, and the dispatcher turns once. The host is not
//! `Send`, so it lives on the test's thread with the dispatcher, and a client's thread
//! changes it through [`Commands`], each change made before the client goes on.

#[allow(dead_code)]
#[path = "../../../core/tests/netvsc/fake.rs"]
mod fake;

mod serve;
mod suites;

use std::{
    cell::RefCell,
    rc::Rc,
    sync::mpsc::{Receiver, Sender, channel},
    thread::JoinHandle,
};

use fake::{Bytes, DEADLINE, Host, Netvsp, PAGE, Settings, parts};
use moruna_netvsc::Netvsc;
use moruna_netvsc_server::{Driver, factory};
use moruna_svc::{Channel, Dispatcher, deadline_in};
use moruna_vmbus::testing::{self, TestChannel};

/// The driver under test, shared by its connections.
pub type Served = Rc<RefCell<Driver<TestChannel<Netvsp>, Bytes>>>;

/// A change a client's thread asks of a card's host, made on the test's thread with the
/// host's side of the card's rings; and who waits for it to be made.
pub type Command = (
    Box<dyn FnOnce(&mut Host, &mut testing::Host) + Send>,
    Sender<()>,
);

/// Where a client's thread sends its commands for each card's host.
#[derive(Clone)]
pub struct Commands(Vec<Sender<Command>>);

impl Commands {
    /// Has card `card`'s host do `change` at the world's next turn, and waits until it
    /// has.
    pub fn to(
        &self,
        card: usize,
        change: impl FnOnce(&mut Host, &mut testing::Host) + Send + 'static,
    ) {
        let (done, made) = channel();
        self.0[card]
            .send((Box::new(change), done))
            .expect("the world");
        made.recv().expect("the world");
    }
}

/// One card: its driver, its host, the commands for it, and how many of the frames its
/// host took have gone to the other card's host.
struct Card {
    driver: Served,
    host: Rc<RefCell<Host>>,
    commands: Receiver<Command>,
    bridged: usize,
}

/// A started card over the fake host, with rings of `to_host` and `to_guest` bytes and
/// buffers of `settings`, after `prepare` has set the host up.
pub fn start(
    to_host: usize,
    to_guest: usize,
    settings: Settings,
    prepare: impl FnOnce(&mut Host),
) -> (Netvsc<TestChannel<Netvsp>, Bytes>, Rc<RefCell<Host>>) {
    let mut parts = parts(to_host, to_guest, settings);
    prepare(&mut parts.host.borrow_mut());
    parts.host.borrow_mut().flush(parts.channel.host());
    let card = Netvsc::start(parts.channel, parts.receive, parts.send, Some(DEADLINE))
        .unwrap_or_else(|e| panic!("the card did not start: {e}"));
    (card, parts.host)
}

/// The process's loop on the host, over one card or two whose hosts are joined.
pub struct World {
    cards: Vec<Card>,
    dispatcher: Dispatcher,
    /// Whether a frame one card's host takes goes in at the other's.
    joined: bool,
}

impl World {
    /// One card, with rings of 16 pages, after `prepare`.
    pub fn new(settings: Settings, prepare: impl FnOnce(&mut Host)) -> (World, Commands) {
        World::with_rings(16 * PAGE, 16 * PAGE, settings, prepare)
    }

    /// One card, with rings of these many bytes, after `prepare`.
    pub fn with_rings(
        to_host: usize,
        to_guest: usize,
        settings: Settings,
        prepare: impl FnOnce(&mut Host),
    ) -> (World, Commands) {
        let mut world = World {
            cards: Vec::new(),
            dispatcher: Dispatcher::new(),
            joined: false,
        };
        let sender = world.add(start(to_host, to_guest, settings, prepare));
        (world, Commands(vec![sender]))
    }

    /// Two cards on one link: each frame out of one goes in at the other.
    pub fn pair(settings: Settings) -> (World, Commands) {
        let (mut world, mut commands) = World::new(settings, |_| ());
        let sender = world.add(start(16 * PAGE, 16 * PAGE, settings, |_| ()));
        commands.0.push(sender);
        world.joined = true;
        (world, commands)
    }

    fn add(
        &mut self,
        (card, host): (Netvsc<TestChannel<Netvsp>, Bytes>, Rc<RefCell<Host>>),
    ) -> Sender<Command> {
        let (sender, commands) = channel();
        self.cards.push(Card {
            driver: Rc::new(RefCell::new(Driver::new(card))),
            host,
            commands,
            bridged: 0,
        });
        sender
    }

    /// Card `card`'s driver.
    pub fn driver(&self, card: usize) -> &Served {
        &self.cards[card].driver
    }

    /// Card `card`'s host.
    pub fn host(&self, card: usize) -> &Rc<RefCell<Host>> {
        &self.cards[card].host
    }

    /// The client's end of a new listener of card `card`'s.
    pub fn listener(&mut self, card: usize) -> Channel {
        let (ours, theirs) = Channel::create().unwrap();
        self.dispatcher
            .listen(ours, factory(self.cards[card].driver.clone()))
            .unwrap();
        theirs
    }

    /// One turn of the process's loop.
    pub fn turn(&mut self) {
        for card in &self.cards {
            let mut driver = card.driver.borrow_mut();
            while let Ok((command, done)) = card.commands.try_recv() {
                command(&mut card.host.borrow_mut(), driver.card().channel().host());
                let _ = done.send(());
            }
            card.host.borrow_mut().flush(driver.card().channel().host());
            driver.service();
            for (pending, outcome) in driver.take_ready() {
                let _ = self.dispatcher.complete(pending, outcome);
            }
        }
        if self.joined {
            for (from, to) in [(0, 1), (1, 0)] {
                let frames: Vec<Vec<u8>> =
                    self.cards[from].host.borrow().frames[self.cards[from].bridged..].to_vec();
                self.cards[from].bridged += frames.len();
                let mut host = self.cards[to].host.borrow_mut();
                for frame in &frames {
                    host.inject(frame);
                }
            }
        }
        self.dispatcher.turn(deadline_in(1_000_000)).unwrap();
    }

    /// Turns until `client`'s thread ends, then a few more, for its connections to go.
    pub fn serve<T>(&mut self, client: JoinHandle<T>) -> T {
        while !client.is_finished() {
            self.turn();
        }
        for _ in 0..8 {
            self.turn();
        }
        client.join().expect("the client's thread")
    }
}
