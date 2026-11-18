//! The SCSI side: the five commands a block driver sends, the SCSI request each travels in,
//! what a completion says of it, and the two answers the caller decodes from its own page.

use scsi_defs::{
    AdditionalSenseCode, Cdb10, Cdb16, Cdb16Flags, CdbFlags, CdbInquiry, InquiryFlags,
    ReadCapacity16Data, ReadCapacityDataEx, SERVICE_ACTION_READ_CAPACITY16, ScsiOp, ScsiStatus,
    SenseDataErrorCode, SenseKey, ServiceActionIn16, VPD_SERIAL_NUMBER, VpdPageHeader,
    srb::SrbStatus,
};
use storvsp_protocol::{
    MAX_DATA_BUFFER_LENGTH_WITH_PADDING, NtStatus, Packet, SCSI_IOCTL_DATA_IN, SCSI_IOCTL_DATA_OUT,
    SCSI_REQUEST_LEN_V2, ScsiRequest,
};
use zerocopy::{FromZeros, IntoBytes};

use crate::{Malformed, ring::field};

/// Which disk of the controller behind the channel: SCSI's path, target and logical unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Address {
    /// The path (bus) on the controller.
    pub path: u8,
    /// The target on that path.
    pub target: u8,
    /// The logical unit of that target.
    pub lun: u8,
}

/// A disk whose capacity has been read: where it is, how many blocks it has, and their size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Disk {
    address: Address,
    blocks: u64,
    block_size: u32,
}

impl Disk {
    /// Bytes [`Storvsc::read_capacity`](crate::Storvsc::read_capacity) asks the device for.
    pub const CAPACITY_BYTES: u32 = size_of::<ReadCapacity16Data>() as u32;

    /// Decodes READ CAPACITY 16's answer: the bytes the device wrote into the caller's page,
    /// as many as the completion reports.
    pub fn from_capacity(address: Address, answer: &[u8]) -> Result<Disk, Malformed> {
        let data: ReadCapacityDataEx = field(answer).ok_or(Malformed(
            "a capacity shorter than its last block and its block size",
        ))?;
        // The answer names the last block, not the count.
        let blocks = data
            .logical_block_address
            .get()
            .checked_add(1)
            .ok_or(Malformed("a capacity whose last block is 2^64 - 1"))?;
        let block_size = data.bytes_per_block.get();
        if block_size < 512 || !block_size.is_power_of_two() {
            return Err(Malformed(
                "a block size that is not 512 or a larger power of two",
            ));
        }
        Ok(Disk {
            address,
            blocks,
            block_size,
        })
    }

    /// Where the disk is on its controller.
    pub fn address(&self) -> Address {
        self.address
    }

    /// The disk's capacity, in blocks.
    pub fn blocks(&self) -> u64 {
        self.blocks
    }

    /// Bytes in a block: 512 or a larger power of two.
    pub fn block_size(&self) -> u32 {
        self.block_size
    }
}

/// Bytes [`Storvsc::read_serial`](crate::Storvsc::read_serial) asks the device for: a VPD
/// page's header and the longest serial a one-byte allocation length can ask for.
pub const SERIAL_BYTES: u32 = 255;

/// Decodes the INQUIRY answer for VPD page 0x80: the device's serial, as the device gives it.
/// SPC makes it ASCII and lets the device pad it with spaces; nothing is trimmed here.
/// `answer` is the bytes the device wrote into the caller's page, as many as the completion
/// reports.
pub fn serial(answer: &[u8]) -> Result<&[u8], Malformed> {
    let header: VpdPageHeader =
        field(answer).ok_or(Malformed("a serial page shorter than its header"))?;
    if header.page_code != VPD_SERIAL_NUMBER {
        return Err(Malformed("a page that is not the serial's, 0x80"));
    }
    answer[size_of::<VpdPageHeader>()..]
        .get(..header.page_length as usize)
        .ok_or(Malformed("a serial longer than the bytes that arrived"))
}

/// The sense data of a failed command: SCSI's key, code and qualifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sense {
    /// The broad kind of error.
    pub key: SenseKey,
    /// The additional sense code.
    pub code: AdditionalSenseCode,
    /// The additional sense code's qualifier.
    pub qualifier: u8,
}

/// A request the device or the host failed, with everything the completion says of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScsiError {
    /// The host's status for the packet.
    pub status: NtStatus,
    /// The request block's status.
    pub srb: SrbStatus,
    /// SCSI's status.
    pub scsi: ScsiStatus,
    /// The sense data, when the completion carries any it marks as valid.
    pub sense: Option<Sense>,
}

/// Why a request that reached the host did not succeed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The device or the host failed it.
    Scsi(ScsiError),
    /// Its completion is not one a storvsp sends.
    Malformed(Malformed),
    /// The host reported success and moved fewer bytes than the read or write asked for.
    Short {
        /// The bytes the request asked for.
        asked: u32,
        /// The bytes the completion reports.
        moved: u32,
    },
}

/// One command: its descriptor block, which way its data goes, and whether success means
/// every byte asked for.
pub(crate) struct Command {
    cdb: [u8; MAX_DATA_BUFFER_LENGTH_WITH_PADDING],
    cdb_length: u8,
    data_in: bool,
    /// A read or a write moves exactly what was asked; an inquiry may answer with less.
    pub(crate) exact: bool,
}

