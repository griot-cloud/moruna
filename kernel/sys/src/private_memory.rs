//! Exclusive service-private staging. Never constructed from an incoming handle,
//! a shared Mapping, or DMA pages; export consumes the private owner.
use crate::{Error, Handle, Mapping, Memory, Result, map, raw};

/// Fresh memory charged to an explicitly supplied account. Unlike a received
/// writable Memory capability, this owner has never published a handle or DMA
/// loan, so a closure may safely borrow exclusive bytes. No Debug/Clone or handle
/// accessor exists. Dropping an unexported owner wipes its whole backing first.
pub struct PrivateMemory {
    memory: Option<Memory>,
    mapping: Option<Mapping<'static>>,
    bytes: usize,
    mapped: u64,
}
impl PrivateMemory {
    /// Allocate fresh non-DMA storage in the actual supplied account (KB5 ABI).
    /// The kernel checks account WRITE authority; no caller identity is inferred.
    pub fn create_in(bytes: u64, account: &Handle) -> Result<Self> {
        if bytes == 0 || bytes > isize::MAX as u64 {
            return Err(Error::InvalidArgs);
        }
        let mapped = bytes.checked_add(4095).ok_or(Error::InvalidArgs)? / 4096 * 4096;
        let length = usize::try_from(bytes).map_err(|_| Error::InvalidArgs)?;
        let memory = Memory::create_in(mapped, account)?;
        // Map before accepting any secrets. A map failure drops fresh zero pages;
        // after construction Drop can wipe without allocating another mapping.
        let mapping = memory.map_detached(0, mapped, map::WRITE)?;
        Ok(Self {
            memory: Some(memory),
            mapping: Some(mapping),
            bytes: length,
            mapped,
        })
    }
    /// Logical bytes, excluding hidden page padding.
    pub fn len(&self) -> usize {
        self.bytes
    }
    /// This allocation always contains at least one byte.
    pub fn is_empty(&self) -> bool {
        false
    }
    /// Borrow only during this closure. The result cannot contain a borrow of the
    /// temporary bytes; transferring or duplicating a handle is impossible here.
    pub fn with_bytes<T>(&mut self, operation: impl FnOnce(&mut [u8]) -> T) -> Result<T> {
        let mapping = self.mapping.as_ref().ok_or(Error::BadState)?;
        let address = usize::try_from(mapping.addr()).map_err(|_| Error::InvalidArgs)?;
        address.checked_add(self.bytes).ok_or(Error::InvalidArgs)?;
        // SAFETY: memory_create_in allocated fresh, initialized non-DMA pages.
        // This wrapper owns their only handle and exposes no duplication, mapping,
        // pointer or loan API. &mut self excludes a second closure. The mapping is
        // live for this call, aligned/non-null under the native mapping ABI, and
        // bytes <= isize::MAX. A reference cannot escape the generic closure result.
        // into_memory consumes this owner only after this borrow/mapping has ended.
        let bytes = unsafe { core::slice::from_raw_parts_mut(address as *mut u8, self.bytes) };
        Ok(operation(bytes))
    }
    /// Publish the allocation only after every exclusive borrow has ended. The
    /// returned Memory follows ordinary shared/volatile mapping rules thereafter.
    /// Export intentionally transfers responsibility for any remaining plaintext.
    pub fn into_memory(mut self) -> Memory {
        drop(self.mapping.take());
        self.memory
            .take()
            .expect("private memory owns its allocation")
    }
}
impl Drop for PrivateMemory {
    fn drop(&mut self) {
        if let Some(mapping) = &self.mapping {
            // Volatile stores cannot be removed as dead writes. No physical-media
            // sanitization is claimed; these are private RAM pages only.
            for offset in (0..self.mapped).step_by(8) {
                raw::store64(mapping.addr() + offset, 0);
            }
            core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
        }
    }
}
