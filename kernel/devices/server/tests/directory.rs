//! Host tests of the device manager's directory (item 1.10b): block devices registered
//! through a `register*` listener, found by serial through a `directory*` one, and what
//! each listener's connections may not ask. Item 1.11b: network cards registered by name,
//! and lookups that wait while a registrar's connection is open, and for nothing else.

use std::cell::RefCell;
use std::rc::Rc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use devices_protocol::{Block, Entry, directory};
use moruna_devices::{Access, Directory, Drivers, Registry};
use moruna_svc::Handler;
use moruna_svc::{
    CallError, Channel, ClientCore, Context, Dispatcher, Encoder, Outcome, ProtocolError, Status,
    Wire, deadline_in,
};

/// The device manager's own drivers, as the directory sees them: `block0`, running, and
/// `nvme0`, a data disk with no service; on q35, also `net0`, virtio-net's card.
struct Fake {
    /// The device manager's end of `block0`'s listener.
    block0: Channel,
    /// The device manager's end of `net0`'s listener, on q35.
    net0: Option<Channel>,
}

impl Drivers for Fake {
    fn lookup(&mut self, _: &mut Context<'_>, name: &str) -> Option<Outcome> {
        match (name, &self.net0) {
            ("block0", _) => Some(Outcome::reply(moruna_svc::connect(&self.block0).unwrap())),
            ("net0", Some(net0)) => Some(Outcome::reply(moruna_svc::connect(net0).unwrap())),
            ("nvme0", _) => Some(Outcome::Failure(directory::Error::NotFound.code())),
            _ => None,
        }
    }

    fn list(&self) -> Vec<Entry> {
        let mut entries = vec![
            ("block0".into(), 0x1af4_1042, "virtio-blk".into(), 1),
            ("nvme0".into(), 0x1b36_0010, "-".into(), 0),
        ];
        if self.net0.is_some() {
            entries.push(("net0".into(), 0x1af4_1041, "virtio-net".into(), 1));
        }
        entries
    }
}

/// The directory, and the driver's end of each connection's `block0` (and `net0`) listener.
struct World {
    registry: Rc<RefCell<Registry>>,
    dispatcher: Dispatcher,
    block0: Vec<Channel>,
    /// Whether virtio-net publishes `net0`, as on q35.
    q35: bool,
    net0: Vec<Channel>,
}

impl World {
    fn new() -> World {
        World {
            registry: Rc::new(RefCell::new(Registry::new())),
            dispatcher: Dispatcher::new(),
            block0: Vec::new(),
            q35: false,
            net0: Vec::new(),
        }
    }

    /// A machine with virtio-net, whose driver the device manager publishes as `net0`.
    fn q35() -> World {
        World {
            q35: true,
            ..World::new()
        }
    }

    /// The directory a connection through a listener of `access` is served by.
    fn directory(&mut self, access: Access) -> Directory<Fake> {
        let (block0, driver) = Channel::create().unwrap();
        // Each connection's `block0` is a listener of its own, whose driver end stays here.
        self.block0.push(driver);
        let net0 = self.q35.then(|| {
            let (net0, driver) = Channel::create().unwrap();
            self.net0.push(driver);
            net0
        });
        Directory::new(Fake { block0, net0 }, self.registry.clone(), access)
    }

    /// A `register*` listener, as the device manager serves one: its driver's end. A
    /// connection through it is a registrar's from the moment it is accepted.
    fn register_listener(&mut self) -> Channel {
        let (ours, theirs) = Channel::create().unwrap();
        let registry = self.registry.clone();
        self.dispatcher
            .listen(
                ours,
                Box::new(move || {
                    // A registrar looks nothing up: its `block0` is no one's.
                    let block0 = Channel::create().unwrap().0;
                    Box::new(Directory::new(
                        Fake { block0, net0: None },
                        registry.clone(),
                        Access::Register,
                    )) as Box<dyn Handler>
                }),
            )
            .unwrap();
        theirs
    }

    /// The client's end of a connection served by `directory`.
    fn serve_one(&mut self, directory: Directory<Fake>) -> Channel {
        let (mine, theirs) = Channel::create().unwrap();
        self.dispatcher.serve(theirs, Box::new(directory)).unwrap();
        mine
    }

    /// A connection through a listener of `access`.
    fn connection(&mut self, access: Access) -> Channel {
        let directory = self.directory(access);
        self.serve_one(directory)
    }

