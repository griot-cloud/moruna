//! The device manager's directory (SDD 25, d.6), as a library: the drivers' published
//! services, which the process looks up ([`Drivers`]), and the block devices registered by
//! drivers the node's supervisor starts ([`Registry`]). `main.rs` is the process; on a host
//! it is an empty program, and this library is what the host tests run.
//!
//! What a connection may ask depends on the listener it came through ([`Access`]): a
//! `directory*` listener's may look up and list, and a `register*` listener's may register a
//! block device and nothing else. The boot description links a `register*` listener only to
//! a driver the supervisor starts (item 1.10b: `storvsc`), so no client can put a name in
//! the directory, and that driver can learn nothing from it.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{rc::Rc, string::String, vec::Vec};
use core::cell::RefCell;

use devices_protocol::{Block, Entry, REGISTER_SLOTS, directory};
use moruna_svc::{
    Channel, Context, Decoder, Handler, Outcome, ProtocolError, ProtocolId, Signals, Status, Wire,
    connect, wait_one,
};

/// What a connection to the directory may ask, by the listener it came through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// A client's: `lookup` and `list`.
    Lookup,
    /// A driver's that the supervisor starts: `register` alone.
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

/// A registered block device, and the device manager's end of the listener its driver
/// serves it on.
struct Registered {
    block: Block,
    listener: Channel,
}

impl Registered {
    /// Whether its driver has let go of its listener.
    fn gone(&self) -> bool {
        wait_one(&self.listener.0, Signals::PEER_CLOSED, 0).is_ok()
    }
}

/// The block devices drivers have registered, by serial.
#[derive(Default)]
pub struct Registry {
    blocks: Vec<Registered>,
}

impl Registry {
    /// No block device registered.
    pub fn new() -> Registry {
        Registry::default()
    }

    /// Registers `block`, served on `listener`, unless its serial is a name taken already:
    /// `published` says whether a driver's service has it. A serial whose driver has let go
    /// of its listener is taken no more: the driver started again registers it again.
    pub fn register(
        &mut self,
        block: Block,
        listener: Channel,
        published: bool,
    ) -> Result<(), directory::Error> {
        if !block.valid() {
            return Err(directory::Error::Invalid);
        }
        if published {
            return Err(directory::Error::Exists);
        }
        match self
            .blocks
            .iter()
            .position(|registered| registered.block.serial == block.serial)
        {
            Some(at) if self.blocks[at].gone() => {
                self.blocks[at] = Registered { block, listener };
            }
            Some(_) => return Err(directory::Error::Exists),
            None => self.blocks.push(Registered { block, listener }),
        }
        Ok(())
    }

    /// The block device registered under `serial`.
    pub fn find(&self, serial: &str) -> Option<&Block> {
        self.entry(serial).map(|registered| &registered.block)
    }

    fn entry(&self, serial: &str) -> Option<&Registered> {
        self.blocks
            .iter()
            .find(|registered| registered.block.serial == serial)
    }

    /// A new connection to the block device registered under `serial`: `None` if none is,
    /// and [`directory::Error::Unavailable`] once its driver has let go of its listener.
    pub fn connect(&self, serial: &str) -> Option<Result<Channel, directory::Error>> {
        let registered = self.entry(serial)?;
        Some(connect(&registered.listener).map_err(|_| directory::Error::Unavailable))
    }

    /// Every block device registered, in the order they were.
    pub fn blocks(&self) -> impl Iterator<Item = &Block> {
        self.blocks.iter().map(|registered| &registered.block)
    }
}

/// One connection to the directory.
pub struct Directory<D: Drivers> {
    drivers: D,
    registry: Rc<RefCell<Registry>>,
    access: Access,
}

impl<D: Drivers> Directory<D> {
    /// A connection through a listener of `access`, served from `drivers` and `registry`.
    pub fn new(drivers: D, registry: Rc<RefCell<Registry>>, access: Access) -> Directory<D> {
        Directory {
            drivers,
            registry,
            access,
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
            (1 | 2 | 4, Access::Register) | (3, Access::Lookup) => refused(),
            (1, Access::Lookup) => {
                let Some(name) = arguments::<String>(d) else {
                    return bad;
                };
                if let Some(outcome) = self.drivers.lookup(cx, &name) {
                    return outcome;
                }
                match self.registry.borrow().connect(&name) {
                    Some(Ok(channel)) => Outcome::reply(channel),
                    Some(Err(error)) => Outcome::Failure(error.code()),
                    None => Outcome::Failure(directory::Error::NotFound.code()),
                }
            }
            (2, Access::Lookup) => match d.finish() {
                Ok(()) => Outcome::reply(self.drivers.list()),
                Err(_) => bad,
            },
            (4, Access::Lookup) => match d.finish() {
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
                match self
                    .registry
                    .borrow_mut()
                    .register(block, listener, published)
                {
                    Ok(()) => Outcome::reply(()),
                    Err(error) => Outcome::Failure(error.code()),
                }
            }
            _ => Outcome::Status(Status::UnknownMethod),
        }
    }
}
