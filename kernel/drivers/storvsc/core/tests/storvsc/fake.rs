//! A fake storvsp: the host's side of storvsc, the [`Device`] behind `moruna-vmbus`'s in-memory
//! [`TestChannel`], its disks byte vectors, strict about the order of the initialisation and
//! the size and content of every packet the guest sends. It follows OpenVMM's `storvsp`
//! (`vm/devices/storage/storvsp`) and its `scsidisk` for what a host answers, and panics when
//! the guest breaks the protocol, so that a test fails where the fault is.
//!
//! It reads what the guest sends when the channel calls it, as a host would. It writes its
//! answers through a second writer on the same ring, so that a test can also deliver them,
//! or packets of its own, between the library's calls: `TestChannel` reaches its host only
//! from inside those calls, and the library owns the channel.
//!
//! Beside it: the guest memory its disks read and write by page number, and a small
//! fixed-seed random number generator.

use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    rc::Rc,
};

use guestmem_core::{MemoryWrite, ranges::PagedRange};
use moruna_storvsc::{Address, PAGE};
use moruna_vmbus::testing::{Device, Host as Vmbus, TestChannel};
use scsi_defs::{
    AdditionalSenseCode, Cdb10, Cdb16, CdbInquiry, DIRECT_ACCESS_DEVICE, ReadCapacity16Data,
    ReadCapacityDataEx, SERVICE_ACTION_READ_CAPACITY16, ScsiOp, ScsiStatus, SenseData, SenseKey,
    ServiceActionIn16, VPD_SERIAL_NUMBER, VpdPageHeader,
    srb::{SrbStatus, SrbStatusAndFlags},
};
use storvsp_protocol::{
    ChannelProperties, NtStatus, Operation, Packet, ProtocolVersion, SCSI_IOCTL_DATA_IN,
    SCSI_IOCTL_DATA_OUT, SCSI_REQUEST_LEN_MAX, SCSI_REQUEST_LEN_V2,
    STORAGE_CHANNEL_SUPPORTS_MULTI_CHANNEL, ScsiRequest, VERSION_BLUE, VERSION_WIN6, VERSION_WIN7,
    VERSION_WIN8,
};
use vmbus_ring::{
    FlatRingMem, IncomingPacketType, OutgoingPacket, OutgoingPacketType, OutgoingRing, Ring,
    TransferPageRange, WriteError, gparange::MultiPagedRangeBuf,
};
use zerocopy::{FromBytes, FromZeros, IntoBytes};

/// Bytes of the header of every storvsp packet, and of every packet's payload.
pub const HEADER: usize = size_of::<Packet>();
pub const PACKET: usize = HEADER + SCSI_REQUEST_LEN_MAX;

/// Bytes of each ring's data, unless a test asks for another size.
pub const RING: usize = 4 * PAGE;

/// SplitMix64, seeded: the same numbers on every run.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A number below `n`.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

/// The guest's memory, as the host sees it: pages by number.
pub struct Memory {
    bytes: Vec<u8>,
}

impl Memory {
    pub fn new(pages: usize) -> Memory {
        Memory {
            bytes: vec![0; pages * PAGE],
        }
    }

    fn page(&self, gpn: u64) -> Option<usize> {
        let at = usize::try_from(gpn).ok()?.checked_mul(PAGE)?;
        (at + PAGE <= self.bytes.len()).then_some(at)
    }

    /// The first `len` bytes of the pages `gpns`, in order. `None` names a page the guest
    /// does not have.
    pub fn read(&self, gpns: &[u64], len: usize) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(len);
        for gpn in gpns {
            let at = self.page(*gpn)?;
            let take = PAGE.min(len - out.len());
            out.extend_from_slice(&self.bytes[at..at + take]);
        }
        (out.len() == len).then_some(out)
    }

    /// Writes `data` over the first bytes of the pages `gpns`, in order.
    pub fn write(&mut self, gpns: &[u64], data: &[u8]) -> Option<()> {
        let mut chunks = data.chunks(PAGE);
        for gpn in gpns {
            let Some(chunk) = chunks.next() else { break };
            let at = self.page(*gpn)?;
            self.bytes[at..at + chunk.len()].copy_from_slice(chunk);
        }
        chunks.next().is_none().then_some(())
    }
}

