//! `virtio-blk` over a file: one device per disk the host names, read-only or read-write per
//! device.
//!
//! Requests are served synchronously in the vCPU thread that notified the queue, with
//! positioned reads and writes (`pread`/`pwrite`) in chunks of [`CHUNK_BYTES`], so a request
//! never allocates more than one chunk however large the guest makes it. A request the device
//! cannot serve is answered with a status (`IOERR`, `UNSUPP`), never with a monitor failure; a
//! chain too malformed to carry a status byte is returned with length 0. A read-only disk
//! offers `VIRTIO_BLK_F_RO`, answers every write with `IOERR`, and is opened read-only by the
//! monitor, so no path in the monitor could write it.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::Arc;

use virtio_bindings::virtio_blk::{
    VIRTIO_BLK_F_FLUSH, VIRTIO_BLK_F_RO, VIRTIO_BLK_F_SEG_MAX, VIRTIO_BLK_F_SIZE_MAX,
    VIRTIO_BLK_ID_BYTES, VIRTIO_BLK_S_IOERR, VIRTIO_BLK_S_OK, VIRTIO_BLK_S_UNSUPP,
    VIRTIO_BLK_T_FLUSH, VIRTIO_BLK_T_GET_ID, VIRTIO_BLK_T_IN, VIRTIO_BLK_T_OUT,
};
use virtio_queue::{DescriptorChain, Queue, QueueT};

use super::Mem;
use super::virtio::{ID_BLOCK, Interrupt, QUEUE_MAX_SIZE, VirtioDevice, read_config_bytes};
use crate::config::DiskConfig;
use crate::error::{Result, VmmError};

/// Sector size, fixed by the virtio specification.
pub const SECTOR_BYTES: u64 = 512;
/// Bytes copied between the file and guest memory per step: bounds the monitor's buffer.
pub const CHUNK_BYTES: usize = 256 << 10;
/// Largest segment the driver may use (`size_max`): 1 MiB, Linux's default request ceiling.
pub const SIZE_MAX: u32 = 1 << 20;
/// Most data segments per request (`seg_max`): the queue size less the header and status.
pub const SEG_MAX: u32 = QUEUE_MAX_SIZE as u32 - 2;

/// One disk.
pub struct Block {
    file: File,
    path: PathBuf,
    read_only: bool,
    capacity_sectors: u64,
    id: String,
    buf: Vec<u8>,
}

impl Block {
    /// Open `disk`, taking an advisory lock (shared for read-only, exclusive for read-write) so
    /// two monitors cannot write one disk; `index` names the device for `GET_ID`.
    pub fn open(disk: &DiskConfig, index: usize) -> Result<Self> {
        let shown = disk.path.display().to_string();
        let mut file = OpenOptions::new()
            .read(true)
            .write(!disk.read_only)
            .open(&disk.path)
            .map_err(|e| VmmError::config("--disk", format!("{shown}: {e}")))?;
        let locked = if disk.read_only {
            file.try_lock_shared()
        } else {
            file.try_lock()
        };
        locked.map_err(|e| {
            VmmError::config(
                "--disk",
                format!("{shown} is in use by another process ({e})"),
            )
        })?;
        let len = file
            .seek(SeekFrom::End(0))
            .map_err(|e| VmmError::config("--disk", format!("{shown}: {e}")))?;
        Ok(Block {
            file,
            path: disk.path.clone(),
            read_only: disk.read_only,
            capacity_sectors: len / SECTOR_BYTES,
            id: format!("moruna-disk-{index}"),
            buf: vec![0; CHUNK_BYTES],
        })
    }

    /// The disk's size in sectors, as the guest sees it.
    pub fn capacity_sectors(&self) -> u64 {
        self.capacity_sectors
    }

    /// True when the guest may not write the disk.
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    fn config_space(&self) -> [u8; 16] {
        let mut c = [0u8; 16];
        c[0..8].copy_from_slice(&self.capacity_sectors.to_le_bytes());
        c[8..12].copy_from_slice(&SIZE_MAX.to_le_bytes());
        c[12..16].copy_from_slice(&SEG_MAX.to_le_bytes());
        c
    }

