//! Interrupt objects (SDD 20, f.7): an interrupt line a driver process waits on.
//!
//! The line is a global system interrupt routed through the I/O APIC to an OSTD IRQ
//! line. When it fires, the object raises `SIGNALED`; the driver waits on it, services
//! its device, and acknowledges with `interrupt_ack`, which clears the signal.

use alloc::sync::Arc;

use moruna_abi::{Error, Signals};
use ostd::{
    arch::irq::{IRQ_CHIP, MappedIrqLine},
    irq::IrqLine,
};

use crate::signal::Signaller;

/// The highest global system interrupt number the kernel routes.
pub const GSI_MAX: u32 = 255;

pub struct Interrupt {
    pub signaller: Arc<Signaller>,
    _line: MappedIrqLine,
}

impl Interrupt {
    pub fn new(gsi: u32) -> Result<Interrupt, Error> {
        if gsi > GSI_MAX {
            return Err(Error::InvalidArgs);
        }
        let chip = IRQ_CHIP.get().ok_or(Error::BadState)?;
        let mut line = IrqLine::alloc().map_err(|_| Error::NoMemory)?;
        let signaller = Arc::new(Signaller::new(Signals::NONE));
        let raised = signaller.clone();
        line.on_active(move |_| raised.update(Signals::NONE, Signals::SIGNALED));
        // The chip refuses a line it does not have or one already routed (KK-I7).
        let mapped = chip
            .map_gsi_pin_to(line, gsi)
            .map_err(|_| Error::AccessDenied)?;
        Ok(Interrupt {
            signaller,
            _line: mapped,
        })
    }

    pub fn ack(&self) {
        self.signaller.update(Signals::SIGNALED, Signals::NONE);
    }
}