impl Command {
    fn new(cdb: &[u8], data_in: bool, exact: bool) -> Command {
        let mut bytes = [0u8; MAX_DATA_BUFFER_LENGTH_WITH_PADDING];
        bytes[..cdb.len()].copy_from_slice(cdb);
        Command {
            cdb: bytes,
            cdb_length: cdb.len() as u8,
            data_in,
            exact,
        }
    }
}

/// READ CAPACITY 16: the last block and the block size.
pub(crate) fn read_capacity16() -> Command {
    let cdb = ServiceActionIn16 {
        operation_code: ScsiOp::READ_CAPACITY16,
        service_action: SERVICE_ACTION_READ_CAPACITY16,
        logical_block: [0; 8],
        allocation_length: Disk::CAPACITY_BYTES.to_be_bytes(),
        flags: 0,
        control: 0,
    };
    Command::new(cdb.as_bytes(), true, false)
}

/// INQUIRY for vital product data page 0x80, the unit's serial number.
pub(crate) fn inquiry_serial() -> Command {
    let cdb = CdbInquiry {
        operation_code: ScsiOp::INQUIRY.0,
        flags: InquiryFlags::new().with_vpd(true),
        page_code: VPD_SERIAL_NUMBER,
        allocation_length: (SERIAL_BYTES as u16).into(),
        control: 0,
    };
    Command::new(cdb.as_bytes(), true, false)
}

/// READ 16 of `count` blocks from `block`.
pub(crate) fn read16(block: u64, count: u32) -> Command {
    Command::new(
        transfer16(ScsiOp::READ16, block, count).as_bytes(),
        true,
        true,
    )
}

/// WRITE 16 of `count` blocks at `block`.
pub(crate) fn write16(block: u64, count: u32) -> Command {
    Command::new(
        transfer16(ScsiOp::WRITE16, block, count).as_bytes(),
        false,
        true,
    )
}

fn transfer16(operation_code: ScsiOp, block: u64, count: u32) -> Cdb16 {
    Cdb16 {
        operation_code,
        flags: Cdb16Flags::new(),
        logical_block: block.into(),
        transfer_blocks: count.into(),
        reserved2: 0,
        control: 0,
    }
}

/// SYNCHRONIZE CACHE of the whole disk: block 0, and a count of 0 for "to the end".
pub(crate) fn synchronize_cache() -> Command {
    let cdb = Cdb10 {
        operation_code: ScsiOp::SYNCHRONIZE_CACHE,
        flags: CdbFlags::new(),
        logical_block: 0.into(),
        reserved2: 0,
        transfer_blocks: 0.into(),
        control: 0,
    };
    Command::new(cdb.as_bytes(), false, true)
}

/// The SCSI request a command travels in, with the fields OpenVMM's guest sets
/// (`vm/devices/storage/storage_tests/tests/tests/storvsc.rs:24-80`): the address, the
/// request's length, the descriptor block, the data's length and its direction.
pub(crate) fn request(address: Address, command: &Command, bytes: u32) -> ScsiRequest {
    ScsiRequest {
        length: SCSI_REQUEST_LEN_V2 as u16,
        path_id: address.path,
        target_id: address.target,
        lun: address.lun,
        cdb_length: command.cdb_length,
        data_in: if command.data_in {
            SCSI_IOCTL_DATA_IN
        } else {
            SCSI_IOCTL_DATA_OUT
        },
        data_transfer_length: bytes,
        payload: command.cdb,
        ..ScsiRequest::new_zeroed()
    }
}

/// What a completion says of the request it answers: the bytes moved, or why not. `asked` is
/// the request's byte count, and `exact` whether success means all of them.
pub(crate) fn outcome(
    header: &Packet,
    body: &[u8],
    asked: u32,
    exact: bool,
) -> Result<u32, Failure> {
    let answer: ScsiRequest = field(body).ok_or(Failure::Malformed(Malformed(
        "a completion shorter than its SCSI request",
    )))?;
    let srb = answer.srb_status.status();
    if header.status != NtStatus::SUCCESS
        || srb != SrbStatus::SUCCESS
        || answer.scsi_status != ScsiStatus::GOOD
    {
        // The host puts the sense data where the request had its descriptor block
        // (`storvsp:1405-1421`), and says so in the status.
        let sense = answer
            .srb_status
            .autosense_valid()
            .then(|| sense(&answer.payload))
            .flatten();
        return Err(Failure::Scsi(ScsiError {
            status: header.status,
            srb,
            scsi: answer.scsi_status,
            sense,
        }));
    }
    let moved = answer.data_transfer_length;
    if moved > asked {
        return Err(Failure::Malformed(Malformed(
            "a completion that moved more bytes than the request asked for",
        )));
    }
    if exact && moved != asked {
        return Err(Failure::Short { asked, moved });
    }
    Ok(moved)
}

/// The key, code and qualifier of sense data, in SPC's fixed or descriptor format.
fn sense(bytes: &[u8; MAX_DATA_BUFFER_LENGTH_WITH_PADDING]) -> Option<Sense> {
    let (key, code, qualifier) = match SenseDataErrorCode(bytes[0] & 0x7f) {
        SenseDataErrorCode::FIXED_CURRENT | SenseDataErrorCode::FIXED_DEFERRED => {
            (bytes[2], bytes[12], bytes[13])
        }
        SenseDataErrorCode::DESCRIPTOR_CURRENT | SenseDataErrorCode::DESCRIPTOR_DEFERRED => {
            (bytes[1], bytes[2], bytes[3])
        }
        _ => return None,
    };
    Some(Sense {
        key: SenseKey(key & 0x0f),
        code: AdditionalSenseCode(code),
        qualifier,
    })
}
