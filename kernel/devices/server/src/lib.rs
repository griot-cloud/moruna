//! The device manager's directory (SDD 25, d.6), as a library: the drivers' published
//! services, which the process looks up ([`Drivers`]), and the devices registered by
//! drivers the node's supervisor starts ([`Registry`]): block devices, and from directory
//! minor 2 network cards. `main.rs` is the process; on a host it is an empty program, and
//! this library is what the host tests run.
//!
//! What a connection may ask depends on the listener it came through ([`Access`]): a
//! `directory*` listener's may look up and list, and a `register*` listener's may register a
//! device and nothing else. Only drivers are given a `register*` slot: the boot description
//! links one to each driver the supervisor starts (`storvsc`, item 1.10b; `netvsc`, item
//! 1.11b), and on a node the device manager gives one to each block driver it starts
//! (virtio-blk, item 3.3). So no client can put a name in the directory, and a driver can
//! learn nothing from it.
//!
//! Such a driver starts beside the directory's clients: the network server looks `net0` up as
//! soon as it starts, and `netvsc` registers it only once Azure's card is up; the image
//! filesystem looks `system` up as soon as it starts, and the boot disk's virtio-blk
//! registers it only once the disk is up (on Servercore's KVM the lookup came first, item
//! 1.6d). So every `register*` slot handed out, by the supervisor or by the device manager,
//! is held from then until its driver has finished registering ([`Held`]): a lookup of a
//! name nothing has waits while any held slot's driver has not finished
//! ([`Registry::expect`]), whether or not it has connected yet. A driver finishes by closing
//! its connection through the slot ([`Registry::finished`]), or by letting the slot go, or
//! ending, without having connected through it (a virtio-blk disk with nothing to register).
//! The lookup is answered when the name is registered, or with `NotFound` once every held
//! slot's driver has finished. A slot not handed out holds nothing (the supervisor links
//! storvsc's and netvsc's only on Hyper-V: elsewhere the link goes with the driver), and a
//! name a driver of the device manager publishes (`net0` of virtio-net) is answered at once
//! whatever waits. The wait cannot hang the node: a driver that ends finishes its slot (the
//! device manager gives a driver it starts again a new one), and one that hangs runs into
//! the boot's deadlines: the client's own call (imagefs's and the network server's lookups
//! wait 300 s) and `platform`'s instance-data deadline, which names it.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{boxed::Box, rc::Rc, string::String, vec::Vec};
use core::cell::RefCell;

use devices_protocol::{Block, Entry, REGISTER_SLOTS, directory, valid_name};
use moruna_svc::{
    Channel, Context, Decoder, Factory, Handler, Outcome, Pending, ProtocolError, ProtocolId,
    Signals, Status, Wire, connect, wait_one,
};

/// What a connection to the directory may ask, by the listener it came through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// A client's: `lookup` and `list`.
    Lookup,
    /// A driver's that the supervisor starts: `register` and `register_net` alone.
    Register,
}

impl Access {
    /// The access a listener slot gives its connections: `directory*` and
    /// [`REGISTER_SLOTS`]`*`, and none for another slot, which is not the directory's.
    pub fn of(slot: &str) -> Option<Access> {
        if slot.starts_with("directory") {
            Some(Access::Lookup)
        } else if slot.starts_with(REGISTER_SLOTS) {
            Some(Access::Register)
        } else {
            None
        }
    }
}