    /// Serve one request; returns the used length.
    fn serve(&mut self, chain: DescriptorChain<&Mem>, mem: &Mem) -> u32 {
        let Ok(mut reader) = chain.clone().reader(mem) else {
            return 0;
        };
        let Ok(mut writer) = chain.writer(mem) else {
            return 0;
        };
        let writable = writer.available_bytes();
        if writable == 0 {
            // No room for a status byte: nothing can be reported to the driver.
            return 0;
        }
        let Ok(mut status_w) = writer.split_at(writable - 1) else {
            return 0;
        };
        let mut header = [0u8; 16];
        let (status, data_written) = if reader.read_exact(&mut header).is_err() {
            (VIRTIO_BLK_S_IOERR, 0)
        } else {
            let kind = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
            let mut s = [0u8; 8];
            s.copy_from_slice(&header[8..16]);
            let sector = u64::from_le_bytes(s);
            match kind {
                VIRTIO_BLK_T_IN => self.read_in(sector, &mut writer),
                VIRTIO_BLK_T_OUT => (self.write_out(sector, &mut reader), 0),
                VIRTIO_BLK_T_FLUSH => (self.flush(), 0),
                VIRTIO_BLK_T_GET_ID => self.get_id(&mut writer),
                _ => (VIRTIO_BLK_S_UNSUPP, 0),
            }
        };
        if status_w.write_all(&[status as u8]).is_err() {
            return 0;
        }
        (data_written + 1) as u32
    }

    /// The byte offset of `sector` if `[sector, sector + len)` lies on the disk.
    fn range(&self, sector: u64, len: usize) -> Option<u64> {
        let start = sector.checked_mul(SECTOR_BYTES)?;
        let end = start.checked_add(len as u64)?;
        ((len as u64).is_multiple_of(SECTOR_BYTES) && end <= self.capacity_sectors * SECTOR_BYTES)
            .then_some(start)
    }

    fn read_in(&mut self, sector: u64, w: &mut virtio_queue::Writer<'_>) -> (u32, usize) {
        let len = w.available_bytes();
        let Some(mut off) = self.range(sector, len) else {
            return (VIRTIO_BLK_S_IOERR, 0);
        };
        let mut done = 0;
        while done < len {
            let n = CHUNK_BYTES.min(len - done);
            if self.file.read_exact_at(&mut self.buf[..n], off).is_err()
                || w.write_all(&self.buf[..n]).is_err()
            {
                return (VIRTIO_BLK_S_IOERR, done);
            }
            done += n;
            off += n as u64;
        }
        (VIRTIO_BLK_S_OK, done)
    }

    fn write_out(&mut self, sector: u64, r: &mut virtio_queue::Reader<'_>) -> u32 {
        if self.read_only {
            return VIRTIO_BLK_S_IOERR;
        }
        let len = r.available_bytes();
        let Some(mut off) = self.range(sector, len) else {
            return VIRTIO_BLK_S_IOERR;
        };
        let mut done = 0;
        while done < len {
            let n = CHUNK_BYTES.min(len - done);
            if r.read_exact(&mut self.buf[..n]).is_err()
                || self.file.write_all_at(&self.buf[..n], off).is_err()
            {
                return VIRTIO_BLK_S_IOERR;
            }
            done += n;
            off += n as u64;
        }
        VIRTIO_BLK_S_OK
    }

    fn flush(&mut self) -> u32 {
        if self.read_only {
            return VIRTIO_BLK_S_OK;
        }
        match self.file.sync_data() {
            Ok(()) => VIRTIO_BLK_S_OK,
            Err(_) => VIRTIO_BLK_S_IOERR,
        }
    }

    fn get_id(&self, w: &mut virtio_queue::Writer<'_>) -> (u32, usize) {
        let mut id = [0u8; VIRTIO_BLK_ID_BYTES as usize];
        let src = self.id.as_bytes();
        let n = src.len().min(id.len());
        id[..n].copy_from_slice(&src[..n]);
        let n = w.available_bytes().min(id.len());
        match w.write_all(&id[..n]) {
            Ok(()) => (VIRTIO_BLK_S_OK, n),
            Err(_) => (VIRTIO_BLK_S_IOERR, 0),
        }
    }
}

