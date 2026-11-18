//! Hyper-V's mechanism for VMBus (ABI 2.7; `architecture/moruna-os-system.md`, section
//! 1F, "VMBus, split by the microkernel rule"; work item 1.9c).
//!
//! When OSTD has found Hyper-V and enabled CPU 0's SynIC, boot makes `root.hyperv` for the
//! process the manifest names, and these are the six calls it reaches:
//!
//! - `hyperv_post`: `HvCallPostMessage`, at most 240 bytes, answered with Hyper-V's status;
//! - `hyperv_take`: SINT 2's message copied out and its slot emptied, never waiting;
//! - `hyperv_map_events`: CPU 0's event-flags page mapped into the caller, once;
//! - `hyperv_interrupt`: an interrupt object for SINT 2, which is how the caller waits,
//!   and CPU 0's VP index, which VMBus targets;
//! - `hyperv_signal_create`: an `HvSignal` for one connection ID, to give to a driver;
//! - `hyperv_signal`: `HvCallSignalEvent` through an `HvSignal`, and nothing else.
//!
//! Every one but the last needs `root.hyperv` with `WRITE`; the last needs the signal with
//! `WRITE`, which is all a signal can do. A signal's connection ID is fixed when it is
//! made, and a duplicate holds no more rights than its source, so a signal is never
//! widened. The VMBus protocol itself is the `vmbus` process's.

use alloc::sync::Arc;

use moruna_abi::{
    Error, ResourceKind, charge,
    hyperv::{self, Authority},
};
use ostd::{
    arch::kernel::hyperv::{Synic, synic},
    sync::SpinLock,
};

use crate::{
    account::{Account, Charge},
    interrupt::Interrupt,
    memory::MemoryObject,
    object::{Handle, Object},
    process::Process,
    syscall, usercopy,
};

/// What a call returns: `rax` and `rdx`, or the error.
pub(crate) type Answer = Result<(u64, u64), Error>;

/// Whether the core can make `root.hyperv`: OSTD found Hyper-V and enabled CPU 0's SynIC.
pub fn available() -> bool {
    synic().is_some()
}

/// The right to signal one Hyper-V connection ID (`ObjectType::HvSignal`).
pub struct HvSignal {
    connection_id: u32,
    _charge: Charge,
}

const _: () = assert!(core::mem::size_of::<HvSignal>() as u64 <= charge::OBJECT);

/// CPU 0's event-flags page as an object a process maps, made at its first mapping. It
/// is the kernel's, so its record is charged to an account of its own, as the clock
/// page's is.
static EVENTS: SpinLock<Option<Arc<MemoryObject>>> = SpinLock::new(None);

/// The SynIC, if `root` names `root.hyperv` with the right the ABI's table gives
/// (`hyperv::authority`).
fn root(p: &Process, root: u64) -> Result<&'static Synic, Error> {
    syscall::typed(p, root, Authority::Root.right(), |h| {
        syscall::as_resource(h, ResourceKind::Hyperv)
    })?;
    // Only a machine with the SynIC has `root.hyperv`.
    synic().ok_or(Error::BadState)
}

/// A 32-bit argument: a connection ID or a message type.
fn word(value: u64) -> Result<u32, Error> {
    u32::try_from(value).map_err(|_| Error::InvalidArgs)
}

fn insert(p: &Process, object: Object) -> Answer {
    let value = p.insert(Handle::new(object))?;
    Ok((value as u64, 0))
}

/// `hyperv_post(root, connection, type, payload, length)`.
pub(crate) fn hyperv_post(p: &Process, a: [u64; 6]) -> Answer {
    let synic = root(p, a[0])?;
    let (connection_id, message_type) = (word(a[1])?, word(a[2])?);
    let length = a[4];
    if !hyperv::payload_fits(length) {
        return Err(Error::InvalidArgs);
    }
    let mut payload = [0; hyperv::PAYLOAD_MAX];
    let payload = &mut payload[..length as usize];
    usercopy::read_exact(&p.space, a[3], payload)?;
    let status = synic
        .post_message(connection_id, message_type, payload)
        .ok_or(Error::InvalidArgs)?;
    Ok((u64::from(status), 0))
}

/// `hyperv_take(root, out)`.
pub(crate) fn hyperv_take(p: &Process, a: [u64; 6]) -> Answer {
    let synic = root(p, a[0])?;
    let mut message = [0; hyperv::MESSAGE_SIZE];
    // The buffer is written before the slot is emptied, so that a message is never taken
    // and then lost to a buffer the caller cannot have written.
    usercopy::write(&p.space, a[1], &message)?;
    if !synic.take_message(&mut message) {
        return Ok((0, 0));
    }
    usercopy::write(&p.space, a[1], &message)?;
    Ok((1, 0))
}

/// `hyperv_map_events(root)`.
pub(crate) fn hyperv_map_events(p: &Process, a: [u64; 6]) -> Answer {
    let synic = root(p, a[0])?;
    let page = {
        let mut events = EVENTS.lock();
        match &*events {
            Some(page) => page.clone(),
            None => {
                let account = Account::machine(charge::memory(1));
                let page = Arc::new(MemoryObject::of_frame(synic.event_flags(), &account)?);
                *events = Some(page.clone());
                page
            }
        }
    };
    const _: () = assert!(Synic::EVENT_FLAGS_OFFSET == hyperv::EVENT_FLAGS_OFFSET);
    Ok((p.space.map_page_once_writable(&page)?, 0))
}

/// `hyperv_interrupt(root)`: the interrupt, and in `rdx` CPU 0's VP index.
pub(crate) fn hyperv_interrupt(p: &Process, a: [u64; 6]) -> Answer {
    let synic = root(p, a[0])?;
    let interrupt = Interrupt::sint(synic.interrupt(), p.account())?;
    let (handle, _) = insert(p, Object::Interrupt(Arc::new(interrupt)))?;
    Ok((handle, u64::from(synic.vp_index())))
}

/// `hyperv_signal_create(root, connection)`.
pub(crate) fn hyperv_signal_create(p: &Process, a: [u64; 6]) -> Answer {
    root(p, a[0])?;
    let signal = HvSignal {
        connection_id: word(a[1])?,
        _charge: Charge::new(p.account(), charge::OBJECT)?,
    };
    insert(p, Object::HvSignal(Arc::new(signal)))
}

/// `hyperv_signal(signal)`: event flag 0 of the signal's connection ID, the flag every
/// VMBus channel is signalled on.
pub(crate) fn hyperv_signal(p: &Process, a: [u64; 6]) -> Answer {
    let signal = syscall::typed(p, a[0], Authority::Signal.right(), |h| match &h.object {
        Object::HvSignal(s) => Ok(s.clone()),
        _ => Err(Error::WrongType),
    })?;
    let synic = synic().ok_or(Error::BadState)?;
    Ok((u64::from(synic.signal_event(signal.connection_id, 0)), 0))
}