/// One disk behind the fake: its blocks as bytes, and what it says of itself.
pub struct FakeDisk {
    pub block_size: u32,
    pub bytes: Vec<u8>,
    pub serial: Vec<u8>,
    pub flushes: usize,
}

impl FakeDisk {
    pub fn new(block_size: u32, blocks: u64, serial: &[u8]) -> FakeDisk {
        FakeDisk {
            block_size,
            bytes: vec![0; (block_size as u64 * blocks) as usize],
            serial: serial.to_vec(),
            flushes: 0,
        }
    }

    pub fn blocks(&self) -> u64 {
        self.bytes.len() as u64 / self.block_size as u64
    }
}

/// What the fake does wrong to the next SCSI request, when a test asks it to.
#[derive(Clone, Copy)]
pub enum Fault {
    /// Fails the command with this result.
    Scsi {
        srb: SrbStatus,
        scsi: ScsiStatus,
        sense: Option<SenseData>,
    },
    /// Answers with a payload of this many bytes.
    Size(usize),
    /// Answers with this operation where COMPLETE_IO belongs.
    Operation(Operation),
    /// Answers with this status on an otherwise good completion.
    Status(NtStatus),
    /// Reports this many bytes moved.
    Moved(u32),
    /// Sends the completion twice.
    Twice,
    /// Fails the command with a check condition whose sense data is these twenty bytes.
    Sense([u8; 20]),
}

/// A completion the fake has made and not yet sent.
struct Answer {
    id: u64,
    payload: Vec<u8>,
    copies: usize,
}

/// A kind of packet for [`Host::inject`]: everything a ring can carry.
#[derive(Clone, Copy, Debug)]
pub enum Raw {
    Completion,
    InBand,
    InBandWithCompletion,
    GpaDirect,
    TransferPages,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Begin,
    Version,
    Properties,
    End,
    Ready,
}

/// What a SCSI command came to, as `storvsp`'s `ScsiResult`.
struct ScsiResult {
    scsi: ScsiStatus,
    srb: SrbStatus,
    moved: usize,
    sense: Option<SenseData>,
}

impl ScsiResult {
    fn good(moved: usize) -> ScsiResult {
        ScsiResult {
            scsi: ScsiStatus::GOOD,
            srb: SrbStatus::SUCCESS,
            moved,
            sense: None,
        }
    }

    /// `scsidisk`'s answer to a request it will not do (`scsidisk/src/lib.rs:999-1004`).
    fn illegal(code: AdditionalSenseCode) -> ScsiResult {
        ScsiResult {
            scsi: ScsiStatus::CHECK_CONDITION,
            srb: SrbStatus::INVALID_REQUEST,
            moved: 0,
            sense: Some(SenseData::new(SenseKey::ILLEGAL_REQUEST, code, 0)),
        }
    }
}

/// The channel's two rings as the fake sees them.
struct Rings {
    /// The ring the guest writes.
    to_host: FlatRingMem,
    /// The ring the guest reads.
    to_guest: FlatRingMem,
    /// The fake's writer on the ring the guest reads.
    writer: OutgoingRing<FlatRingMem>,
}

/// The fake storvsp's state, shared by the [`Fake`] device and the test.
pub struct Host {
    rings: Option<Rings>,
    /// Set when a write wants the guest told; the device tells it on its next call.
    signal: bool,
    phase: Phase,
    answers: VecDeque<Answer>,

