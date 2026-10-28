//! Kernel objects and handles (SDD 20, e.1 and e.2).
//!
//! An object is reached only through a handle: an object plus the rights the holder
//! has over it. The set of object types is the kernel's whole vocabulary; everything
//! else is built by processes out of these.

use alloc::sync::Arc;
use core::ops::Range;

use moruna_abi::{ObjectType, ResourceKind, Rights, resource_range};

use crate::{
    channel::Endpoint,
    clock::Timer,
    interrupt::Interrupt,
    memory::{DeviceMemory, MemoryObject},
    process::{Process, Thread},
    signal::Signaller,
};

/// The kernel's debug console: a write-only sink to the serial line, given only to the
/// processes the manifest marks.
pub struct Console;

/// A capability to create objects of one kind out of nothing: processes, or device
/// memory and interrupt lines within `range` (physical addresses or GSIs). A root
/// resource covers its kind's whole range; `resource_narrow` derives one over a
/// sub-range, so a supervisor can hand an untrusted driver only its own device.
pub struct Resource {
    pub kind: ResourceKind,
    pub range: Range<u64>,
}

impl Resource {
    /// The root resource of `kind`, covering everything of that kind.
    pub fn root(kind: ResourceKind) -> Resource {
        let range = match kind {
            ResourceKind::Process => 0..0,
            ResourceKind::DeviceMemory => 0..resource_range::DEVICE_END,
            ResourceKind::Interrupt => 0..resource_range::INTERRUPT_END,
        };
        Resource { kind, range }
    }

    /// Whether `start .. start + len` lies inside this resource's range.
    pub fn covers(&self, start: u64, len: u64) -> bool {
        start
            .checked_add(len)
            .is_some_and(|end| start >= self.range.start && end <= self.range.end)
    }
}

#[derive(Clone)]
pub enum Object {
    Process(Arc<Process>),
    Thread(Arc<Thread>),
    Channel(Arc<Endpoint>),
    Memory(Arc<MemoryObject>),
    Timer(Arc<Timer>),
    Interrupt(Arc<Interrupt>),
    DeviceMemory(Arc<DeviceMemory>),
    Console(Arc<Console>),
    Resource(Arc<Resource>),
}

impl Object {
    pub fn kind(&self) -> ObjectType {
        match self {
            Object::Process(_) => ObjectType::Process,
            Object::Thread(_) => ObjectType::Thread,
            Object::Channel(_) => ObjectType::Channel,
            Object::Memory(_) => ObjectType::Memory,
            Object::Timer(_) => ObjectType::Timer,
            Object::Interrupt(_) => ObjectType::Interrupt,
            Object::DeviceMemory(_) => ObjectType::DeviceMemory,
            Object::Console(_) => ObjectType::Console,
            Object::Resource(_) => ObjectType::Resource,
        }
    }

    /// The signals of a waitable object; `None` for an object with none.
    pub fn signaller(&self) -> Option<&Signaller> {
        match self {
            Object::Process(p) => Some(&p.signaller),
            Object::Thread(t) => Some(&t.signaller),
            Object::Channel(e) => Some(e.signaller()),
            Object::Timer(t) => Some(&t.signaller),
            Object::Interrupt(i) => Some(&*i.signaller),
            Object::Memory(_)
            | Object::DeviceMemory(_)
            | Object::Console(_)
            | Object::Resource(_) => None,
        }
    }

    /// The rights a newly created object's first handle carries.
    pub fn default_rights(kind: ObjectType) -> Rights {
        let base = Rights::DUPLICATE | Rights::TRANSFER;
        match kind {
            ObjectType::Process
            | ObjectType::Thread
            | ObjectType::Channel
            | ObjectType::Timer
            | ObjectType::Interrupt => base | Rights::READ | Rights::WRITE | Rights::WAIT,
            ObjectType::Memory | ObjectType::DeviceMemory => {
                base | Rights::READ | Rights::WRITE | Rights::MAP
            }
            ObjectType::Console | ObjectType::Resource => base | Rights::WRITE,
        }
    }
}

#[derive(Clone)]
pub struct Handle {
    pub object: Object,
    pub rights: Rights,
}

impl Handle {
    /// A handle with the default rights of the object's type.
    pub fn new(object: Object) -> Handle {
        let rights = Object::default_rights(object.kind());
        Handle { object, rights }
    }
}
