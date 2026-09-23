//! `BufferView`, a shareable read-only view over bytes a reactor operation may read after the
//! call returns (contracts d.3). `unsafe` is permitted here for constructing a view over an
//! owner's bytes (section l).

use std::any::Any;
use std::sync::Arc;

use crate::buffer::Allocator;
use crate::error::{ConvertError, MorunaError};
use crate::tensor::ManagedTensor;
use crate::tier::Tier;

/// A read-only, `Send + 'static` view over bytes that a reactor operation may read
/// after the call returns (a DMA source). It owns a reference to whatever keeps the
/// bytes alive, so a failed operation loses nothing (RE-I1 drops the view, not the
/// bytes). All constructors are safe; the `unsafe` is inside this crate (l).
pub struct BufferView {
    ptr: *const u8,
    len: usize,
    tier: Tier,
    owner: Arc<dyn Any + Send + Sync>,
}

// SAFETY: the view only reads, and `owner` (which is `Send + Sync`) keeps the bytes alive and
// in `tier` for the view's lifetime; the raw pointer is never written through.
unsafe impl Send for BufferView {}
// SAFETY: as above; every method takes `&self` and reads only.
unsafe impl Sync for BufferView {}

impl BufferView {
    /// Wrap `len` bytes at `ptr` kept alive by `owner`.
    ///
    /// # Safety
    /// `ptr` must be valid for `len` bytes in `tier` for as long as `owner` lives.
    pub(crate) unsafe fn from_raw(
        ptr: *const u8,
        len: usize,
        tier: Tier,
        owner: Arc<dyn Any + Send + Sync>,
    ) -> BufferView {
        BufferView {
            ptr,
            len,
            tier,
            owner,
        }
    }

    /// Byte length.
    pub fn len(&self) -> usize {
        self.len
    }

    /// True when the view covers no bytes.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The tier the bytes are in.
    pub fn tier(&self) -> Tier {
        self.tier
    }

    /// Host-visible pointer; `None` for a device view.
    pub fn host_ptr(&self) -> Option<*const u8> {
        match self.tier {
            Tier::PinnedHost | Tier::Host => Some(self.ptr),
            Tier::Device(_) => None,
            Tier::Disk(_) => None,
            Tier::Remote(_, _) => None,
        }
    }

    /// Device pointer as an integer; `None` for a host view.
    pub fn device_ptr(&self) -> Option<u64> {
        match self.tier {
            Tier::Device(_) => Some(self.ptr as usize as u64),
            Tier::PinnedHost | Tier::Host => None,
            Tier::Disk(_) => None,
            Tier::Remote(_, _) => None,
        }
    }

    /// The bytes as a slice, for a host-tier view; `None` otherwise.
    pub fn as_host_slice(&self) -> Option<&[u8]> {
        let ptr = self.host_ptr()?;
        if self.len == 0 {
            return Some(&[]);
        }
        // SAFETY: a host-tier view's bytes are readable for `len` bytes while `owner` lives,
        // and nothing writes through a `BufferView`.
        Some(unsafe { std::slice::from_raw_parts(ptr, self.len) })
    }

    /// Over an Arrow buffer whose allocation the allocator owns (`contains`); the tier
    /// is the allocator's for that region. `Staging("not an arena buffer")` otherwise.
    pub fn of_arrow(
        buf: &arrow::buffer::Buffer,
        alloc: &dyn Allocator,
    ) -> crate::Result<BufferView> {
        let ptr = buf.as_ptr();
        if !alloc.contains(ptr) {
            return Err(MorunaError::Staging("not an arena buffer".into()));
        }
        let Some(tier) = alloc.tier_of(ptr) else {
            return Err(MorunaError::Staging("not an arena buffer".into()));
        };
        let owner: Arc<dyn Any + Send + Sync> = Arc::new(buf.clone());
        // SAFETY: `owner` is a clone of the Arrow buffer, which keeps its allocation (and so
        // the `len` bytes at `ptr`) alive for the view's lifetime; the allocator vouches for
        // the tier.
        Ok(unsafe { BufferView::from_raw(ptr, buf.len(), tier, owner) })
    }

    /// Over the whole contiguous byte range of a tensor; tier = the tensor's.
    pub fn of_tensor(t: &Arc<ManagedTensor>) -> crate::Result<BufferView> {
        if !t.is_contiguous() {
            return Err(MorunaError::Convert(ConvertError::NotContiguous));
        }
        let (base, offset) = t.data_ptr();
        let len = t.byte_len() as usize;
        let ptr = base.wrapping_add(offset as usize).cast_const();
        let owner: Arc<dyn Any + Send + Sync> = Arc::clone(t) as Arc<_>;
        // SAFETY: the tensor's bytes start at `data + byte_offset` and span `byte_len` bytes
        // in the tensor's tier (the DLPack contract); `owner` keeps the tensor alive.
        Ok(unsafe { BufferView::from_raw(ptr, len, t.tier(), owner) })
    }

    /// A sub-range of this view (for writing one page-aligned piece of a record).
    ///
    /// # Panics
    /// If `offset + len` exceeds this view's length, which is a programming error.
    pub fn slice(&self, offset: usize, len: usize) -> BufferView {
        assert!(
            offset.checked_add(len).is_some_and(|end| end <= self.len),
            "BufferView::slice: {offset}+{len} exceeds view length {}",
            self.len
        );
        BufferView {
            ptr: self.ptr.wrapping_add(offset),
            len,
            tier: self.tier,
            owner: Arc::clone(&self.owner),
        }
    }
}

impl std::fmt::Debug for BufferView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BufferView")
            .field("ptr", &self.ptr)
            .field("len", &self.len)
            .field("tier", &self.tier)
            .finish()
    }
}