    pub memory: Memory,
    pub disks: BTreeMap<(u8, u8, u8), FakeDisk>,
    /// The versions this host speaks.
    pub accepts: Vec<u16>,
    /// The properties it answers with.
    pub max_transfer_bytes: u32,
    pub sub_channels: u16,
    /// A step of the initialisation it refuses, and with what.
    pub refuse: Option<(Operation, NtStatus)>,
    /// While set, it reads nothing the guest sends.
    pub stalled: bool,
    /// While set, it keeps its completions back.
    pub hold: bool,
    pub faults: VecDeque<Fault>,

    /// Every packet the guest sent: its operation and its transaction ID.
    pub seen: Vec<(Operation, u64)>,
    /// Every version the guest offered, in order.
    pub offered: Vec<u16>,
    /// The version agreed.
    pub version: Option<u16>,

    /// Times the guest signalled, and times it waited with nothing to wake it.
    pub signals: usize,
    pub idles: usize,
    /// Set to take the channel away, as a rescind does, at the device's next call.
    pub rescind: bool,
}

impl Host {
    fn rings(&self) -> &Rings {
        self.rings
            .as_ref()
            .expect("the fake's rings are set up with the channel")
    }

    /// The ring the guest writes, for tests that corrupt it.
    pub fn to_host(&self) -> &FlatRingMem {
        &self.rings().to_host
    }

    /// The ring the guest reads, for tests that corrupt it.
    pub fn to_guest(&self) -> &FlatRingMem {
        &self.rings().to_guest
    }

    /// Completions made and not yet sent.
    pub fn held(&self) -> usize {
        self.answers.len()
    }

    /// Puts the completions held back in a random order.
    pub fn shuffle(&mut self, rng: &mut Rng) {
        let answers = self.answers.make_contiguous();
        for i in (1..answers.len()).rev() {
            answers.swap(i, rng.below(i as u64 + 1) as usize);
        }
    }

    /// Stops holding completions back and sends those held.
    pub fn release(&mut self) {
        self.hold = false;
        self.send_answers();
    }

    /// Reads everything the guest has sent through the channel, and answers it.
    fn serve(&mut self, vmbus: &mut Vmbus) {
        if self.stalled {
            return;
        }
        loop {
            let received = match vmbus.read() {
                Ok(Some(received)) => received,
                Ok(None) => break,
                Err(e) => panic!("fake storvsp: the guest's ring: {e}"),
            };
            let packet = received.packet;
            let id = packet
                .transaction_id
                .expect("fake storvsp: a packet that asks for no completion");
            let payload = received.payload;
            assert_eq!(payload.len(), PACKET, "fake storvsp: the packet's size");
            let header = Packet::read_from_prefix(&payload).unwrap().0;
            assert_eq!(header.flags, 0, "fake storvsp: the packet's flags");
            assert_eq!(header.status, NtStatus::SUCCESS, "fake storvsp: its status");
            let body: [u8; SCSI_REQUEST_LEN_MAX] = payload[HEADER..].try_into().unwrap();
            let ranges = match packet.typ {
                IncomingPacketType::InBand => None,
                IncomingPacketType::GpaDirect(count, _) => {
                    let (words, rest) = received.ranges.as_chunks::<8>();
                    assert!(rest.is_empty(), "fake storvsp: page ranges in whole words");
                    let words = words.iter().map(|word| u64::from_le_bytes(*word)).collect();
                    Some(
                        MultiPagedRangeBuf::from_range_buffer(count as usize, words)
                            .expect("fake storvsp: the packet's page ranges"),
                    )
                }
                other => panic!("fake storvsp: a packet of type {other:?}"),
            };
            self.seen.push((header.operation, id));
            match header.operation {
                Operation::EXECUTE_SRB => self.execute(id, &body, ranges),
                operation => {
                    assert!(ranges.is_none(), "fake storvsp: pages on {operation:?}");
                    self.initialise(id, operation, &body);
                }
            }
        }
        self.send_answers();
    }

