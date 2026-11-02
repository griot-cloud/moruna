//! Actual major-2 block driver consumer. Namespace mutation requires a real exclusive
//! connection lease. No device is formatted or opened implicitly and no file emulation
//! is used. Timeout/refusal closes this adapter to new IO; peer-owned original pins and
//! driver lease drain remain live until original device completions/reset prove quiescence.
use alloc::{rc::Rc, string::String, vec::Vec};
use block_protocol::{Info, block};
use core::cell::RefCell;
use devices_protocol::directory;
use moruna_svc::{CallError, DEADLINE_INFINITE, Encoder, Memory, Wire};

#[path = "device_async.rs"]
mod async_io;
pub(crate) use async_io::reserved::Prepared as ReservedIo;
pub use async_io::{CancelState, Completion, MAX_IN_FLIGHT, Observation, Observed, Poll, Ticket};
/// Exact native block refusal or transport diagnostic; never replaced with empty data.
#[derive(Clone, Debug)]
pub enum Error {
    /// Actual driver/SDK call result, including lease and device failure.
    Driver(CallError<block::Error>),
    /// Actual directory lookup failure.
    Directory(CallError<directory::Error>),
    /// Invalid capacity, alignment, geometry, or request bounds.
    Geometry,
    /// The adapter was closed after an uncertain original operation or explicit release.
    Closed,
    /// Shared physical controller is executing another admitted original operation.
    Busy,
    /// Actual SDK Memory/account refusal before submission.
    Memory(moruna_svc::Error),
    /// Original operation cannot yet be proven quiescent; its owners remain retained.
    Quarantined,
}
/// One actual exclusive namespace connection. Dropping it closes the SDK channel; the
/// driver must retain its lease through original CQEs and final durability barrier.
pub struct Device {
    controller: Rc<RefCell<Controller>>,
    base_sector: u64,
    info: Info,
}
impl Device {
    /// One actual physical connection/lease, never one writer per logical region.
    pub fn open(
        directory: &mut directory::Client,
        name: &str,
        timeout_ns: u64,
    ) -> Result<Self, Error> {
        let controller = Controller::open(directory, name, timeout_ns)?;
        let info = controller.info;
        Ok(Self {
            controller: Rc::new(RefCell::new(controller)),
            base_sector: 0,
            info,
        })
    }
    /// Split one owned namespace into explicit disjoint checked views. No second lease
    /// is acquired; base+range is checked against actual capacity before any view exists.
    pub fn regions(
        self,
        regions: &[moruna_storage_format::config::Region],
    ) -> Result<Vec<Self>, Error> {
        let sector = self.info.sector_size as u64;
        moruna_storage_format::config::validate_regions(
            regions,
            self.info.sector_size,
            self.capacity(),
        )
        .map_err(|_| Error::Geometry)?;
        let mut views = Vec::new();
        for region in regions {
            let mut info = self.info;
            info.sectors = region.bytes / sector;
            views.push(Self {
                controller: self.controller.clone(),
                base_sector: self
                    .base_sector
                    .checked_add(region.offset / sector)
                    .ok_or(Error::Geometry)?,
                info,
            });
        }
        Ok(views)
    }
    /// Actual logical view geometry derived from validated physical geometry/bounds.
    pub fn info(&self) -> Info {
        self.info
    }
    /// Checked logical capacity, never an asserted capacity larger than the real device.
    pub fn capacity(&self) -> u64 {
        self.info.sectors * self.info.sector_size as u64
    }
    fn sector(&self, sector: u64, count: u32) -> Result<u64, Error> {
        if count == 0
            || count > self.info.max_sectors
            || sector
                .checked_add(count as u64)
                .is_none_or(|end| end > self.info.sectors)
        {
            return Err(Error::Geometry);
        }
        self.base_sector.checked_add(sector).ok_or(Error::Geometry)
    }
    /// Read through the one physical owner with exact checked region translation.
    pub fn read(&mut self, sector: u64, count: u32, loan: Memory) -> Result<Memory, Error> {
        let sector = self.sector(sector, count)?;
        self.controller
            .try_borrow_mut()
            .map_err(|_| Error::Busy)?
            .read(sector, count, loan)
    }
    /// Write through that SAME namespace lease; a region cannot escape into its neighbor.
    pub fn write(&mut self, sector: u64, count: u32, loan: Memory) -> Result<Memory, Error> {
        let sector = self.sector(sector, count)?;
        self.controller
            .try_borrow_mut()
            .map_err(|_| Error::Busy)?
            .write(sector, count, loan)
    }
    /// Actual device flush, covering acknowledged journal/cache writes across regions.
    pub fn flush(&mut self) -> Result<(), Error> {
        self.controller
            .try_borrow_mut()
            .map_err(|_| Error::Busy)?
            .flush()
    }
    /// Exact last physical IO diagnostic; borrow contention makes observation unavailable.
    pub fn last_error(&self) -> Option<Error> {
        self.controller
            .try_borrow()
            .ok()
            .and_then(|controller| controller.last_error().cloned())
    }
    /// Observed soft deadlines, without interpreting them as IO cancellation/completion.
    pub fn soft_timeouts(&self) -> Option<u64> {
        self.controller
            .try_borrow()
            .ok()
            .map(|controller| controller.soft_timeouts())
    }
    /// Release only the final physical owner. Other live region views cannot have their
    /// lease revoked by closing one sibling; ordinary Drop keeps the shared owner alive.
    pub fn close(self) -> Result<(), Error> {
        Rc::try_unwrap(self.controller)
            .map_err(|_| Error::Busy)?
            .into_inner()
            .close()
    }
}
struct Controller {
    client: Option<block::Client>,
    lease: u64,
    info: Info,
    timeout_ns: u64,
    soft_timeouts: u64,
    last_error: Option<Error>,
    async_state: async_io::State,
}
impl Controller {
    /// Resolve exactly the configured directory name, handshake major 2 and claim its
    /// actual writer lease. A busy namespace refuses; no alternate disk is selected.
    pub fn open(
        directory: &mut directory::Client,
        name: &str,
        timeout_ns: u64,
    ) -> Result<Self, Error> {
        if name.is_empty() || timeout_ns == 0 || timeout_ns == u64::MAX {
            return Err(Error::Geometry);
        }
        let channel = directory
            .lookup(String::from(name))
            .map_err(Error::Directory)?;
        let mut client = block::Client::connect(channel, moruna_svc::deadline_in(timeout_ns))
            .map_err(Error::Driver)?;
        client.core().set_timeout(timeout_ns);
        let info = client.info().map_err(Error::Driver)?;
        if info.read_only
            || info.sector_size < 512
            || !info.sector_size.is_power_of_two()
            || info.max_sectors == 0
            || info.sectors == 0
            || info.sectors.checked_mul(info.sector_size as u64).is_none()
        {
            return Err(Error::Geometry);
        }
        let lease = client.acquire_write().map_err(Error::Driver)?;
        if lease == 0 {
            return Err(Error::Geometry);
        }
        Ok(Self {
            client: Some(client),
            lease,
            info,
            timeout_ns,
            soft_timeouts: 0,
            last_error: None,
            async_state: async_io::State::new(),
        })
    }
    /// Validated real geometry; no caller-supplied capacity is substituted.
    /// Checked capacity in bytes, established by the real driver's info reply.
    fn bounds(&self, sector: u64, count: u32) -> Result<(), Error> {
        if count == 0
            || count > self.info.max_sectors
            || sector
                .checked_add(count as u64)
                .is_none_or(|end| end > self.info.sectors)
        {
            return Err(Error::Geometry);
        }
        Ok(())
    }
    fn result<T>(&mut self, result: Result<T, CallError<block::Error>>) -> Result<T, Error> {
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                self.client = None;
                let diagnostic = Error::Driver(error);
                self.last_error = Some(diagnostic.clone());
                Err(diagnostic)
            }
        }
    }
    /// Actual last driver/transport diagnostic, retained independently of compact wire refusal.
    pub fn last_error(&self) -> Option<&Error> {
        self.last_error.as_ref()
    }
    /// Soft wait deadlines that expired while the original request remained owned.
    /// They never cancel/release its Memory or turn a cancellation ack into completion.
    pub fn soft_timeouts(&self) -> u64 {
        self.soft_timeouts
    }
    fn original<T: Wire>(&mut self, ordinal: u16, body: Encoder) -> Result<T, Error> {
        let result = self.original_once(ordinal, body);
        if let Err(cause) = &result {
            self.last_error = Some(cause.clone());
            if !matches!(cause, Error::Busy) {
                self.client = None;
            }
        }
        result
    }
    fn original_once<T: Wire>(&mut self, ordinal: u16, body: Encoder) -> Result<T, Error> {
        if self.async_state.has_pending() {
            return Err(Error::Busy);
        }

        let client = self.client.as_mut().ok_or(Error::Closed)?;
        let id = client
            .core()
            .send(ordinal, body)
            .map_err(|error| Error::Driver(error.typed()))?;
        let frame = client
            .core()
            .reply(id, moruna_svc::deadline_in(self.timeout_ns));
        let mut frame = match frame {
            Err(CallError::TimedOut) => {
                // Retain the original stack/loan and request ID. Never resubmit. The
                // actual driver watchdog/reset path must answer after DMA quiescence.
                self.soft_timeouts = self.soft_timeouts.saturating_add(1);
                client.core().reply(id, DEADLINE_INFINITE)
            }
            other => other,
        }
        .map_err(|error| Error::Driver(error.typed()))?;
        let mut decoder = frame.decoder();
        let value = T::decode(&mut decoder).map_err(|_| Error::Driver(CallError::Malformed))?;
        decoder
            .finish()
            .map_err(|_| Error::Driver(CallError::Malformed))?;
        Ok(value)
    }
    /// Actual driver read into caller-owned DMA Memory. Caller retains its original loan
    /// until this original reply or driver-owned pin recovery; no host slice is forged.
    pub fn read(&mut self, sector: u64, count: u32, loan: Memory) -> Result<Memory, Error> {
        self.bounds(sector, count)?;
        let mut body = Encoder::new();
        sector.encode(&mut body).map_err(|_| Error::Geometry)?;
        count.encode(&mut body).map_err(|_| Error::Geometry)?;
        loan.encode(&mut body).map_err(|_| Error::Geometry)?;
        self.original(2, body)
    }
    /// Actual driver write under this connection's lease. A failure poisons this adapter;
    /// it never retries an uncertain mutation or reuses a possibly live DMA destination.
    pub fn write(&mut self, sector: u64, count: u32, loan: Memory) -> Result<Memory, Error> {
        self.bounds(sector, count)?;
        let mut body = Encoder::new();
        self.lease.encode(&mut body).map_err(|_| Error::Geometry)?;
        sector.encode(&mut body).map_err(|_| Error::Geometry)?;
        count.encode(&mut body).map_err(|_| Error::Geometry)?;
        loan.encode(&mut body).map_err(|_| Error::Geometry)?;
        self.original(3, body)
    }
    /// Original required device durability barrier under the actual namespace lease.
    pub fn flush(&mut self) -> Result<(), Error> {
        if self.async_state.has_pending() {
            return Err(Error::Busy);
        }

        let result = self.client.as_mut().ok_or(Error::Closed)?.flush(self.lease);
        self.result(result)
    }
    /// Explicit successful shutdown only after the driver's original IO/final flush. A
    /// failed release preserves its exact diagnostic and closes admission, not success.
    pub fn close(mut self) -> Result<(), Error> {
        if self.async_state.has_pending() {
            return Err(Error::Busy);
        }

        let result = self
            .client
            .as_mut()
            .ok_or(Error::Closed)?
            .release_write(self.lease);
        self.result(result)
    }
}
