//! Host tests of the device manager's directory (item 1.10b): block devices registered
//! through a `register*` listener, found by serial through a `directory*` one, and what
//! each listener's connections may not ask.

use std::cell::RefCell;
use std::rc::Rc;
use std::thread::{self, JoinHandle};

use devices_protocol::{Block, Entry, directory};
use moruna_devices::{Access, Directory, Drivers, Registry};
use moruna_svc::{
    CallError, Channel, ClientCore, Context, Dispatcher, Encoder, Outcome, ProtocolError, Status,
    Wire, deadline_in,
};

/// The device manager's own drivers, as the directory sees them: `block0`, running, and
/// `nvme0`, a data disk with no service.
struct Fake {
    /// The device manager's end of `block0`'s listener.
    block0: Channel,
}

impl Drivers for Fake {
    fn lookup(&mut self, _: &mut Context<'_>, name: &str) -> Option<Outcome> {
        match name {
            "block0" => Some(Outcome::reply(moruna_svc::connect(&self.block0).unwrap())),
            "nvme0" => Some(Outcome::Failure(directory::Error::NotFound.code())),
            _ => None,
        }
    }

    fn list(&self) -> Vec<Entry> {
        vec![
            ("block0".into(), 0x1af4_1042, "virtio-blk".into(), 1),
            ("nvme0".into(), 0x1b36_0010, "-".into(), 0),
        ]
    }
}

/// The directory, and the driver's end of each connection's `block0` listener.
struct World {
    registry: Rc<RefCell<Registry>>,
    dispatcher: Dispatcher,
    block0: Vec<Channel>,
}

impl World {
    fn new() -> World {
        World {
            registry: Rc::new(RefCell::new(Registry::new())),
            dispatcher: Dispatcher::new(),
            block0: Vec::new(),
        }
    }

    /// A connection through a listener of `access`.
    fn connection(&mut self, access: Access) -> Channel {
        let (mine, theirs) = Channel::create().unwrap();
        let (block0, driver) = Channel::create().unwrap();
        // Each connection's `block0` is a listener of its own, whose driver end stays here.
        self.block0.push(driver);
        let directory = Directory::new(Fake { block0 }, self.registry.clone(), access);
        self.dispatcher.serve(theirs, Box::new(directory)).unwrap();
        mine
    }

    /// Serves until `client` ends, then until its connections are gone.
    fn serve<T>(&mut self, client: JoinHandle<T>) -> T {
        while !client.is_finished() {
            self.dispatcher.turn(deadline_in(5_000_000)).unwrap();
        }
        for _ in 0..4 {
            self.dispatcher.turn(deadline_in(5_000_000)).unwrap();
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
        )
    }));
    let refused = || Err(CallError::Remote(directory::Error::Refused));
    assert_eq!(answers, (refused(), refused(), refused()));
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
        let mut extra = Encoder::new();
        7u32.encode(&mut extra).unwrap();
        let listed = l.call(2, extra).map(|_| ());
        let mut number = Encoder::new();
        7u32.encode(&mut number).unwrap();
        let looked = l.call(1, number).map(|_| ());
        let unknown = l.call(9, Encoder::new()).map(|_| ());
        [short, listed, looked, unknown]
    }));
    let bad = || Err(CallError::Status(Status::BadMessage));
    assert_eq!(
        answers,
        [
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
    assert_eq!((directory::ID.major, directory::ID.minor), (1, 2));
}
