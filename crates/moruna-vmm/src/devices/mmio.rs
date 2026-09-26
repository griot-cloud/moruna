//! The `virtio-mmio` transport, version 2 (virtio 1.2, 4.2): the register state machine a
//! guest driver programs, wrapped around any [`VirtioDevice`].
//!
//! The rules the state machine keeps:
//! - features are negotiated only between ACKNOWLEDGE|DRIVER and FEATURES_OK, and FEATURES_OK
//!   is refused (not latched) when the driver accepted a feature the device did not offer or
//!   did not accept `VIRTIO_F_VERSION_1`;
//! - queue registers are writable only after FEATURES_OK and before DRIVER_OK, and only for a
//!   queue that is not ready;
//! - DRIVER_OK activates the device only if every ready queue is valid in guest memory; if
//!   activation or a later request fails, the device sets DEVICE_NEEDS_RESET and raises a
//!   configuration interrupt instead of taking the monitor down;
//! - writing 0 to Status resets the device and every queue.

use std::sync::Arc;

use virtio_queue::{Queue, QueueT};

use super::virtio::{F_VERSION_1, Interrupt, VirtioDevice, status};
use super::{Device, Mem};
use crate::error::{Result, VmmError};

/// Register offsets (virtio 1.2, table 4.1).
pub mod regs {
    /// "virt".
    pub const MAGIC: u64 = 0x000;
    /// 2 (modern).
    pub const VERSION: u64 = 0x004;
    /// Virtio device id.
    pub const DEVICE_ID: u64 = 0x008;
    /// Vendor id.
    pub const VENDOR_ID: u64 = 0x00c;
    /// Device features, 32 bits selected by `DEVICE_FEATURES_SEL`.
    pub const DEVICE_FEATURES: u64 = 0x010;
    /// Selects the device feature word.
    pub const DEVICE_FEATURES_SEL: u64 = 0x014;
    /// Driver features, 32 bits selected by `DRIVER_FEATURES_SEL`.
    pub const DRIVER_FEATURES: u64 = 0x020;
    /// Selects the driver feature word.
    pub const DRIVER_FEATURES_SEL: u64 = 0x024;
    /// Selects the queue the queue registers refer to.
    pub const QUEUE_SEL: u64 = 0x030;
    /// Maximum size of the selected queue.
    pub const QUEUE_NUM_MAX: u64 = 0x034;
    /// Size of the selected queue.
    pub const QUEUE_NUM: u64 = 0x038;
    /// The selected queue is ready.
    pub const QUEUE_READY: u64 = 0x044;
    /// The driver has new buffers in the queue written here.
    pub const QUEUE_NOTIFY: u64 = 0x050;
    /// Pending interrupt reasons.
    pub const INTERRUPT_STATUS: u64 = 0x060;
    /// Acknowledge interrupt reasons.
    pub const INTERRUPT_ACK: u64 = 0x064;
    /// Device status.
    pub const STATUS: u64 = 0x070;
    /// Descriptor table address, low 32 bits.
    pub const QUEUE_DESC_LOW: u64 = 0x080;
    /// Descriptor table address, high 32 bits.
    pub const QUEUE_DESC_HIGH: u64 = 0x084;
    /// Available ring address, low.
    pub const QUEUE_DRIVER_LOW: u64 = 0x090;
    /// Available ring address, high.
    pub const QUEUE_DRIVER_HIGH: u64 = 0x094;
    /// Used ring address, low.
    pub const QUEUE_DEVICE_LOW: u64 = 0x0a0;
    /// Used ring address, high.
    pub const QUEUE_DEVICE_HIGH: u64 = 0x0a4;
    /// Shared memory region select (no regions: lengths read as all ones).
    pub const SHM_SEL: u64 = 0x0ac;
    /// Shared memory length, low.
    pub const SHM_LEN_LOW: u64 = 0x0b0;
    /// Shared memory length, high.
    pub const SHM_LEN_HIGH: u64 = 0x0b4;
    /// Configuration generation.
    pub const CONFIG_GENERATION: u64 = 0x0fc;
    /// Start of the device configuration space.
    pub const CONFIG: u64 = 0x100;
}

/// "virt", little-endian.
pub const MAGIC_VALUE: u32 = 0x7472_6976;
/// Vendor id reported by every Moruna device: "MRNA".
pub const VENDOR: u32 = 0x414e_524d;
/// Size of one device's MMIO window.
pub const WINDOW_BYTES: u64 = 0x1000;

