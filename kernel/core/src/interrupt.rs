//! Interrupt objects (SDD 20, f.7; SDD 25, f.2): an interrupt a driver process waits
//! on.
//!
//! A line is either a global system interrupt routed through the I/O APIC to an OSTD
//! IRQ line, or an OSTD IRQ line with no pin at all, which a device raises by writing
//! a message (MSI or MSI-X) the kernel returns with the object. When it fires, the
//! object raises `SIGNALED`; the driver waits on it, services its device, and
//! acknowledges with `interrupt_ack`, which clears the signal.

use alloc::sync::Arc;

use moruna_abi::{Error, Signals, resource_range};
use ostd::{
    arch::irq::{IRQ_CHIP, MappedIrqLine},
    irq::IrqLine,
    sync::SpinLock,
};

use crate::signal::Signaller;

/// The highest global system interrupt number the kernel routes.
pub const GSI_MAX: u32 = 255;

/// The message-signalled interrupt slots in use: one per live MSI object.
static MSI_SLOTS: SpinLock<[bool; resource_range::MSI_END as usize]> =
    SpinLock::new([false; resource_range::MSI_END as usize]);

/// A slot held for the life of an MSI object.
struct MsiSlot(usize);

impl MsiSlot {
    fn take(slot: usize) -> Result<MsiSlot, Error> {
        let mut slots = MSI_SLOTS.lock();
        match slots.get_mut(slot) {
            Some(used) if !*used => {
                *used = true;
                Ok(MsiSlot(slot))
            }
            Some(_) => Err(Error::AccessDenied),
            None => Err(Error::InvalidArgs),
        }
    }
}

impl Drop for MsiSlot {
    fn drop(&mut self) {
        if let Some(used) = MSI_SLOTS.lock().get_mut(self.0) {
            *used = false;
        }
    }
}

/// The base of the x86_64 MSI address window.
const MSI_ADDRESS_BASE: u32 = 0xfee0_0000;
/// The local APIC an MSI in compatibility format is delivered to: the boot processor's,
/// which is 0 on QEMU and on the machines Moruna targets.
const MSI_DESTINATION: u32 = 0;

/// What an interrupt object holds for as long as it lives; dropping it unroutes the
/// line and frees the slot.
enum Line {
    Gsi { _mapped: MappedIrqLine },
    Msi { _line: IrqLine, _slot: MsiSlot },
}

pub struct Interrupt {
    pub signaller: Arc<Signaller>,
    _line: Line,
}

fn line_raising(signaller: &Arc<Signaller>) -> Result<IrqLine, Error> {
    let mut line = IrqLine::alloc().map_err(|_| Error::Limit)?;
    let raised = signaller.clone();
    line.on_active(move |_| raised.update(Signals::NONE, Signals::SIGNALED));
    Ok(line)
}

impl Interrupt {
    pub fn new(gsi: u32) -> Result<Interrupt, Error> {
        if gsi > GSI_MAX {
            return Err(Error::InvalidArgs);
        }
        let chip = IRQ_CHIP.get().ok_or(Error::BadState)?;
        let signaller = Arc::new(Signaller::new(Signals::NONE));
        let line = line_raising(&signaller)?;
        // The chip refuses a line it does not have or one already routed (KK-I7).
        let mapped = chip
            .map_gsi_pin_to(line, gsi)
            .map_err(|_| Error::AccessDenied)?;
        Ok(Interrupt {
            signaller,
            _line: Line::Gsi { _mapped: mapped },
        })
    }

    /// An interrupt raised by a message, in MSI slot `slot`; returns it with the
    /// message's address and data. A slot another live object holds is `AccessDenied`.
    pub fn msi(slot: u64) -> Result<(Interrupt, u32, u32), Error> {
        let slot = usize::try_from(slot).map_err(|_| Error::InvalidArgs)?;
        let held = MsiSlot::take(slot)?;
        let signaller = Arc::new(Signaller::new(Signals::NONE));
        let line = line_raising(&signaller)?;
        let (address, data) = match line.remapping_index() {
            // The remappable format (Intel VT-d, 5.1.5.2): the handle in address bits
            // 19:5 and 2, interrupt format and SHV set, the subhandle (data) zero.
            Some(index) => {
                let index = index as u32;
                (
                    MSI_ADDRESS_BASE
                        | ((index & 0x7fff) << 5)
                        | (1 << 4)
                        | (1 << 3)
                        | (((index >> 15) & 1) << 2),
                    0,
                )
            }
            // The compatibility format: the destination in bits 19:12, physical
            // destination mode; the vector in the data, fixed delivery, edge.
            None => (
                MSI_ADDRESS_BASE | (MSI_DESTINATION << 12),
                line.num() as u32,
            ),
        };
        Ok((
            Interrupt {
                signaller,
                _line: Line::Msi {
                    _line: line,
                    _slot: held,
                },
            },
            address,
            data,
        ))
    }

    pub fn ack(&self) {
        self.signaller.update(Signals::SIGNALED, Signals::NONE);
    }
}
