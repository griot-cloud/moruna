//! A data disk the supervisor granted, brought up by the engine as Moruna's virtio-blk driver
//! process brings its device up (`kernel/drivers/virtio-blk`), with `moruna-pci`'s
//! configuration space and MSI-X code and `moruna-virtio`'s transport and split queue.
//!
//! The supervisor grants a disk as one channel under the disk's start-page name, whose one
//! message carries the function's handles and, as its bytes, their names in order: `config`
//! (its configuration window), `msi` (its MSI slots), `dma` (its DMA resource) and one `barN`
//! for each memory BAR. The disk's file length is its virtio-blk serial, in decimal, when the
//! launcher set one; otherwise its capacity.

use std::sync::Arc;

use moruna_abi::Error;
use moruna_mmio::{Region, Sub};
use moruna_pci::{Bar, Config, command};
use moruna_sys::{
    Channel, DeviceMemory, Handle, Interrupt, Mapping, Memory, PAGE_SIZE, Pin, ResourceKind,
    Signals, dma_release, map,
};
use moruna_virtio::{Layout, NO_VECTOR, Queue, Transport, VENDOR, kind, queue};

use super::disk::{Disk, Futex, Port};
use super::engine::{Engine, ID_BYTES, Kind, Op, SLOT, Slots};
use super::plan;
use crate::arena::{Objects, Pins};

/// virtio-blk feature bits (virtio 1.2, 5.2.3).
pub const F_SEG_MAX: u64 = 1 << 2;
/// The device is read only.
pub const F_RO: u64 = 1 << 5;
/// The device says its block size.
pub const F_BLK_SIZE: u64 = 1 << 6;
/// The device has a write cache to flush.
pub const F_FLUSH: u64 = 1 << 9;
/// A modern virtio-blk function's device id, and a transitional one's.
pub const DEVICE_IDS: [u16; 2] = [moruna_virtio::MODERN_BASE + kind::BLOCK, 0x1001];
/// The MSI-X entry the queue interrupts through.
pub const VECTOR: u16 = 0;
/// Requests one disk keeps in flight at most: a slot each.
pub const SLOTS: usize = 64;
/// The largest body of one request.
pub const BODY_CAP: u64 = 1 << 20;

/// The handles a granted disk's message carries, by name.
#[derive(Debug, PartialEq, Eq)]
pub struct Grant<T> {
    /// The configuration window.
    pub config: T,
    /// The MSI slots.
    pub msi: T,
    /// The DMA resource.
    pub dma: T,
    /// Each memory BAR's window, by index.
    pub bars: Vec<(usize, T)>,
}

impl<T> Grant<T> {
    /// The grant whose handles are `handles`, named in order by `names`.
    pub fn parse(names: &str, handles: Vec<T>) -> Result<Grant<T>, String> {
        let names: Vec<&str> = names.split_ascii_whitespace().collect();
        if names.len() != handles.len() {
            return Err(format!(
                "the grant names {} handles and carries {}",
                names.len(),
                handles.len()
            ));
        }
        let (mut config, mut msi, mut dma, mut bars) = (None, None, None, Vec::new());
        for (name, handle) in names.into_iter().zip(handles) {
            let slot = match name {
                "config" => &mut config,
                "msi" => &mut msi,
                "dma" => &mut dma,
                bar => {
                    let index = bar
                        .strip_prefix("bar")
                        .and_then(|i| i.parse::<usize>().ok())
                        .filter(|i| *i < 6)
                        .ok_or_else(|| format!("the grant names an unknown handle `{bar}`"))?;
                    if bars.iter().any(|(i, _)| *i == index) {
                        return Err(format!("the grant names bar{index} twice"));
                    }
                    bars.push((index, handle));
                    continue;
                }
            };
            if slot.replace(handle).is_some() {
                return Err(format!("the grant names `{name}` twice"));
            }
        }
        let need = |h: Option<T>, what: &str| h.ok_or_else(|| format!("the grant has no `{what}`"));
        Ok(Grant {
            config: need(config, "config")?,
            msi: need(msi, "msi")?,
            dma: need(dma, "dma")?,
            bars,
        })
    }
}

/// The length a serial names, when it is a decimal number of at most `capacity`.
pub fn serial_length(serial: &[u8], capacity: u64) -> Option<u64> {
    let text = serial.split(|b| *b == 0).next()?;
    if text.is_empty() || !text.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(text)
        .ok()?
        .parse::<u64>()
        .ok()
        .filter(|n| *n <= capacity)
}

