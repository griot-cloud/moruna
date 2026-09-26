//! `virtio-mem` (virtio 1.2, 5.15): memory the host can add to and take from the running
//! guest, within the `--memory-max` given at boot (MH 4.8.2, H12).
//!
//! The device owns one region of guest-physical address space, above boot memory, whose host
//! mapping is reserved at boot and never touched until the guest plugs a block (anonymous,
//! `MAP_NORESERVE`), so an unplugged block costs the host nothing. A resize sets
//! `requested_size` and raises a configuration interrupt; the guest's driver then plugs or
//! unplugs blocks until `plugged_size` matches, and the guest kernel onlines what it plugged
//! (`memhp_default_state=online_movable`). Unplugged blocks are discarded on the host
//! (`madvise(MADV_DONTNEED)`), which returns their memory to the host.
//!
//! The device refuses, with the specification's status codes, every request outside the
//! region, not block-aligned, of zero blocks, plugging past `requested_size`, or plugging a
//! plugged block or unplugging an unplugged one.

use std::io::{Read, Write};
use std::sync::Arc;

use virtio_queue::{Queue, QueueT};

use super::Mem;
use super::virtio::{ID_MEM, Interrupt, VirtioDevice, read_config_bytes};
use crate::error::{Result, VmmError};

/// Block size: 2 MiB, the x86_64 and 4 KiB-page aarch64 pageblock, so the guest can plug in
/// sub-blocks of a Linux memory block.
pub const BLOCK_BYTES: u64 = 2 << 20;
/// Queue size of the one request queue: the driver sends one request at a time.
pub const QUEUE_SIZE: u16 = 128;
/// Feature: the driver must not touch unplugged memory.
pub const F_UNPLUGGED_INACCESSIBLE: u64 = 1 << 1;

/// Request types.
pub mod req {
    /// Plug blocks.
    pub const PLUG: u16 = 0;
    /// Unplug blocks.
    pub const UNPLUG: u16 = 1;
    /// Unplug everything.
    pub const UNPLUG_ALL: u16 = 2;
    /// Report the state of blocks.
    pub const STATE: u16 = 3;
}

/// Response types.
pub mod resp {
    /// Done.
    pub const ACK: u16 = 0;
    /// Refused for now (plugging past `requested_size`).
    pub const NACK: u16 = 1;
    /// Busy (not used by this device).
    pub const BUSY: u16 = 2;
    /// A malformed request.
    pub const ERROR: u16 = 3;
}

/// Block states in a STATE response.
pub mod state {
    /// Every block in the range is plugged.
    pub const PLUGGED: u16 = 0;
    /// Every block in the range is unplugged.
    pub const UNPLUGGED: u16 = 1;
    /// Some of each.
    pub const MIXED: u16 = 2;
}

/// Size of a request.
pub const REQ_BYTES: usize = 24;
/// Size of a response.
pub const RESP_BYTES: usize = 16;

/// Returns unplugged memory to the host.
pub trait MemBacking: Send {
    /// Drop the host pages behind guest-physical `[gpa, gpa + len)`.
    fn discard(&self, mem: &Mem, gpa: u64, len: u64) -> Result<()>;
}

/// The real backing: `madvise(MADV_DONTNEED)` on the host mapping.
pub struct MadviseBacking;

impl MemBacking for MadviseBacking {
    fn discard(&self, mem: &Mem, gpa: u64, len: u64) -> Result<()> {
        use vm_memory::{GuestAddress, GuestMemoryBackend};
        let host = mem
            .get_host_address(GuestAddress(gpa))
            .map_err(|e| VmmError::device("virtio-mem", format!("discard {gpa:#x}: {e}")))?;
        // The whole range must lie in one mapping, which it does because the hot-plug region
        // is one region; checked rather than assumed.
        let last = gpa
            .checked_add(len.saturating_sub(1))
            .ok_or_else(|| VmmError::device("virtio-mem", "discard range overflows"))?;
        let host_last = mem
            .get_host_address(GuestAddress(last))
            .map_err(|e| VmmError::device("virtio-mem", format!("discard {last:#x}: {e}")))?;
        if (host_last as usize).wrapping_sub(host as usize) != (len - 1) as usize {
            return Err(VmmError::device(
                "virtio-mem",
                "discard range spans two mappings",
            ));
        }
        // SAFETY: `host .. host + len` is one contiguous range of the guest memory mapping,
        // checked just above; MADV_DONTNEED on an anonymous private mapping only drops its
        // pages (later reads see zeros) and cannot invalidate any Rust reference, because the
        // monitor holds no references into guest memory, only volatile accessors.
        let rc = unsafe { libc::madvise(host.cast(), len as usize, libc::MADV_DONTNEED) };
        if rc != 0 {
            return Err(VmmError::io(
                "madvise",
                format!("{gpa:#x}+{len:#x}"),
                &std::io::Error::last_os_error(),
            ));
        }
        Ok(())
    }
}

