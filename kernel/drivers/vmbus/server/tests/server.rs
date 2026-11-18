//! Host tests of the `vmbus` process (work item 1.9c): the server over a fake of Hyper-V's
//! side, the strict fake host of `moruna-vmbus`'s own tests, with drivers that reach it
//! through `moruna.driver.vmbus` on the SDK's model of channels.

#[allow(dead_code)]
#[path = "../../core/tests/vmbus/host.rs"]
mod host;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;
use std::thread::{self, JoinHandle};

use host::{FakeHost, NIC, SCSI, connection_of, instance};
use moruna_svc::{
    CallError, Channel, ClientCore, Dispatcher, Encoder, Handle, Memory, ObjectType, Status, Wire,
    deadline_in,
};
use moruna_svc::{Error as SysError, limits};
use moruna_vmbus::{ChannelId, GpadlId, Hv, HvError, HvMessage, MemoryId, PostError, Version};
use moruna_vmbus_server::{Host, Vmbus, factory, message, page_numbers, post_result};
use vmbus_protocol::{Class, Id, Offer, Opened, vmbus};

/// The node under the process: Hyper-V's side is the fake host; the rest is recorded.
struct Node {
    hv: FakeHost,
    console: Vec<String>,
    /// Pinned memory, by ID: how many pages.
    pinned: BTreeMap<u64, usize>,
    released: Vec<u64>,
    signals: Vec<u32>,
    next: u64,
    fail_rings: Option<vmbus::Error>,
    fail_signal: Option<vmbus::Error>,
}

impl Node {
    fn new(hv: FakeHost) -> Node {
        Node {
            hv,
            console: Vec::new(),
            pinned: BTreeMap::new(),
            released: Vec::new(),
            signals: Vec::new(),
            next: 0,
            fail_rings: None,
            fail_signal: None,
        }
    }

    fn id(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    /// Memory of `pages` pages, its page numbers far from every other's.
    fn memory(&mut self, pages: usize) -> MemoryId {
        let id = self.id();
        self.pinned.insert(id, pages);
        self.hv.add_memory(id, id * 0x1_0000, pages)
    }

    fn said(&self, text: &str) -> bool {
        self.console.iter().any(|line| line.contains(text))
    }
}

impl Hv for Node {
    fn post_message(&mut self, connection: u32, typ: u32, payload: &[u8]) -> Result<(), PostError> {
        self.hv.post_message(connection, typ, payload)
    }

    fn signal_event(&mut self, connection: u32, flag: u16) {
        self.hv.signal_event(connection, flag);
    }

    fn take_message(&mut self) -> Option<HvMessage> {
        self.hv.take_message()
    }

    fn test_and_clear_event(&mut self, flag: u16) -> bool {
        self.hv.test_and_clear_event(flag)
    }

    fn page_numbers(&mut self, memory: MemoryId) -> Option<Vec<u64>> {
        self.hv.page_numbers(memory)
    }
}

impl Host for Node {
    fn rings(&mut self, pages: u32) -> Result<(Handle, MemoryId), vmbus::Error> {
        if let Some(error) = self.fail_rings.take() {
            return Err(error);
        }
        let handle = Memory::create(u64::from(pages) * 4096).unwrap().0;
        Ok((handle, self.memory(pages as usize)))
    }

    fn pin(&mut self, memory: Handle) -> Result<MemoryId, vmbus::Error> {
        match memory.info() {
            Ok((ObjectType::Memory, _)) => Ok(self.memory(2)),
            _ => Err(vmbus::Error::BadMemory),
        }
    }

    fn release(&mut self, memory: MemoryId) {
        assert!(
            self.pinned.remove(&memory.0).is_some(),
            "memory released once"
        );
        self.hv.memory.remove(&memory.0);
        self.released.push(memory.0);
    }

    fn signal(&mut self, connection_id: u32) -> Result<Handle, vmbus::Error> {
        if let Some(error) = self.fail_signal.take() {
            return Err(error);
        }
        self.signals.push(connection_id);
        Ok(Handle::inert())
    }

