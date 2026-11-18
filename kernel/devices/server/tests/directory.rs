//! Host tests of the device manager's directory (item 1.10b): block devices registered
//! through a `register*` listener, found by serial through a `directory*` one, and what
//! each listener's connections may not ask. Item 1.11b: network cards registered by name,
//! and lookups that wait while a held `register*` slot's driver has not finished, and for
//! nothing else. Item 1.6d: whoever started the driver given the slot, each block driver
//! its own, and a driver that lets its slot go, or ends, without connecting has finished.

use std::cell::RefCell;
use std::rc::Rc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use devices_protocol::{Block, Entry, directory};
use moruna_devices::{Access, Directory, Drivers, Held, Registry};
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

    /// A `register*` slot handed to a driver, served as the device manager serves every one,
    /// whoever started the driver (`node.rs`: the supervisor's `register*` start slots, and
    /// the `register` slot it gives each block driver it starts): held from now on
    /// ([`Held`]). The driver's end.
    fn hand_out(&mut self) -> Channel {
        let (ours, theirs) = Channel::create().unwrap();
        let factory = Held::new(&self.registry).factory(|| Fake {
            // A registrar looks nothing up: its `block0` is no one's.
            block0: Channel::create().unwrap().0,
            net0: None,
        });
        self.dispatcher.listen(ours, factory).unwrap();
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

/// Who handed a driver its `register*` slot.
#[derive(Clone, Copy, Debug)]
enum Starter {
    /// The node's supervisor: a `register*` slot of the device manager's start page, linked
    /// to a driver the supervisor starts (`register-storvsc`, `register-netvsc`).
    Supervisor,
    /// The device manager itself: the `register` slot it gives each block driver it starts,
    /// on a node (virtio-blk).
    DeviceManager,
}

/// Every driver given a `register*` slot, who starts it, and the name a client looks up
/// before it has registered it: virtio-blk the boot disk's `system`, which imagefs looks up
/// (Servercore run 8), storvsc Azure's OS disk `boot`, and netvsc `net0`, which the network
/// server looks up.
const REGISTRARS: [(&str, Starter, &str); 3] = [
    ("virtio-blk", Starter::DeviceManager, "system"),
    ("storvsc", Starter::Supervisor, "boot"),
    ("netvsc", Starter::Supervisor, "net0"),
];

#[test]
fn a_lookup_made_before_a_driver_registers_is_answered_by_it_whoever_started_the_driver() {
    for (driver, starter, name) in REGISTRARS {
        let mut world = World::new();
        let slot = world.hand_out();
        let early = world.connection(Access::Lookup);
        let unknown = world.connection(Access::Lookup);
        let (listener, driver_end) = Channel::create().unwrap();
        let (waited, found, held, missing) = world.serve(thread::spawn(move || {
            // A client looks the name up as soon as it starts, before the driver has
            // registered it, and another looks up a name no driver will register.
            let found = thread::spawn(move || client(early).lookup(name.into()));
            let missing = thread::spawn(move || client(unknown).lookup("lun-9".into()).map(|_| ()));
            thread::sleep(Duration::from_millis(100));
            let waited = !found.is_finished() && !missing.is_finished();
            // The driver comes up: it connects through its slot and lets the slot go, as
            // `open_directory` does, then registers.
            let channel = moruna_svc::connect(&slot).unwrap();
            drop(slot);
            let mut r = client(channel);
            let registered = if name == "net0" {
                r.register_net(name.into(), listener)
            } else {
                let disk = Block {
                    read_only: true,
                    boot: true,
                    ..disk(name)
                };
                r.register(disk, listener)
            };
            registered.unwrap();
            let found = found.join().unwrap();
            thread::sleep(Duration::from_millis(50));
            let held = !missing.is_finished();
            // It has finished registering: its connection closes.
            drop(r);
            (waited, found, held, missing.join().unwrap())
        }));
        let by = format!("{driver}, started by the {starter:?}");
        assert!(
            waited,
            "{by}: a lookup made before it registers waits for it"
        );
        let found = found.unwrap_or_else(|e| panic!("{by}: {name} was answered {e:?}"));
        assert!(arrived(&driver_end, &found), "{by}: the lookup reaches it");
        assert!(held, "{by}: a name nothing has waits while it registers");
        assert_eq!(
            missing,
            Err(CallError::Remote(directory::Error::NotFound)),
            "{by}: answered once it has finished"
        );
        let registry = world.registry.borrow();
        assert!(!registry.awaited(), "{by}: it has finished");
        assert_eq!(registry.waiting(), 0, "{by}");
    }
}

#[test]
fn each_block_driver_holds_a_lookup_until_it_has_finished_registering() {
    // A node with two virtio-blk disks: the boot disk's driver registers `system` and
    // finishes, and a volume's driver is slower. A lookup of the volume's serial waits for
    // its own driver, not only for the first to finish.
    let mut world = World::new();
    let boot = world.hand_out();
    let volume = world.hand_out();
    let early = world.connection(Access::Lookup);
    let (system, _system_driver) = Channel::create().unwrap();
    let (vol, vol_driver) = Channel::create().unwrap();
    let (waited, found) = world.serve(thread::spawn(move || {
        let found = thread::spawn(move || client(early).lookup("vol-a".into()));
        let mut r = client(moruna_svc::connect(&boot).unwrap());
        r.register(disk("system"), system).unwrap();
        drop((r, boot));
        thread::sleep(Duration::from_millis(100));
        let waited = !found.is_finished();
        let mut r = client(moruna_svc::connect(&volume).unwrap());
        r.register(disk("vol-a"), vol).unwrap();
        drop(r);
        (waited, found.join().unwrap())
    }));
    assert!(waited, "the volume's driver has not finished");
    assert!(arrived(&vol_driver, &found.unwrap()));
    let registry = world.registry.borrow();
    assert!(!registry.awaited());
    assert_eq!(registry.blocks().count(), 2);
}

#[test]
fn a_driver_that_lets_its_slot_go_or_ends_without_connecting_has_finished() {
    // virtio-blk with nothing to register (a disk with no serial, not the boot disk) lets its
    // slot go; a driver that ends before connecting lets it go with every handle it held.
    // Either has finished, registering nothing: a lookup of a name nothing has is answered
    // `NotFound` once both have, and not before.
    let mut world = World::new();
    let nothing = world.hand_out();
    let ended = world.hand_out();
    let lookup = world.connection(Access::Lookup);
    let (waited, still, answer) = world.serve(thread::spawn(move || {
        let answer = thread::spawn(move || client(lookup).lookup("lun-9".into()).map(|_| ()));
        thread::sleep(Duration::from_millis(100));
        let waited = !answer.is_finished();
        drop(nothing);
        thread::sleep(Duration::from_millis(100));
        let still = !answer.is_finished();
        drop(ended);
        (waited, still, answer.join().unwrap())
    }));
    assert!(waited, "both slots are held");
    assert!(still, "one slot's driver has not finished");
    assert_eq!(answer, Err(CallError::Remote(directory::Error::NotFound)));
    let registry = world.registry.borrow();
    assert!(!registry.awaited());
    assert_eq!(registry.waiting(), 0);
}

#[test]
fn a_slot_handed_out_to_a_driver_that_never_starts_is_finished() {
    // The device manager holds a block driver's slot before it starts it; should the start
    // fail, the hold goes with the slot, and nothing waits for a driver that is not there.
    let registry = Rc::new(RefCell::new(Registry::new()));
    let held = Held::new(&registry);
    assert!(registry.borrow().awaited());
    drop(held);
    assert!(!registry.borrow().awaited());
    // A hold that goes while the registry is borrowed changes nothing, and panics nowhere.
    let held = Held::new(&registry);
    {
        let _borrowed = registry.borrow_mut();
        drop(held);
    }
    assert!(registry.borrow().awaited());
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
fn a_lookup_waits_for_a_held_slots_driver_until_the_name_is_registered_or_it_has_finished() {
    let mut world = World::new();
    let slot = world.hand_out();
    let early = world.connection(Access::Lookup);
    let unknown = world.connection(Access::Lookup);
    assert!(world.registry.borrow().awaited());
    let (listener, driver_end) = Channel::create().unwrap();
    let answers = world.serve(thread::spawn(move || {
        // The network server looks its card up before the driver has registered it, and
        // another client looks up a name no driver will register.
        let mut r = client(moruna_svc::connect(&slot).unwrap());
        let net0 = thread::spawn(move || client(early).lookup("net0".into()));
        let lun9 = thread::spawn(move || client(unknown).lookup("lun-9".into()).map(|_| ()));
        thread::sleep(Duration::from_millis(100));
        assert!(!net0.is_finished() && !lun9.is_finished(), "both wait");
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
fn with_no_register_slot_held_nothing_waits() {
    // Off Hyper-V the supervisor links no register slot to the device manager: the link
    // goes with its driver. A name nothing has is answered at once.
    let mut world = World::new();
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
}

#[test]
fn a_registry_awaits_each_held_slot_until_its_driver_has_finished() {
    let mut registry = Registry::new();
    assert!(!registry.awaited());
    let (storvsc, netvsc) = (registry.expect(), registry.expect());
    registry.finished(storvsc);
    registry.finished(storvsc);
    // A number no slot was given changes nothing.
    registry.finished(7);
    assert!(registry.awaited(), "netvsc has not finished");
    registry.finished(netvsc);
    assert!(!registry.awaited());
    assert!(registry.take_ready().is_empty());
}

#[test]
fn a_held_slots_driver_not_yet_connected_holds_a_lookup_until_it_connects_and_registers() {
    let mut world = World::new();
    let slot = world.hand_out();
    let lookup = world.connection(Access::Lookup);
    let (card, driver_end) = Channel::create().unwrap();
    let answers = world.serve(thread::spawn(move || {
        // The network server looks net0 up before netvsc has even connected: the slot is
        // held, so its driver is on this machine, and the lookup waits for it.
        let net0 = thread::spawn(move || client(lookup).lookup("net0".into()));
        thread::sleep(Duration::from_millis(100));
        assert!(
            !net0.is_finished(),
            "the lookup waits for the slot's driver"
        );
        let mut r = client(moruna_svc::connect(&slot).unwrap());
        thread::sleep(Duration::from_millis(50));
        assert!(!net0.is_finished(), "connected, not registered");
        r.register_net("net0".into(), card).unwrap();
        drop(r);
        net0.join().unwrap()
    }));
    assert!(arrived(&driver_end, &answers.unwrap()));
    assert!(!world.registry.borrow().awaited());
}

#[test]
fn virtio_nets_net0_is_answered_at_once_even_while_a_held_slot_waits() {
    // On q35 no register slot is held and virtio-net publishes net0. Even with a held slot
    // whose driver has not finished, a name the device manager's drivers publish does not
    // wait for it.
    let mut world = World::q35();
    let _slot = world.hand_out();
    let lookup = world.connection(Access::Lookup);
    let found = world.serve(thread::spawn(move || client(lookup).lookup("net0".into())));
    assert!(arrived(&world.net0[0], &found.unwrap()));
    assert!(world.registry.borrow().awaited());
}

#[test]
fn a_network_cards_name_taken_or_no_name_is_refused() {
    let mut world = World::new();
    let register = world.connection(Access::Register);
    let answers = world.serve(thread::spawn(move || {
        let mut r = client(register);
        // Each listener's driver end is kept: a name whose driver has gone is free again.
        let kept = std::cell::RefCell::new(Vec::new());
        let one = || {
            let (listener, driver) = Channel::create().unwrap();
            kept.borrow_mut().push(driver);
            listener
        };
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
        // Each listener's driver end is kept: a serial whose driver has gone is free again.
        let kept = std::cell::RefCell::new(Vec::new());
        let one = || {
            let (listener, driver) = Channel::create().unwrap();
            kept.borrow_mut().push(driver);
            listener
        };
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

#[test]
fn a_driver_started_again_registers_its_disk_again() {
    let mut world = World::new();
    let register = world.connection(Access::Register);
    let lookup = world.connection(Access::Lookup);
    let (first, gone) = Channel::create().unwrap();
    let (second, driver_end) = Channel::create().unwrap();
    let (answers, found) = world.serve(thread::spawn(move || {
        let mut r = client(register);
        let before = r.register(disk("vol-a"), first);
        drop(gone);
        let again = r.register(
            Block {
                sectors: 4096,
                ..disk("vol-a")
            },
            second,
        );
        (
            (before, again),
            client(lookup).lookup("vol-a".into()).unwrap(),
        )
    }));
    assert_eq!(answers, (Ok(()), Ok(())));
    assert!(
        arrived(&driver_end, &found),
        "the lookup reaches the new driver"
    );
    let registry = world.registry.borrow();
    assert_eq!(registry.blocks().count(), 1);
    assert_eq!(registry.find("vol-a").unwrap().sectors, 4096);
}

#[test]
fn blocks_lists_every_registered_disk_through_a_directory_listener_only() {
    let mut world = World::new();
    let register = world.connection(Access::Register);
    let lookup = world.connection(Access::Lookup);
    let (a, a_driver) = Channel::create().unwrap();
    let (b, b_driver) = Channel::create().unwrap();
    let (listed, refused) = world.serve(thread::spawn(move || {
        let mut r = client(register);
        r.register(disk("vol-b"), a).unwrap();
        let boot = Block {
            boot: true,
            read_only: true,
            ..disk("boot")
        };
        r.register(boot, b).unwrap();
        (client(lookup).blocks().unwrap(), r.blocks().map(|_| ()))
    }));
    drop((a_driver, b_driver));
    assert_eq!(
        listed
            .iter()
            .map(|b| (b.serial.as_str(), b.boot))
            .collect::<Vec<_>>(),
        [("vol-b", false), ("boot", true)]
    );
    assert_eq!(listed[0], disk("vol-b"));
    assert_eq!(
        refused,
        Err(CallError::Remote(directory::Error::Refused)),
        "a register listener may not list"
    );
    assert_eq!((directory::ID.major, directory::ID.minor), (1, 3));
}
