//! Submission queue entries and completion queue entries (NVMe Base 2.0, 4.1 to 4.2):
//! the 64-byte commands a driver writes and the 16-byte completions it reads.

use core::fmt;

use moruna_abi::Error;
use moruna_mmio::Region;

/// Admin command opcodes (NVMe Base 2.0, 5).
pub mod admin {
    pub const DELETE_SQ: u8 = 0x00;
    pub const CREATE_SQ: u8 = 0x01;
    pub const DELETE_CQ: u8 = 0x04;
    pub const CREATE_CQ: u8 = 0x05;
    pub const IDENTIFY: u8 = 0x06;
    pub const SET_FEATURES: u8 = 0x09;
}

/// NVM command set opcodes (NVM Command Set 1.0, 3).
pub mod nvm {
    pub const FLUSH: u8 = 0x00;
    pub const WRITE: u8 = 0x01;
    pub const READ: u8 = 0x02;
}

/// Identify's Controller or Namespace Structure values (CNS).
pub mod cns {
    pub const NAMESPACE: u32 = 0x00;
    pub const CONTROLLER: u32 = 0x01;
    pub const ACTIVE_NAMESPACES: u32 = 0x02;
}

/// The Number of Queues feature.
pub const FEATURE_QUEUES: u32 = 0x07;

/// A command, less its identifier, which the queue assigns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Command {
    pub opcode: u8,
    pub nsid: u32,
    pub prp1: u64,
    pub prp2: u64,
    pub cdw10: u32,
    pub cdw11: u32,
    pub cdw12: u32,
    pub cdw13: u32,
    pub cdw14: u32,
    pub cdw15: u32,
}

impl Command {
    /// Identify (CNS `cns`) into the page at `prp1`.
    pub fn identify(cns: u32, nsid: u32, prp1: u64) -> Command {
        Command {
            opcode: admin::IDENTIFY,
            nsid,
            prp1,
            cdw10: cns,
            ..Command::default()
        }
    }

    /// Create I/O Completion Queue `qid` of `entries`, physically contiguous at `prp1`,
    /// interrupting through MSI-X entry `vector`.
    pub fn create_cq(qid: u16, entries: u16, prp1: u64, vector: u16) -> Command {
        Command {
            opcode: admin::CREATE_CQ,
            prp1,
            cdw10: ((entries as u32 - 1) << 16) | qid as u32,
            // IV, IEN (bit 1) and PC (bit 0).
            cdw11: ((vector as u32) << 16) | 0b11,
            ..Command::default()
        }
    }

    /// Create I/O Submission Queue `qid` of `entries` at `prp1`, completing on `cqid`.
    pub fn create_sq(qid: u16, entries: u16, prp1: u64, cqid: u16) -> Command {
        Command {
            opcode: admin::CREATE_SQ,
            prp1,
            cdw10: ((entries as u32 - 1) << 16) | qid as u32,
            // CQID, priority medium (QPRIO 0b01, ignored without WRR) and PC.
            cdw11: ((cqid as u32) << 16) | (0b01 << 1) | 1,
            ..Command::default()
        }
    }

    pub fn delete_sq(qid: u16) -> Command {
        Command {
            opcode: admin::DELETE_SQ,
            cdw10: qid as u32,
            ..Command::default()
        }
    }

    pub fn delete_cq(qid: u16) -> Command {
        Command {
            opcode: admin::DELETE_CQ,
            cdw10: qid as u32,
            ..Command::default()
        }
    }

    /// Set Features, Number of Queues: `n` submission and `n` completion queues.
    pub fn set_queue_count(n: u16) -> Command {
        let zero_based = (n as u32).saturating_sub(1);
        Command {
            opcode: admin::SET_FEATURES,
            cdw10: FEATURE_QUEUES,
            cdw11: (zero_based << 16) | zero_based,
            ..Command::default()
        }
    }

    /// Read or Write of `blocks` logical blocks from `lba` of namespace `nsid`.
    pub fn transfer(write: bool, nsid: u32, lba: u64, blocks: u32, prp: (u64, u64)) -> Command {
        Command {
            opcode: if write { nvm::WRITE } else { nvm::READ },
            nsid,
            prp1: prp.0,
            prp2: prp.1,
            cdw10: lba as u32,
            cdw11: (lba >> 32) as u32,
            cdw12: blocks.saturating_sub(1) & 0xffff,
            ..Command::default()
        }
    }

