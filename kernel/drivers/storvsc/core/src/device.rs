//! The device: storvsc's initialisation, then requests in flight, each matched to its
//! completion by transaction ID.

use alloc::collections::BTreeMap;

use guestmem_core::ranges::PagedRange;
use storvsp_protocol::{
    ChannelProperties, NtStatus, Operation, ProtocolVersion, VERSION_BLUE, VERSION_THRESHOLD,
    VERSION_WIN6, VERSION_WIN7, VERSION_WIN8,
};
use zerocopy::IntoBytes;

use crate::{
    Address, Channel, Disk, Error, Failure, Malformed, PAGE, Wait,
    ring::{Kind, Received, Rings, field},
    scsi::{self, Command, SERIAL_BYTES},
};

/// The protocol versions offered, newest first: every one `storvsp_protocol` defines.
pub const VERSIONS: [u16; 5] = [
    VERSION_THRESHOLD,
    VERSION_BLUE,
    VERSION_WIN8,
    VERSION_WIN7,
    VERSION_WIN6,
];

/// What [`Storvsc::poll`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The initialisation finished: requests may be made.
    Ready,
    /// A request finished.
    Completed(Completion),
}

/// The end of one request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Completion {
    /// The transaction ID the request returned.
    pub id: u64,
    /// The bytes moved, or why the request failed.
    pub result: Result<u32, Failure>,
}

/// The four steps of the initialisation, in storvsc's order (`storvsc:310-376`).
#[derive(Clone, Copy)]
enum Step {
    Begin,
    /// Asking for `VERSIONS[n]`.
    Version(usize),
    Properties,
    End,
}

#[derive(Clone, Copy)]
enum State {
    /// The step in flight and its transaction ID.
    Starting(Step, u64),
    Ready,
    Failed,
}

/// What is kept of a request in flight.
struct Pending {
    /// The bytes it asked the device to move.
    asked: u32,
    /// Whether success means all of them.
    exact: bool,
}

/// A storvsc device on one VMBus channel.
pub struct Storvsc<C: Channel> {
    channel: C,
    rings: Rings,
    state: State,
    /// The next transaction ID. IDs start at 1 and are never used twice, so a completion
    /// the host sends twice, or late, names nothing in flight.
    next_id: u64,
    /// The most requests in flight; 0 until the properties answer sets it.
    limit: usize,
    pending: BTreeMap<u64, Pending>,
    version: Option<u16>,
    max_transfer: u32,
}

impl<C: Channel> Storvsc<C> {
    /// Takes an open channel and sends the first step of the initialisation. Poll until
    /// [`Event::Ready`].
    pub fn new(channel: C) -> Result<Storvsc<C>, Error> {
        let rings = Rings::new(&channel).map_err(Error::Ring)?;
        let mut device = Storvsc {
            channel,
            rings,
            state: State::Failed,
            next_id: 1,
            limit: 0,
            pending: BTreeMap::new(),
            version: None,
            max_transfer: 0,
        };
        device.start(Step::Begin)?;
        Ok(device)
    }

    /// The protocol version the host accepted, once it has.
    pub fn version(&self) -> Option<u16> {
        self.version
    }

    /// The most bytes one request may carry: the limit in the host's properties answer, and
    /// no more than one packet in this channel's ring can name. Zero until ready.
    pub fn max_transfer_bytes(&self) -> u32 {
        self.max_transfer
    }

    /// The most requests in flight at once: as many as the ring to the host holds of the
    /// largest request, at least 1 and at most 256. The properties answer carries no such
    /// limit (`storvsp_protocol::ChannelProperties`; `storvsp:1101-1146`), so it follows from
    /// the ring and from the largest transfer, which that answer bounds. Zero until ready.
    /// A request the ring still has no room for is refused with [`Error::Full`].
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// The requests sent and not yet completed.
    pub fn in_flight(&self) -> usize {
        self.pending.len()
    }

