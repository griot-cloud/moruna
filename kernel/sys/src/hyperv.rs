//! Hyper-V's mechanism for VMBus (ABI 2.7; `architecture/moruna-os-system.md`, section
//! 1F), for the `vmbus` process that holds `root.hyperv`, and the signal handles it gives
//! VMBus drivers.

use core::sync::atomic::{AtomicU32, Ordering};

pub use moruna_abi::hyperv::{BUSY, EVENT_FLAGS, MESSAGE_SIZE, PAYLOAD_MAX, SUCCESS};
use moruna_abi::{RawHandle, hyperv, nr};

use crate::{Handle, Interrupt, Result, check, raw};

/// `root.hyperv`: the core's six calls of Hyper-V's mechanism.
#[derive(Debug)]
pub struct Hyperv(pub Handle);

impl Hyperv {
    /// Posts `payload`, at most [`PAYLOAD_MAX`] bytes, as a message of `message_type` to
    /// `connection` (`HvCallPostMessage`). Returns Hyper-V's status: [`SUCCESS`], [`BUSY`]
    /// when it has no room for the message now, or another.
    pub fn post(&self, connection: u32, message_type: u32, payload: &[u8]) -> Result<u64> {
        check(raw::hyperv_post(
            self.0.raw(),
            connection,
            message_type,
            payload,
        ))
    }

    /// Copies the message waiting on SINT 2 to `out`, as Hyper-V wrote it, and frees its
    /// slot for the next. False when none waits; it never waits for one.
    pub fn take(&self, out: &mut [u8; MESSAGE_SIZE]) -> Result<bool> {
        check(raw::hyperv_take(self.0.raw(), out)).map(|taken| taken != 0)
    }

    /// CPU 0's event flags, mapped into this process: the same mapping each time.
    pub fn events(&self) -> Result<EventFlags> {
        let address = check(raw::call1(nr::HYPERV_MAP_EVENTS, u64::from(self.0.raw())).0)?;
        Ok(EventFlags { address })
    }

    /// An interrupt object for SINT 2, raised for each message and event flag Hyper-V
    /// gives it; waited on and acknowledged as a device's is.
    pub fn interrupt(&self) -> Result<Interrupt> {
        self.sint().map(|sint| sint.interrupt)
    }

    /// SINT 2's interrupt, as [`interrupt`](Self::interrupt) gives it, and the VP it
    /// arrives on.
    pub fn sint(&self) -> Result<Sint> {
        let (v, vp_index) = raw::call1(nr::HYPERV_INTERRUPT, u64::from(self.0.raw()));
        let v = check(v)?;
        Ok(Sint {
            interrupt: Interrupt(Handle::from_raw(v as RawHandle)),
            vp_index: vp_index as u32,
        })
    }

    /// A handle that signals `connection` and nothing else, to give to the driver of the
    /// channel whose offer named it.
    pub fn signal(&self, connection: u32) -> Result<HvSignal> {
        let v = check(
            raw::call2(
                nr::HYPERV_SIGNAL_CREATE,
                u64::from(self.0.raw()),
                u64::from(connection),
            )
            .0,
        )?;
        Ok(HvSignal(Handle::from_raw(v as RawHandle)))
    }
}

/// SINT 2 of CPU 0's SynIC: its interrupt, and CPU 0's VP index.
#[derive(Debug)]
pub struct Sint {
    /// Raised for each message and event flag Hyper-V gives SINT 2.
    pub interrupt: Interrupt,
    /// CPU 0's VP index, as Hyper-V numbers its virtual processors: the target a VMBus
    /// client names for its contact and every channel. It is not always 0.
    pub vp_index: u32,
}

/// The right to signal one connection ID: a VMBus channel's driver signals the host with
/// it when its ring says the host needs it.
#[derive(Debug)]
pub struct HvSignal(pub Handle);

impl HvSignal {
    /// `HvCallSignalEvent` on the connection ID, event flag 0. Returns Hyper-V's status.
    pub fn signal(&self) -> Result<u64> {
        check(raw::call1(nr::HYPERV_SIGNAL, u64::from(self.0.raw())).0)
    }
}

/// SINT 2's event flags, in CPU 0's event-flags page, mapped for as long as the process
/// lives: Hyper-V sets a channel's flag when the host signals it.
#[derive(Debug)]
pub struct EventFlags {
    address: u64,
}

impl EventFlags {
    /// Tests event flag `flag` and clears it, as one atomic step: whether it was set. A
    /// flag beyond SINT 2's [`EVENT_FLAGS`] is never set.
    pub fn test_and_clear(&self, flag: u16) -> bool {
        if u32::from(flag) >= EVENT_FLAGS {
            return false;
        }
        let at = self.address + hyperv::EVENT_FLAGS_OFFSET as u64 + u64::from(flag / 32) * 4;
        let bit = 1u32 << (flag % 32);
        // SAFETY: the kernel keeps the event-flags page mapped, readable and writable, at
        // `address` for as long as the process lives, and `at` is a 4-aligned word of SINT
        // 2's flags inside it. Hyper-V sets the flags with atomic writes, and every access
        // here is atomic too.
        let word = unsafe { AtomicU32::from_ptr(at as *mut u32) };
        word.fetch_and(!bit, Ordering::AcqRel) & bit != 0
    }
}
