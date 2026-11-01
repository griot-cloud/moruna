//! Validated actual shared Memory access. No Rust slice is formed from a peer-writable
//! mapping; private crypto/header slices come only from SDK fresh PrivateMemory.
use moruna_svc::{Error, Handle, Mapping, Memory, ObjectType, Rights, map};
/// Native page granule used by Memory mapping and block registered destinations.
pub const PAGE: u64 = 4096;
/// Actual bounded loan kept alive with its validated mapping through original IO.
pub struct Loan {
    memory: Memory,
    mapping: Mapping<'static>,
    offset: u64,
    length: u64,
}
impl Loan {
    /// Validate handle type/rights and actual mapping backing before consuming peer data.
    pub fn new(memory: Memory, offset: u64, length: u64, writable: bool) -> Result<Self, Error> {
        let (kind, rights) = memory.0.info()?;
        let required = Rights::READ | Rights::MAP;
        if kind != ObjectType::Memory
            || !rights.contains(required)
            || writable && !rights.contains(Rights::WRITE)
        {
            return Err(Error::AccessDenied);
        }
        let end = offset.checked_add(length).ok_or(Error::OutOfRange)?;
        let mapped = end.max(1).checked_add(PAGE - 1).ok_or(Error::OutOfRange)? / PAGE * PAGE;
        let mapping = memory.map_detached(0, mapped, if writable { map::WRITE } else { 0 })?;
        Ok(Self {
            memory,
            mapping,
            offset,
            length,
        })
    }
    /// Exact charged destination span, excluding underlying page rounding.
    pub fn len(&self) -> u64 {
        self.length
    }
    /// Whether the logical range is empty.
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }
    /// Actual byte offset within the original caller's memory object.
    pub fn offset(&self) -> u64 {
        self.offset
    }
    /// Clone only the bounded driver/network-required rights while retaining this original.
    /// Original operations must complete before this owner and associated account retire.
    pub fn duplicate(&self, rights: Rights) -> Result<Memory, Error> {
        Ok(Memory(self.memory.0.duplicate(rights)?))
    }
    /// Volatile copy from shared peer bytes into explicitly charged private staging.
    pub fn read(&self, at: u64, destination: &mut [u8]) -> Result<(), Error> {
        let end = at
            .checked_add(destination.len() as u64)
            .ok_or(Error::OutOfRange)?;
        if end > self.length {
            return Err(Error::OutOfRange);
        }
        let offset = usize::try_from(self.offset.checked_add(at).ok_or(Error::OutOfRange)?)
            .map_err(|_| Error::OutOfRange)?;
        self.mapping.read(offset, destination)
    }
    /// Volatile copy into the validated shared destination after successful object authority.
    pub fn write(&self, at: u64, source: &[u8]) -> Result<(), Error> {
        let end = at
            .checked_add(source.len() as u64)
            .ok_or(Error::OutOfRange)?;
        if end > self.length {
            return Err(Error::OutOfRange);
        }
        let offset = usize::try_from(self.offset.checked_add(at).ok_or(Error::OutOfRange)?)
            .map_err(|_| Error::OutOfRange)?;
        self.mapping.write(offset, source)
    }
}
/// Allocate actual requester-account DMA memory, never the service's ambient heap account.
pub fn dma(bytes: u64, account: &Handle) -> Result<Memory, Error> {
    let size = bytes
        .max(1)
        .checked_add(PAGE - 1)
        .ok_or(Error::OutOfRange)?
        / PAGE
        * PAGE;
    Memory::create_dma_in(size, account)
}