    /// The initialisation, as `storvsp` runs it (`storvsp/src/lib.rs:1036-1213`): each step
    /// in its order, and anything else answered INVALID_DEVICE_STATE.
    fn initialise(&mut self, id: u64, operation: Operation, body: &[u8; SCSI_REQUEST_LEN_MAX]) {
        if let Some((refused, status)) = self.refuse
            && refused == operation
        {
            return self.answer(id, status, &[]);
        }
        let zero = [0u8; SCSI_REQUEST_LEN_MAX];
        match (self.phase, operation) {
            (Phase::Begin, Operation::BEGIN_INITIALIZATION) => {
                assert_eq!(body, &zero, "fake storvsp: BEGIN_INITIALIZATION's body");
                self.phase = Phase::Version;
                self.answer(id, NtStatus::SUCCESS, &[]);
            }
            (Phase::Version, Operation::QUERY_PROTOCOL_VERSION) => {
                let version = ProtocolVersion::read_from_prefix(body).unwrap().0;
                assert_eq!(version.reserved, 0, "fake storvsp: the version's reserved");
                assert_eq!(body[4..], zero[4..], "fake storvsp: the version's padding");
                self.offered.push(version.major_minor);
                let status = if self.accepts.contains(&version.major_minor) {
                    self.version = Some(version.major_minor);
                    self.phase = Phase::Properties;
                    NtStatus::SUCCESS
                } else {
                    NtStatus::REVISION_MISMATCH
                };
                self.answer(id, status, version.as_bytes());
            }
            (Phase::Properties, Operation::QUERY_PROPERTIES) => {
                assert_eq!(body, &zero, "fake storvsp: QUERY_PROPERTIES's body");
                let multi = self.version >= Some(VERSION_WIN8);
                let properties = ChannelProperties {
                    reserved: 0,
                    maximum_sub_channel_count: if multi { self.sub_channels } else { 0 },
                    reserved2: 0,
                    flags: if multi {
                        STORAGE_CHANNEL_SUPPORTS_MULTI_CHANNEL
                    } else {
                        0
                    },
                    max_transfer_bytes: self.max_transfer_bytes,
                    reserved3: [0; 2],
                };
                self.phase = Phase::End;
                self.answer(id, NtStatus::SUCCESS, properties.as_bytes());
            }
            (Phase::End, Operation::END_INITIALIZATION) => {
                assert_eq!(body, &zero, "fake storvsp: END_INITIALIZATION's body");
                self.phase = Phase::Ready;
                self.answer(id, NtStatus::SUCCESS, &[]);
            }
            (phase, operation) => panic!("fake storvsp: {operation:?} in {phase:?}"),
        }
    }