    /// Serves until `client` ends, then until its connections are gone, sending the answers
    /// of lookups that waited as the device manager's loop does.
    fn serve<T>(&mut self, client: JoinHandle<T>) -> T {
        let turn = |world: &mut World| {
            world.dispatcher.turn(deadline_in(5_000_000)).unwrap();
            for (pending, outcome) in world.registry.borrow_mut().take_ready() {
                let _ = world.dispatcher.complete(pending, outcome);
            }
        };
        while !client.is_finished() {
            turn(self);
        }
        for _ in 0..4 {
            turn(self);
        }
        client.join().expect("the client's thread")
    }
}

fn client(channel: Channel) -> directory::Client {
    directory::Client::connect(channel, deadline_in(10_000_000_000)).expect("hello")
}

fn disk(serial: &str) -> Block {
    Block {
        serial: serial.into(),
        sector_size: 512,
        sectors: 2048,
        read_only: false,
        boot: false,
    }
}

/// What a lookup's channel reached: the connection that arrived on `listener`, the driver's
/// end, which hears what the client writes.
fn arrived(listener: &Channel, client: &Channel) -> bool {
    let mut bytes = [0u8; 64];
    let mut handles = [None];
    let Ok((_, 1)) = listener.read(&mut bytes, &mut handles) else {
        return false;
    };
    let theirs = Channel(handles[0].take().unwrap());
    client.write_with(b"ping", &mut []).unwrap();
    let mut got = [0u8; 4];
    matches!(theirs.read(&mut got, &mut []), Ok((4, 0))) && &got == b"ping"
}

#[test]
fn a_registered_disk_is_found_by_its_serial() {
    let mut world = World::new();
    let register = world.connection(Access::Register);
    let lookup = world.connection(Access::Lookup);
    let (listener, driver_end) = Channel::create().unwrap();
    let found = world.serve(thread::spawn(move || {
        let mut r = client(register);
        let boot = Block {
            read_only: true,
            boot: true,
            ..disk("boot")
        };
        r.register(boot, listener).unwrap();
        client(lookup).lookup("boot".into()).unwrap()
    }));
    assert!(arrived(&driver_end, &found));
    let registry = world.registry.borrow();
    let boot = registry.find("boot").unwrap();
    assert!(boot.boot && boot.read_only);
    assert_eq!(registry.blocks().count(), 1);
    assert!(registry.find("lun-0").is_none());
}

#[test]
fn a_registered_network_card_is_found_by_its_name() {
    let mut world = World::new();
    let register = world.connection(Access::Register);
    let lookup = world.connection(Access::Lookup);
    let (listener, driver_end) = Channel::create().unwrap();
    let found = world.serve(thread::spawn(move || {
        client(register)
            .register_net("net0".into(), listener)
            .unwrap();
        client(lookup).lookup("net0".into()).unwrap()
    }));
    assert!(arrived(&driver_end, &found));
    let registry = world.registry.borrow();
    assert_eq!(registry.nets().collect::<Vec<_>>(), ["net0"]);
    assert!(registry.find("net0").is_none(), "a card is no block device");
    assert_eq!(registry.blocks().count(), 0);
}

#[test]
fn a_lookup_waits_while_a_registrar_is_connected_until_the_name_is_registered_or_it_closes() {
    let mut world = World::new();
    let registrar = world.connection(Access::Register);
    let early = world.connection(Access::Lookup);
    let unknown = world.connection(Access::Lookup);
    assert!(world.registry.borrow().awaited());
    let (listener, driver_end) = Channel::create().unwrap();
    let answers = world.serve(thread::spawn(move || {
        // The network server looks its card up before the driver has registered it, and
        // another client looks up a name no driver will register.
        let net0 = thread::spawn(move || client(early).lookup("net0".into()));
        let lun9 = thread::spawn(move || client(unknown).lookup("lun-9".into()).map(|_| ()));
        thread::sleep(Duration::from_millis(100));
        assert!(!net0.is_finished() && !lun9.is_finished(), "both wait");
        let mut r = client(registrar);
        r.register_net("net0".into(), listener).unwrap();
        let net0 = net0.join().unwrap();
        thread::sleep(Duration::from_millis(50));
        assert!(
            !lun9.is_finished(),
            "a name nothing has waits while the driver registers"
        );
        // The driver has finished: its connection closes.
        drop(r);
        (net0, lun9.join().unwrap())
    }));
    assert!(arrived(&driver_end, &answers.0.unwrap()));
    assert_eq!(
        answers.1,
        Err(CallError::Remote(directory::Error::NotFound))
    );
    let registry = world.registry.borrow();
    assert!(!registry.awaited());
    assert_eq!(registry.waiting(), 0);
}

