//! Owned block IO used by the durable volume. The file implementation is only a format
//! fixture; Linux raw devices use the same transaction interface without buffered fallback.
#![forbid(unsafe_code)]
use crate::{Error, Operation, Result, Role, io};
use moruna_kernel::{Buffer, BufferView};
use std::os::unix::fs::FileExt;
use std::{
    fs::{File, OpenOptions},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

/// Exclusive block transaction. Every returned operation retains admission through its
/// actual original IO, independently of the transaction handle or consuming future.
pub trait Transaction: Send + Sync {
    /// Exact read into an owned destination.
    fn read(&self, offset: u64, dst: Buffer) -> Operation<'static, Buffer>;
    /// Exact immutable write.
    fn write(&self, offset: u64, src: BufferView) -> Operation<'static, ()>;
    /// Durable barrier covering all preceding completed writes.
    fn sync(&self) -> Operation<'static, ()>;
}
/// Explicit geometry and exclusive local admission; not an authorization boundary.
pub trait Blocks: Send + Sync {
    /// Actual configured/validated capacity.
    fn capacity(&self) -> u64;
    /// Required offset, pointer and length alignment.
    fn alignment(&self) -> usize;
    /// Truthful durability/fixture role.
    fn role(&self) -> Role;
    /// Refuse overlapping admission rather than outliving a cancelled original operation.
    fn begin(self: Arc<Self>) -> Result<Box<dyn Transaction>>;
}
#[cfg(target_os = "linux")]
impl Blocks for crate::raw::Device {
    fn capacity(&self) -> u64 {
        self.capacity()
    }
    fn alignment(&self) -> usize {
        self.alignment()
    }
    fn role(&self) -> Role {
        self.role()
    }
    fn begin(self: Arc<Self>) -> Result<Box<dyn Transaction>> {
        Ok(Box::new(self.writer()?))
    }
}
#[cfg(target_os = "linux")]
impl Transaction for crate::raw::Writer {
    fn read(&self, offset: u64, dst: Buffer) -> Operation<'static, Buffer> {
        self.read(offset, dst)
    }
    fn write(&self, offset: u64, src: BufferView) -> Operation<'static, ()> {
        self.write(offset, src)
    }
    fn sync(&self) -> Operation<'static, ()> {
        self.sync()
    }
}
/// Regular-file format proof, never advertised as raw/NVMe/direct IO.
pub struct FileBlocks {
    file: Arc<File>,
    capacity: u64,
    alignment: usize,
    busy: Arc<AtomicBool>,
}
impl FileBlocks {
    /// Create a new explicit fixture file; never truncate an existing volume.
    pub fn create(path: &Path, capacity: u64, alignment: usize) -> Result<Arc<Self>> {
        geometry(capacity, alignment)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| io("create block fixture", e))?;
        file.lock().map_err(|e| io("lock block fixture", e))?;
        file.set_len(capacity)
            .and_then(|_| file.sync_all())
            .map_err(|e| io("size block fixture", e))?;
        Ok(Arc::new(Self {
            file: Arc::new(file),
            capacity,
            alignment,
            busy: Arc::new(AtomicBool::new(false)),
        }))
    }
    /// Reopen an existing file for replay, preserving all bytes and its exclusive OS lock.
    pub fn open(path: &Path, alignment: usize) -> Result<Arc<Self>> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| io("open block fixture", e))?;
        let capacity = file
            .metadata()
            .map_err(|e| io("stat block fixture", e))?
            .len();
        geometry(capacity, alignment)?;
        file.lock().map_err(|e| io("lock block fixture", e))?;
        Ok(Arc::new(Self {
            file: Arc::new(file),
            capacity,
            alignment,
            busy: Arc::new(AtomicBool::new(false)),
        }))
    }
}
fn geometry(capacity: u64, alignment: usize) -> Result<()> {
    if alignment < 512
        || !alignment.is_power_of_two()
        || capacity == 0
        || !capacity.is_multiple_of(alignment as u64)
        || capacity > i64::MAX as u64
    {
        return Err(Error::Invalid("block fixture geometry"));
    }
    Ok(())
}
struct FileLease {
    owner: Arc<FileBlocks>,
}
impl Drop for FileLease {
    fn drop(&mut self) {
        self.owner.busy.store(false, Ordering::Release);
    }
}
struct FileTransaction(Arc<FileLease>);
impl Blocks for FileBlocks {
    fn capacity(&self) -> u64 {
        self.capacity
    }
    fn alignment(&self) -> usize {
        self.alignment
    }
    fn role(&self) -> Role {
        Role::FormatProof
    }
    fn begin(self: Arc<Self>) -> Result<Box<dyn Transaction>> {
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::Unavailable("block transaction still owns IO"))?;
        Ok(Box::new(FileTransaction(Arc::new(FileLease {
            owner: self,
        }))))
    }
}
impl FileLease {
    fn range(&self, offset: u64, len: usize) -> Result<()> {
        if !offset.is_multiple_of(self.owner.alignment as u64)
            || !len.is_multiple_of(self.owner.alignment)
            || offset
                .checked_add(len as u64)
                .is_none_or(|n| n > self.owner.capacity)
        {
            return Err(Error::Invalid("block fixture range"));
        }
        Ok(())
    }
}
impl Transaction for FileTransaction {
    fn read(&self, offset: u64, mut dst: Buffer) -> Operation<'static, Buffer> {
        let lease = Arc::clone(&self.0);
        Box::pin(async move {
            lease.range(offset, dst.len())?;
            if dst.host_ptr().is_none() {
                return Err(Error::Invalid("block fixture host destination"));
            }
            tokio::task::spawn_blocking(move || {
                lease
                    .owner
                    .file
                    .read_exact_at(dst.as_mut(), offset)
                    .map_err(|e| io("read block fixture", e))?;
                Ok(dst)
            })
            .await
            .map_err(|_| Error::Unavailable("block fixture read worker"))?
        })
    }
    fn write(&self, offset: u64, src: BufferView) -> Operation<'static, ()> {
        let lease = Arc::clone(&self.0);
        Box::pin(async move {
            lease.range(offset, src.len())?;
            tokio::task::spawn_blocking(move || {
                let bytes = src
                    .as_host_slice()
                    .ok_or(Error::Invalid("block fixture host source"))?;
                lease
                    .owner
                    .file
                    .write_all_at(bytes, offset)
                    .map_err(|e| io("write block fixture", e))
            })
            .await
            .map_err(|_| Error::Unavailable("block fixture write worker"))?
        })
    }
    fn sync(&self) -> Operation<'static, ()> {
        let lease = Arc::clone(&self.0);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                lease
                    .owner
                    .file
                    .sync_all()
                    .map_err(|e| io("sync block fixture", e))
            })
            .await
            .map_err(|_| Error::Unavailable("block fixture sync worker"))?
        })
    }
}