    /// One SCSI request, checked field by field against what OpenVMM's guest sends, then
    /// run against the disk it names.
    fn execute(
        &mut self,
        id: u64,
        body: &[u8; SCSI_REQUEST_LEN_MAX],
        ranges: Option<MultiPagedRangeBuf>,
    ) {
        assert_eq!(
            self.phase,
            Phase::Ready,
            "fake storvsp: EXECUTE_SRB too early"
        );
        let request = ScsiRequest::read_from_bytes(body).unwrap();
        assert_eq!(request.length as usize, SCSI_REQUEST_LEN_V2);
        assert_eq!(request.srb_status.into_bits(), 0);
        assert_eq!(request.scsi_status, ScsiStatus::GOOD);
        assert_eq!(
            (
                request.reserved1,
                request.sense_info_ex_length,
                request.properties
            ),
            (0, 0, 0)
        );
        assert_eq!(
            (request.reserve, request.queue_tag, request.queue_action),
            (0, 0, 0)
        );
        assert_eq!(
            (
                request.srb_flags,
                request.time_out_value,
                request.queue_sort_key
            ),
            (0, 0, 0)
        );
        let cdb_length = request.cdb_length as usize;
        assert!(
            request.payload[cdb_length..].iter().all(|b| *b == 0),
            "fake storvsp: bytes after the descriptor block"
        );
        let len = request.data_transfer_length as usize;
        let gpns = match &ranges {
            None => {
                assert_eq!(len, 0, "fake storvsp: data and no pages");
                Vec::new()
            }
            Some(ranges) => {
                assert_eq!(ranges.range_count(), 1, "fake storvsp: one range a request");
                let range = ranges.first().unwrap();
                assert_eq!(range.offset(), 0, "fake storvsp: the range's offset");
                assert_eq!(range.len(), len, "fake storvsp: the range's length");
                range.gpns().to_vec()
            }
        };

        let fault = self.faults.pop_front();
        let result = match fault {
            Some(Fault::Scsi { srb, scsi, sense }) => ScsiResult {
                scsi,
                srb,
                moved: 0,
                sense,
            },
            _ => self.scsi(&request, &gpns),
        };

        // `storvsp`'s completion (`storvsp/src/lib.rs:1389-1432`).
        let mut payload = [0u8; 0x14];
        if let Some(sense) = result.sense {
            payload[..size_of::<SenseData>()].copy_from_slice(sense.as_bytes());
        }
        let mut response = ScsiRequest {
            length: size_of::<ScsiRequest>() as u16,
            scsi_status: result.scsi,
            srb_status: SrbStatusAndFlags::new()
                .with_status(result.srb)
                .with_autosense_valid(result.sense.is_some()),
            data_transfer_length: result.moved as u32,
            cdb_length: storvsp_protocol::CDB16GENERIC_LENGTH as u8,
            sense_info_ex_length: storvsp_protocol::VMSCSI_SENSE_BUFFER_SIZE as u8,
            payload,
            ..ScsiRequest::new_zeroed()
        };
        let mut header = Packet {
            operation: Operation::COMPLETE_IO,
            flags: 0,
            status: nt_status(result.srb),
        };
        let mut size = PACKET;
        let mut copies = 1;
        match fault {
            Some(Fault::Size(n)) => size = n,
            Some(Fault::Operation(operation)) => header.operation = operation,
            Some(Fault::Status(status)) => header.status = status,
            Some(Fault::Moved(n)) => response.data_transfer_length = n,
            Some(Fault::Twice) => copies = 2,
            Some(Fault::Sense(bytes)) => {
                response.payload = bytes;
                response.scsi_status = ScsiStatus::CHECK_CONDITION;
                response.srb_status = SrbStatusAndFlags::new()
                    .with_status(SrbStatus::ERROR)
                    .with_autosense_valid(true);
                header.status = nt_status(SrbStatus::ERROR);
            }
            Some(Fault::Scsi { .. }) | None => {}
        }
        let mut bytes = [header.as_bytes(), response.as_bytes()].concat();
        bytes.resize(size, 0);
        self.answers.push_back(Answer {
            id,
            payload: bytes,
            copies,
        });
    }