    /// Asks for the capacity of the disk at `address` (READ CAPACITY 16). The device writes
    /// its answer at the start of the guest page `page`; once the request completes, give
    /// the bytes it reports to [`Disk::from_capacity`].
    pub fn read_capacity(&mut self, address: Address, page: u64) -> Result<u64, Error> {
        self.request(
            address,
            scsi::read_capacity16(),
            Disk::CAPACITY_BYTES,
            &[page],
        )
    }

    /// Asks for the serial of the disk at `address` (INQUIRY, VPD page 0x80). The device
    /// writes its answer at the start of the guest page `page`; once the request completes,
    /// give the bytes it reports to [`serial`](crate::serial).
    pub fn read_serial(&mut self, address: Address, page: u64) -> Result<u64, Error> {
        self.request(address, scsi::inquiry_serial(), SERIAL_BYTES, &[page])
    }

    /// Reads `count` blocks from `block` into the guest pages `pages`, from the first page's
    /// first byte (READ 16).
    pub fn read(
        &mut self,
        disk: &Disk,
        block: u64,
        count: u32,
        pages: &[u64],
    ) -> Result<u64, Error> {
        let bytes = span(disk, block, count)?;
        self.request(disk.address(), scsi::read16(block, count), bytes, pages)
    }

    /// Writes `count` blocks at `block` from the guest pages `pages`, from the first page's
    /// first byte (WRITE 16).
    pub fn write(
        &mut self,
        disk: &Disk,
        block: u64,
        count: u32,
        pages: &[u64],
    ) -> Result<u64, Error> {
        let bytes = span(disk, block, count)?;
        self.request(disk.address(), scsi::write16(block, count), bytes, pages)
    }

    /// Asks the disk at `address` to put every write it has answered on stable storage
    /// (SYNCHRONIZE CACHE).
    pub fn flush(&mut self, address: Address) -> Result<u64, Error> {
        self.request(address, scsi::synchronize_cache(), 0, &[])
    }

    /// Reads what the host has sent: one step of the initialisation, or one completion.
    /// `None` when the ring is empty.
    ///
    /// An error here is about one packet, which is gone, and the device goes on, except
    /// during the initialisation and for [`Error::Ring`]: after those every call answers
    /// [`Error::Failed`].
    pub fn poll(&mut self) -> Result<Option<Event>, Error> {
        loop {
            let starting = match self.state {
                State::Starting(step, id) => Some((step, id)),
                State::Ready => None,
                State::Failed => return Err(Error::Failed),
            };
            let packet = match self.rings.receive(&mut self.channel) {
                Ok(Some(packet)) => packet,
                Ok(None) => return Ok(None),
                Err(e) => return Err(self.broken(e)),
            };
            match starting {
                Some((step, id)) => match self.advance(step, id, &packet) {
                    Ok(true) => return Ok(Some(Event::Ready)),
                    Ok(false) => {}
                    Err(e) => {
                        self.state = State::Failed;
                        return Err(e);
                    }
                },
                None => {
                    if let Some(completion) = self.complete(&packet)? {
                        return Ok(Some(Event::Completed(completion)));
                    }
                }
            }
        }
    }

    /// Sleeps until the host signals, or until `deadline`, nanoseconds on the node's monotonic
    /// clock, passes; with none, only a signal or the channel ends the wait. Returns
    /// [`Wait::Signalled`] at once when there is a packet to read, or room again after a
    /// request was refused with [`Error::Full`] for want of it.
    pub fn wait(&mut self, deadline: Option<u64>) -> Result<Wait, Error> {
        if matches!(self.state, State::Failed) {
            return Err(Error::Failed);
        }
        match self.rings.wait(&mut self.channel, deadline) {
            Ok(wait) => Ok(wait),
            Err(e) => Err(self.broken(e)),
        }
    }