/// The device.
pub struct VirtioMem {
    addr: u64,
    region_size: u64,
    plugged: Vec<u64>,
    plugged_size: u64,
    requested_size: u64,
    backing: Box<dyn MemBacking>,
}

impl VirtioMem {
    /// A device owning guest-physical `[addr, addr + region_size)`; both block-aligned.
    pub fn new(addr: u64, region_size: u64, backing: Box<dyn MemBacking>) -> Result<Self> {
        if !addr.is_multiple_of(BLOCK_BYTES)
            || !region_size.is_multiple_of(BLOCK_BYTES)
            || region_size == 0
        {
            return Err(VmmError::device(
                "virtio-mem",
                format!("region {addr:#x}+{region_size:#x} is not a whole number of blocks"),
            ));
        }
        let blocks = region_size / BLOCK_BYTES;
        Ok(VirtioMem {
            addr,
            region_size,
            plugged: vec![0; blocks.div_ceil(64) as usize],
            plugged_size: 0,
            requested_size: 0,
            backing,
        })
    }

    /// Bytes the guest has plugged.
    pub fn plugged_size(&self) -> u64 {
        self.plugged_size
    }

    /// Bytes the host has asked for.
    pub fn requested_size(&self) -> u64 {
        self.requested_size
    }

    /// The region's size.
    pub fn region_size(&self) -> u64 {
        self.region_size
    }

    /// Ask the guest to hold `bytes` of hot-plugged memory. Refused beyond the region and
    /// when not a whole number of blocks, so what the host asked is exactly what the guest
    /// sees.
    pub fn set_requested(&mut self, bytes: u64) -> Result<()> {
        if bytes > self.region_size {
            return Err(VmmError::Control(format!(
                "{bytes} bytes of hot-plug memory exceeds the {} reserved at boot",
                self.region_size
            )));
        }
        if !bytes.is_multiple_of(BLOCK_BYTES) {
            return Err(VmmError::Control(format!(
                "{bytes} is not a multiple of the {BLOCK_BYTES}-byte block"
            )));
        }
        self.requested_size = bytes;
        Ok(())
    }

    fn config_space(&self) -> [u8; 56] {
        let mut c = [0u8; 56];
        c[0..8].copy_from_slice(&BLOCK_BYTES.to_le_bytes());
        // node_id 0 and padding.
        c[16..24].copy_from_slice(&self.addr.to_le_bytes());
        c[24..32].copy_from_slice(&self.region_size.to_le_bytes());
        c[32..40].copy_from_slice(&self.region_size.to_le_bytes());
        c[40..48].copy_from_slice(&self.plugged_size.to_le_bytes());
        c[48..56].copy_from_slice(&self.requested_size.to_le_bytes());
        c
    }

    fn is_plugged(&self, block: u64) -> bool {
        self.plugged[(block / 64) as usize] & (1 << (block % 64)) != 0
    }

    fn set_plugged(&mut self, block: u64, on: bool) {
        let w = &mut self.plugged[(block / 64) as usize];
        if on {
            *w |= 1 << (block % 64);
        } else {
            *w &= !(1 << (block % 64));
        }
    }

    /// The first block and block count of a request, if it lies in the region.
    fn blocks(&self, addr: u64, nb: u16) -> Option<(u64, u64)> {
        let nb = nb as u64;
        let off = addr.checked_sub(self.addr)?;
        let len = nb.checked_mul(BLOCK_BYTES)?;
        (nb > 0 && off % BLOCK_BYTES == 0 && off.checked_add(len)? <= self.region_size)
            .then_some((off / BLOCK_BYTES, nb))
    }