    /// The five commands the library sends, as `scsidisk` answers them; any other is
    /// the guest's fault.
    fn scsi(&mut self, request: &ScsiRequest, gpns: &[u64]) -> ScsiResult {
        let len = request.data_transfer_length as usize;
        let key = (request.path_id, request.target_id, request.lun);
        let Some(disk) = self.disks.get_mut(&key) else {
            // `storvsp/src/lib.rs:808-813`.
            return ScsiResult {
                scsi: ScsiStatus::CHECK_CONDITION,
                srb: SrbStatus::INVALID_LUN,
                moved: 0,
                sense: None,
            };
        };
        let direction = |data_in: u8, cdb_length: usize| {
            assert_eq!(
                request.data_in, data_in,
                "fake storvsp: the data's direction"
            );
            assert_eq!(request.cdb_length as usize, cdb_length);
        };
        match ScsiOp(request.payload[0]) {
            op @ (ScsiOp::READ16 | ScsiOp::WRITE16) => {
                let read = op == ScsiOp::READ16;
                direction(
                    if read {
                        SCSI_IOCTL_DATA_IN
                    } else {
                        SCSI_IOCTL_DATA_OUT
                    },
                    size_of::<Cdb16>(),
                );
                let cdb = Cdb16::read_from_prefix(&request.payload).unwrap().0;
                assert_eq!(cdb.flags.into_bits(), 0);
                assert_eq!((cdb.reserved2, cdb.control), (0, 0));
                let (block, count) = (cdb.logical_block.get(), cdb.transfer_blocks.get() as u64);
                let size = disk.block_size as u64;
                // `scsidisk/src/lib.rs:770-790`: no blocks, more than the buffer holds,
                // or past the end.
                if count == 0
                    || count * size > len as u64
                    || disk.blocks() <= block
                    || disk.blocks() - block < count
                {
                    return ScsiResult::illegal(AdditionalSenseCode::ILLEGAL_BLOCK);
                }
                assert_eq!(
                    count * size,
                    len as u64,
                    "fake storvsp: the transfer's length"
                );
                let at = (block * size) as usize;
                if read {
                    let data = disk.bytes[at..at + len].to_vec();
                    self.memory
                        .write(gpns, &data)
                        .expect("fake storvsp: a page the guest does not have");
                } else {
                    let data = self
                        .memory
                        .read(gpns, len)
                        .expect("fake storvsp: a page the guest does not have");
                    disk.bytes[at..at + len].copy_from_slice(&data);
                }
                ScsiResult::good(len)
            }
            ScsiOp::READ_CAPACITY16 => {
                direction(SCSI_IOCTL_DATA_IN, size_of::<ServiceActionIn16>());
                let cdb = ServiceActionIn16::read_from_prefix(&request.payload)
                    .unwrap()
                    .0;
                assert_eq!(cdb.service_action, SERVICE_ACTION_READ_CAPACITY16);
                assert_eq!(cdb.logical_block, [0; 8]);
                assert_eq!(u32::from_be_bytes(cdb.allocation_length) as usize, len);
                assert_eq!((cdb.flags, cdb.control), (0, 0));
                assert_eq!(len, size_of::<ReadCapacity16Data>());
                // `scsidisk/src/lib.rs:581-611`.
                let data = ReadCapacity16Data {
                    ex: ReadCapacityDataEx {
                        logical_block_address: (disk.blocks() - 1).into(),
                        bytes_per_block: disk.block_size.into(),
                    },
                    ..FromZeros::new_zeroed()
                };
                self.memory
                    .write(gpns, data.as_bytes())
                    .expect("fake storvsp: a page the guest does not have");
                ScsiResult::good(size_of::<ReadCapacity16Data>())
            }
            ScsiOp::INQUIRY => {
                direction(SCSI_IOCTL_DATA_IN, size_of::<CdbInquiry>());
                let cdb = CdbInquiry::read_from_prefix(&request.payload).unwrap().0;
                assert!(
                    cdb.flags.vpd() && !cdb.flags.csd(),
                    "fake storvsp: a VPD inquiry"
                );
                assert_eq!(cdb.page_code, VPD_SERIAL_NUMBER);
                assert_eq!(cdb.allocation_length.get() as usize, len);
                assert_eq!(cdb.control, 0);
                // `scsidisk/src/inquiry.rs:86-113`, `:498-500`: a disk with no serial has
                // no such page.
                if disk.serial.is_empty() {
                    return ScsiResult::illegal(AdditionalSenseCode::INVALID_CDB);
                }
                let header = VpdPageHeader {
                    device_type: DIRECT_ACCESS_DEVICE,
                    page_code: VPD_SERIAL_NUMBER,
                    reserved: 0,
                    page_length: disk.serial.len() as u8,
                };
                let mut page = [header.as_bytes(), &disk.serial].concat();
                page.truncate(len);
                self.memory
                    .write(gpns, &page)
                    .expect("fake storvsp: a page the guest does not have");
                ScsiResult::good(page.len())
            }
            ScsiOp::SYNCHRONIZE_CACHE => {
                direction(SCSI_IOCTL_DATA_OUT, size_of::<Cdb10>());
                let cdb = Cdb10::read_from_prefix(&request.payload).unwrap().0;
                assert_eq!(cdb.flags.into_bits(), 0);
                assert_eq!((cdb.logical_block.get(), cdb.transfer_blocks.get()), (0, 0));
                assert_eq!((cdb.reserved2, cdb.control), (0, 0));
                assert_eq!(len, 0, "fake storvsp: a flush with data");
                disk.flushes += 1;
                ScsiResult::good(0)
            }
            op => panic!("fake storvsp: {op:?}, which the library has no business sending"),
        }
    }