#[test]
fn a_register_listener_whose_driver_is_not_here_or_never_connects_holds_no_lookup() {
    let mut world = World::new();
    // One whose driver is not on this machine: its end is gone. One whose driver is here
    // and never connects: its end stays, unused.
    drop(world.register_listener());
    let silent = world.register_listener();
    let lookup = world.connection(Access::Lookup);
    let answers = world.serve(thread::spawn(move || {
        let mut c = client(lookup);
        (
            c.lookup("net0".into()).map(|_| ()),
            c.lookup("lun-0".into()).map(|_| ()),
        )
    }));
    let not_found = || Err(CallError::Remote(directory::Error::NotFound));
    assert_eq!(answers, (not_found(), not_found()), "answered, not held");
    let registry = world.registry.borrow();
    assert!(!registry.awaited());
    assert_eq!(registry.waiting(), 0);
    drop(silent);
}

#[test]
fn a_registrar_connected_through_its_listener_holds_lookups_until_it_closes() {
    let mut world = World::new();
    let listener = world.register_listener();
    let lookup = world.connection(Access::Lookup);
    let (card, driver_end) = Channel::create().unwrap();
    let answers = world.serve(thread::spawn(move || {
        // The driver connects first thing, as netvsc does, and registers once its card is
        // up; the network server's lookup meanwhile waits.
        let mut r = directory::Client::connect(
            moruna_svc::connect(&listener).unwrap(),
            deadline_in(10_000_000_000),
        )
        .unwrap();
        let net0 = thread::spawn(move || client(lookup).lookup("net0".into()));
        thread::sleep(Duration::from_millis(100));
        assert!(!net0.is_finished(), "the lookup waits for the registrar");
        r.register_net("net0".into(), card).unwrap();
        drop(r);
        net0.join().unwrap()
    }));
    assert!(arrived(&driver_end, &answers.unwrap()));
    assert!(!world.registry.borrow().awaited());
}

#[test]
fn on_q35_virtio_nets_net0_is_answered_at_once_even_while_a_registrar_is_open() {
    let mut world = World::q35();
    let registrar = world.connection(Access::Register);
    let lookup = world.connection(Access::Lookup);
    let found = world.serve(thread::spawn(move || {
        // A registrar is connected and has not finished: a name a driver of the device
        // manager publishes does not wait for it.
        let _registrar = client(registrar);
        client(lookup).lookup("net0".into())
    }));
    assert!(arrived(&world.net0[1], &found.unwrap()));
}

#[test]
fn a_network_cards_name_taken_or_no_name_is_refused() {
    let mut world = World::new();
    let register = world.connection(Access::Register);
    let answers = world.serve(thread::spawn(move || {
        let mut r = client(register);
        let one = || Channel::create().unwrap().0;
        [
            r.register_net("net0".into(), one()),
            r.register_net("net0".into(), one()),
            r.register(disk("net0"), one()),
            r.register_net("block0".into(), one()),
            r.register_net("".into(), one()),
            r.register_net("net 1".into(), one()),
            r.register(disk("lun-0"), one()),
            r.register_net("lun-0".into(), one()),
        ]
    }));
    let exists = || Err(CallError::Remote(directory::Error::Exists));
    let invalid = || Err(CallError::Remote(directory::Error::Invalid));
    assert_eq!(
        answers,
        [
            Ok(()),
            exists(),
            exists(),
            exists(),
            invalid(),
            invalid(),
            Ok(()),
            exists()
        ]
    );
    let registry = world.registry.borrow();
    assert_eq!(registry.nets().collect::<Vec<_>>(), ["net0"]);
    assert_eq!(registry.blocks().count(), 1);
}

#[test]
fn a_published_service_is_found_before_the_registry_and_an_unknown_name_is_not() {
    let mut world = World::new();
    let lookup = world.connection(Access::Lookup);
    let answers = world.serve(thread::spawn(move || {
        let mut c = client(lookup);
        (
            c.lookup("block0".into()).map(|_| ()),
            c.lookup("nvme0".into()).map(|_| ()),
            c.lookup("lun-9".into()).map(|_| ()),
            c.list().unwrap(),
        )
    }));
    assert_eq!(answers.0, Ok(()));
    assert_eq!(
        answers.1,
        Err(CallError::Remote(directory::Error::NotFound))
    );
    assert_eq!(
        answers.2,
        Err(CallError::Remote(directory::Error::NotFound))
    );
    assert_eq!(answers.3.len(), 2);
}