    fn say(&mut self, line: fmt::Arguments<'_>) {
        self.console.push(line.to_string());
    }
}

type Shared = Rc<RefCell<Vmbus<Node>>>;

/// The process over the node `hv` is part of, targeting the VP the host expects.
fn on(hv: FakeHost) -> Vmbus<Node> {
    let vp = hv.target_vp;
    Vmbus::new(Node::new(hv), vp)
}

/// The process over a host that offers a network card and a disk, as channels 1 and 2,
/// with contact made.
fn connected() -> Shared {
    let mut hv = FakeHost::new();
    hv.devices = vec![(NIC, instance(1)), (SCSI, instance(2))];
    let vmbus = Rc::new(RefCell::new(on(hv)));
    vmbus.borrow_mut().start();
    vmbus
}

/// A disk driver's connection to the process, through the `scsi` listener: its end, and
/// the dispatcher serving the other.
fn driver(vmbus: &Shared, d: &mut Dispatcher) -> Channel {
    driver_of(vmbus, d, Class::Scsi)
}

/// A connection through the listener of `class`.
fn driver_of(vmbus: &Shared, d: &mut Dispatcher, class: Class) -> Channel {
    let (mine, theirs) = Channel::create().unwrap();
    d.serve(theirs, factory(vmbus.clone(), class)()).unwrap();
    mine
}

/// The empty messages waiting on a wake channel, read, and then how it stands: `ShouldWait`
/// while `vmbus` holds the other end, `PeerClosed` once it has let go.
fn wakes(wake: &Channel) -> (usize, SysError) {
    let mut count = 0;
    loop {
        match wake.read(&mut [], &mut []) {
            Ok((0, 0)) => count += 1,
            Ok(other) => panic!("a wake message that is not empty: {other:?}"),
            Err(error) => return (count, error),
        }
    }
}

/// Answers delivered by the process outside a request.
fn flush(vmbus: &Shared, d: &mut Dispatcher) {
    loop {
        let next = vmbus.borrow_mut().finished();
        let Some((pending, outcome)) = next else {
            return;
        };
        let _ = d.complete(pending, outcome);
    }
}

/// Serves until the driver's thread ends, doing `between` after each turn; then until
/// its connection is gone.
fn serve<T>(
    vmbus: &Shared,
    d: &mut Dispatcher,
    client: JoinHandle<T>,
    mut between: impl FnMut(&mut Vmbus<Node>),
) -> T {
    while !client.is_finished() {
        d.turn(deadline_in(5_000_000)).unwrap();
        between(&mut vmbus.borrow_mut());
        flush(vmbus, d);
    }
    for _ in 0..4 {
        d.turn(deadline_in(5_000_000)).unwrap();
        flush(vmbus, d);
    }
    client.join().expect("the driver's thread")
}

fn client(channel: Channel) -> vmbus::Client {
    vmbus::Client::connect(channel, deadline_in(10_000_000_000)).expect("hello")
}

/// The page numbers the host holds for a GPADL.
fn ring_pages(node: &Node, gpadl: GpadlId) -> Vec<u64> {
    node.hv.gpadl_pages(gpadl).expect("an accepted GPADL")
}

#[test]
fn contact_is_made_at_start_and_every_offer_is_said_on_the_console() {
    let vmbus = connected();
    let vmbus = vmbus.borrow();
    let console = &vmbus.host().console;
    assert!(console[0].starts_with(&format!(
        "vmbus: offer {NIC} instance {} channel 1 subchannel 0",
        instance(1)
    )));
    assert_eq!(
        console[1],
        format!(
            "vmbus: offer {SCSI} instance {} channel 2 subchannel 0",
            instance(2)
        )
    );
    assert_eq!(
        console[2],
        "vmbus: connected, version Copper, CPU 0 is VP 0"
    );
    assert_eq!(console.len(), 3);
    assert_eq!(vmbus.fatal(), None);
    assert_eq!((vmbus.pending(), vmbus.channels()), (0, 0));
}

#[test]
fn the_contact_and_a_channel_target_the_vp_the_core_gives() {
    // CPU 0 as VP 5: the strict host refuses a contact or an open that names another.
    let mut hv = FakeHost::new();
    hv.target_vp = 5;
    hv.devices = vec![(SCSI, instance(1))];
    let vmbus = Rc::new(RefCell::new(on(hv)));
    vmbus.borrow_mut().start();
    assert!(
        vmbus
            .borrow()
            .host()
            .said("vmbus: connected, version Copper, CPU 0 is VP 5")
    );
    let mut d = Dispatcher::new();
    let end = driver(&vmbus, &mut d);
    let end = serve(
        &vmbus,
        &mut d,
        thread::spawn(move || {
            let mut c = client(end);
            c.open(1, 8, 4).unwrap();
            c.into_core().into_channel()
        }),
        |_| {},
    );
    assert!(vmbus.borrow().host().hv.is_open(ChannelId(1)));
    drop(end);
}

#[test]
fn a_driver_lists_the_offers_of_its_class_only() {
    let vmbus = connected();
    let mut d = Dispatcher::new();
    let disks = driver(&vmbus, &mut d);
    let nics = driver_of(&vmbus, &mut d, Class::Net);
    let lists = serve(
        &vmbus,
        &mut d,
        thread::spawn(move || {
            let mut c = client(disks);
            let mut n = client(nics);
            [
                c.offers(Id(SCSI)),
                c.offers(Id(NIC)),
                c.offers(Id(instance(9))),
                n.offers(Id(NIC)),
                n.offers(Id(SCSI)),
            ]
            .map(|answer| answer.unwrap())
        }),
        |_| {},
    );
    let disk = Offer {
        channel: 2,
        instance: Id(instance(2)),
        subchannel: 0,
    };
    let nic = Offer {
        channel: 1,
        instance: Id(instance(1)),
        subchannel: 0,
    };
    assert_eq!(lists, [vec![disk], vec![], vec![], vec![nic], vec![]]);
}

#[test]
fn offers_asked_before_contact_wait_for_it() {
    let mut hv = FakeHost::new();
    hv.devices = vec![(SCSI, instance(2))];
    // The host is busy: the request for contact waits in the client.
    hv.busy = 1;
    let vmbus = Rc::new(RefCell::new(on(hv)));
    vmbus.borrow_mut().start();
    assert!(vmbus.borrow().has_unsent());
    let mut d = Dispatcher::new();
    let end = driver(&vmbus, &mut d);
    let nics = driver_of(&vmbus, &mut d, Class::Net);
    let mut retried = false;
    let lists = serve(
        &vmbus,
        &mut d,
        thread::spawn(move || {
            let disks = thread::spawn(move || client(end).offers(Id(SCSI)));
            let other = client(nics).offers(Id(SCSI));
            (disks.join().unwrap(), other)
        }),
        |vmbus| {
            if vmbus.pending() == 2 && !retried {
                retried = true;
                vmbus.retry();
            }
        },
    );
    assert!(retried, "the answers waited for contact");
    assert_eq!(lists.0.unwrap().len(), 1);
    assert_eq!(
        lists.1.unwrap(),
        [],
        "not the class of the connection that asked"
    );
    assert!(!vmbus.borrow().has_unsent());
}

#[test]
fn opening_a_channel_gives_its_rings_signal_and_wake_channel() {
    let vmbus = connected();
    let mut d = Dispatcher::new();
    let end = driver(&vmbus, &mut d);
    let (opened, end) = serve(
        &vmbus,
        &mut d,
        thread::spawn(move || {
            let mut c = client(end);
            let opened = c.open(2, 8, 4).unwrap();
            (opened, c.into_core().into_channel())
        }),
        |_| {},
    );
    let Opened {
        rings,
        signal,
        wake,
    } = opened;
    assert_eq!(rings.info().unwrap().0, ObjectType::Memory);
    assert!(signal.info().is_ok());
    assert_eq!(wake.info().unwrap().0, ObjectType::Channel);
    // Nothing has come from the host yet.
    assert_eq!(wakes(&Channel(wake)), (0, SysError::ShouldWait));
    let vmbus = vmbus.borrow();
    let node = vmbus.host();
    assert!(node.hv.is_open(ChannelId(2)));
    // The rings are the first memory pinned: eight pages, shared whole as the GPADL.
    let gpadl = node.hv.ring_gpadl(ChannelId(2)).unwrap();
    assert_eq!(
        ring_pages(node, gpadl),
        (0x1_0000..0x1_0008).collect::<Vec<_>>()
    );
    // The signal handle signals the connection ID of channel 2's offer.
    assert_eq!(node.signals, [connection_of(ChannelId(2))]);
    assert_eq!(vmbus.channels(), 1);
    assert_eq!(vmbus.pending(), 0);
    drop(end);
}

#[test]
fn opens_the_process_cannot_make_are_refused_with_their_reason() {
    let vmbus = connected();
    let mut d = Dispatcher::new();
    let end = driver(&vmbus, &mut d);
    let answers = serve(
        &vmbus,
        &mut d,
        thread::spawn(move || {
            let mut c = client(end);
            let unknown = c.open(99, 8, 4).map(|_| ());
            let network = c.open(1, 8, 4).map(|_| ());
            let small = c.open(2, 3, 2).map(|_| ());
            let lopsided = c.open(2, 8, 7).map(|_| ());
            let first = c.open(2, 8, 4).map(|_| ());
            let second = c.open(2, 8, 4).map(|_| ());
            [unknown, network, small, lopsided, first, second]
        }),
        |_| {},
    );
    assert_eq!(
        answers,
        [
            Err(CallError::Remote(vmbus::Error::NoSuchChannel)),
            Err(CallError::Remote(vmbus::Error::NoSuchChannel)),
            Err(CallError::Remote(vmbus::Error::BadRings)),
            Err(CallError::Remote(vmbus::Error::BadRings)),
            Ok(()),
            Err(CallError::Remote(vmbus::Error::Busy)),
        ]
    );
}

#[test]
fn what_the_node_cannot_give_is_handed_back() {
    let vmbus = connected();
    let mut d = Dispatcher::new();
    let end = driver(&vmbus, &mut d);
    {
        // An open asks the node for rings, then a signal: each fails once, in that order,
        // one open each.
        let mut v = vmbus.borrow_mut();
        let node = v.host_mut();
        node.fail_rings = Some(vmbus::Error::NoResources);
        node.fail_signal = Some(vmbus::Error::NoResources);
    }
    let answers = serve(
        &vmbus,
        &mut d,
        thread::spawn(move || {
            let mut c = client(end);
            let answers = [(); 3].map(|()| c.open(2, 8, 4).map(|_| ()));
            (answers, c.into_core().into_channel())
        }),
        |_| {},
    );
    let refused = || Err(CallError::Remote(vmbus::Error::NoResources));
    assert_eq!(answers.0, [refused(), refused(), Ok(())]);
    // The rings of the one refused after its rings were made were released.
    let vmbus = vmbus.borrow();
    assert_eq!(vmbus.host().released.len(), 1);
    assert_eq!(vmbus.host().pinned.len(), 1);
    assert_eq!(vmbus.channels(), 1);
    drop(answers.1);
}

#[test]
fn an_open_the_host_refuses_is_refused_and_its_memory_released() {
    let vmbus = connected();
    vmbus.borrow_mut().host_mut().hv.fail_open = Some(5);
    let mut d = Dispatcher::new();
    let end = driver(&vmbus, &mut d);
    let answer = serve(
        &vmbus,
        &mut d,
        thread::spawn(move || client(end).open(2, 8, 4).map(|_| ())),
        |_| {},
    );
    assert_eq!(answer, Err(CallError::Remote(vmbus::Error::Refused)));
    let vmbus = vmbus.borrow();
    let node = vmbus.host();
    assert!(node.pinned.is_empty() && node.released.len() == 1);
    assert!(node.said("the host refused to open channel 2: status 0x5"));
    assert_eq!(vmbus.channels(), 0);
}

#[test]
fn a_driver_shares_memory_on_its_open_channel() {
    let vmbus = connected();
    let mut d = Dispatcher::new();
    let end = driver(&vmbus, &mut d);
    let other = driver(&vmbus, &mut d);
    let (shared, refused, end) = serve(
        &vmbus,
        &mut d,
        thread::spawn(move || {
            let mut c = client(end);
            let before = c.share(2, Memory::create(8192).unwrap().0);
            c.open(2, 8, 4).unwrap();
            let shared = c.share(2, Memory::create(8192).unwrap().0).unwrap();
            let not_memory = c.share(2, Channel::create().unwrap().0.0);
            let elsewhere = client(other).share(2, Memory::create(8192).unwrap().0);
            (
                shared,
                [before, not_memory, elsewhere],
                c.into_core().into_channel(),
            )
        }),
        |_| {},
    );
    assert_eq!(
        refused.map(|r| r.map(|_| ())),
        [
            Err(CallError::Remote(vmbus::Error::NotOpen)),
            Err(CallError::Remote(vmbus::Error::BadMemory)),
            Err(CallError::Remote(vmbus::Error::Busy)),
        ]
    );
    let vmbus = vmbus.borrow();
    let node = vmbus.host();
    // The second memory pinned, after the rings: two pages.
    let pages = node.hv.gpadl_pages(GpadlId(shared)).unwrap();
    assert_eq!(pages, [0x2_0000, 0x2_0001]);
    assert_eq!(node.hv.gpadl_count(), 2);
    drop(end);
}

#[test]
fn memory_the_host_refuses_is_refused_and_released() {
    let vmbus = connected();
    let mut d = Dispatcher::new();
    let end = driver(&vmbus, &mut d);
    let c = serve(
        &vmbus,
        &mut d,
        thread::spawn(move || {
            let mut c = client(end);
            c.open(2, 8, 4).unwrap();
            c
        }),
        |_| {},
    );
    // The next GPADL, the driver's memory, is refused.
    vmbus.borrow_mut().host_mut().hv.fail_gpadl = Some(-1);
    let (answer, c) = serve(
        &vmbus,
        &mut d,
        thread::spawn(move || {
            let mut c = c;
            (c.share(2, Memory::create(4096).unwrap().0), c)
        }),
        |_| {},
    );
    assert_eq!(answer, Err(CallError::Remote(vmbus::Error::Refused)));
    let vmbus = vmbus.borrow();
    assert_eq!(vmbus.host().released.len(), 1);
    assert!(vmbus.host().said("the host refused memory on channel 2"));
    assert_eq!(vmbus.channels(), 1);
    drop(c);
}

#[test]
fn closing_tears_down_the_shared_memory_and_lets_go_of_the_rings() {
    let vmbus = connected();
    let mut d = Dispatcher::new();
    let end = driver(&vmbus, &mut d);
    let answers = serve(
        &vmbus,
        &mut d,
        thread::spawn(move || {
            let mut c = client(end);
            let wake = Channel(c.open(2, 8, 4).unwrap().wake);
            c.share(2, Memory::create(8192).unwrap().0).unwrap();
            let closed = c.close(2);
            // Let go of before the answer: the driver hears of it with no message.
            let woken = wakes(&wake);
            let again = c.close(2);
            let never = c.close(1);
            (closed, woken, again, never)
        }),
        |_| {},
    );
    assert_eq!(
        answers,
        (
            Ok(()),
            (0, SysError::PeerClosed),
            Err(CallError::Remote(vmbus::Error::NotOpen)),
            Err(CallError::Remote(vmbus::Error::NotOpen))
        )
    );
    let vmbus = vmbus.borrow();
    let node = vmbus.host();
    assert!(!node.hv.is_open(ChannelId(2)));
    assert_eq!(node.hv.gpadl_count(), 0);
    assert!(node.pinned.is_empty());
    assert_eq!(node.released.len(), 2);
    assert_eq!((vmbus.channels(), vmbus.pending()), (0, 0));
}

#[test]
fn the_hosts_signal_wakes_the_driver_of_that_channel_only() {
    let vmbus = connected();
    let mut d = Dispatcher::new();
    let disks = driver(&vmbus, &mut d);
    let nics = driver_of(&vmbus, &mut d, Class::Net);
    let (wake, nic_wake, ends) = serve(
        &vmbus,
        &mut d,
        thread::spawn(move || {
            let mut c = client(disks);
            let mut n = client(nics);
            let wake = Channel(c.open(2, 8, 4).unwrap().wake);
            let nic_wake = Channel(n.open(1, 4, 2).unwrap().wake);
            (
                wake,
                nic_wake,
                [c.into_core().into_channel(), n.into_core().into_channel()],
            )
        }),
        |_| {},
    );
    let mut v = vmbus.borrow_mut();
    v.host_mut().hv.signal_guest(ChannelId(2));
    v.interrupted();
    v.host_mut().hv.signal_guest(ChannelId(2));
    v.interrupted();
    // An interrupt with no flag for the channel wakes nobody.
    v.interrupted();
    assert_eq!(wakes(&wake), (2, SysError::ShouldWait));
    assert_eq!(wakes(&nic_wake), (0, SysError::ShouldWait));
    // A driver that does not read its wake channel fills it, and the writes that find it
    // full are dropped: the message waiting already says the same.
    for _ in 0..limits::CHANNEL_DEPTH + 5 {
        v.host_mut().hv.signal_guest(ChannelId(2));
        v.interrupted();
    }
    assert_eq!(wakes(&wake), (limits::CHANNEL_DEPTH, SysError::ShouldWait));
    assert_eq!(v.fatal(), None);
    drop(ends);
}

#[test]
fn a_channel_that_is_not_open_is_not_the_processs_to_wake() {
    let vmbus = connected();
    let mut v = vmbus.borrow_mut();
    // Channel 1 is not open: its flag is not the process's to read.
    v.host_mut().hv.signal_guest(ChannelId(1));
    v.interrupted();
    assert!(v.host_mut().hv.test_and_clear(ChannelId(1)));
}

#[test]
fn a_driver_that_goes_has_its_channels_closed() {
    let vmbus = connected();
    let mut d = Dispatcher::new();
    let disks = driver(&vmbus, &mut d);
    let nics = driver_of(&vmbus, &mut d, Class::Net);
    serve(
        &vmbus,
        &mut d,
        thread::spawn(move || {
            let mut c = client(disks);
            let mut n = client(nics);
            c.open(2, 8, 4).unwrap();
            n.open(1, 4, 2).unwrap();
            c.share(2, Memory::create(4096).unwrap().0).unwrap();
        }),
        |_| {},
    );
    let vmbus = vmbus.borrow();
    let node = vmbus.host();
    assert!(!node.hv.is_open(ChannelId(1)) && !node.hv.is_open(ChannelId(2)));
    assert_eq!(node.hv.gpadl_count(), 0);
    assert!(node.pinned.is_empty());
    assert_eq!(vmbus.channels(), 0);
}

#[test]
fn a_channel_opening_when_its_driver_goes_is_closed_once_open() {
    let vmbus = connected();
    vmbus.borrow_mut().host_mut().hv.hold_open = true;
    let mut d = Dispatcher::new();
    let end = driver(&vmbus, &mut d);
    let mut opening = false;
    serve(
        &vmbus,
        &mut d,
        thread::spawn(move || {
            let mut core = client(end).into_core();
            let mut e = Encoder::new();
            (2u32, 8u32, 4u32).encode(&mut e).unwrap();
            let txid = core.send(2, e).unwrap();
            // Gone before the answer, which the host holds back.
            assert_eq!(
                core.reply(txid, deadline_in(200_000_000)).map(|_| ()),
                Err(CallError::TimedOut)
            );
        }),
        |vmbus| opening |= vmbus.pending() == 1,
    );
    assert!(opening);
    let mut v = vmbus.borrow_mut();
    assert_eq!((v.channels(), v.pending()), (1, 0));
    v.host_mut().hv.release();
    v.interrupted();
    let node = v.host();
    assert!(!node.hv.is_open(ChannelId(2)));
    assert!(node.pinned.is_empty() && node.released.len() == 1);
    assert_eq!(v.channels(), 0);
}

#[test]
fn a_rescinded_channel_is_gone_for_its_driver() {
    let vmbus = connected();
    let mut d = Dispatcher::new();
    let end = driver(&vmbus, &mut d);
    let end = serve(
        &vmbus,
        &mut d,
        thread::spawn(move || {
            let mut c = client(end);
            let wake = Channel(c.open(2, 8, 4).unwrap().wake);
            c.share(2, Memory::create(4096).unwrap().0).unwrap();
            (wake, c.into_core().into_channel())
        }),
        |_| {},
    );
    {
        let mut v = vmbus.borrow_mut();
        v.host_mut().hv.rescind(ChannelId(2));
        v.interrupted();
        let node = v.host();
        assert!(node.said("vmbus: channel 2 rescinded"));
        // The driver hears the device has gone: its wake channel is closed.
        assert_eq!(wakes(&end.0), (0, SysError::PeerClosed));
        assert!(node.pinned.is_empty(), "the rings and the shared memory");
        assert_eq!(node.hv.released, [2]);
        assert_eq!(v.channels(), 0);
    }
    drop(end);
    let end = driver(&vmbus, &mut d);
    let answers = serve(
        &vmbus,
        &mut d,
        thread::spawn(move || {
            let mut c = client(end);
            (
                c.offers(Id(SCSI)).unwrap(),
                c.close(2),
                c.open(2, 8, 4).map(|_| ()),
            )
        }),
        |_| {},
    );
    assert_eq!(
        answers,
        (
            vec![],
            Err(CallError::Remote(vmbus::Error::NotOpen)),
            Err(CallError::Remote(vmbus::Error::NoSuchChannel))
        )
    );
}

#[test]
fn a_post_hyper_v_refuses_is_fatal() {
    let mut hv = FakeHost::new();
    hv.refuse_post = Some(HvError::InvalidConnectionId);
    let vmbus = Rc::new(RefCell::new(on(hv)));
    vmbus.borrow_mut().start();
    let v = vmbus.borrow();
    let fatal = v.fatal().unwrap();
    assert!(fatal.to_string().starts_with("Hyper-V refused a post: "));
    assert!(v.host().said("vmbus: Hyper-V refused a post"));
}

#[test]
fn contact_the_host_refuses_is_fatal_and_answers_what_waited() {
    // No version the client offers.
    let mut hv = FakeHost::up_to(Version::Win8_1);
    hv.busy = 1;
    let vmbus = Rc::new(RefCell::new(on(hv)));
    vmbus.borrow_mut().start();
    let mut d = Dispatcher::new();
    let end = driver(&vmbus, &mut d);
    let mut retried = false;
    let answers = serve(
        &vmbus,
        &mut d,
        thread::spawn(move || {
            let mut c = client(end);
            (c.offers(Id(SCSI)), c.open(2, 8, 4).map(|_| ()))
        }),
        |vmbus| {
            if vmbus.pending() == 1 && !retried {
                retried = true;
                vmbus.retry();
            }
        },
    );
    assert_eq!(
        answers,
        (
            Err(CallError::Remote(vmbus::Error::NotConnected)),
            Err(CallError::Remote(vmbus::Error::NotConnected))
        )
    );
    let v = vmbus.borrow();
    assert!(
        v.fatal()
            .unwrap()
            .to_string()
            .starts_with("the host refused contact")
    );
}

#[test]
fn a_message_from_the_host_that_is_refused_is_said_sixteen_times_at_most() {
    let vmbus = connected();
    let mut v = vmbus.borrow_mut();
    for _ in 0..20 {
        v.host_mut().hv.send_bytes(&[1, 2, 3]);
    }
    v.interrupted();
    assert_eq!(v.refused(), 20);
    let lines = v
        .host()
        .console
        .iter()
        .filter(|line| line.starts_with("vmbus: refused a message from the host"))
        .count();
    assert_eq!(lines, 16);
    assert_eq!(v.fatal(), None);
}

#[test]
fn requests_that_are_no_requests_are_refused() {
    let vmbus = connected();
    let mut d = Dispatcher::new();
    let end = driver(&vmbus, &mut d);
    let answers = serve(
        &vmbus,
        &mut d,
        thread::spawn(move || {
            let mut core =
                ClientCore::connect(end, vmbus::ID, deadline_in(10_000_000_000)).expect("hello");
            let unknown = core.call(9, Encoder::new()).map(|_| ());
            let mut short = Encoder::new();
            7u32.encode(&mut short).unwrap();
            let short = core.call(2, short).map(|_| ());
            [unknown, short]
        }),
        |_| {},
    );
    assert_eq!(
        answers,
        [
            Err(CallError::Status(Status::UnknownMethod)),
            Err(CallError::Status(Status::BadMessage)),
        ]
    );
}

#[test]
fn a_posts_status_is_posted_busy_or_refused() {
    assert_eq!(post_result(0), Ok(()));
    assert_eq!(post_result(0x13), Err(PostError::Busy));
    assert_eq!(
        post_result(0x5),
        Err(PostError::Hv(HvError::InvalidParameter))
    );
    assert_eq!(
        post_result(0x12),
        Err(PostError::Hv(HvError::InvalidConnectionId))
    );
    // Not a status Hyper-V has: refused all the same.
    assert_eq!(
        post_result(0x1_0000),
        Err(PostError::Hv(HvError::InvalidParameter))
    );
}

#[test]
fn a_message_taken_reads_as_hyper_v_wrote_it() {
    let mut bytes = [0u8; 256];
    bytes[0..4].copy_from_slice(&1u32.to_le_bytes());
    bytes[4] = 3;
    bytes[5] = 1;
    bytes[6..8].copy_from_slice(&0xbeefu16.to_le_bytes());
    bytes[8..16].copy_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
    bytes[16..19].copy_from_slice(&[7, 8, 9]);
    bytes[255] = 0xaa;
    let m = message(&bytes);
    assert_eq!(m.header.typ.0, 1);
    assert_eq!(m.header.len, 3);
    assert!(m.header.flags.message_pending());
    assert_eq!(m.header.rsvd, 0xbeef);
    assert_eq!(m.header.id, 0x0102_0304_0506_0708);
    assert_eq!(m.payload_buffer[..3], [7, 8, 9]);
    assert_eq!(m.payload_buffer[239], 0xaa);
    // What the fake host sends the client is what it reads back.
    let sent = HvMessage::new(hvdef::HvMessageType(1), 0, &[4, 5, 6]);
    let mut raw = [0u8; 256];
    raw[0..4].copy_from_slice(&sent.header.typ.0.to_le_bytes());
    raw[4] = sent.header.len;
    raw[16..].copy_from_slice(&sent.payload_buffer);
    let back = message(&raw);
    assert_eq!(back.header.len, 3);
    assert_eq!(back.payload_buffer, sent.payload_buffer);
}

#[test]
fn pinned_addresses_are_page_numbers() {
    assert_eq!(page_numbers(&[0x1000, 0x7_f000, 0]), Some(vec![1, 0x7f, 0]));
    assert_eq!(page_numbers(&[]), Some(vec![]));
    assert_eq!(page_numbers(&[0x1000, 0x1008]), None);
}

/// Work item 1.11d, with the feature `trace-first-frames` only: when the host's signals of a
/// network card's channel reached the process, on the node's clock, said sixteen to a line.
#[cfg(feature = "trace-first-frames")]
mod trace {
    use std::cell::Cell;