impl VirtioDevice for Block {
    fn name(&self) -> &'static str {
        "virtio-blk"
    }

    fn device_type(&self) -> u32 {
        ID_BLOCK
    }

    fn features(&self) -> u64 {
        let mut f = (1u64 << VIRTIO_BLK_F_FLUSH)
            | (1u64 << VIRTIO_BLK_F_SIZE_MAX)
            | (1u64 << VIRTIO_BLK_F_SEG_MAX);
        if self.read_only {
            f |= 1 << VIRTIO_BLK_F_RO;
        }
        f
    }

    fn queue_max_sizes(&self) -> Vec<u16> {
        vec![QUEUE_MAX_SIZE]
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
            .ok_or_else(|| VmmError::device("virtio-blk", format!("no queue {index}")))?;
        let mut any = false;
        while let Some(chain) = q.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            let len = self.serve(chain, mem);
            q.add_used(mem, head, len)
                .map_err(|e| VmmError::device("virtio-blk", format!("used ring: {e}")))?;
            any = true;
        }
        Ok(any)
    }

    fn reset(&mut self) {}
}

impl std::fmt::Debug for Block {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Block")
            .field("path", &self.path)
            .field("read_only", &self.read_only)
            .field("capacity_sectors", &self.capacity_sectors)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::Device;
    use crate::devices::mmio::{MmioTransport, regs};
    use crate::devices::virtio::int;
    use crate::testing::{DriverQueue, bring_up, counting_irq, guest_memory, rd, scratch_dir, wr};
    use vm_memory::{Bytes, GuestAddress};

    const HDR: u64 = 0x40000;
    const DATA: u64 = 0x50000;
    const STATUS: u64 = 0x90000;

    struct Rig {
        t: MmioTransport<Block>,
        q: DriverQueue,
        mem: Mem,
        path: PathBuf,
    }

    fn rig(read_only: bool, sectors: u64) -> Rig {
        let dir = scratch_dir("blk");
        let path = dir.join("disk.img");
        let content: Vec<u8> = (0..sectors * SECTOR_BYTES)
            .map(|i| (i % 251) as u8)
            .collect();
        std::fs::write(&path, content).unwrap();
        let blk = Block::open(
            &DiskConfig {
                path: path.clone(),
                read_only,
            },
            3,
        )
        .unwrap();
        let mem = guest_memory(4 << 20);
        let (_, irq) = counting_irq();
        let mut t = MmioTransport::new(blk, mem.clone(), Interrupt::new(irq)).unwrap();
        let q = DriverQueue::new(0x10000, 16);
        bring_up(&mut t, &[&q]);
        Rig { t, q, mem, path }
    }

    fn header(mem: &Mem, kind: u32, sector: u64) {
        mem.write_obj(kind, GuestAddress(HDR)).unwrap();
        mem.write_obj(0u32, GuestAddress(HDR + 4)).unwrap();
        mem.write_obj(sector, GuestAddress(HDR + 8)).unwrap();
    }

    fn submit(r: &mut Rig, bufs: &[(u64, u32, bool)]) -> (u32, u8) {
        r.mem.write_obj(0xffu8, GuestAddress(STATUS)).unwrap();
        r.q.add(&r.mem, bufs);
        wr(&mut r.t, regs::QUEUE_NOTIFY, 0);
        let used = r.q.used(&r.mem);
        assert_eq!(used.len(), 1);
        (used[0].1, r.mem.read_obj(GuestAddress(STATUS)).unwrap())
    }