#[test]
fn each_listener_allows_its_own_requests_only() {
    let mut world = World::new();
    let register = world.connection(Access::Register);
    let lookup = world.connection(Access::Lookup);
    let answers = world.serve(thread::spawn(move || {
        let mut r = client(register);
        let mut l = client(lookup);
        (
            r.lookup("block0".into()).map(|_| ()),
            r.list().map(|_| ()),
            l.register(disk("lun-0"), Channel::create().unwrap().0),
            l.register_net("net0".into(), Channel::create().unwrap().0),
        )
    }));
    let refused = || Err(CallError::Remote(directory::Error::Refused));
    assert_eq!(answers, (refused(), refused(), refused(), refused()));
    assert_eq!(world.registry.borrow().blocks().count(), 0);
}

#[test]
fn a_serial_taken_or_a_device_that_is_no_disk_is_refused() {
    let mut world = World::new();
    let register = world.connection(Access::Register);
    let answers = world.serve(thread::spawn(move || {
        let mut r = client(register);
        let one = || Channel::create().unwrap().0;
        [
            r.register(disk("lun-0"), one()),
            r.register(disk("lun-0"), one()),
            r.register(disk("block0"), one()),
            r.register(disk("nvme0"), one()),
            r.register(disk(""), one()),
            r.register(
                Block {
                    sectors: 0,
                    ..disk("lun-1")
                },
                one(),
            ),
            r.register(disk("lun-1"), one()),
        ]
    }));
    let exists = || Err(CallError::Remote(directory::Error::Exists));
    let invalid = || Err(CallError::Remote(directory::Error::Invalid));
    assert_eq!(
        answers,
        [
            Ok(()),
            exists(),
            exists(),
            exists(),
            invalid(),
            invalid(),
            Ok(())
        ]
    );
    let serials: Vec<_> = world
        .registry
        .borrow()
        .blocks()
        .map(|b| b.serial.clone())
        .collect();
    assert_eq!(serials, ["lun-0", "lun-1"]);
}

#[test]
fn a_disk_whose_driver_let_go_of_its_listener_is_unavailable() {
    let mut world = World::new();
    let register = world.connection(Access::Register);
    let lookup = world.connection(Access::Lookup);
    let answer = world.serve(thread::spawn(move || {
        let (listener, driver_end) = Channel::create().unwrap();
        client(register).register(disk("lun-0"), listener).unwrap();
        drop(driver_end);
        client(lookup).lookup("lun-0".into()).map(|_| ())
    }));
    assert_eq!(
        answer,
        Err(CallError::Remote(directory::Error::Unavailable))
    );
}

#[test]
fn requests_that_are_no_requests_are_refused() {
    let mut world = World::new();
    let register = world.connection(Access::Register);
    let lookup = world.connection(Access::Lookup);
    let answers = world.serve(thread::spawn(move || {
        let mut r =
            ClientCore::connect(register, directory::ID, deadline_in(10_000_000_000)).unwrap();
        let mut l =
            ClientCore::connect(lookup, directory::ID, deadline_in(10_000_000_000)).unwrap();
        // A register with a block and no listener.
        let mut short = Encoder::new();
        disk("lun-0").encode(&mut short).unwrap();
        let short = r.call(3, short).map(|_| ());
        // A card's name and no listener.
        let mut nameless = Encoder::new();
        String::from("net0").encode(&mut nameless).unwrap();
        let nameless = r.call(4, nameless).map(|_| ());
        let mut extra = Encoder::new();
        7u32.encode(&mut extra).unwrap();
        let listed = l.call(2, extra).map(|_| ());
        let mut number = Encoder::new();
        7u32.encode(&mut number).unwrap();
        let looked = l.call(1, number).map(|_| ());
        let unknown = l.call(9, Encoder::new()).map(|_| ());
        [short, nameless, listed, looked, unknown]
    }));
    let bad = || Err(CallError::Status(Status::BadMessage));
    assert_eq!(
        answers,
        [
            bad(),
            bad(),
            bad(),
            bad(),
            Err(CallError::Status(Status::UnknownMethod))
        ]
    );
}

#[test]
fn a_slot_gives_its_listener_its_access() {
    assert_eq!(Access::of("directory"), Some(Access::Lookup));
    assert_eq!(Access::of("directory-net"), Some(Access::Lookup));
    assert_eq!(Access::of("register-storvsc"), Some(Access::Register));
    assert_eq!(Access::of("control"), None);
    assert_eq!(Access::of("supervisor"), None);
}
