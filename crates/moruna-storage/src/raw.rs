//! Linux raw-device IO. Only an explicitly selected exclusive block device is opened.
//! The direct path uses O_DIRECT and the uring path reuses the reactor's original-operation
//! completion ownership; a cancelled consumer never frees a still-used arena buffer.

use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{FileExt, FileTypeExt, OpenOptionsExt};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::{Error, Operation, Result, Role, io};
use moruna_kernel::{Buffer, BufferView, Completion};

/// The selected IO path; no available-path fallback is hidden inside this adapter.
pub enum PathMode {
    /// Owned direct pread/pwrite on the blocking pool.
    Direct,
    /// A reactor configured with guaranteed direct IO and io_uring. Its RE-I1/RE-I7
    /// ownership is required: resolve only after the original operation completes.
    Uring(Arc<dyn Direct>),
}
/// Strict reactor capability minted only after its actual selected direct/uring paths
/// are guaranteed, not merely probed available. Binding verifies O_DIRECT on the owned FD.
pub trait Direct: Send + Sync {
    /// Bind one retained descriptor; implementations must refuse unavailable/no-fallback paths.
    fn bind(
        &self,
        fd: Arc<OwnedFd>,
    ) -> std::result::Result<Arc<dyn AlignedDevice>, Box<moruna_kernel::MorunaError>>;
}
/// A bound strict descriptor. No path reopen, buffered fallback or engine fallback is legal.
pub trait AlignedDevice: Send + Sync {
    /// Exact original-operation completion into the owned charged buffer.
    fn read(&self, offset: u64, dst: Buffer) -> Completion<Buffer>;
    /// Exact original-operation completion retaining the source view.
    fn write(&self, offset: u64, src: BufferView) -> Completion<()>;
}
enum Selected {
    Direct,
    Uring(Arc<dyn AlignedDevice>),
}
/// One exclusive raw device and its validated geometry.
pub struct Device {
    file: File,
    capacity: u64,
    alignment: usize,
    role: Role,
    mode: Selected,
    writer: Arc<AtomicBool>,
}

