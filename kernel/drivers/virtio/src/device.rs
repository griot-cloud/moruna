//! A virtio device opened by its driver process (SDD 25, f.7), from the handles its
//! supervisor gave it: the function's windows, one MSI-X interrupt for every queue,
//! and DMA memory (`moruna_pci::dma::DmaBuffer`) for the rings.

use alloc::{format, string::String};

use moruna_mmio::Sub;
use moruna_pci::{command, dma::DmaBuffer, function::Function};
use moruna_svc::{Handle, Interrupt, Mapping, dma_release, map};

use crate::{
    MODERN_BASE, NO_VECTOR, VENDOR,
    queue::{self, Queue},
    transport::{Layout, Transport},
};

/// The MSI-X entry every queue of a Moruna driver interrupts through.
pub const VECTOR: u16 = 0;

/// A queue of the device, in its own pinned memory.
pub struct VirtQueue {
    pub queue: Queue<Mapping<'static>>,
    pub index: u16,
    notify_off: u16,
    _memory: DmaBuffer,
}

/// A virtio device, ready for its queues.
pub struct Device {
    pub function: Function,
    pub transport: Transport<Mapping<'static>>,
    /// The features negotiated.
    pub features: u64,
    /// The DMA resource: rings and buffers are pinned through it.
    pub dma: Handle,
}

impl Device {
    /// Opens the virtio device of type `kind` the supervisor gave this process:
    /// claims its windows, routes its MSI-X entry 0 to an interrupt, resets it (so it
    /// uses no memory a previous driver gave it, which the quarantine then releases),
    /// lets it master the bus, and negotiates `wanted` and `required` features.
    /// Returns the device and the interrupt every queue raises.
    pub fn open(kind: u16, wanted: u64, required: u64) -> Result<(Device, Interrupt), String> {
        let mut function = Function::open()?;
        let (vendor, device) = (
            function.config.vendor().map_err(|e| e.name())?,
            function.config.device().map_err(|e| e.name())?,
        );
        if vendor != VENDOR || device != MODERN_BASE + kind {
            return Err(format!(
                "the function is {vendor:04x}:{device:04x}, not a modern virtio device of type {kind}"
            ));
        }
        let layout = Layout::read(&function.config).map_err(|e| format!("{e}"))?;
        function
            .config
            .enable(command::MEMORY | command::INTX_DISABLE)
            .map_err(|e| format!("command register: {}", e.name()))?;
        let msix = function
            .config
            .msix()
            .map_err(|e| format!("MSI-X capability: {}", e.name()))?
            .ok_or("the device has no MSI-X, and Moruna has no line-based interrupts for it")?;
        let (interrupt, message) = function.interrupt(VECTOR as u64)?;
        {
            let table = function
                .bar(msix.table_bar)
                .ok_or_else(|| format!("no window for the MSI-X table's bar{}", msix.table_bar))?;
            let len = table.map.len() as usize;
            let window = Sub::new(&table.map, table.delta as usize, len - table.delta as usize)
                .map_err(|e| format!("MSI-X table window: {}", e.name()))?;
            msix.program(&window, VECTOR, message.address, message.data)
                .map_err(|e| format!("MSI-X table: {}", e.name()))?;
            msix.enable(&function.config)
                .map_err(|e| format!("MSI-X enable: {}", e.name()))?;
        }
        let dma = function.take_dma()?;
        let transport = Transport::new(function.take_bars(), layout).map_err(|e| format!("{e}"))?;
        transport.reset().map_err(|e| format!("reset: {e}"))?;
        dma_release(&dma).map_err(|e| format!("dma_release: {}", e.name()))?;
        function.enable_bus_master()?;
        let features = transport
            .negotiate(wanted, required)
            .map_err(|e| format!("features: {e}"))?;
        transport
            .set_config_vector(NO_VECTOR)
            .map_err(|e| format!("config vector: {e}"))?;
        Ok((
            Device {
                function,
                transport,
                features,
                dma,
            },
            interrupt,
        ))
    }

    /// Queue `index` with at most `max` entries, interrupting through the device's
    /// interrupt.
    pub fn queue(&self, index: u16, max: u16) -> Result<VirtQueue, String> {
        let offered = self
            .transport
            .queue_max(index)
            .map_err(|e| format!("queue {index}: {e}"))?;
        let mut size = offered.min(max).min(queue::MAX_SIZE);
        while !size.is_power_of_two() {
            size &= size - 1;
        }
        let memory = DmaBuffer::new(&self.dma, queue::BYTES as u64)?;
        let rings = [
            memory.addr(queue::DESC),
            memory.addr(queue::DRIVER),
            memory.addr(queue::DEVICE),
        ];
        let ring_map = memory
            .memory
            .map_detached(0, queue::BYTES as u64, map::WRITE)
            .map_err(|e| format!("queue {index}: mapping: {}", e.name()))?;
        let queue = Queue::new(ring_map, size).map_err(|e| format!("queue {index}: {e:?}"))?;
        let notify_off = self
            .transport
            .setup_queue(index, size, rings, VECTOR)
            .map_err(|e| format!("queue {index}: {e}"))?;
        Ok(VirtQueue {
            queue,
            index,
            notify_off,
            _memory: memory,
        })
    }

    /// Tells the device the driver is ready.
    pub fn ready(&self) -> Result<(), String> {
        self.transport.driver_ok().map_err(|e| format!("{e}"))
    }

    /// Tells the device `q` has new buffers.
    pub fn notify(&self, q: &VirtQueue) -> Result<(), String> {
        self.transport
            .notify(q.index, q.notify_off)
            .map_err(|e| format!("notify: {e}"))
    }
}