/// A virtio device on the `virtio-mmio` transport.
pub struct MmioTransport<D: VirtioDevice> {
    device: D,
    mem: Mem,
    interrupt: Arc<Interrupt>,
    queues: Vec<Queue>,
    queue_sel: u32,
    device_features_sel: u32,
    driver_features_sel: u32,
    driver_features: u64,
    status: u32,
    config_generation: u32,
    active: bool,
    last_error: Option<String>,
}

impl<D: VirtioDevice> MmioTransport<D> {
    /// Wrap `device`, which will read and write `mem` and interrupt through `interrupt`.
    pub fn new(device: D, mem: Mem, interrupt: Arc<Interrupt>) -> Result<Self> {
        let queues = device
            .queue_max_sizes()
            .into_iter()
            .map(|s| {
                Queue::new(s).map_err(|e| VmmError::device(device.name(), format!("queue: {e}")))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(MmioTransport {
            device,
            mem,
            interrupt,
            queues,
            queue_sel: 0,
            device_features_sel: 0,
            driver_features_sel: 0,
            driver_features: 0,
            status: 0,
            config_generation: 0,
            active: false,
            last_error: None,
        })
    }

    /// The wrapped device.
    pub fn device(&self) -> &D {
        &self.device
    }

    /// Mutable access to the device outside the queue path (resize requests).
    pub fn device_mut(&mut self) -> &mut D {
        &mut self.device
    }

    /// The device status register.
    pub fn status(&self) -> u32 {
        self.status
    }

    /// True between a successful DRIVER_OK and the next reset.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// The failure that set DEVICE_NEEDS_RESET, if any.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// The device's interrupt.
    pub fn interrupt(&self) -> Arc<Interrupt> {
        self.interrupt.clone()
    }

    /// Run `f` on the device with its queues and memory, from a backend thread; a device
    /// that is not active is not touched and `f` is not called. A failure marks the device
    /// broken, as a failed request does. Returns `f`'s result.
    pub fn with_active<R>(
        &mut self,
        f: impl FnOnce(&mut D, &mut [Queue], &Mem) -> Result<R>,
    ) -> Option<R> {
        if !self.active {
            return None;
        }
        match f(&mut self.device, &mut self.queues, &self.mem) {
            Ok(r) => Some(r),
            Err(e) => {
                self.fail(e.to_string());
                None
            }
        }
    }

    /// The configuration space changed underneath the guest: bump the generation and raise
    /// a configuration interrupt.
    pub fn config_changed(&mut self) -> Result<()> {
        self.config_generation = self.config_generation.wrapping_add(1);
        self.interrupt.signal_config()
    }

    fn fail(&mut self, msg: String) {
        self.last_error = Some(msg);
        self.status |= status::DEVICE_NEEDS_RESET;
        self.active = false;
        // The guest learns of the failure through the configuration interrupt; if that
        // cannot be raised either there is nothing further to tell it.
        let _ = self.config_changed();
    }

    fn reset(&mut self) {
        self.device.reset();
        for q in &mut self.queues {
            q.reset();
        }
        self.queue_sel = 0;
        self.device_features_sel = 0;
        self.driver_features_sel = 0;
        self.driver_features = 0;
        self.status = 0;
        self.active = false;
        self.last_error = None;
        self.interrupt.clear();
    }

    fn offered(&self) -> u64 {
        self.device.features() | F_VERSION_1
    }

    fn selected(&mut self) -> Option<&mut Queue> {
        self.queues.get_mut(self.queue_sel as usize)
    }

    /// Queue registers may be written after FEATURES_OK, before DRIVER_OK, while the selected
    /// queue is not ready.
    fn queue_writable(&self) -> bool {
        self.status & status::FEATURES_OK != 0
            && self.status & status::DRIVER_OK == 0
            && self
                .queues
                .get(self.queue_sel as usize)
                .is_some_and(|q| !q.ready())
    }

    fn set_status(&mut self, v: u32) {
        if v == 0 {
            self.reset();
            return;
        }
        let added = v & !self.status;
        if added & status::FEATURES_OK != 0 {
            let offered = self.offered();
            if self.driver_features & !offered != 0 || self.driver_features & F_VERSION_1 == 0 {
                // Refused: FEATURES_OK is not latched and the driver sees it missing.
                self.status = v & !status::FEATURES_OK;
                return;
            }
        }
        self.status = v;
        if added & status::DRIVER_OK != 0 && self.status & status::FEATURES_OK != 0 {
            self.activate();
        }
    }

    fn activate(&mut self) {
        let mem = &self.mem;
        if let Some(i) = self
            .queues
            .iter()
            .position(|q| q.ready() && !q.is_valid(mem))
        {
            self.fail(format!("queue {i} is not valid in guest memory"));
            return;
        }
        match self
            .device
            .activate(self.driver_features, self.interrupt.clone())
        {
            Ok(()) => self.active = true,
            Err(e) => self.fail(e.to_string()),
        }
    }

    fn notify(&mut self, index: u32) {
        if !self.active {
            return;
        }
        let i = index as usize;
        if !self.queues.get(i).is_some_and(|q| q.ready()) {
            return;
        }
        match self.device.process_queue(i, &mut self.queues, &self.mem) {
            Ok(true) => {
                if let Err(e) = self.interrupt.signal_used() {
                    self.fail(e.to_string());
                }
            }
            Ok(false) => {}
            Err(e) => self.fail(e.to_string()),
        }
    }

    fn write_reg(&mut self, offset: u64, v: u32) {
        use regs::*;
        match offset {
            DEVICE_FEATURES_SEL => self.device_features_sel = v,
            DRIVER_FEATURES_SEL => self.driver_features_sel = v,
            DRIVER_FEATURES => {
                let feature_phase =
                    self.status & status::DRIVER != 0 && self.status & status::FEATURES_OK == 0;
                if feature_phase {
                    match self.driver_features_sel {
                        0 => {
                            self.driver_features = (self.driver_features & !0xffff_ffff) | v as u64
                        }
                        1 => {
                            self.driver_features =
                                (self.driver_features & 0xffff_ffff) | ((v as u64) << 32)
                        }
                        _ => {}
                    }
                }
            }
            QUEUE_SEL => self.queue_sel = v,
            QUEUE_NUM if self.queue_writable() => {
                if let Some(q) = self.selected() {
                    q.set_size(v as u16);
                }
            }
            QUEUE_READY => {
                let fo = self.status & status::FEATURES_OK != 0;
                let dok = self.status & status::DRIVER_OK != 0;
                if let Some(q) = self.selected()
                    && fo
                    && !dok
                {
                    q.set_ready(v == 1);
                }
            }
            QUEUE_DESC_LOW | QUEUE_DESC_HIGH | QUEUE_DRIVER_LOW | QUEUE_DRIVER_HIGH
            | QUEUE_DEVICE_LOW | QUEUE_DEVICE_HIGH
                if self.queue_writable() =>
            {
                let part = |o: u64, l: u64| if o == l { Some(v) } else { None };
                if let Some(q) = self.selected() {
                    match offset {
                        QUEUE_DESC_LOW | QUEUE_DESC_HIGH => q.set_desc_table_address(
                            part(offset, QUEUE_DESC_LOW),
                            part(offset, QUEUE_DESC_HIGH),
                        ),
                        QUEUE_DRIVER_LOW | QUEUE_DRIVER_HIGH => q.set_avail_ring_address(
                            part(offset, QUEUE_DRIVER_LOW),
                            part(offset, QUEUE_DRIVER_HIGH),
                        ),
                        _ => q.set_used_ring_address(
                            part(offset, QUEUE_DEVICE_LOW),
                            part(offset, QUEUE_DEVICE_HIGH),
                        ),
                    }
                }
            }
            QUEUE_NOTIFY => self.notify(v),
            INTERRUPT_ACK => self.interrupt.ack(v),
            STATUS => self.set_status(v),
            // Writes to read-only registers, and queue writes outside their phase, are
            // ignored as the specification requires of a device.
            _ => {}
        }
    }

    fn read_reg(&self, offset: u64) -> u32 {
        use regs::*;
        let q = self.queues.get(self.queue_sel as usize);
        match offset {
            MAGIC => MAGIC_VALUE,
            VERSION => 2,
            DEVICE_ID => self.device.device_type(),
            VENDOR_ID => VENDOR,
            DEVICE_FEATURES => match self.device_features_sel {
                0 => self.offered() as u32,
                1 => (self.offered() >> 32) as u32,
                _ => 0,
            },
            QUEUE_NUM_MAX => q.map_or(0, |q| q.max_size() as u32),
            QUEUE_READY => q.map_or(0, |q| q.ready() as u32),
            INTERRUPT_STATUS => self.interrupt.status(),
            STATUS => self.status,
            SHM_LEN_LOW | SHM_LEN_HIGH => u32::MAX,
            CONFIG_GENERATION => self.config_generation,
            _ => 0,
        }
    }
}

impl<D: VirtioDevice> Device for MmioTransport<D> {
    fn name(&self) -> &'static str {
        self.device.name()
    }

    fn read(&mut self, offset: u64, data: &mut [u8]) {
        if offset >= regs::CONFIG {
            self.device.read_config(offset - regs::CONFIG, data);
            return;
        }
        if data.len() != 4 {
            data.fill(0);
            return;
        }
        data.copy_from_slice(&self.read_reg(offset).to_le_bytes());
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        if offset >= regs::CONFIG {
            self.device.write_config(offset - regs::CONFIG, data);
            return;
        }
        let Ok(bytes) = <[u8; 4]>::try_from(data) else {
            return;
        };
        self.write_reg(offset, u32::from_le_bytes(bytes));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::devices::virtio::{int, read_config_bytes};
    use crate::testing::{DriverQueue, bring_up, counting_irq, guest_memory, rd, wr};

    /// A device with two queues that returns every chain it is given, used length 0.
    pub(crate) struct Loop {
        pub activated: Option<u64>,
        pub fail_activate: bool,
        pub fail_process: bool,
        pub config: [u8; 8],
        pub resets: u32,
    }

    impl Loop {
        pub fn new() -> Self {
            Loop {
                activated: None,
                fail_activate: false,
                fail_process: false,
                config: [1, 2, 3, 4, 5, 6, 7, 8],
                resets: 0,
            }
        }
    }

    impl VirtioDevice for Loop {
        fn name(&self) -> &'static str {
            "loop"
        }
        fn device_type(&self) -> u32 {
            0x42
        }
        fn features(&self) -> u64 {
            1 << 3
        }
        fn queue_max_sizes(&self) -> Vec<u16> {
            vec![16, 8]
        }
        fn read_config(&self, offset: u64, data: &mut [u8]) {
            read_config_bytes(&self.config, offset, data);
        }
        fn write_config(&mut self, offset: u64, data: &[u8]) {
            if let Some(b) = self.config.get_mut(offset as usize) {
                *b = data[0];
            }
        }
        fn activate(&mut self, acked: u64, _i: Arc<Interrupt>) -> Result<()> {
            if self.fail_activate {
                return Err(VmmError::device("loop", "no"));
            }
            self.activated = Some(acked);
            Ok(())
        }
        fn process_queue(&mut self, index: usize, queues: &mut [Queue], mem: &Mem) -> Result<bool> {
            if self.fail_process {
                return Err(VmmError::device("loop", "bad request"));
            }
            let q = &mut queues[index];
            let mut any = false;
            while let Some(chain) = q.pop_descriptor_chain(mem) {
                q.add_used(mem, chain.head_index(), 0).unwrap();
                any = true;
            }
            Ok(any)
        }
        fn reset(&mut self) {
            self.resets += 1;
            self.activated = None;
        }
    }

    fn transport() -> (MmioTransport<Loop>, Mem, Arc<crate::testing::CountingIrq>) {
        let mem = guest_memory(1 << 20);
        let (c, irq) = counting_irq();
        (
            MmioTransport::new(Loop::new(), mem.clone(), Interrupt::new(irq)).unwrap(),
            mem,
            c,
        )
    }

    #[test]
    fn vm_t8_identity_registers() {
        let (mut t, _, _) = transport();
        assert_eq!(rd(&mut t, regs::MAGIC), MAGIC_VALUE);
        assert_eq!(rd(&mut t, regs::VERSION), 2);
        assert_eq!(rd(&mut t, regs::DEVICE_ID), 0x42);
        assert_eq!(rd(&mut t, regs::VENDOR_ID), VENDOR);
        wr(&mut t, regs::DEVICE_FEATURES_SEL, 0);
        assert_eq!(rd(&mut t, regs::DEVICE_FEATURES), 1 << 3);
        wr(&mut t, regs::DEVICE_FEATURES_SEL, 1);
        assert_eq!(rd(&mut t, regs::DEVICE_FEATURES), 1);
        wr(&mut t, regs::DEVICE_FEATURES_SEL, 2);
        assert_eq!(rd(&mut t, regs::DEVICE_FEATURES), 0);
        wr(&mut t, regs::QUEUE_SEL, 1);
        assert_eq!(rd(&mut t, regs::QUEUE_NUM_MAX), 8);
        wr(&mut t, regs::QUEUE_SEL, 5);
        assert_eq!(rd(&mut t, regs::QUEUE_NUM_MAX), 0);
        assert_eq!(rd(&mut t, regs::QUEUE_READY), 0);
        assert_eq!(rd(&mut t, regs::SHM_LEN_LOW), u32::MAX);
        assert_eq!(rd(&mut t, regs::SHM_LEN_HIGH), u32::MAX);
        wr(&mut t, regs::SHM_SEL, 0);
        assert_eq!(rd(&mut t, 0x0f0), 0);
        // Registers are 32-bit: a narrower access reads zero and a narrower write is dropped.
        let mut b = [0xaa; 2];
        t.read(regs::MAGIC, &mut b);
        assert_eq!(b, [0, 0]);
        t.write(regs::STATUS, &[1]);
        assert_eq!(t.status(), 0);
        // Config space is byte-addressable.
        let mut c = [0u8; 2];
        t.read(regs::CONFIG + 2, &mut c);
        assert_eq!(c, [3, 4]);
        t.write(regs::CONFIG + 1, &[9]);
        t.read(regs::CONFIG + 1, &mut c[..1]);
        assert_eq!(c[0], 9);
        assert_eq!(t.name(), "loop");
    }

    #[test]
    fn vm_t8_negotiation_and_activation() {
        let (mut t, mem, irq) = transport();
        let mut q0 = DriverQueue::new(0x10000, 16);
        let q1 = DriverQueue::new(0x20000, 8);
        let acked = bring_up(&mut t, &[&q0, &q1]);
        assert_eq!(acked, F_VERSION_1 | 1 << 3);
        assert!(t.is_active());
        assert_eq!(t.device().activated, Some(acked));
        assert_eq!(
            rd(&mut t, regs::STATUS) & status::DRIVER_OK,
            status::DRIVER_OK
        );

        // A notify runs the queue and raises a used-ring interrupt.
        q0.add(&mem, &[(0x30000, 16, false)]);
        wr(&mut t, regs::QUEUE_NOTIFY, 0);
        assert_eq!(q0.used(&mem), vec![(0, 0)]);
        assert_eq!(rd(&mut t, regs::INTERRUPT_STATUS), int::USED_RING);
        assert_eq!(irq.get(), 1);
        wr(&mut t, regs::INTERRUPT_ACK, int::USED_RING);
        assert_eq!(rd(&mut t, regs::INTERRUPT_STATUS), 0);
        // A notify with nothing new, or for a queue that does not exist, does nothing.
        wr(&mut t, regs::QUEUE_NOTIFY, 0);
        wr(&mut t, regs::QUEUE_NOTIFY, 7);
        assert_eq!(irq.get(), 1);

        // Queue registers are frozen after DRIVER_OK.
        wr(&mut t, regs::QUEUE_SEL, 0);
        wr(&mut t, regs::QUEUE_NUM, 4);
        assert_eq!(t.queues[0].size(), 16);
        wr(&mut t, regs::QUEUE_READY, 0);
        assert!(t.queues[0].ready());

        // Reset clears everything.
        wr(&mut t, regs::STATUS, 0);
        assert!(!t.is_active());
        assert_eq!(t.status(), 0);
        assert!(!t.queues[0].ready());
        // bring_up reset the device once before negotiating.
        assert_eq!(t.device().resets, 2);
        assert_eq!(rd(&mut t, regs::INTERRUPT_STATUS), 0);
    }

    #[test]
    fn vm_t8_features_ok_refused_for_unoffered_or_legacy() {
        let (mut t, _, _) = transport();
        wr(&mut t, regs::STATUS, status::ACKNOWLEDGE | status::DRIVER);
        // A feature the device never offered.
        wr(&mut t, regs::DRIVER_FEATURES_SEL, 0);
        wr(&mut t, regs::DRIVER_FEATURES, 1 << 5);
        wr(&mut t, regs::DRIVER_FEATURES_SEL, 1);
        wr(&mut t, regs::DRIVER_FEATURES, 1);
        let s = status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK;
        wr(&mut t, regs::STATUS, s);
        assert_eq!(rd(&mut t, regs::STATUS) & status::FEATURES_OK, 0);
        // Without VERSION_1.
        wr(&mut t, regs::DRIVER_FEATURES_SEL, 0);
        wr(&mut t, regs::DRIVER_FEATURES, 1 << 3);
        wr(&mut t, regs::DRIVER_FEATURES_SEL, 1);
        wr(&mut t, regs::DRIVER_FEATURES, 0);
        wr(&mut t, regs::STATUS, s);
        assert_eq!(rd(&mut t, regs::STATUS) & status::FEATURES_OK, 0);
        // A third feature word is ignored; the right set is accepted.
        wr(&mut t, regs::DRIVER_FEATURES_SEL, 2);
        wr(&mut t, regs::DRIVER_FEATURES, 0xffff);
        wr(&mut t, regs::DRIVER_FEATURES_SEL, 1);
        wr(&mut t, regs::DRIVER_FEATURES, 1);
        wr(&mut t, regs::STATUS, s);
        assert_eq!(
            rd(&mut t, regs::STATUS) & status::FEATURES_OK,
            status::FEATURES_OK
        );
        // Features are frozen after FEATURES_OK.
        wr(&mut t, regs::DRIVER_FEATURES, 0);
        assert_eq!(t.driver_features, F_VERSION_1 | 1 << 3);
        // Queue writes before FEATURES_OK are ignored (tested on a fresh device).
        let (mut t2, _, _) = transport();
        wr(&mut t2, regs::QUEUE_NUM, 4);
        wr(&mut t2, regs::QUEUE_READY, 1);
        assert_eq!(t2.queues[0].size(), 16);
        assert!(!t2.queues[0].ready());
    }

    #[test]
    fn vm_t8_bad_queue_or_failing_device_needs_reset() {
        // A queue whose rings lie outside guest memory fails activation.
        let (mut t, _, irq) = transport();
        let bad = DriverQueue::new(0x7fff_0000, 16);
        bring_up(&mut t, &[&bad]);
        assert!(!t.is_active());
        assert_ne!(t.status() & status::DEVICE_NEEDS_RESET, 0);
        assert!(t.last_error().unwrap().contains("queue 0"));
        assert_eq!(rd(&mut t, regs::INTERRUPT_STATUS), int::CONFIG_CHANGE);
        assert_eq!(rd(&mut t, regs::CONFIG_GENERATION), 1);
        assert_eq!(irq.get(), 1);

        // A device that refuses activation.
        let (mut t, _, _) = transport();
        t.device_mut().fail_activate = true;
        bring_up(&mut t, &[&DriverQueue::new(0x10000, 16)]);
        assert!(!t.is_active());
        assert!(t.last_error().unwrap().contains("no"));

        // A device whose request processing fails is marked broken, the monitor survives.
        let (mut t, mem, _) = transport();
        let mut q = DriverQueue::new(0x10000, 16);
        bring_up(&mut t, &[&q]);
        t.device_mut().fail_process = true;
        q.add(&mem, &[(0x30000, 16, false)]);
        wr(&mut t, regs::QUEUE_NOTIFY, 0);
        assert!(!t.is_active());
        assert!(t.last_error().unwrap().contains("bad request"));
        // A broken device ignores further notifies until reset.
        wr(&mut t, regs::QUEUE_NOTIFY, 0);
        wr(&mut t, regs::STATUS, 0);
        assert!(t.last_error().is_none());
    }

    #[test]
    fn vm_t8_with_active_and_config_change() {
        let (mut t, _, irq) = transport();
        assert!(t.with_active(|_, _, _| Ok(1)).is_none());
        bring_up(&mut t, &[&DriverQueue::new(0x10000, 16)]);
        assert_eq!(
            t.with_active(|d, q, _| Ok((d.resets, q.len()))),
            Some((1, 2))
        );
        assert!(
            t.with_active(|_, _, _| -> Result<()> { Err(VmmError::device("loop", "backend")) })
                .is_none()
        );
        assert!(!t.is_active());
        t.config_changed().unwrap();
        assert_eq!(rd(&mut t, regs::CONFIG_GENERATION), 2);
        assert!(irq.get() >= 2);
        assert!(Arc::ptr_eq(&t.interrupt(), &t.interrupt));
    }
}