/// Exclusive transaction admission for this already exclusively opened raw device.
/// Submitted operations retain admission independently of this handle. A volume layer
/// must recover any partial transaction before it publishes another durable commit.
pub struct Writer {
    device: Arc<Device>,
    lease: Arc<WriterLease>,
}
struct WriterLease(Arc<AtomicBool>);
impl WriterLease {
    fn acquire(admission: &Arc<AtomicBool>) -> Result<Arc<Self>> {
        admission
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::Unavailable("raw writer still owns outstanding IO"))?;
        Ok(Arc::new(Self(Arc::clone(admission))))
    }
}
impl Drop for WriterLease {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
impl Writer {
    /// Read while retaining this transaction through original completion.
    pub fn read(&self, offset: u64, dst: Buffer) -> Operation<'static, Buffer> {
        self.device
            .read_owned(offset, dst, Some(Arc::clone(&self.lease)))
    }
    /// Write while retaining this transaction through original completion.
    pub fn write(&self, offset: u64, src: BufferView) -> Operation<'static, ()> {
        self.device
            .write_owned(offset, src, Arc::clone(&self.lease))
    }
    /// Flush under the same transaction admission; completion is not a commit parser.
    pub fn sync(&self) -> Operation<'static, ()> {
        self.device.sync_owned(Arc::clone(&self.lease))
    }
}
impl Device {
    /// Open an explicit block device O_DIRECT|O_EXCL, validate its actual capacity and
    /// logical sector size, and take the exclusive writer lock. Never opens a default disk.
    pub fn open(path: &Path, role: Role, mode: PathMode) -> Result<Arc<Self>> {
        if !matches!(
            role,
            Role::SingleNodeHome | Role::CheckpointMetadata | Role::CacheScratch
        ) {
            return Err(Error::Invalid("raw device role"));
        }
        if !path.is_absolute() {
            return Err(Error::Invalid("raw device must be explicit and absolute"));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_DIRECT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)
            .map_err(|e| io("open exclusive raw device", e))?;
        if !file
            .metadata()
            .map_err(|e| io("stat raw device", e))?
            .file_type()
            .is_block_device()
        {
            return Err(Error::Invalid("raw home requires a block device"));
        }
        file.lock().map_err(|e| io("lock raw device", e))?;
        let mut capacity = 0u64;
        let mut sector = 0i32;
        // SAFETY: this exclusively owned Linux block descriptor is live. BLKGETSIZE64 writes
        // exactly a u64 and BLKSSZGET exactly an int into these live local variables.
        let size_result =
            unsafe { libc::ioctl(file.as_raw_fd(), 0x8008_1272u32 as _, &raw mut capacity) };
        if size_result != 0 {
            return Err(io("read raw capacity", std::io::Error::last_os_error()));
        }
        // SAFETY: same live descriptor and exact-size local output as above.
        let sector_result =
            unsafe { libc::ioctl(file.as_raw_fd(), libc::BLKSSZGET, &raw mut sector) };
        if sector_result != 0 {
            return Err(io("read raw geometry", std::io::Error::last_os_error()));
        }
        let alignment = usize::try_from(sector)
            .map_err(|_| Error::Invalid("raw sector size"))?
            .max(4096);
        if !alignment.is_power_of_two()
            || capacity == 0
            || !capacity.is_multiple_of(alignment as u64)
        {
            return Err(Error::Invalid("raw capacity or sector alignment"));
        }
        let mode = match mode {
            PathMode::Direct => Selected::Direct,
            PathMode::Uring(capability) => {
                let fd = Arc::new(OwnedFd::from(
                    file.try_clone()
                        .map_err(|e| io("retain direct descriptor", e))?,
                ));
                Selected::Uring(capability.bind(fd).map_err(Error::Reactor)?)
            }
        };
        Ok(Arc::new(Self {
            file,
            capacity,
            alignment,
            role,
            mode,
            writer: Arc::new(AtomicBool::new(false)),
        }))
    }
    /// Consume the unique unopened-to-consumers direct device for explicit initial
    /// provisioning. Arc uniqueness proves no outstanding original/transaction alias;
    /// the OS O_EXCL descriptor and common formatter retain ownership through sync.
    /// Returns no plaintext cache/home store facade and never runs as mount fallback.
    pub fn provision(
        self: Arc<Self>,
        root: &moruna_storage_format::banks::Root,
        layout: Option<&moruna_storage_format::config::BootConfig>,
    ) -> std::result::Result<(), crate::provision::Error> {
        let device = Arc::try_unwrap(self).map_err(|_| crate::provision::Error::Invalid {
            phase: crate::provision::Phase::Preflight,
            reason: "raw provisioning requires unique original-IO ownership",
        })?;
        if !matches!(device.mode, Selected::Direct) {
            return Err(crate::provision::Error::Invalid {
                phase: crate::provision::Phase::Preflight,
                reason: "explicit direct provisioning mode required",
            });
        }
        crate::provision::initialize(
            &device.file,
            device.capacity,
            device.alignment,
            device.role,
            root,
            layout,
        )
    }
    /// Acquire local transaction admission without waiting or hiding an overlapping writer.
    /// The OS-level exclusive device ownership remains held independently by Device.
    pub fn writer(self: &Arc<Self>) -> Result<Writer> {
        let lease = WriterLease::acquire(&self.writer)?;
        Ok(Writer {
            device: Arc::clone(self),
            lease,
        })
    }
    /// Actual device capacity from BLKGETSIZE64.
    pub fn capacity(&self) -> u64 {
        self.capacity
    }
    /// Required offset, pointer and length alignment. No uncharged halo is allocated.
    pub fn alignment(&self) -> usize {
        self.alignment
    }
    /// Explicit configured role.
    pub fn role(&self) -> Role {
        self.role
    }
    fn range(&self, offset: u64, bytes: usize, pointer: usize) -> Result<()> {
        if !offset.is_multiple_of(self.alignment as u64)
            || !bytes.is_multiple_of(self.alignment)
            || !pointer.is_multiple_of(self.alignment)
            || offset
                .checked_add(bytes as u64)
                .is_none_or(|end| end > self.capacity || end > i64::MAX as u64)
        {
            return Err(Error::Invalid("raw capacity/alignment range"));
        }
        Ok(())
    }
    /// Exact direct range into charged aligned arena pages. Dropping this future detaches
    /// the blocking operation or forwarding completion, which retains its buffer/device
    /// until the original read is over; it does not free on a cancellation CQE.
    pub fn read(self: &Arc<Self>, offset: u64, dst: Buffer) -> Operation<'static, Buffer> {
        self.read_owned(offset, dst, None)
    }
    fn read_owned(
        self: &Arc<Self>,
        offset: u64,
        dst: Buffer,
        lease: Option<Arc<WriterLease>>,
    ) -> Operation<'static, Buffer> {
        let device = Arc::clone(self);
        Box::pin(async move {
            let pointer = dst
                .host_ptr()
                .ok_or(Error::Invalid("raw read needs host arena buffer"))?;
            device.range(offset, dst.len(), pointer as usize)?;
            match &device.mode {
                Selected::Direct => tokio::task::spawn_blocking(move || {
                    let _until_original_completion = lease;
                    let mut dst = dst;
                    device
                        .file
                        .read_exact_at(dst.as_mut(), offset)
                        .map_err(|e| io("raw direct read", e))?;
                    Ok(dst)
                })
                .await
                .map_err(|_| Error::Unavailable("raw read worker"))?,
                Selected::Uring(reactor) => {
                    let (sender, completion) = Completion::channel();
                    let original = reactor.read(offset, dst);
                    original.then(Box::new(move |result| {
                        let _until_original_completion = (device, lease);
                        sender.resolve(result);
                    }));
                    completion.await.map_err(|e| Error::Reactor(Box::new(e)))
                }
            }
        })
    }
    /// Exact direct write from a retained charged view, with the same original-completion
    /// ownership rule as read. Call `sync` before acknowledging a durable commit.
    fn write_owned(
        self: &Arc<Self>,
        offset: u64,
        src: BufferView,
        lease: Arc<WriterLease>,
    ) -> Operation<'static, ()> {
        let device = Arc::clone(self);
        Box::pin(async move {
            let pointer = src
                .host_ptr()
                .ok_or(Error::Invalid("raw write needs host arena view"))?;
            device.range(offset, src.len(), pointer as usize)?;
            match &device.mode {
                Selected::Direct => tokio::task::spawn_blocking(move || {
                    let _until_original_completion = lease;
                    let bytes = src
                        .as_host_slice()
                        .ok_or(Error::Invalid("raw write host view"))?;
                    device
                        .file
                        .write_all_at(bytes, offset)
                        .map_err(|e| io("raw direct write", e))
                })
                .await
                .map_err(|_| Error::Unavailable("raw write worker"))?,
                Selected::Uring(reactor) => {
                    let (sender, completion) = Completion::channel();
                    reactor.write(offset, src).then(Box::new(move |result| {
                        let _until_original_completion = (device, lease);
                        sender.resolve(result);
                    }));
                    completion.await.map_err(|e| Error::Reactor(Box::new(e)))
                }
            }
        })
    }
    /// Flush data and commit records to the device before claiming durability.
    fn sync_owned(self: &Arc<Self>, lease: Arc<WriterLease>) -> Operation<'static, ()> {
        let device = Arc::clone(self);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let _until_original_completion = lease;
                device.file.sync_all().map_err(|e| io("sync raw device", e))
            })
            .await
            .map_err(|_| Error::Unavailable("raw sync worker"))?
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Admission-only controlled proof: actual original-CQE retention is tested by the
    // reactor lifecycle suite and still requires the queued Linux integration proof.
    #[test]
    fn writer_admission_waits_for_every_owned_operation() {
        let admission = Arc::new(AtomicBool::new(false));
        let transaction = WriterLease::acquire(&admission).unwrap();
        let original_read = Arc::clone(&transaction);
        let original_write = Arc::clone(&transaction);
        drop(transaction);
        assert!(WriterLease::acquire(&admission).is_err());
        drop(original_read);
        assert!(WriterLease::acquire(&admission).is_err());
        drop(original_write);
        let next = WriterLease::acquire(&admission).unwrap();
        assert!(WriterLease::acquire(&admission).is_err());
        drop(next);
        assert!(!admission.load(Ordering::Acquire));
    }
}