    #[test]
    fn vm_t9_read_write_flush_get_id() {
        let mut r = rig(false, 16);
        assert_eq!(rd(&mut r.t, regs::DEVICE_ID), ID_BLOCK);
        let mut cap = [0u8; 8];
        r.t.read(regs::CONFIG, &mut cap);
        assert_eq!(u64::from_le_bytes(cap), 16);
        assert!(!r.t.device().read_only());
        assert_eq!(r.t.device().capacity_sectors(), 16);
        assert!(format!("{:?}", r.t.device()).contains("disk.img"));

        // Read sectors 2..4 into a buffer split over two descriptors.
        header(&r.mem, VIRTIO_BLK_T_IN, 2);
        let (len, st) = submit(
            &mut r,
            &[
                (HDR, 16, false),
                (DATA, 512, true),
                (DATA + 0x1000, 512, true),
                (STATUS, 1, true),
            ],
        );
        assert_eq!((len, st), (1025, VIRTIO_BLK_S_OK as u8));
        let mut got = vec![0u8; 512];
        r.mem
            .read_slice(&mut got, GuestAddress(DATA + 0x1000))
            .unwrap();
        let want: Vec<u8> = (3 * 512..4 * 512).map(|i| (i % 251) as u8).collect();
        assert_eq!(got, want);
        assert_eq!(
            rd(&mut r.t, regs::INTERRUPT_STATUS) & int::USED_RING,
            int::USED_RING
        );

        // Write sector 5, then read it back from the file.
        r.mem.write_slice(&[7u8; 512], GuestAddress(DATA)).unwrap();
        header(&r.mem, VIRTIO_BLK_T_OUT, 5);
        let (len, st) = submit(
            &mut r,
            &[(HDR, 16, false), (DATA, 512, false), (STATUS, 1, true)],
        );
        assert_eq!((len, st), (1, VIRTIO_BLK_S_OK as u8));
        let file = std::fs::read(&r.path).unwrap();
        assert_eq!(&file[5 * 512..6 * 512], &[7u8; 512][..]);

        header(&r.mem, VIRTIO_BLK_T_FLUSH, 0);
        assert_eq!(
            submit(&mut r, &[(HDR, 16, false), (STATUS, 1, true)]),
            (1, VIRTIO_BLK_S_OK as u8)
        );

        header(&r.mem, VIRTIO_BLK_T_GET_ID, 0);
        let (len, st) = submit(
            &mut r,
            &[(HDR, 16, false), (DATA, 20, true), (STATUS, 1, true)],
        );
        assert_eq!((len, st), (21, VIRTIO_BLK_S_OK as u8));
        let mut id = [0u8; 13];
        r.mem.read_slice(&mut id, GuestAddress(DATA)).unwrap();
        assert_eq!(&id, b"moruna-disk-3");
    }

    #[test]
    fn vm_t9_large_requests_are_chunked() {
        let mut r = rig(false, 2048); // 1 MiB disk
        let len = 1u32 << 20;
        header(&r.mem, VIRTIO_BLK_T_IN, 0);
        let (used, st) = submit(
            &mut r,
            &[(HDR, 16, false), (0x100000, len, true), (STATUS, 1, true)],
        );
        assert_eq!((used, st), (len + 1, VIRTIO_BLK_S_OK as u8));
        let mut got = vec![0u8; len as usize];
        r.mem.read_slice(&mut got, GuestAddress(0x100000)).unwrap();
        assert!(got.iter().enumerate().all(|(i, b)| *b == (i % 251) as u8));
        r.mem
            .write_slice(&vec![3u8; len as usize], GuestAddress(0x100000))
            .unwrap();
        header(&r.mem, VIRTIO_BLK_T_OUT, 0);
        let (_, st) = submit(
            &mut r,
            &[(HDR, 16, false), (0x100000, len, false), (STATUS, 1, true)],
        );
        assert_eq!(st, VIRTIO_BLK_S_OK as u8);
        assert!(std::fs::read(&r.path).unwrap().iter().all(|b| *b == 3));
    }

    #[test]
    fn vm_t9_read_only_disk_refuses_writes() {
        let mut r = rig(true, 8);
        wr(&mut r.t, regs::DEVICE_FEATURES_SEL, 0);
        let f = rd(&mut r.t, regs::DEVICE_FEATURES);
        assert_ne!(f & (1 << VIRTIO_BLK_F_RO), 0);
        let before = std::fs::read(&r.path).unwrap();
        r.mem.write_slice(&[9u8; 512], GuestAddress(DATA)).unwrap();
        header(&r.mem, VIRTIO_BLK_T_OUT, 0);
        let (_, st) = submit(
            &mut r,
            &[(HDR, 16, false), (DATA, 512, false), (STATUS, 1, true)],
        );
        assert_eq!(st, VIRTIO_BLK_S_IOERR as u8);
        assert_eq!(std::fs::read(&r.path).unwrap(), before);
        header(&r.mem, VIRTIO_BLK_T_FLUSH, 0);
        assert_eq!(submit(&mut r, &[(HDR, 16, false), (STATUS, 1, true)]).1, 0);
        // A second read-only open shares the lock.
        let again = Block::open(
            &DiskConfig {
                path: r.path.clone(),
                read_only: true,
            },
            0,
        );
        assert!(again.is_ok());
    }