/// The services the device manager's own drivers publish.
pub trait Drivers {
    /// The answer to a lookup of `name`: a connection to the driver that publishes it,
    /// or a deferred answer while it is restarted. `None` when no driver publishes it.
    fn lookup(&mut self, cx: &mut Context<'_>, name: &str) -> Option<Outcome>;

    /// Every service the drivers publish.
    fn list(&self) -> Vec<Entry>;
}

/// What a driver registered.
enum Kind {
    /// A block device, served as `moruna.driver.block`.
    Block(Block),
    /// A network card, served as `moruna.driver.net`.
    Net,
}

/// A registered device, its name, and the device manager's end of the listener its driver
/// serves it on.
struct Registered {
    name: String,
    kind: Kind,
    listener: Channel,
}

impl Registered {
    /// Whether its driver has let go of its listener: a driver started again registers its
    /// device again, and the name is free for it (item 3.3).
    fn gone(&self) -> bool {
        wait_one(&self.listener.0, Signals::PEER_CLOSED, 0).is_ok()
    }
}

/// The devices drivers have registered, by name, and the lookups waiting for them.
#[derive(Default)]
pub struct Registry {
    entries: Vec<Registered>,
    /// For each held `register*` slot: whether its driver has finished registering.
    finished: Vec<bool>,
    /// Lookups of a name nothing has, while a held slot's driver has not finished.
    waiting: Vec<(String, Pending)>,
    /// Answers to lookups that waited, for the device manager's loop to send.
    ready: Vec<(Pending, Outcome)>,
}

impl Registry {
    /// No device registered, and no `register*` slot held.
    pub fn new() -> Registry {
        Registry::default()
    }

    /// Registers `block`, served on `listener`, unless its serial is a name taken already:
    /// `published` says whether a driver's service has it. A name whose driver has let go of
    /// its listener is taken no more.
    pub fn register(
        &mut self,
        block: Block,
        listener: Channel,
        published: bool,
    ) -> Result<(), directory::Error> {
        if !block.valid() {
            return Err(directory::Error::Invalid);
        }
        let name = block.serial.clone();
        self.add(name, Kind::Block(block), listener, published)
    }

    /// Registers a network card under `name`, served on `listener`, unless the name is
    /// taken already: `published` says whether a driver's service has it.
    pub fn register_net(
        &mut self,
        name: String,
        listener: Channel,
        published: bool,
    ) -> Result<(), directory::Error> {
        if !valid_name(&name) {
            return Err(directory::Error::Invalid);
        }
        self.add(name, Kind::Net, listener, published)
    }

    fn add(
        &mut self,
        name: String,
        kind: Kind,
        listener: Channel,
        published: bool,
    ) -> Result<(), directory::Error> {
        if published {
            return Err(directory::Error::Exists);
        }
        let entry = Registered {
            name,
            kind,
            listener,
        };
        let at = match self.entries.iter().position(|e| e.name == entry.name) {
            // Its driver has gone: the driver started again takes the name.
            Some(at) if self.entries[at].gone() => {
                self.entries[at] = entry;
                at
            }
            Some(_) => return Err(directory::Error::Exists),
            None => {
                self.entries.push(entry);
                self.entries.len() - 1
            }
        };
        // The lookups that waited for it.
        let name = &self.entries[at].name;
        let (found, waiting): (Vec<_>, Vec<_>) = core::mem::take(&mut self.waiting)
            .into_iter()
            .partition(|(wanted, _)| wanted == name);
        self.waiting = waiting;
        for (wanted, pending) in found {
            let outcome = lookup_outcome(self.connect(&wanted));
            self.ready.push((pending, outcome));
        }
        Ok(())
    }

    /// The block device registered under `serial`.
    pub fn find(&self, serial: &str) -> Option<&Block> {
        match &self.entry(serial)?.kind {
            Kind::Block(block) => Some(block),
            Kind::Net => None,
        }
    }

    fn entry(&self, name: &str) -> Option<&Registered> {
        self.entries
            .iter()
            .find(|registered| registered.name == name)
    }

    /// A new connection to the device registered under `name`: `None` if none is, and
    /// [`directory::Error::Unavailable`] once its driver has let go of its listener.
    pub fn connect(&self, name: &str) -> Option<Result<Channel, directory::Error>> {
        let registered = self.entry(name)?;
        Some(connect(&registered.listener).map_err(|_| directory::Error::Unavailable))
    }

    /// Every block device registered, in the order they were.
    pub fn blocks(&self) -> impl Iterator<Item = &Block> {
        self.entries
            .iter()
            .filter_map(|registered| match &registered.kind {
                Kind::Block(block) => Some(block),
                Kind::Net => None,
            })
    }