    /// Queues the completion of an initialisation step: the status, and a body zero padded
    /// to the size of the request (`storvsp/src/lib.rs:560-600`).
    fn answer(&mut self, id: u64, status: NtStatus, body: &[u8]) {
        let header = Packet {
            operation: Operation::COMPLETE_IO,
            flags: 0,
            status,
        };
        let mut payload = [header.as_bytes(), body].concat();
        payload.resize(PACKET, 0);
        self.answers.push_back(Answer {
            id,
            payload,
            copies: 1,
        });
    }

    fn send_answers(&mut self) {
        while !self.hold
            && let Some(answer) = self.answers.front()
        {
            let (id, payload) = (answer.id, answer.payload.clone());
            if !self.put(OutgoingPacketType::Completion, id, &payload) {
                break;
            }
            let answer = self.answers.front_mut().unwrap();
            answer.copies -= 1;
            if answer.copies == 0 {
                self.answers.pop_front();
            }
        }
    }

    /// Writes one packet into the ring to the guest. False when the ring has no room.
    fn put(&mut self, typ: OutgoingPacketType<'_>, id: u64, payload: &[u8]) -> bool {
        let writer = &self.rings().writer;
        let mut offset = writer.outgoing().unwrap();
        let packet = OutgoingPacket {
            transaction_id: id,
            size: payload.len(),
            typ,
        };
        match writer.write(&mut offset, &packet) {
            Ok(range) => {
                range.writer(writer).write(payload).unwrap();
                writer.set_pending_send_size(0).unwrap();
                if writer.commit_write(&mut offset) {
                    self.signal = true;
                }
                true
            }
            // As a host does: ask the guest for a signal once it has read enough to make
            // room, and send the rest then.
            Err(WriteError::Full(size)) => {
                writer.set_pending_send_size(size).unwrap();
                false
            }
            Err(WriteError::Corrupt(e)) => panic!("fake storvsp: the ring to the guest: {e}"),
        }
    }

    /// Sends the guest any packet at all. False when the ring has no room.
    pub fn inject(&mut self, raw: Raw, id: u64, payload: &[u8]) -> bool {
        let pages = [PagedRange::new(0, PAGE, &[7]).unwrap()];
        let transfer = [TransferPageRange {
            byte_count: 64,
            byte_offset: 0,
        }];
        let typ = match raw {
            Raw::Completion => OutgoingPacketType::Completion,
            Raw::InBand => OutgoingPacketType::InBandNoCompletion,
            Raw::InBandWithCompletion => OutgoingPacketType::InBandWithCompletion,
            Raw::GpaDirect => OutgoingPacketType::GpaDirect(&pages),
            Raw::TransferPages => OutgoingPacketType::TransferPages(1, &transfer),
        };
        self.put(typ, id, payload)
    }

    /// A packet's payload: a header with `operation` and `status`, then `body`, zero padded
    /// to the size every storvsp packet has.
    pub fn packet(operation: Operation, status: NtStatus, body: &[u8]) -> Vec<u8> {
        let header = Packet {
            operation,
            flags: 0,
            status,
        };
        let mut payload = [header.as_bytes(), body].concat();
        payload.resize(PACKET, 0);
        payload
    }
}

