//! Pre-reserved reusable DMA/control ownership for bank-copy maintenance. This child
//! uses the SAME Controller/lease/original map, not another block connection or codec.
use super::*;
use crate::memory::{Loan, dma};
use moruna_svc::PrivateMemory;
/// One charged DMA page/span and control reservation. No accessor exposes mutable
/// backing while an original request may own it. A dropped/uncertain future seals reuse.
pub(crate) struct Prepared {
    owner: Rc<core::cell::RefCell<Controller>>,
    base: u64,
    sectors: u64,
    account: Rc<moruna_sys::Account>,
    memory: Memory,
    loan: Loan,
    budget: Memory,
    bytes: usize,
    in_flight: bool,
}
impl Prepared {
    /// Allocate and validate all backing BEFORE the caller freezes logical admissions.
    pub(crate) fn new(
        device: &Device,
        account: Rc<moruna_sys::Account>,
        bytes: usize,
    ) -> Result<Self, Error> {
        let sector = device.info.sector_size as usize;
        if bytes == 0 || !bytes.is_multiple_of(sector) {
            return Err(Error::Geometry);
        }
        let memory = dma(bytes as u64, &account.0).map_err(Error::Memory)?;
        let budget = Memory::create_in(4096, &account.0).map_err(Error::Memory)?;
        if !account.matches(&memory.0).map_err(Error::Memory)?
            || !account.matches(&budget.0).map_err(Error::Memory)?
        {
            return Err(Error::Geometry);
        }
        let loan = Loan::new(
            Memory(
                memory
                    .0
                    .duplicate(Rights::READ | Rights::WRITE | Rights::MAP)
                    .map_err(Error::Memory)?,
            ),
            0,
            bytes as u64,
            true,
        )
        .map_err(Error::Memory)?;
        Ok(Self {
            owner: device.controller.clone(),
            base: device.base_sector,
            sectors: device.info.sectors,
            account,
            memory,
            loan,
            budget,
            bytes,
            in_flight: false,
        })
    }
    fn device(&self, device: &Device) -> Result<(), Error> {
        if self.in_flight
            || !Rc::ptr_eq(&self.owner, &device.controller)
            || self.base != device.base_sector
            || self.sectors != device.info.sectors
        {
            return Err(Error::Busy);
        }
        Ok(())
    }
    fn submit(
        &mut self,
        device: &Device,
        offset: u64,
        bytes: usize,
        phase: Phase,
    ) -> Result<Ticket, Error> {
        self.device(device)?;
        let flush = matches!(phase, Phase::Flush);
        let sector = u64::from(device.info.sector_size);
        if bytes > self.bytes
            || !offset.is_multiple_of(sector)
            || !(bytes as u64).is_multiple_of(sector)
            || (!flush && bytes == 0)
        {
            return Err(Error::Geometry);
        }
        let count = u32::try_from(bytes as u64 / sector).map_err(|_| Error::Geometry)?;
        let at = if flush {
            0
        } else {
            device.sector(offset / sector, count)?
        };
        let original = if flush {
            None
        } else {
            Some(Memory(
                self.memory
                    .0
                    .duplicate(
                        Rights::READ
                            | Rights::WRITE
                            | Rights::MAP
                            | Rights::DUPLICATE
                            | Rights::TRANSFER,
                    )
                    .map_err(Error::Memory)?,
            ))
        };
        let budget = Memory(
            self.budget
                .0
                .duplicate(Rights::READ | Rights::TRANSFER)
                .map_err(Error::Memory)?,
        );
        let mut owner = self.owner.try_borrow_mut().map_err(|_| Error::Busy)?;
        let id = owner.admit_async()?;
        let mut body = Encoder::new();
        let ordinal = match phase {
            Phase::Read => 2,
            Phase::Write => {
                owner.lease.encode(&mut body).map_err(|_| Error::Geometry)?;
                3
            }
            Phase::Flush => {
                owner.lease.encode(&mut body).map_err(|_| Error::Geometry)?;
                4
            }
            _ => return Err(Error::Geometry),
        };
        if let Some(memory) = &original {
            at.encode(&mut body).map_err(|_| Error::Geometry)?;
            count.encode(&mut body).map_err(|_| Error::Geometry)?;
            let rights = Rights::READ
                | Rights::MAP
                | Rights::TRANSFER
                | if matches!(phase, Phase::Read) {
                    Rights::WRITE
                } else {
                    Rights::READ
                };
            Memory(memory.0.duplicate(rights).map_err(Error::Memory)?)
                .encode(&mut body)
                .map_err(|_| Error::Geometry)?;
        }
        let txid = owner
            .client
            .as_mut()
            .ok_or(Error::Closed)?
            .core()
            .send(ordinal, body)
            .map_err(|error| Error::Driver(error.typed()))?;
        let deadline = moruna_svc::deadline_in(owner.timeout_ns);
        owner.async_state.pending.insert(
            id,
            Pending {
                txid,
                phase,
                original,
                account: Some(self.account.clone()),
                _budget: Some(budget),
                sector: at,
                count,
                region_base: self.base,
                region_sectors: self.sectors,
                bytes: bytes as u64,
                cancelled: false,
                deadline,
                expired: false,
                quarantine: None,
            },
        );
        self.in_flight = true;
        Ok(Ticket {
            owner: self.owner.clone(),
            id,
            base: self.base,
            sectors: self.sectors,
        })
    }
    async fn complete(
        &mut self,
        device: &Device,
        ticket: Ticket,
        bytes: usize,
        stable: bool,
    ) -> Result<(), Error> {
        let completion = core::future::poll_fn(|_cx| match device.poll(&ticket) {
            Ok(Poll::Pending { .. }) => core::task::Poll::Pending,
            Ok(Poll::Complete(done)) => core::task::Poll::Ready(Ok(done)),
            Ok(Poll::Quarantined(error)) | Err(error) => core::task::Poll::Ready(Err(error)),
        })
        .await?;
        // Only an actual validated ORIGINAL completion permits reusing private backing.
        // If the returned driver result is an error, backing is quiescent but this whole
        // maintenance transaction must still fail; it cannot acknowledge durability.
        self.in_flight = false;
        let actual = completion.result?;
        if actual != bytes as u64 || stable && !completion.stable {
            return Err(Error::Geometry);
        }
        Ok(())
    }
    /// Read actual original bytes into already reserved private staging; no backing
    /// allocation is performed between freeze and the original completion.
    pub(crate) async fn read(
        &mut self,
        device: &Device,
        offset: u64,
        dst: &mut PrivateMemory,
        bytes: usize,
    ) -> Result<(), Error> {
        self.device(device)?;
        let max = device.info.max_sectors as usize * device.info.sector_size as usize;
        if bytes > self.bytes {
            return Err(Error::Geometry);
        }
        for at in (0..bytes).step_by(max) {
            let len = (bytes - at).min(max);
            let ticket = self.submit(device, offset + at as u64, len, Phase::Read)?;
            self.complete(device, ticket, len, false).await?;
            dst.with_bytes(|private| {
                let range = private.get_mut(at..at + len).ok_or(Error::Geometry)?;
                self.loan.read(0, range).map_err(Error::Memory)
            })
            .map_err(Error::Memory)??;
        }
        Ok(())
    }
    /// Complete each original write before reuse; source is copied into preallocated
    /// DMA via safe volatile access, never a shared mutable Rust slice.
    pub(crate) async fn write(
        &mut self,
        device: &Device,
        offset: u64,
        src: &mut PrivateMemory,
        bytes: usize,
    ) -> Result<(), Error> {
        self.device(device)?;
        let max = device.info.max_sectors as usize * device.info.sector_size as usize;
        if bytes > self.bytes {
            return Err(Error::Geometry);
        }
        for at in (0..bytes).step_by(max) {
            let len = (bytes - at).min(max);
            src.with_bytes(|private| {
                let range = private.get(at..at + len).ok_or(Error::Geometry)?;
                self.loan.write(0, range).map_err(Error::Memory)
            })
            .map_err(Error::Memory)??;
            let ticket = self.submit(device, offset + at as u64, len, Phase::Write)?;
            self.complete(device, ticket, len, false).await?;
        }
        Ok(())
    }
    /// Actual physical flush original, using the same pre-reserved control owner.
    pub(crate) async fn flush(&mut self, device: &Device) -> Result<(), Error> {
        let ticket = self.submit(device, 0, 0, Phase::Flush)?;
        self.complete(device, ticket, 0, true).await
    }
}