    /// Asks the host to signal the driver when there is something to do, without sleeping:
    /// true when there is already, a packet to [`poll`](Storvsc::poll) or room again after
    /// [`Error::Full`]. A driver that waits on handles, its clients' and the `vmbus` wake
    /// channel's, polls until `None`, then arms, and waits only when this answers false:
    /// the host's next packet signals the channel. Unlike [`wait`](Storvsc::wait), it leaves
    /// the host's signal on.
    pub fn arm(&mut self) -> Result<bool, Error> {
        if matches!(self.state, State::Failed) {
            return Err(Error::Failed);
        }
        match self.rings.arm(&self.channel) {
            Ok(ready) => Ok(ready),
            Err(e) => Err(self.broken(e)),
        }
    }

    /// The channel, for the driver that waits on it: on a node, its wake channel.
    pub fn channel(&self) -> &C {
        &self.channel
    }

    /// A corrupt ring ends the device.
    fn broken(&mut self, e: Error) -> Error {
        if matches!(e, Error::Ring(_)) {
            self.state = State::Failed;
        }
        e
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    /// Sends a step of the initialisation and makes it the one in flight.
    fn start(&mut self, step: Step) -> Result<(), Error> {
        let version;
        let (operation, body): (Operation, &[u8]) = match step {
            Step::Begin => (Operation::BEGIN_INITIALIZATION, &[]),
            Step::Version(n) => {
                // Out of versions to step down to.
                let major_minor = *VERSIONS.get(n).ok_or(Error::NoVersion)?;
                version = ProtocolVersion {
                    major_minor,
                    reserved: 0,
                };
                (Operation::QUERY_PROTOCOL_VERSION, version.as_bytes())
            }
            Step::Properties => (Operation::QUERY_PROPERTIES, &[]),
            Step::End => (Operation::END_INITIALIZATION, &[]),
        };
        let id = self.take_id();
        self.send(id, operation, body, None)?;
        self.state = State::Starting(step, id);
        Ok(())
    }

    fn send(
        &mut self,
        id: u64,
        operation: Operation,
        body: &[u8],
        data: Option<PagedRange<'_>>,
    ) -> Result<(), Error> {
        match self
            .rings
            .send(&mut self.channel, id, operation, body, data)
        {
            Ok(()) => Ok(()),
            Err(e) => Err(self.broken(e)),
        }
    }

    /// Takes the host's answer to the step in flight and sends the next. True once the last
    /// step is answered. During the initialisation the host may send nothing but the
    /// completion of that step (`storvsc:649-661`), and must answer it with success
    /// (`storvsc:663-668`), except that a version it does not speak is answered with
    /// `REVISION_MISMATCH` (`storvsp:1057-1090`), and the next older one is offered.
    fn advance(&mut self, step: Step, id: u64, packet: &Received) -> Result<bool, Error> {
        let (header, body) = match packet.kind {
            Kind::Completion(got) if got == id => packet.completion().map_err(Error::Malformed)?,
            Kind::Completion(got) => return Err(Error::UnknownTransaction(got)),
            Kind::InBand | Kind::Other => {
                return Err(Error::Malformed(Malformed(
                    "a packet that is not a completion, during the initialisation",
                )));
            }
        };
        let (operation, next) = match step {
            Step::Begin => (Operation::BEGIN_INITIALIZATION, Some(Step::Version(0))),
            Step::Version(n) => {
                if header.status == NtStatus::REVISION_MISMATCH {
                    self.start(Step::Version(n + 1))?;
                    return Ok(false);
                }
                (Operation::QUERY_PROTOCOL_VERSION, Some(Step::Properties))
            }
            Step::Properties => (Operation::QUERY_PROPERTIES, Some(Step::End)),
            Step::End => (Operation::END_INITIALIZATION, None),
        };
        if header.status != NtStatus::SUCCESS {
            return Err(Error::Refused {
                operation,
                status: header.status,
            });
        }
        match step {
            Step::Version(n) => self.version = VERSIONS.get(n).copied(),
            Step::Properties => {
                let properties: ChannelProperties = field(body).ok_or(Error::Malformed(
                    Malformed("a properties answer shorter than the properties"),
                ))?;
                let named = Rings::max_pages(&self.channel) as u64 * PAGE as u64;
                self.max_transfer = named.min(properties.max_transfer_bytes as u64) as u32;
                // The largest request names the pages of the largest transfer.
                let pages = (self.max_transfer as usize).div_ceil(PAGE);
                self.limit = Rings::limit(&self.channel, pages);
            }
            Step::Begin | Step::End => {}
        }
        match next {
            Some(next) => {
                self.start(next)?;
                Ok(false)
            }
            None => {
                self.state = State::Ready;
                Ok(true)
            }
        }
    }

    /// Takes a packet the host sent once ready: a completion, matched to its request by
    /// transaction ID (`storvsc:505-525`), or a packet of the host's own, of which only the
    /// bus-changed notice is expected and it asks nothing of this driver (`storvsc:495-503`).
    fn complete(&mut self, packet: &Received) -> Result<Option<Completion>, Error> {
        match packet.kind {
            Kind::Completion(id) => {
                let pending = self
                    .pending
                    .remove(&id)
                    .ok_or(Error::UnknownTransaction(id))?;
                let result = match packet.completion() {
                    Ok((header, body)) => {
                        scsi::outcome(&header, body, pending.asked, pending.exact)
                    }
                    Err(what) => Err(Failure::Malformed(what)),
                };
                Ok(Some(Completion { id, result }))
            }
            Kind::InBand => match packet.operation().map_err(Error::Malformed)? {
                Operation::ENUMERATE_BUS => Ok(None),
                operation => Err(Error::Unexpected(operation)),
            },
            Kind::Other => Err(Error::Malformed(Malformed(
                "a packet that is neither a completion nor in-band",
            ))),
        }
    }

    /// Sends one SCSI command for `bytes` of data in the first pages of `pages`, and returns
    /// its transaction ID.
    fn request(
        &mut self,
        address: Address,
        command: Command,
        bytes: u32,
        pages: &[u64],
    ) -> Result<u64, Error> {
        match self.state {
            State::Starting(..) => return Err(Error::NotReady),
            State::Ready => {}
            State::Failed => return Err(Error::Failed),
        }
        if bytes > self.max_transfer {
            return Err(Error::TooLarge);
        }
        let pages = pages
            .get(..(bytes as usize).div_ceil(PAGE))
            .ok_or(Error::ShortBuffer)?;
        if self.pending.len() >= self.limit {
            return Err(Error::Full);
        }
        // Data travels as a GPA direct packet naming the caller's pages (`storvsc:563-588`).
        // A command with no data is in-band: a GPA direct packet must name a range, and the
        // ring refuses one that names none.
        let data = match bytes {
            0 => None,
            _ => Some(PagedRange::new(0, bytes as usize, pages).ok_or(Error::ShortBuffer)?),
        };
        let request = scsi::request(address, &command, bytes);
        let id = self.take_id();
        self.send(id, Operation::EXECUTE_SRB, request.as_bytes(), data)?;
        self.pending.insert(
            id,
            Pending {
                asked: bytes,
                exact: command.exact,
            },
        );
        Ok(id)
    }
}

/// The bytes of `count` blocks from `block`, when they lie within the disk.
fn span(disk: &Disk, block: u64, count: u32) -> Result<u32, Error> {
    if count == 0 {
        return Err(Error::Empty);
    }
    let end = block.checked_add(count as u64).ok_or(Error::OutOfRange)?;
    if end > disk.blocks() {
        return Err(Error::OutOfRange);
    }
    u32::try_from(count as u64 * disk.block_size() as u64).map_err(|_| Error::TooLarge)
}