    /// Handle one request; returns the response type and, for STATE, the state.
    fn handle(&mut self, mem: &Mem, kind: u16, addr: u64, nb: u16) -> Result<(u16, u16)> {
        match kind {
            req::UNPLUG_ALL => {
                self.unplug_all(mem)?;
                Ok((resp::ACK, 0))
            }
            req::PLUG | req::UNPLUG | req::STATE => {
                let Some((first, n)) = self.blocks(addr, nb) else {
                    return Ok((resp::ERROR, 0));
                };
                let plugged = (first..first + n).filter(|b| self.is_plugged(*b)).count() as u64;
                match kind {
                    req::STATE => {
                        let s = if plugged == n {
                            state::PLUGGED
                        } else if plugged == 0 {
                            state::UNPLUGGED
                        } else {
                            state::MIXED
                        };
                        Ok((resp::ACK, s))
                    }
                    req::PLUG => {
                        if plugged != 0 {
                            return Ok((resp::ERROR, 0));
                        }
                        if self.plugged_size + n * BLOCK_BYTES > self.requested_size {
                            return Ok((resp::NACK, 0));
                        }
                        (first..first + n).for_each(|b| self.set_plugged(b, true));
                        self.plugged_size += n * BLOCK_BYTES;
                        Ok((resp::ACK, 0))
                    }
                    _ => {
                        if plugged != n {
                            return Ok((resp::ERROR, 0));
                        }
                        self.backing.discard(
                            mem,
                            self.addr + first * BLOCK_BYTES,
                            n * BLOCK_BYTES,
                        )?;
                        (first..first + n).for_each(|b| self.set_plugged(b, false));
                        self.plugged_size -= n * BLOCK_BYTES;
                        Ok((resp::ACK, 0))
                    }
                }
            }
            _ => Ok((resp::ERROR, 0)),
        }
    }

    fn unplug_all(&mut self, mem: &Mem) -> Result<()> {
        let blocks = self.region_size / BLOCK_BYTES;
        let mut b = 0;
        while b < blocks {
            if !self.is_plugged(b) {
                b += 1;
                continue;
            }
            let start = b;
            while b < blocks && self.is_plugged(b) {
                self.set_plugged(b, false);
                b += 1;
            }
            self.backing.discard(
                mem,
                self.addr + start * BLOCK_BYTES,
                (b - start) * BLOCK_BYTES,
            )?;
        }
        self.plugged_size = 0;
        Ok(())
    }
}

