//! What a virtio device is to the transport: its id, features, config space and queues,
//! independent of how the guest reaches it. `virtio-mmio` ([`super::mmio`]) is the only
//! transport today; a PCI transport later would drive the same trait.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use virtio_bindings::virtio_config::VIRTIO_F_VERSION_1;
use virtio_bindings::virtio_ids;
use virtio_queue::Queue;

use super::{Irq, Mem};
use crate::error::Result;

/// `virtio-blk`.
pub const ID_BLOCK: u32 = virtio_ids::VIRTIO_ID_BLOCK;
/// `virtio-vsock`.
pub const ID_VSOCK: u32 = virtio_ids::VIRTIO_ID_VSOCK;
/// `virtio-mem`.
pub const ID_MEM: u32 = virtio_ids::VIRTIO_ID_MEM;

/// The feature every device offers and every driver must accept: the modern (1.x) interface.
pub const F_VERSION_1: u64 = 1 << VIRTIO_F_VERSION_1;

/// Device status bits (virtio 1.2, 2.1).
pub mod status {
    /// The guest noticed the device.
    pub const ACKNOWLEDGE: u32 = 1;
    /// The guest has a driver for it.
    pub const DRIVER: u32 = 2;
    /// The driver is ready.
    pub const DRIVER_OK: u32 = 4;
    /// Feature negotiation is complete.
    pub const FEATURES_OK: u32 = 8;
    /// The device hit an error it cannot recover from without a reset.
    pub const DEVICE_NEEDS_RESET: u32 = 64;
    /// The driver gave up on the device.
    pub const FAILED: u32 = 128;
}

/// Interrupt status bits (virtio-mmio `InterruptStatus`).
pub mod int {
    /// A used buffer was returned.
    pub const USED_RING: u32 = 1;
    /// The configuration space changed.
    pub const CONFIG_CHANGE: u32 = 2;
}

/// Queue size the block and vsock devices offer: 256, what Firecracker and Cloud Hypervisor
/// offer, enough in-flight requests for one guest's block and vsock traffic.
pub const QUEUE_MAX_SIZE: u16 = 256;

/// A device's interrupt: the status the guest reads and acknowledges, and the line it is
/// raised on. Shared between the vCPU thread (through the transport) and any backend thread.
pub struct Interrupt {
    status: AtomicU32,
    irq: Arc<dyn Irq>,
}

impl Interrupt {
    /// An interrupt raised on `irq`.
    pub fn new(irq: Arc<dyn Irq>) -> Arc<Self> {
        Arc::new(Interrupt {
            status: AtomicU32::new(0),
            irq,
        })
    }

    /// Record that used buffers were returned and raise the line.
    pub fn signal_used(&self) -> Result<()> {
        self.signal(int::USED_RING)
    }

    /// Record that the configuration changed and raise the line.
    pub fn signal_config(&self) -> Result<()> {
        self.signal(int::CONFIG_CHANGE)
    }

    fn signal(&self, bit: u32) -> Result<()> {
        self.status.fetch_or(bit, Ordering::SeqCst);
        self.irq
            .trigger()
            .map_err(|e| crate::error::VmmError::io("irq", "trigger", &e))
    }

    /// The pending status bits.
    pub fn status(&self) -> u32 {
        self.status.load(Ordering::SeqCst)
    }

    /// The guest acknowledged `bits`.
    pub fn ack(&self, bits: u32) {
        self.status.fetch_and(!bits, Ordering::SeqCst);
    }

    /// Clear everything (device reset).
    pub fn clear(&self) {
        self.status.store(0, Ordering::SeqCst);
    }
}

/// A virtio device behind a transport.
pub trait VirtioDevice: Send {
    /// A short name for diagnostics.
    fn name(&self) -> &'static str;
    /// The virtio device id.
    fn device_type(&self) -> u32;
    /// The features offered; [`F_VERSION_1`] is added by the transport.
    fn features(&self) -> u64;
    /// The number of queues and each one's maximum size.
    fn queue_max_sizes(&self) -> Vec<u16>;
    /// Read the device configuration space.
    fn read_config(&self, offset: u64, data: &mut [u8]);
    /// Write the device configuration space (most devices ignore writes).
    fn write_config(&mut self, offset: u64, data: &[u8]);
    /// The driver set DRIVER_OK with `acked` features; the queues are valid.
    fn activate(&mut self, acked: u64, interrupt: Arc<Interrupt>) -> Result<()>;
    /// The driver notified queue `index`. Returns true when used buffers were returned and
    /// the guest should be interrupted.
    fn process_queue(&mut self, index: usize, queues: &mut [Queue], mem: &Mem) -> Result<bool>;
    /// Back to the state before activation.
    fn reset(&mut self);
}

/// Copy `src` (a config space image) into `data` at `offset`; out-of-range bytes read as 0.
pub fn read_config_bytes(src: &[u8], offset: u64, data: &mut [u8]) {
    for (i, b) in data.iter_mut().enumerate() {
        *b = usize::try_from(offset)
            .ok()
            .and_then(|o| o.checked_add(i))
            .and_then(|o| src.get(o))
            .copied()
            .unwrap_or(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::counting_irq;

    #[test]
    fn vm_t8_interrupt_status_and_ack() {
        let (c, irq) = counting_irq();
        let i = Interrupt::new(irq);
        i.signal_used().unwrap();
        i.signal_config().unwrap();
        assert_eq!(i.status(), int::USED_RING | int::CONFIG_CHANGE);
        assert_eq!(c.get(), 2);
        i.ack(int::USED_RING);
        assert_eq!(i.status(), int::CONFIG_CHANGE);
        i.clear();
        assert_eq!(i.status(), 0);
    }

    #[test]
    fn vm_t8_config_bytes_read_past_the_end_as_zero() {
        let src = [1u8, 2, 3];
        let mut d = [9u8; 4];
        read_config_bytes(&src, 1, &mut d);
        assert_eq!(d, [2, 3, 0, 0]);
        read_config_bytes(&src, u64::MAX, &mut d);
        assert_eq!(d, [0; 4]);
    }
}