    pub fn flush(nsid: u32) -> Command {
        Command {
            opcode: nvm::FLUSH,
            nsid,
            ..Command::default()
        }
    }

    /// Writes the command, with identifier `cid`, as the entry at `at` of `sq`.
    pub fn write<R: Region + ?Sized>(&self, sq: &R, at: usize, cid: u16) -> Result<(), Error> {
        let dw = [
            self.opcode as u32 | (cid as u32) << 16,
            self.nsid,
            0,
            0,
            0,
            0,
            self.prp1 as u32,
            (self.prp1 >> 32) as u32,
            self.prp2 as u32,
            (self.prp2 >> 32) as u32,
            self.cdw10,
            self.cdw11,
            self.cdw12,
            self.cdw13,
            self.cdw14,
            self.cdw15,
        ];
        for (i, word) in dw.iter().enumerate() {
            sq.write32(at + i * 4, *word)?;
        }
        Ok(())
    }
}

/// A completion's status field (NVMe Base 2.0, 4.2.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Status {
    /// Status Code Type: 0 generic, 1 command specific, 2 media and data integrity, 3
    /// path related, 7 vendor specific.
    pub sct: u8,
    pub sc: u8,
    /// Do Not Retry.
    pub dnr: bool,
}

impl Status {
    /// The status in bits 17 to 31 of a completion's fourth dword.
    pub fn from_dword(dw3: u32) -> Status {
        let field = (dw3 >> 17) as u16;
        Status {
            sc: field as u8,
            sct: ((field >> 8) & 0x7) as u8,
            dnr: field & (1 << 14) != 0,
        }
    }

    pub fn ok(&self) -> bool {
        self.sct == 0 && self.sc == 0
    }

    /// The status's name, for the generic and media codes a block driver meets.
    pub fn name(&self) -> &'static str {
        match (self.sct, self.sc) {
            (0, 0x00) => "success",
            (0, 0x01) => "invalid command opcode",
            (0, 0x02) => "invalid field in command",
            (0, 0x03) => "command id conflict",
            (0, 0x04) => "data transfer error",
            (0, 0x05) => "aborted: power loss notification",
            (0, 0x06) => "internal error",
            (0, 0x07) => "aborted: requested",
            (0, 0x08) => "aborted: submission queue deleted",
            (0, 0x0b) => "invalid namespace or format",
            (0, 0x13) => "invalid PRP offset",
            (0, 0x80) => "LBA out of range",
            (0, 0x81) => "capacity exceeded",
            (0, 0x82) => "namespace not ready",
            (1, 0x00) => "completion queue invalid",
            (1, 0x01) => "invalid queue identifier",
            (1, 0x02) => "invalid queue size",
            (1, 0x08) => "invalid interrupt vector",
            (1, 0x20) => "namespace is write protected",
            (2, 0x80) => "write fault",
            (2, 0x81) => "unrecovered read error",
            (2, 0x86) => "access denied",
            (2, 0x87) => "deallocated or unwritten logical block",
            _ => "unnamed status",
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (type {:#x}, code {:#04x}{})",
            self.name(),
            self.sct,
            self.sc,
            if self.dnr { ", do not retry" } else { "" }
        )
    }
}

/// A completion queue entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Completion {
    /// Command specific: what Set Features granted, for one.
    pub dw0: u32,
    pub sq_head: u16,
    pub sq_id: u16,
    pub cid: u16,
    pub status: Status,
}

impl Completion {
    /// The phase tag of the entry at `at`, read before the rest of it.
    pub fn phase<R: Region + ?Sized>(cq: &R, at: usize) -> Result<bool, Error> {
        Ok(cq.read32(at + 12)? & (1 << 16) != 0)
    }

    /// The entry at `at`, read after its phase tag has said it is new.
    pub fn read<R: Region + ?Sized>(cq: &R, at: usize) -> Result<Completion, Error> {
        let dw0 = cq.read32(at)?;
        let dw2 = cq.read32(at + 8)?;
        let dw3 = cq.read32(at + 12)?;
        Ok(Completion {
            dw0,
            sq_head: dw2 as u16,
            sq_id: (dw2 >> 16) as u16,
            cid: dw3 as u16,
            status: Status::from_dword(dw3),
        })
    }
}