    #[test]
    fn vm_t9_bad_requests_get_a_status() {
        let mut r = rig(false, 8);
        // Past the end.
        header(&r.mem, VIRTIO_BLK_T_IN, 8);
        assert_eq!(
            submit(
                &mut r,
                &[(HDR, 16, false), (DATA, 512, true), (STATUS, 1, true)]
            )
            .1,
            VIRTIO_BLK_S_IOERR as u8
        );
        header(&r.mem, VIRTIO_BLK_T_OUT, u64::MAX);
        assert_eq!(
            submit(
                &mut r,
                &[(HDR, 16, false), (DATA, 512, false), (STATUS, 1, true)]
            )
            .1,
            VIRTIO_BLK_S_IOERR as u8
        );
        // Not a whole number of sectors.
        header(&r.mem, VIRTIO_BLK_T_IN, 0);
        assert_eq!(
            submit(
                &mut r,
                &[(HDR, 16, false), (DATA, 100, true), (STATUS, 1, true)]
            )
            .1,
            VIRTIO_BLK_S_IOERR as u8
        );
        // Unknown type.
        header(&r.mem, 99, 0);
        assert_eq!(
            submit(&mut r, &[(HDR, 16, false), (STATUS, 1, true)]),
            (1, VIRTIO_BLK_S_UNSUPP as u8)
        );
        // A short header.
        assert_eq!(
            submit(&mut r, &[(HDR, 8, false), (STATUS, 1, true)]).1,
            VIRTIO_BLK_S_IOERR as u8
        );
        // No writable byte for the status: returned with length 0, nothing written.
        header(&r.mem, VIRTIO_BLK_T_IN, 0);
        r.q.add(&r.mem, &[(HDR, 16, false)]);
        wr(&mut r.t, regs::QUEUE_NOTIFY, 0);
        assert_eq!(r.q.used(&r.mem)[0].1, 0);
        // A buffer outside guest memory: length 0, and the device keeps working.
        r.q.add(
            &r.mem,
            &[
                (HDR, 16, false),
                (0xffff_0000, 512, true),
                (STATUS, 1, true),
            ],
        );
        wr(&mut r.t, regs::QUEUE_NOTIFY, 0);
        assert_eq!(r.q.used(&r.mem)[0].1, 0);
        assert!(r.t.is_active());
        // A short GET_ID buffer gets as many bytes as fit.
        header(&r.mem, VIRTIO_BLK_T_GET_ID, 0);
        assert_eq!(
            submit(
                &mut r,
                &[(HDR, 16, false), (DATA, 4, true), (STATUS, 1, true)]
            ),
            (5, VIRTIO_BLK_S_OK as u8)
        );
        // No such queue.
        let mut qs: Vec<Queue> = vec![];
        assert!(r.t.device_mut().process_queue(1, &mut qs, &r.mem).is_err());
    }

    #[test]
    fn vm_t9_open_refusals() {
        let dir = scratch_dir("blk-open");
        let absent = Block::open(
            &DiskConfig {
                path: dir.join("absent"),
                read_only: true,
            },
            0,
        );
        assert!(matches!(
            absent,
            Err(VmmError::Config {
                field: "--disk",
                ..
            })
        ));
        // A read-write disk is exclusive: a second writer is refused naming the disk.
        let path = dir.join("d.img");
        std::fs::write(&path, [0u8; 1024]).unwrap();
        let cfg = DiskConfig {
            path: path.clone(),
            read_only: false,
        };
        let _first = Block::open(&cfg, 0).unwrap();
        match Block::open(&cfg, 1) {
            Err(VmmError::Config { field, msg }) => {
                assert_eq!(field, "--disk");
                assert!(msg.contains("in use"), "{msg}");
            }
            other => panic!("{other:?}"),
        }
    }
}