/// A mapped window, read and written as `moruna-mmio` reads and writes a mapping on the
/// kernel: each access one volatile load or store of its width.
pub struct Window(pub Mapping<'static>);

impl Region for Window {
    fn len(&self) -> usize {
        self.0.len() as usize
    }
    fn read8(&self, offset: usize) -> Result<u8, Error> {
        self.0.read8(offset)
    }
    fn read16(&self, offset: usize) -> Result<u16, Error> {
        self.0.read16(offset)
    }
    fn read32(&self, offset: usize) -> Result<u32, Error> {
        self.0.read32(offset)
    }
    fn read64(&self, offset: usize) -> Result<u64, Error> {
        self.0.read64(offset)
    }
    fn write8(&self, offset: usize, value: u8) -> Result<(), Error> {
        self.0.write8(offset, value)
    }
    fn write16(&self, offset: usize, value: u16) -> Result<(), Error> {
        self.0.write16(offset, value)
    }
    fn write32(&self, offset: usize, value: u32) -> Result<(), Error> {
        self.0.write32(offset, value)
    }
    fn write64(&self, offset: usize, value: u64) -> Result<(), Error> {
        self.0.write64(offset, value)
    }
}

/// Memory the device reads or writes that is the engine's own (the rings, the slots): a DMA
/// memory object pinned for the device, with each page's device address.
struct Dma {
    memory: Memory,
    _pin: Pin,
    pages: Vec<u64>,
}

impl Dma {
    fn new(dma: &Handle, bytes: u64) -> Result<Dma, String> {
        let memory = Memory::create_dma(bytes).map_err(|e| format!("DMA memory: {}", e.name()))?;
        let mut pages = vec![0u64; bytes.div_ceil(PAGE_SIZE) as usize];
        let (pin, n) = Pin::create(dma, &memory.0, true, &mut pages)
            .map_err(|e| format!("pinning DMA memory: {}", e.name()))?;
        pages.truncate(n);
        Ok(Dma {
            memory,
            _pin: pin,
            pages,
        })
    }

    fn addr(&self, offset: usize) -> u64 {
        self.pages[offset / PAGE_SIZE as usize] + (offset as u64 % PAGE_SIZE)
    }

    fn window(&self) -> Result<Window, String> {
        let len = self.pages.len() as u64 * PAGE_SIZE;
        self.memory
            .map_detached(0, len, map::WRITE)
            .map(Window)
            .map_err(|e| format!("mapping DMA memory: {}", e.name()))
    }
}

/// A claimed window of device memory, mapped.
fn claim(resource: &Handle, what: &str) -> Result<(DeviceMemory, Window, u64), String> {
    let info = resource
        .resource_info()
        .map_err(|e| format!("{what}: resource_info: {}", e.name()))?;
    if info.kind != ResourceKind::DeviceMemory {
        return Err(format!("{what}: a {:?} resource", info.kind));
    }
    let memory = DeviceMemory::create(resource, info.base, info.len).map_err(|e| {
        format!(
            "{what}: claiming {:#x}+{:#x}: {}",
            info.base,
            info.len,
            e.name()
        )
    })?;
    let mapping = memory
        .map_detached(0, info.len, map::WRITE)
        .map_err(|e| format!("{what}: mapping: {}", e.name()))?;
    Ok((memory, Window(mapping), info.base))
}

/// The device side of a disk's I/O thread.
struct Device {
    transport: Transport<Window>,
    notify_off: u16,
    interrupt: Interrupt,
    dma: Arc<Handle>,
    _claims: Vec<DeviceMemory>,
    _rings: Dma,
    _slots: Dma,
}

impl Port for Device {
    fn ring(&self) -> Result<(), String> {
        self.transport
            .notify(0, self.notify_off)
            .map_err(|e| format!("{e}"))
    }

    fn wait_interrupt(&self, ns: u64) {
        let deadline = moruna_sys::now().saturating_add(ns);
        if moruna_sys::wait_one(&self.interrupt.0, Signals::SIGNALED, deadline).is_ok() {
            let _ = self.interrupt.ack();
        }
    }

    fn reset(&self) {
        let _ = self.transport.reset();
        let _ = dma_release(&self.dma);
    }
}

/// Takes the grant under `slot`: the one message on its channel.
pub fn take(slot: &str) -> Result<Grant<Handle>, String> {
    let channel = Channel(
        crate::start::take(slot).ok_or_else(|| format!("the engine was granted no `{slot}`"))?,
    );
    let mut bytes = vec![0u8; 256];
    let mut handles: Vec<Option<Handle>> = (0..16).map(|_| None).collect();
    let (n, h) = channel
        .read(&mut bytes, &mut handles)
        .map_err(|e| format!("`{slot}`: reading its grant: {}", e.name()))?;
    let names = String::from_utf8_lossy(&bytes[..n]).into_owned();
    Grant::parse(&names, handles.into_iter().take(h).flatten().collect())
        .map_err(|why| format!("`{slot}`: {why}"))
}

/// Brings up the disk granted under `slot`, its reads and writes landing by DMA in the
/// regions of `objects`, and starts its I/O thread.
pub fn open(slot: &str, objects: &Arc<Objects>, writable: bool) -> Result<Disk, String> {
    let grant = take(slot)?;
    let at = |what: &str| format!("`{slot}`: {what}");
    let (config_claim, config_window, _) = claim(&grant.config, &at("config"))?;
    let config = Config::new(config_window).map_err(|e| at(&format!("config: {}", e.name())))?;
    let (vendor, device) = (
        config.vendor().map_err(|e| at(e.name()))?,
        config.device().map_err(|e| at(e.name()))?,
    );
    if vendor != VENDOR || !DEVICE_IDS.contains(&device) {
        return Err(at(&format!(
            "the function is {vendor:04x}:{device:04x}, not a virtio-blk device"
        )));
    }
    let layout = Layout::read(&config).map_err(|e| at(&format!("{e}")))?;
    config
        .enable(command::MEMORY | command::INTX_DISABLE)
        .map_err(|e| at(&format!("command register: {}", e.name())))?;
    let mut claims = vec![config_claim];
    let mut bars: [Option<(Window, u64)>; 6] = Default::default();
    for (index, resource) in &grant.bars {
        let (claim, window, base) = claim(resource, &at(&format!("bar{index}")))?;
        let delta = match config.bar_base(*index) {
            Ok(Bar::Memory { base: bar, .. }) => bar
                .checked_sub(base)
                .filter(|d| (*d as usize) < window.len())
                .ok_or_else(|| at(&format!("bar{index} at {bar:#x} is outside its window")))?,
            Ok(_) => return Err(at(&format!("bar{index} is not a memory BAR"))),
            Err(e) => return Err(at(&format!("bar{index}: {}", e.name()))),
        };
        bars[*index] = Some((window, delta));
        claims.push(claim);
    }
    let msix = config
        .msix()
        .map_err(|e| at(&format!("MSI-X capability: {}", e.name())))?
        .ok_or_else(|| at("the device has no MSI-X"))?;
    let msi_info = grant
        .msi
        .resource_info()
        .map_err(|e| at(&format!("msi: {}", e.name())))?;
    let (interrupt, message) = Interrupt::create_msi(&grant.msi, msi_info.base)
        .map_err(|e| at(&format!("interrupt: {}", e.name())))?;
    {
        let (table, delta) = bars[msix.table_bar]
            .as_ref()
            .ok_or_else(|| at(&format!("no window for MSI-X's bar{}", msix.table_bar)))?;
        let window = Sub::new(table, *delta as usize, table.len() - *delta as usize)
            .map_err(|e| at(&format!("MSI-X table window: {}", e.name())))?;
        msix.program(&window, VECTOR, message.address, message.data)
            .map_err(|e| at(&format!("MSI-X table: {}", e.name())))?;
        msix.enable(&config)
            .map_err(|e| at(&format!("MSI-X enable: {}", e.name())))?;
    }
    let dma = Arc::new(grant.dma);
    let transport = Transport::new(bars, layout).map_err(|e| at(&format!("{e}")))?;
    transport.reset().map_err(|e| at(&format!("reset: {e}")))?;
    dma_release(&dma).map_err(|e| at(&format!("dma_release: {}", e.name())))?;
    config
        .enable(command::MEMORY | command::BUS_MASTER | command::INTX_DISABLE)
        .map_err(|e| at(&format!("command register: {}", e.name())))?;
    let features = transport
        .negotiate(F_SEG_MAX | F_BLK_SIZE | F_FLUSH | F_RO, 0)
        .map_err(|e| at(&format!("features: {e}")))?;
    transport
        .set_config_vector(NO_VECTOR)
        .map_err(|e| at(&format!("config vector: {e}")))?;
    let (sectors, seg_max) = transport
        .device_config(|c| Ok((c.read64(0)?, c.read32(12)?)))
        .map_err(|e| at(&format!("device configuration: {e}")))?;
    let seg_max = if features & F_SEG_MAX != 0 && seg_max > 0 {
        seg_max
    } else {
        // Without the feature a device takes the queue's length (virtio 1.2, 5.2.6.1).
        u32::from(queue::MAX_SIZE)
    };
    let offered = transport
        .queue_max(0)
        .map_err(|e| at(&format!("queue 0: {e}")))?;
    let mut size = offered.min(queue::MAX_SIZE);
    while !size.is_power_of_two() {
        size &= size - 1;
    }
    let rings = Dma::new(&dma, queue::BYTES as u64).map_err(|why| at(&why))?;
    let ring_addrs = [
        rings.addr(queue::DESC),
        rings.addr(queue::DRIVER),
        rings.addr(queue::DEVICE),
    ];
    let queue = Queue::new(rings.window().map_err(|why| at(&why))?, size)
        .map_err(|e| at(&format!("queue 0: {e:?}")))?;
    let notify_off = transport
        .setup_queue(0, size, ring_addrs, VECTOR)
        .map_err(|e| at(&format!("queue 0: {e}")))?;
    let slots = Dma::new(&dma, (SLOTS * SLOT) as u64).map_err(|why| at(&why))?;
    let slot_addrs = (0..SLOTS).map(|i| slots.addr(i * SLOT)).collect();
    let slot_window = slots.window().map_err(|why| at(&why))?;
    transport.driver_ok().map_err(|e| at(&format!("{e}")))?;
    let capacity = sectors.saturating_mul(plan::SECTOR);
    let engine = Engine::new(
        queue,
        Slots {
            memory: slot_window,
            addrs: slot_addrs,
        },
        Arc::new(Pins::new(Arc::clone(objects), Arc::clone(&dma))),
        plan::max_body(seg_max, size, PAGE_SIZE, BODY_CAP),
        capacity,
        writable && features & F_RO == 0,
        features & F_FLUSH != 0,
    );
    let port = Device {
        transport,
        notify_off,
        interrupt,
        dma,
        _claims: claims,
        _rings: rings,
        _slots: slots,
    };
    let disk = Disk::start(slot, engine, port, Arc::new(Futex::new()), capacity)
        .map_err(|e| at(&format!("starting its thread: {e}")))?;
    let mut serial = vec![0u8; ID_BYTES];
    let named = disk.run(Op {
        kind: Kind::Id,
        offset: 0,
        addr: serial.as_mut_ptr() as u64,
        len: ID_BYTES as u64,
        done: Box::new(|_| {}),
    });
    if named.is_ok()
        && let Some(length) = serial_length(&serial, capacity)
    {
        disk.set_length(length);
    }
    tracing::info!(
        target: "blk.open",
        slot,
        capacity,
        length = disk.length(),
        queue = size,
        seg_max,
        "a data disk is the engine's"
    );
    Ok(disk)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grant_names_its_handles() {
        let g = Grant::parse("config msi dma bar4 bar1", vec![1, 2, 3, 4, 5]).unwrap();
        assert_eq!(
            g,
            Grant {
                config: 1,
                msi: 2,
                dma: 3,
                bars: vec![(4, 4), (1, 5)]
            }
        );
        for (names, n, why) in [
            ("config msi", 3, "names 2"),
            ("config msi dma config", 4, "twice"),
            ("config msi dma bar6", 4, "unknown"),
            ("config msi dma barx", 4, "unknown"),
            ("config msi dma bar2 bar2", 5, "bar2 twice"),
            ("config msi bar0", 3, "no `dma`"),
            ("msi dma bar0", 3, "no `config`"),
            ("config dma bar0", 3, "no `msi`"),
        ] {
            let e = Grant::parse(names, (0..n).collect::<Vec<u32>>()).unwrap_err();
            assert!(e.contains(why), "{names}: {e}");
        }
    }

    #[test]
    fn a_serial_names_the_files_length() {
        assert_eq!(serial_length(b"123456\0\0\0", 1 << 20), Some(123_456));
        assert_eq!(serial_length(b"99999999", 1000), None, "past the disk");
        assert_eq!(serial_length(b"", 1000), None);
        assert_eq!(serial_length(b"\0\0", 1000), None);
        assert_eq!(serial_length(b"12a", 1000), None);
        assert_eq!(serial_length(b"in.nyc", 1000), None);
        assert_eq!(serial_length(b"0", 1000), Some(0));
    }
}