    /// The name of every network card registered, in the order they were.
    pub fn nets(&self) -> impl Iterator<Item = &str> {
        self.entries
            .iter()
            .filter(|registered| matches!(registered.kind, Kind::Net))
            .map(|registered| registered.name.as_str())
    }

    /// A `register*` slot handed to a driver ([`Held`]): its driver is on this machine, and
    /// lookups of a name nothing has wait until it has [`finished`](Registry::finished),
    /// whether or not it has connected yet. The slot's number, for that.
    pub fn expect(&mut self) -> usize {
        self.finished.push(false);
        self.finished.len() - 1
    }

    /// The driver of held slot `number` has finished registering: a connection through the
    /// slot has closed. Once every held slot's driver has, the lookups still waiting are
    /// answered `NotFound`.
    pub fn finished(&mut self, number: usize) {
        if let Some(finished) = self.finished.get_mut(number) {
            *finished = true;
        }
        if !self.awaited() {
            for (_, pending) in core::mem::take(&mut self.waiting) {
                self.ready
                    .push((pending, Outcome::Failure(directory::Error::NotFound.code())));
            }
        }
    }

    /// Whether a held slot's driver has yet to finish, so a lookup of a name nothing has
    /// waits.
    pub fn awaited(&self) -> bool {
        self.finished.iter().any(|finished| !finished)
    }

    /// Lookups waiting for a name.
    pub fn waiting(&self) -> usize {
        self.waiting.len()
    }

    /// The answers of lookups that waited and now have one, to send.
    pub fn take_ready(&mut self) -> Vec<(Pending, Outcome)> {
        core::mem::take(&mut self.ready)
    }
}

/// A `register*` slot handed to a driver, by the supervisor (a `register*` slot of the device
/// manager's start page, linked to a driver it starts) or by the device manager (the
/// `register` slot it gives each block driver it starts): held from the moment it is handed
/// out until its driver has finished registering, so that a lookup of a name nothing has
/// waits for it ([`Registry::expect`]). One mechanism, whoever started the driver (item 1.6d).
pub struct Held {
    registry: Rc<RefCell<Registry>>,
    /// The slot's number, as [`Registry::expect`] gave it.
    number: usize,
    /// Whether a connection has come through the slot: its close finishes the slot then.
    connected: bool,
}

impl Held {
    /// Hands a slot out: from now on a lookup of a name nothing has waits for its driver.
    pub fn new(registry: &Rc<RefCell<Registry>>) -> Held {
        let number = registry.borrow_mut().expect();
        Held {
            registry: registry.clone(),
            number,
            connected: false,
        }
    }

    /// The factory of the slot's listener: each connection through it is a registrar
    /// ([`Access::Register`]) served from `drivers`, whose close says the driver has
    /// finished ([`Directory::through`]). Should the listener go with no connection having
    /// come through it, its driver having let the slot go or ended first, the slot's driver
    /// has finished then.
    pub fn factory<D, F>(mut self, mut drivers: F) -> Factory
    where
        D: Drivers + 'static,
        F: FnMut() -> D + 'static,
    {
        Box::new(move || {
            self.connected = true;
            let directory = Directory::new(drivers(), self.registry.clone(), Access::Register);
            Box::new(directory.through(self.number)) as Box<dyn Handler>
        })
    }
}

impl Drop for Held {
    /// A slot that goes with no connection come through it: its driver has finished, having
    /// registered nothing.
    fn drop(&mut self) {
        if !self.connected
            && let Ok(mut registry) = self.registry.try_borrow_mut()
        {
            registry.finished(self.number);
        }
    }
}

/// A lookup's answer from the registry.
fn lookup_outcome(found: Option<Result<Channel, directory::Error>>) -> Outcome {
    match found {
        Some(Ok(channel)) => Outcome::reply(channel),
        Some(Err(error)) => Outcome::Failure(error.code()),
        None => Outcome::Failure(directory::Error::NotFound.code()),
    }
}

/// One connection to the directory.
pub struct Directory<D: Drivers> {
    drivers: D,
    registry: Rc<RefCell<Registry>>,
    access: Access,
    /// The held `register*` slot it came through, as [`Registry::expect`] numbered it: its
    /// close says the slot's driver has finished registering.
    slot: Option<usize>,
}

impl<D: Drivers> Directory<D> {
    /// A connection through a listener of `access`, served from `drivers` and `registry`.
    pub fn new(drivers: D, registry: Rc<RefCell<Registry>>, access: Access) -> Directory<D> {
        Directory {
            drivers,
            registry,
            access,
            slot: None,
        }
    }

