//! The kernel's calls, through `moruna-sys` (SDD 29 d.2): the plane as the engine runs it
//! on the Moruna kernel.

use moruna_abi::{Error, PAGE_SIZE, Rights, Signals, map};
use moruna_sys::{
    Channel, Memory, WaitItem,
    plane::{Slot, SlotEvent},
};

use crate::calls::{Calls, Ready, Taken};

/// The calls of the process the engine runs in.
#[derive(Debug, Default)]
pub struct Kernel;

/// The pages that hold `offset .. offset + len`: where they start, and their length.
fn span(offset: u64, len: usize) -> Result<(u64, u64, usize), Error> {
    let first = offset & !(PAGE_SIZE - 1);
    let end = offset
        .checked_add(len as u64)
        .and_then(|e| e.checked_add(PAGE_SIZE - 1))
        .ok_or(Error::OutOfRange)?
        & !(PAGE_SIZE - 1);
    Ok((first, end - first, (offset - first) as usize))
}

impl Calls for Kernel {
    type Memory = Memory;
    type Slot = Slot;
    type Channel = Channel;

    fn memory(&mut self, bytes: u64) -> Result<Memory, Error> {
        Memory::create(bytes)
    }

    fn share(&mut self, memory: &Memory) -> Result<Memory, Error> {
        memory
            .0
            .duplicate(Rights::READ | Rights::MAP | Rights::TRANSFER)
            .map(Memory)
    }

    fn read(&mut self, memory: &Memory, offset: u64, buf: &mut [u8]) -> Result<(), Error> {
        if buf.is_empty() {
            return Ok(());
        }
        let (first, len, within) = span(offset, buf.len())?;
        let mapping = memory.map(first, len, map::READ)?;
        mapping.read(within, buf)
    }

    fn write(&mut self, memory: &Memory, offset: u64, data: &[u8]) -> Result<(), Error> {
        if data.is_empty() {
            return Ok(());
        }
        let (first, len, within) = span(offset, data.len())?;
        let mapping = memory.map(first, len, map::READ | map::WRITE)?;
        mapping.write(within, data)
    }

    fn slot(&mut self, output: bool) -> Result<Slot, Error> {
        Slot::create(output)
    }

    fn push(
        &mut self,
        slot: &Slot,
        memory: Memory,
        offset: u64,
        len: u64,
        cookie: u64,
    ) -> Result<(), (Error, Memory)> {
        slot.push(memory, offset, len, cookie)
    }

    fn take(&mut self, slot: &Slot) -> Result<Taken<Memory>, Error> {
        Ok(match slot.take()? {
            SlotEvent::Released { cookie } => Taken::Released { cookie },
            SlotEvent::Put {
                cookie,
                memory,
                offset,
                len,
            } => Taken::Put {
                cookie,
                memory,
                offset,
                len,
            },
            SlotEvent::Returned { cookie, memory } => Taken::Returned { cookie, memory },
        })
    }

    fn close(&mut self, slot: &Slot) -> Result<(), Error> {
        slot.close()
    }

    fn refuse(&mut self, slot: &Slot, reason: &str) -> Result<(), Error> {
        slot.refuse(reason)
    }

    fn channel(&mut self) -> Result<(Channel, Channel), Error> {
        Channel::create()
    }

    fn receive(&mut self, channel: &Channel, buf: &mut [u8]) -> Result<usize, Error> {
        channel.read(buf, &mut []).map(|(n, _)| n)
    }

    fn send(
        &mut self,
        channel: &Channel,
        bytes: &[u8],
        memory: Option<Memory>,
    ) -> Result<(), Error> {
        let mut handles = [memory.map(|m| m.0)];
        let n = usize::from(handles[0].is_some());
        channel.write_with(bytes, &mut handles[..n])
    }

    fn wait(
        &mut self,
        slots: &[&Slot],
        channels: &[&Channel],
        deadline: u64,
    ) -> Result<Ready, Error> {
        let mut items = [WaitItem::raw(0, Signals::NONE); moruna_abi::limits::WAIT_MAX_ITEMS];
        let n = slots.len() + channels.len();
        if n == 0 || n > items.len() {
            return Err(Error::InvalidArgs);
        }
        let awaited = Signals::READABLE | Signals::PEER_CLOSED;
        for (item, slot) in items.iter_mut().zip(slots) {
            *item = WaitItem::new(&slot.0, awaited);
        }
        for (item, channel) in items[slots.len()..].iter_mut().zip(channels) {
            *item = WaitItem::new(&channel.0, awaited);
        }
        match moruna_sys::wait(&mut items[..n], deadline) {
            Ok(_) => Ok(Ready::Some),
            Err(Error::TimedOut) => Ok(Ready::TimedOut),
            Err(e) => Err(e),
        }
    }
}