    use moruna_vmbus_server::trace::{Interrupts, PER_LINE, TIMED};

    use super::*;

    /// The trace's lines on the node's console.
    fn said(v: &Vmbus<Node>) -> Vec<String> {
        v.host()
            .console
            .iter()
            .filter(|line| line.starts_with("vmbus: trace: "))
            .cloned()
            .collect()
    }

    #[test]
    fn the_signals_of_a_network_cards_channel_are_timed_and_said_sixteen_to_a_line() {
        let vmbus = connected();
        let clock = Rc::new(Cell::new(0));
        let time = clock.clone();
        vmbus
            .borrow_mut()
            .trace_interrupts(Interrupts::new(Box::new(move || time.get())));
        let mut d = Dispatcher::new();
        let disks = driver(&vmbus, &mut d);
        let nics = driver_of(&vmbus, &mut d, Class::Net);
        let ends = serve(
            &vmbus,
            &mut d,
            thread::spawn(move || {
                let mut c = client(disks);
                let mut n = client(nics);
                c.open(2, 8, 4).unwrap();
                n.open(1, 4, 2).unwrap();
                [c.into_core().into_channel(), n.into_core().into_channel()]
            }),
            |_| {},
        );
        let mut v = vmbus.borrow_mut();
        // The disk's channel is not timed; the card's is, each time it is signalled.
        for n in 1..=PER_LINE as u64 {
            clock.set(n * 1_000_000 + 250_000);
            v.host_mut().hv.signal_guest(ChannelId(2));
            v.host_mut().hv.signal_guest(ChannelId(1));
            v.interrupted();
            assert_eq!(said(&v).is_empty(), n < PER_LINE as u64, "signal {n}");
        }
        let times: Vec<String> = (1..=PER_LINE).map(|n| format!("{n}.250")).collect();
        assert_eq!(
            said(&v),
            [format!(
                "vmbus: trace: a network card's channel signalled, its signals 1 to 16 of 256, at (ms on the node's clock): {}",
                times.join(" ")
            )]
        );
        // The rest, to the last timed: then no more.
        for _ in PER_LINE..TIMED + PER_LINE {
            v.host_mut().hv.signal_guest(ChannelId(1));
            v.interrupted();
        }
        let lines = said(&v);
        assert_eq!(lines.len(), TIMED / PER_LINE);
        assert!(lines[lines.len() - 1].contains("its signals 241 to 256 of 256, at"));
        assert_eq!(v.fatal(), None);
        drop(v);
        drop(ends);
    }

    #[test]
    fn a_process_that_is_not_traced_says_nothing_of_its_signals() {
        let vmbus = connected();
        let mut d = Dispatcher::new();
        let nics = driver_of(&vmbus, &mut d, Class::Net);
        let end = serve(
            &vmbus,
            &mut d,
            thread::spawn(move || {
                let mut n = client(nics);
                n.open(1, 4, 2).unwrap();
                n.into_core().into_channel()
            }),
            |_| {},
        );
        let mut v = vmbus.borrow_mut();
        for _ in 0..PER_LINE {
            v.host_mut().hv.signal_guest(ChannelId(1));
            v.interrupted();
        }
        assert!(said(&v).is_empty());
        drop(v);
        drop(end);
    }
}