/// `storvsp`'s status for a request block's status (`storvsp/src/lib.rs:1222-1243`).
fn nt_status(srb: SrbStatus) -> NtStatus {
    match srb {
        SrbStatus::BUSY => NtStatus::DEVICE_BUSY,
        SrbStatus::SUCCESS => NtStatus::SUCCESS,
        SrbStatus::INVALID_LUN
        | SrbStatus::INVALID_TARGET_ID
        | SrbStatus::NO_DEVICE
        | SrbStatus::NO_HBA => NtStatus::DEVICE_DOES_NOT_EXIST,
        SrbStatus::COMMAND_TIMEOUT | SrbStatus::TIMEOUT => NtStatus::IO_TIMEOUT,
        SrbStatus::SELECTION_TIMEOUT => NtStatus::DEVICE_NOT_CONNECTED,
        SrbStatus::BAD_FUNCTION | SrbStatus::BAD_SRB_BLOCK_LENGTH => {
            NtStatus::INVALID_DEVICE_REQUEST
        }
        SrbStatus::DATA_OVERRUN => NtStatus::BUFFER_OVERFLOW,
        SrbStatus::REQUEST_FLUSHED => NtStatus::UNSUCCESSFUL,
        SrbStatus::ABORTED => NtStatus::CANCELLED,
        _ => NtStatus::IO_DEVICE_ERROR,
    }
}

/// The device on the far end of the [`TestChannel`]: it runs the fake storvsp whenever the
/// guest signals or is about to wait.
pub struct Fake(Rc<RefCell<Host>>);

impl Fake {
    fn serve(&self, vmbus: &mut Vmbus) {
        let mut host = self.0.borrow_mut();
        if host.rescind {
            vmbus.rescind();
            return;
        }
        host.serve(vmbus);
        if std::mem::take(&mut host.signal) {
            vmbus.signal_driver();
        }
    }
}

impl Device for Fake {
    fn signalled(&mut self, vmbus: &mut Vmbus) {
        self.0.borrow_mut().signals += 1;
        self.serve(vmbus);
    }

    fn idle(&mut self, vmbus: &mut Vmbus) {
        self.0.borrow_mut().idles += 1;
        self.serve(vmbus);
    }
}

/// The channel the library gets in the tests.
pub type Link = TestChannel<Fake>;

/// The address of the disk every test starts with.
pub const DISK: Address = Address {
    path: 0,
    target: 0,
    lun: 3,
};

/// A channel of two rings of `ring` bytes each, and the fake storvsp on its far end with
/// one disk at [`DISK`]: 512-byte blocks, 2048 of them.
pub fn pair_sized(ring: usize) -> (Link, Rc<RefCell<Host>>) {
    let mut disks = BTreeMap::new();
    disks.insert(
        (DISK.path, DISK.target, DISK.lun),
        FakeDisk::new(512, 2048, b"6002248042d1c9a3"),
    );
    let host = Rc::new(RefCell::new(Host {
        rings: None,
        signal: false,
        phase: Phase::Begin,
        answers: VecDeque::new(),
        memory: Memory::new(256),
        disks,
        // What OpenVMM's storvsp speaks (`storvsp/src/lib.rs:304-316`): not the newest.
        accepts: vec![VERSION_BLUE, VERSION_WIN8, VERSION_WIN7, VERSION_WIN6],
        max_transfer_bytes: 0x40000,
        sub_channels: 3,
        refuse: None,
        stalled: false,
        hold: false,
        faults: VecDeque::new(),
        seen: Vec::new(),
        offered: Vec::new(),
        version: None,
        signals: 0,
        idles: 0,
        rescind: false,
    }));
    let mut link = TestChannel::new(ring, ring, Fake(host.clone())).unwrap();
    let to_host = link.host().incoming().mem().clone();
    let to_guest = link.host().outgoing().mem().clone();
    let writer = OutgoingRing::new(to_guest.clone()).unwrap();
    host.borrow_mut().rings = Some(Rings {
        to_host,
        to_guest,
        writer,
    });
    (link, host)
}

pub fn pair() -> (Link, Rc<RefCell<Host>>) {
    pair_sized(RING)
}
