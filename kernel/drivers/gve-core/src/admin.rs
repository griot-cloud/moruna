//! The admin queue (`gve_adminq.h`; `gve.rst`, "Admin Queue"): a page of 64-byte
//! commands. The driver writes a command into the next slot, writes the count of
//! commands it has issued to the doorbell, and polls the event counter until the
//! device has executed that many; the device writes each command's status into it.

use alloc::format;

use moruna_mmio::{Clock, Region};

use crate::{GveError, PAGE, be32, put_be16, put_be32, put_be64, regs};

/// Bytes of a command, and commands in the queue's page.
pub const COMMAND: usize = 64;
pub const SLOTS: u32 = (PAGE / COMMAND) as u32;
/// The device descriptor version this driver reads.
pub const DESCRIPTOR_VERSION: u32 = 1;
/// The queue page list id that means raw addressing.
pub const RAW_ADDRESSING_QPL: u32 = 0xffff_ffff;
/// The stride of the interrupt doorbell index array the device writes: one cache line.
pub const IRQ_DB_STRIDE: u32 = 64;

/// Opcodes.
pub mod opcode {
    pub const DESCRIBE_DEVICE: u32 = 0x1;
    pub const CONFIGURE_DEVICE_RESOURCES: u32 = 0x2;
    pub const REGISTER_PAGE_LIST: u32 = 0x3;
    pub const UNREGISTER_PAGE_LIST: u32 = 0x4;
    pub const CREATE_TX_QUEUE: u32 = 0x5;
    pub const CREATE_RX_QUEUE: u32 = 0x6;
    pub const DESTROY_TX_QUEUE: u32 = 0x7;
    pub const DESTROY_RX_QUEUE: u32 = 0x8;
    pub const DECONFIGURE_DEVICE_RESOURCES: u32 = 0x9;
}

/// Statuses.
pub mod status {
    pub const UNSET: u32 = 0x0;
    pub const PASSED: u32 = 0x1;
    pub const UNIMPLEMENTED: u32 = 0xffff_fffe;
}

/// A status's name.
pub fn status_name(s: u32) -> &'static str {
    match s {
        0x0 => "unset",
        0x1 => "passed",
        0xffff_fff0 => "aborted",
        0xffff_fff1 => "already exists",
        0xffff_fff2 => "cancelled",
        0xffff_fff3 => "data loss",
        0xffff_fff4 => "deadline exceeded",
        0xffff_fff5 => "failed precondition",
        0xffff_fff6 => "internal error",
        0xffff_fff7 => "invalid argument",
        0xffff_fff8 => "not found",
        0xffff_fff9 => "out of range",
        0xffff_fffa => "permission denied",
        0xffff_fffb => "unauthenticated",
        0xffff_fffc => "resource exhausted",
        0xffff_fffd => "unavailable",
        0xffff_fffe => "unimplemented",
        _ => "unknown error",
    }
}

/// The commands this driver issues.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    DescribeDevice {
        addr: u64,
        length: u32,
    },
    ConfigureDeviceResources {
        counters: u64,
        num_counters: u32,
        irq_dbs: u64,
        num_irq_dbs: u32,
        format: u8,
    },
    RegisterPageList {
        id: u32,
        pages: u32,
        list: u64,
    },
    UnregisterPageList {
        id: u32,
    },
    CreateTxQueue {
        id: u32,
        resources: u64,
        ring: u64,
        qpl: u32,
        ntfy: u32,
        size: u16,
    },
    CreateRxQueue {
        id: u32,
        ntfy: u32,
        resources: u64,
        descriptors: u64,
        data: u64,
        qpl: u32,
        size: u16,
        buffer: u16,
    },
    DestroyTxQueue {
        id: u32,
    },
    DestroyRxQueue {
        id: u32,
    },
    DeconfigureDeviceResources,
}

impl Command {
    pub fn opcode(&self) -> u32 {
        match self {
            Command::DescribeDevice { .. } => opcode::DESCRIBE_DEVICE,
            Command::ConfigureDeviceResources { .. } => opcode::CONFIGURE_DEVICE_RESOURCES,
            Command::RegisterPageList { .. } => opcode::REGISTER_PAGE_LIST,
            Command::UnregisterPageList { .. } => opcode::UNREGISTER_PAGE_LIST,
            Command::CreateTxQueue { .. } => opcode::CREATE_TX_QUEUE,
            Command::CreateRxQueue { .. } => opcode::CREATE_RX_QUEUE,
            Command::DestroyTxQueue { .. } => opcode::DESTROY_TX_QUEUE,
            Command::DestroyRxQueue { .. } => opcode::DESTROY_RX_QUEUE,
            Command::DeconfigureDeviceResources => opcode::DECONFIGURE_DEVICE_RESOURCES,
        }
    }