    /// The connection came through held `register*` slot `number` ([`Registry::expect`]):
    /// when it closes, that slot's driver has finished.
    pub fn through(mut self, number: usize) -> Directory<D> {
        self.slot = Some(number);
        self
    }

    /// Sends the answers of lookups that waited, through this request's context.
    fn answer_waiting(&self, cx: &mut Context<'_>) {
        let ready = self.registry.borrow_mut().take_ready();
        for (pending, outcome) in ready {
            cx.complete(pending, outcome);
        }
    }
}

fn refused() -> Outcome {
    Outcome::Failure(directory::Error::Refused.code())
}

/// A request's arguments, every byte and handle of them.
fn arguments<T: Wire>(d: &mut Decoder<'_>) -> Option<T> {
    let value = T::decode(d).ok()?;
    d.finish().ok()?;
    Some(value)
}

impl<D: Drivers> Handler for Directory<D> {
    fn protocol(&self) -> ProtocolId {
        directory::ID
    }

    fn handle(&mut self, cx: &mut Context<'_>, ordinal: u16, d: &mut Decoder<'_>) -> Outcome {
        let bad = Outcome::Status(Status::BadMessage);
        match (ordinal, self.access) {
            (1 | 2 | 5, Access::Register) | (3 | 4, Access::Lookup) => refused(),
            (1, Access::Lookup) => {
                let Some(name) = arguments::<String>(d) else {
                    return bad;
                };
                if let Some(outcome) = self.drivers.lookup(cx, &name) {
                    return outcome;
                }
                let mut registry = self.registry.borrow_mut();
                match registry.connect(&name) {
                    None if registry.awaited() => {
                        registry.waiting.push((name, cx.defer()));
                        Outcome::Deferred
                    }
                    found => lookup_outcome(found),
                }
            }
            (2, Access::Lookup) => match d.finish() {
                Ok(()) => Outcome::reply(self.drivers.list()),
                Err(_) => bad,
            },
            (5, Access::Lookup) => match d.finish() {
                Ok(()) => {
                    let blocks: Vec<Block> = self.registry.borrow().blocks().cloned().collect();
                    Outcome::reply(blocks)
                }
                Err(_) => bad,
            },
            (3, Access::Register) => {
                let Some((block, listener)) = arguments::<(Block, Channel)>(d) else {
                    return bad;
                };
                let published = self.drivers.list().iter().any(|e| e.0 == block.serial);
                let registered = self
                    .registry
                    .borrow_mut()
                    .register(block, listener, published);
                self.answer_waiting(cx);
                match registered {
                    Ok(()) => Outcome::reply(()),
                    Err(error) => Outcome::Failure(error.code()),
                }
            }
            (4, Access::Register) => {
                let Some((name, listener)) = arguments::<(String, Channel)>(d) else {
                    return bad;
                };
                let published = self.drivers.list().iter().any(|e| e.0 == name);
                let registered = self
                    .registry
                    .borrow_mut()
                    .register_net(name, listener, published);
                self.answer_waiting(cx);
                match registered {
                    Ok(()) => Outcome::reply(()),
                    Err(error) => Outcome::Failure(error.code()),
                }
            }
            _ => Outcome::Status(Status::UnknownMethod),
        }
    }

    /// A registrar's connection closing says its driver has finished: the lookups it may
    /// have left waiting are answered by the device manager's loop
    /// ([`Registry::take_ready`]).
    fn closed(&mut self) {
        if let Some(number) = self.slot
            && let Ok(mut registry) = self.registry.try_borrow_mut()
        {
            registry.finished(number);
        }
    }
}