impl VirtioDevice for VirtioMem {
    fn name(&self) -> &'static str {
        "virtio-mem"
    }

    fn device_type(&self) -> u32 {
        ID_MEM
    }

    fn features(&self) -> u64 {
        F_UNPLUGGED_INACCESSIBLE
    }

    fn queue_max_sizes(&self) -> Vec<u16> {
        vec![QUEUE_SIZE]
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        read_config_bytes(&self.config_space(), offset, data);
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}

    fn activate(&mut self, _acked: u64, _interrupt: Arc<Interrupt>) -> Result<()> {
        Ok(())
    }

    fn process_queue(&mut self, index: usize, queues: &mut [Queue], mem: &Mem) -> Result<bool> {
        let q = queues
            .get_mut(index)
            .ok_or_else(|| VmmError::device("virtio-mem", format!("no queue {index}")))?;
        let mut any = false;
        while let Some(chain) = q.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            let mut used = 0;
            if let (Ok(mut r), Ok(mut w)) = (chain.clone().reader(mem), chain.writer(mem)) {
                let mut raw = [0u8; REQ_BYTES];
                let (kind, st) = if r.read_exact(&mut raw).is_ok() {
                    let kind = u16::from_le_bytes([raw[0], raw[1]]);
                    let mut a = [0u8; 8];
                    a.copy_from_slice(&raw[8..16]);
                    let nb = u16::from_le_bytes([raw[16], raw[17]]);
                    self.handle(mem, kind, u64::from_le_bytes(a), nb)?
                } else {
                    (resp::ERROR, 0)
                };
                let mut out = [0u8; RESP_BYTES];
                out[0..2].copy_from_slice(&kind.to_le_bytes());
                out[8..10].copy_from_slice(&st.to_le_bytes());
                if w.write_all(&out).is_ok() {
                    used = RESP_BYTES as u32;
                }
            }
            q.add_used(mem, head, used)
                .map_err(|e| VmmError::device("virtio-mem", format!("used ring: {e}")))?;
            any = true;
        }
        Ok(any)
    }

    fn reset(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::Device;
    use crate::devices::mmio::{MmioTransport, regs};
    use crate::testing::{DriverQueue, bring_up, counting_irq, guest_memory, rd, wr};
    use std::sync::Mutex;
    use vm_memory::{Bytes, GuestAddress};

    const REGION: u64 = 0x40_0000; // 4 MiB into the test memory
    const REQ: u64 = 0x1000;
    const RESP: u64 = 0x2000;

    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<(u64, u64)>>>);

    impl MemBacking for Recorder {
        fn discard(&self, _mem: &Mem, gpa: u64, len: u64) -> Result<()> {
            self.0.lock().unwrap().push((gpa, len));
            Ok(())
        }
    }

    struct Rig {
        t: MmioTransport<VirtioMem>,
        q: DriverQueue,
        mem: Mem,
        rec: Recorder,
    }

    fn rig() -> Rig {
        let mem = guest_memory(16 << 20);
        let rec = Recorder::default();
        let dev = VirtioMem::new(REGION, 8 * BLOCK_BYTES, Box::new(rec.clone())).unwrap();
        let (_, irq) = counting_irq();
        let mut t = MmioTransport::new(dev, mem.clone(), Interrupt::new(irq)).unwrap();
        let q = DriverQueue::new(0x10000, 16);
        bring_up(&mut t, &[&q]);
        Rig { t, q, mem, rec }
    }

    fn request(r: &mut Rig, kind: u16, addr: u64, nb: u16) -> (u16, u16) {
        let mut raw = [0u8; REQ_BYTES];
        raw[0..2].copy_from_slice(&kind.to_le_bytes());
        raw[8..16].copy_from_slice(&addr.to_le_bytes());
        raw[16..18].copy_from_slice(&nb.to_le_bytes());
        r.mem.write_slice(&raw, GuestAddress(REQ)).unwrap();
        let head = r.q.add(
            &r.mem,
            &[
                (REQ, REQ_BYTES as u32, false),
                (RESP, RESP_BYTES as u32, true),
            ],
        );
        wr(&mut r.t, regs::QUEUE_NOTIFY, 0);
        assert_eq!(r.q.used(&r.mem), vec![(head as u32, RESP_BYTES as u32)]);
        let t: u16 = r.mem.read_obj(GuestAddress(RESP)).unwrap();
        let s: u16 = r.mem.read_obj(GuestAddress(RESP + 8)).unwrap();
        (t, s)
    }

    fn cfg(t: &mut MmioTransport<VirtioMem>, off: u64) -> u64 {
        let mut b = [0u8; 8];
        t.read(regs::CONFIG + off, &mut b);
        u64::from_le_bytes(b)
    }

    #[test]
    fn vm_t16_config_space() {
        let mut r = rig();
        assert_eq!(rd(&mut r.t, regs::DEVICE_ID), ID_MEM);
        assert_eq!(cfg(&mut r.t, 0), BLOCK_BYTES);
        assert_eq!(cfg(&mut r.t, 16), REGION);
        assert_eq!(cfg(&mut r.t, 24), 8 * BLOCK_BYTES);
        assert_eq!(cfg(&mut r.t, 32), 8 * BLOCK_BYTES);
        assert_eq!((cfg(&mut r.t, 40), cfg(&mut r.t, 48)), (0, 0));
        r.t.device_mut().set_requested(4 * BLOCK_BYTES).unwrap();
        r.t.config_changed().unwrap();
        assert_eq!(cfg(&mut r.t, 48), 4 * BLOCK_BYTES);
        assert_eq!(r.t.device().region_size(), 8 * BLOCK_BYTES);
        r.t.write(regs::CONFIG, &[1]);
        assert_eq!(cfg(&mut r.t, 0), BLOCK_BYTES);
    }

    #[test]
    fn vm_t16_plug_unplug_state() {
        let mut r = rig();
        // Nothing requested: a plug is NACKed.
        assert_eq!(request(&mut r, req::PLUG, REGION, 1).0, resp::NACK);
        r.t.device_mut().set_requested(3 * BLOCK_BYTES).unwrap();
        assert_eq!(request(&mut r, req::PLUG, REGION, 2).0, resp::ACK);
        assert_eq!(r.t.device().plugged_size(), 2 * BLOCK_BYTES);
        // Plugging past the request is NACKed; plugging a plugged block is an error.
        assert_eq!(
            request(&mut r, req::PLUG, REGION + 2 * BLOCK_BYTES, 2).0,
            resp::NACK
        );
        assert_eq!(
            request(&mut r, req::PLUG, REGION + BLOCK_BYTES, 1).0,
            resp::ERROR
        );
        assert_eq!(
            request(&mut r, req::PLUG, REGION + 5 * BLOCK_BYTES, 1).0,
            resp::ACK
        );
        assert_eq!(
            request(&mut r, req::STATE, REGION, 2),
            (resp::ACK, state::PLUGGED)
        );
        assert_eq!(
            request(&mut r, req::STATE, REGION + 2 * BLOCK_BYTES, 3),
            (resp::ACK, state::UNPLUGGED)
        );
        assert_eq!(
            request(&mut r, req::STATE, REGION + BLOCK_BYTES, 2),
            (resp::ACK, state::MIXED)
        );
        // Unplug returns memory to the host.
        assert_eq!(request(&mut r, req::UNPLUG, REGION, 1).0, resp::ACK);
        assert_eq!(r.rec.0.lock().unwrap().clone(), vec![(REGION, BLOCK_BYTES)]);
        assert_eq!(r.t.device().plugged_size(), 2 * BLOCK_BYTES);
        assert_eq!(request(&mut r, req::UNPLUG, REGION, 2).0, resp::ERROR);
        // Unplug-all discards every plugged run and zeroes the count.
        assert_eq!(request(&mut r, req::UNPLUG_ALL, 0, 0).0, resp::ACK);
        assert_eq!(r.t.device().plugged_size(), 0);
        assert_eq!(
            r.rec.0.lock().unwrap().clone(),
            vec![
                (REGION, BLOCK_BYTES),
                (REGION + BLOCK_BYTES, BLOCK_BYTES),
                (REGION + 5 * BLOCK_BYTES, BLOCK_BYTES)
            ]
        );
    }

    #[test]
    fn vm_t16_malformed_requests() {
        let mut r = rig();
        r.t.device_mut().set_requested(8 * BLOCK_BYTES).unwrap();
        for (addr, nb) in [
            (REGION - BLOCK_BYTES, 1),     // below the region
            (REGION + 1, 1),               // not aligned
            (REGION, 0),                   // zero blocks
            (REGION + 7 * BLOCK_BYTES, 2), // past the end
            (u64::MAX - 1, 1),             // overflow
        ] {
            assert_eq!(
                request(&mut r, req::PLUG, addr, nb).0,
                resp::ERROR,
                "{addr:#x} {nb}"
            );
        }
        assert_eq!(request(&mut r, 77, REGION, 1).0, resp::ERROR);
        // A short request.
        r.q.add(&r.mem, &[(REQ, 4, false), (RESP, RESP_BYTES as u32, true)]);
        wr(&mut r.t, regs::QUEUE_NOTIFY, 0);
        r.q.used(&r.mem);
        assert_eq!(
            r.mem.read_obj::<u16>(GuestAddress(RESP)).unwrap(),
            resp::ERROR
        );
        // No room for a response: returned with length 0.
        r.q.add(&r.mem, &[(REQ, REQ_BYTES as u32, false)]);
        wr(&mut r.t, regs::QUEUE_NOTIFY, 0);
        assert_eq!(r.q.used(&r.mem)[0].1, 0);
        assert!(r.t.is_active());
        let mut qs: Vec<Queue> = vec![];
        assert!(r.t.device_mut().process_queue(3, &mut qs, &r.mem).is_err());
    }

    #[test]
    fn vm_t16_resize_bounds_and_region_shape() {
        let mut d = VirtioMem::new(0, 4 * BLOCK_BYTES, Box::new(Recorder::default())).unwrap();
        assert!(d.set_requested(5 * BLOCK_BYTES).is_err());
        assert!(d.set_requested(BLOCK_BYTES + 4096).is_err());
        d.set_requested(4 * BLOCK_BYTES).unwrap();
        assert_eq!(d.requested_size(), 4 * BLOCK_BYTES);
        assert!(VirtioMem::new(4096, BLOCK_BYTES, Box::new(Recorder::default())).is_err());
        assert!(VirtioMem::new(0, 0, Box::new(Recorder::default())).is_err());
        assert!(VirtioMem::new(0, 4096, Box::new(Recorder::default())).is_err());
    }

    #[test]
    fn vm_t16_madvise_discard() {
        let mem = guest_memory(8 << 20);
        mem.write_slice(&[0xabu8; 4096], GuestAddress(2 << 20))
            .unwrap();
        MadviseBacking.discard(&mem, 2 << 20, BLOCK_BYTES).unwrap();
        #[cfg(target_os = "linux")]
        {
            let b: u8 = mem.read_obj(GuestAddress(2 << 20)).unwrap();
            assert_eq!(b, 0, "discarded pages read as zero");
        }
        assert!(MadviseBacking.discard(&mem, 64 << 20, BLOCK_BYTES).is_err());
        assert!(MadviseBacking.discard(&mem, 7 << 20, 4 << 20).is_err());
        assert!(MadviseBacking.discard(&mem, u64::MAX - 10, 100).is_err());
    }
}