    /// Writes the command, its status unset, at `at` of `r`. The offsets are those of
    /// `union gve_adminq_command`: the opcode, the status, then the command's fields.
    pub fn write<R: Region + ?Sized>(&self, r: &R, at: usize) -> Result<(), moruna_abi::Error> {
        for o in (0..COMMAND).step_by(8) {
            r.write64(at + o, 0)?;
        }
        put_be32(r, at, self.opcode())?;
        match *self {
            Command::DescribeDevice { addr, length } => {
                put_be64(r, at + 8, addr)?;
                put_be32(r, at + 16, DESCRIPTOR_VERSION)?;
                put_be32(r, at + 20, length)?;
            }
            Command::ConfigureDeviceResources {
                counters,
                num_counters,
                irq_dbs,
                num_irq_dbs,
                format,
            } => {
                put_be64(r, at + 8, counters)?;
                put_be64(r, at + 16, irq_dbs)?;
                put_be32(r, at + 24, num_counters)?;
                put_be32(r, at + 28, num_irq_dbs)?;
                put_be32(r, at + 32, IRQ_DB_STRIDE)?;
                // The notification blocks' MSI-X entries start at 0, the management
                // interrupt's is after them.
                put_be32(r, at + 36, 0)?;
                r.write8(at + 40, format)?;
            }
            Command::RegisterPageList { id, pages, list } => {
                put_be32(r, at + 8, id)?;
                put_be32(r, at + 12, pages)?;
                put_be64(r, at + 16, list)?;
                put_be64(r, at + 24, PAGE as u64)?;
            }
            Command::UnregisterPageList { id }
            | Command::DestroyTxQueue { id }
            | Command::DestroyRxQueue { id } => put_be32(r, at + 8, id)?,
            Command::CreateTxQueue {
                id,
                resources,
                ring,
                qpl,
                ntfy,
                size,
            } => {
                put_be32(r, at + 8, id)?;
                put_be64(r, at + 16, resources)?;
                put_be64(r, at + 24, ring)?;
                put_be32(r, at + 32, qpl)?;
                put_be32(r, at + 36, ntfy)?;
                put_be16(r, at + 48, size)?;
            }
            Command::CreateRxQueue {
                id,
                ntfy,
                resources,
                descriptors,
                data,
                qpl,
                size,
                buffer,
            } => {
                put_be32(r, at + 8, id)?;
                put_be32(r, at + 12, id)?;
                put_be32(r, at + 20, ntfy)?;
                put_be64(r, at + 24, resources)?;
                put_be64(r, at + 32, descriptors)?;
                put_be64(r, at + 40, data)?;
                put_be32(r, at + 48, qpl)?;
                put_be16(r, at + 52, size)?;
                put_be16(r, at + 54, buffer)?;
            }
            Command::DeconfigureDeviceResources => {}
        }
        Ok(())
    }
}

/// The admin queue: its page and how many commands have been issued.
pub struct AdminQueue<M: Region> {
    page: M,
    pub addr: u64,
    issued: u32,
}

impl<M: Region> AdminQueue<M> {
    pub fn new(page: M, addr: u64) -> AdminQueue<M> {
        AdminQueue {
            page,
            addr,
            issued: 0,
        }
    }

    /// Executes `command` and waits for it, at most `timeout_ns`. A device whose event
    /// counter is not the count of commands issued, before or after, is malformed; a
    /// status other than passed is the command's failure.
    pub fn execute<R: Region + ?Sized>(
        &mut self,
        bar: &R,
        clock: &dyn Clock,
        timeout_ns: u64,
        command: &Command,
    ) -> Result<(), GveError> {
        let before = be32(bar, regs::ADMINQ_EVENT_COUNTER)?;
        if before != self.issued {
            return Err(GveError::Malformed(format!(
                "an admin event counter of {before} with {} commands issued",
                self.issued
            )));
        }
        let at = (self.issued % SLOTS) as usize * COMMAND;
        command.write(&self.page, at)?;
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        self.issued = self.issued.wrapping_add(1);
        put_be32(bar, regs::ADMINQ_DOORBELL, self.issued)?;
        let deadline = clock.now().saturating_add(timeout_ns);
        loop {
            let counter = be32(bar, regs::ADMINQ_EVENT_COUNTER)?;
            if counter == self.issued {
                break;
            }
            if counter != before {
                return Err(GveError::Malformed(format!(
                    "an admin event counter of {counter} with {} commands issued",
                    self.issued
                )));
            }
            if clock.now() >= deadline {
                return Err(GveError::Timeout("execute an admin command"));
            }
            clock.pause();
        }
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        match be32(&self.page, at + 4)? {
            status::PASSED => Ok(()),
            status::UNSET => Err(GveError::Malformed(format!(
                "admin command {:#x} executed with its status unset",
                command.opcode()
            ))),
            s => Err(GveError::Command {
                opcode: command.opcode(),
                status: s,
            }),
        }
    }

    /// Forgets the commands issued: the device has been reset.
    pub fn reset(&mut self) -> Result<(), GveError> {
        self.issued = 0;
        for o in (0..PAGE).step_by(8) {
            self.page.write64(o, 0)?;
        }
        Ok(())
    }
}
