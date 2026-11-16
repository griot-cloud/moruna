// SPDX-License-Identifier: MPL-2.0

//! Frames in user space.
//!
//! In OSTD a frame is a physical page with a reference count and typed metadata. Here a
//! frame is a page of a chunk: a memory object the core holds, mapped in the personality's
//! region (`chunk`). Each page keeps its own count and metadata, and goes back to its chunk
//! when its last reference goes. Its "physical address" is its address in the personality,
//! and its chunk and offset are what the core maps into the program
//! (`super::vm_space`).

mod chunk;

use alloc::{sync::Arc, vec::Vec};
use core::{
    any::{Any, TypeId},
    fmt::Debug,
    ops::Range,
    sync::atomic::{AtomicUsize, Ordering},
};

use self::chunk::Chunk;
use super::{
    HasPaddr, HasSize, Infallible, PAGE_SIZE, Paddr, Split, VmReader, VmWriter,
    io::util::{HasVmReaderWriter, VmReaderWriterIdentity},
};
use crate::{Error, Result, sync::non_null::NonNullPtr};

/// A frame's metadata.
///
/// # Safety
///
/// `is_untyped` returns true only for types that also implement [`AnyUFrameMeta`].
pub unsafe trait AnyFrameMeta: Any + Send + Sync {
    /// Called when the frame's last reference goes, with a reader over its bytes. Not
    /// called in user space, where nothing needs it.
    fn on_drop(&mut self, _reader: &mut VmReader<Infallible>) {}

    /// Whether the frame holds untyped memory, which may be read and written.
    fn is_untyped(&self) -> bool {
        false
    }
}

/// The metadata of a frame holding untyped memory.
pub trait AnyUFrameMeta: AnyFrameMeta {}

/// Makes a type a frame's metadata.
#[macro_export]
macro_rules! impl_frame_meta_for {
    ($t:ty) => {
        // SAFETY: the type is not untyped.
        unsafe impl $crate::mm::frame::AnyFrameMeta for $t {}
    };
}

/// Makes a type the metadata of an untyped frame.
#[macro_export]
macro_rules! impl_untyped_frame_meta_for {
    ($t:ty) => {
        // SAFETY: `is_untyped` is true, and the type implements `AnyUFrameMeta`.
        unsafe impl $crate::mm::frame::AnyFrameMeta for $t {
            fn is_untyped(&self) -> bool {
                true
            }
        }
        impl $crate::mm::frame::AnyUFrameMeta for $t {}
    };
}

impl_untyped_frame_meta_for!(());

/// Bytes held in frames.
static HELD: AtomicUsize = AtomicUsize::new(0);

/// The bytes the personality may still allocate as frames: the usable memory its start
/// gave it (`boot::BootInfo::memory_regions`) less what frames hold.
pub fn free_bytes() -> usize {
    let total: usize = crate::boot::try_boot_info().map_or(0, |info| {
        info.memory_regions
            .iter()
            .filter(|r| r.typ() == crate::boot::memory_region::MemoryRegionType::Usable)
            .map(|r| r.len())
            .sum()
    });
    total.saturating_sub(HELD.load(Ordering::Relaxed))
}

/// The chunks frames come from now, and the bytes they hold.
pub fn chunks_held() -> (usize, usize) {
    chunk::held()
}

/// A page: its chunk, kept while the page is, its address and its metadata.
struct Page<M: ?Sized> {
    chunk: Arc<Chunk>,
    addr: usize,
    meta: M,
}

impl<M: ?Sized> Drop for Page<M> {
    fn drop(&mut self) {
        HELD.fetch_sub(PAGE_SIZE, Ordering::Relaxed);
        chunk::free(&self.chunk, (self.addr - self.chunk.base()) / PAGE_SIZE);
    }
}

/// A page of memory with metadata `M`, counted by reference.
pub struct Frame<M: AnyFrameMeta + ?Sized> {
    page: Arc<Page<M>>,
}

/// A frame of untyped memory.
pub type UFrame = Frame<dyn AnyUFrameMeta>;

impl<M: AnyFrameMeta + ?Sized> Debug for Frame<M> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Frame({:#x})", self.page.addr)
    }
}

impl<M: AnyFrameMeta + ?Sized> PartialEq for Frame<M> {
    fn eq(&self, other: &Self) -> bool {
        self.page.addr == other.page.addr
    }
}

impl<M: AnyFrameMeta + ?Sized> Eq for Frame<M> {}

impl<M: AnyFrameMeta + ?Sized> Clone for Frame<M> {
    fn clone(&self) -> Self {
        Self {
            page: self.page.clone(),
        }
    }
}

impl<M: AnyFrameMeta> Frame<M> {
    /// The frame's metadata.
    pub fn meta(&self) -> &M {
        &self.page.meta
    }
}

impl<M: AnyFrameMeta + ?Sized + MetaAsDyn> Frame<M> {
    /// The frame's metadata, untyped.
    pub fn dyn_meta(&self) -> &dyn AnyFrameMeta {
        self.page.meta.as_dyn()
    }
}

impl<M: AnyFrameMeta + ?Sized> Frame<M> {
    /// How many references the frame has.
    pub fn reference_count(&self) -> u64 {
        Arc::strong_count(&self.page) as u64
    }

    /// The object holding the frame, and the frame's offset in it: what the core maps.
    pub(crate) fn object_offset(&self) -> (u32, usize) {
        let chunk = &self.page.chunk;
        (chunk.raw(), self.page.addr - chunk.base())
    }
}

/// Metadata seen as `dyn AnyFrameMeta`, sized or not.
#[doc(hidden)]
pub trait MetaAsDyn {
    /// The metadata, untyped.
    fn as_dyn(&self) -> &dyn AnyFrameMeta;
}

impl<M: AnyFrameMeta> MetaAsDyn for M {
    fn as_dyn(&self) -> &dyn AnyFrameMeta {
        self
    }
}

impl MetaAsDyn for dyn AnyFrameMeta {
    fn as_dyn(&self) -> &dyn AnyFrameMeta {
        self
    }
}

impl MetaAsDyn for dyn AnyUFrameMeta {
    fn as_dyn(&self) -> &dyn AnyFrameMeta {
        self
    }
}

impl<M: AnyFrameMeta + ?Sized> HasPaddr for Frame<M> {
    fn paddr(&self) -> Paddr {
        self.page.addr
    }
}

impl<M: AnyFrameMeta + ?Sized> HasSize for Frame<M> {
    fn size(&self) -> usize {
        PAGE_SIZE
    }
}

impl<M: AnyFrameMeta> From<Frame<M>> for Frame<dyn AnyFrameMeta> {
    fn from(frame: Frame<M>) -> Self {
        Self { page: frame.page }
    }
}

impl<M: AnyUFrameMeta> From<Frame<M>> for UFrame {
    fn from(frame: Frame<M>) -> Self {
        Self { page: frame.page }
    }
}

impl From<UFrame> for Frame<dyn AnyFrameMeta> {
    fn from(frame: UFrame) -> Self {
        Self { page: frame.page }
    }
}

impl<M: AnyFrameMeta> TryFrom<Frame<dyn AnyFrameMeta>> for Frame<M> {
    type Error = Frame<dyn AnyFrameMeta>;

    fn try_from(frame: Frame<dyn AnyFrameMeta>) -> core::result::Result<Self, Self::Error> {
        if (&frame.page.meta as &dyn Any).type_id() != TypeId::of::<M>() {
            return Err(frame);
        }
        let raw = Arc::into_raw(frame.page) as *const Page<M>;
        // SAFETY: the metadata's type is `M`, so the page is a `Page<M>`, and the raw
        // pointer came from `Arc::into_raw`.
        Ok(Self {
            page: unsafe { Arc::from_raw(raw) },
        })
    }
}

impl<M: AnyUFrameMeta + ?Sized> HasVmReaderWriter for Frame<M> {
    type Types = VmReaderWriterIdentity;

    fn reader(&self) -> VmReader<'_, Infallible> {
        // SAFETY: the page is the personality's own memory, alive while `self` is.
        unsafe { VmReader::from_kernel_space(self.page.addr as *const u8, PAGE_SIZE) }
    }

    fn writer(&self) -> VmWriter<'_, Infallible> {
        // SAFETY: as for `reader`; untyped memory may be written through shared frames.
        unsafe { VmWriter::from_kernel_space(self.page.addr as *mut u8, PAGE_SIZE) }
    }
}

/// A borrowed frame: the frame, without a reference of its own.
pub struct FrameRef<'a, M: AnyFrameMeta + ?Sized> {
    inner: core::mem::ManuallyDrop<Frame<M>>,
    _marker: core::marker::PhantomData<&'a Frame<M>>,
}

impl<'a, M: AnyFrameMeta + ?Sized> FrameRef<'a, M> {
    pub(crate) fn new(frame: &'a Frame<M>) -> Self {
        Self {
            // SAFETY: a bitwise copy that is never dropped holds no count of its own, and
            // `'a` keeps the frame it copies alive.
            inner: core::mem::ManuallyDrop::new(unsafe { core::ptr::read(frame) }),
            _marker: core::marker::PhantomData,
        }
    }
}

impl<M: AnyFrameMeta + ?Sized> core::ops::Deref for FrameRef<'_, M> {
    type Target = Frame<M>;

    fn deref(&self) -> &Frame<M> {
        &self.inner
    }
}

impl<M: AnyFrameMeta + ?Sized> Debug for FrameRef<'_, M> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        Debug::fmt(&*self.inner, f)
    }
}

// SAFETY: a frame is an `Arc`, whose pointer is never null and points at its page, which
// is aligned to at least eight bytes (the `Arc`'s counts precede it).
unsafe impl<M: AnyFrameMeta> NonNullPtr for Frame<M> {
    type Target = core::marker::PhantomData<Self>;

    type Ref<'a>
        = FrameRef<'a, M>
    where
        Self: 'a;

    const ALIGN_BITS: u32 = 3;

    fn into_raw(self) -> core::ptr::NonNull<Self::Target> {
        let ptr = Arc::into_raw(self.page).cast_mut();
        // SAFETY: an `Arc`'s pointer is never null.
        unsafe { core::ptr::NonNull::new_unchecked(ptr) }.cast()
    }

    unsafe fn from_raw(raw: core::ptr::NonNull<Self::Target>) -> Self {
        // SAFETY: the caller gives a pointer from `into_raw`, whose count it owns.
        Self {
            page: unsafe { Arc::from_raw(raw.as_ptr().cast_const().cast::<Page<M>>()) },
        }
    }

    unsafe fn raw_as_ref<'a>(raw: core::ptr::NonNull<Self::Target>) -> Self::Ref<'a> {
        FrameRef {
            // SAFETY: as for `from_raw`; the copy is never dropped, so it takes no count.
            inner: core::mem::ManuallyDrop::new(Self {
                page: unsafe { Arc::from_raw(raw.as_ptr().cast_const().cast::<Page<M>>()) },
            }),
            _marker: core::marker::PhantomData,
        }
    }

    fn ref_as_raw(ptr_ref: Self::Ref<'_>) -> core::ptr::NonNull<Self::Target> {
        let ptr = Arc::as_ptr(&ptr_ref.inner.page).cast_mut();
        // SAFETY: as for `into_raw`.
        unsafe { core::ptr::NonNull::new_unchecked(ptr) }.cast()
    }
}

/// Contiguous frames with metadata `M`.
pub struct Segment<M: AnyFrameMeta + ?Sized> {
    frames: Vec<Frame<M>>,
}

/// A segment of untyped memory.
pub type USegment = Segment<dyn AnyUFrameMeta>;

impl<M: AnyFrameMeta + ?Sized> Debug for Segment<M> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "Segment({:#x}, {} pages)",
            self.paddr(),
            self.frames.len()
        )
    }
}

impl<M: AnyFrameMeta + ?Sized> Clone for Segment<M> {
    fn clone(&self) -> Self {
        Self {
            frames: self.frames.clone(),
        }
    }
}

impl<M: AnyFrameMeta + ?Sized> Segment<M> {
    /// The frames from byte `range.start` to `range.end` of the segment, page-aligned.
    ///
    /// # Panics
    ///
    /// If the range is not page-aligned or reaches past the segment.
    pub fn slice(&self, range: &Range<usize>) -> Self {
        assert!(range.start.is_multiple_of(PAGE_SIZE) && range.end.is_multiple_of(PAGE_SIZE));
        assert!(range.start <= range.end && range.end <= self.size());
        Self {
            frames: self.frames[range.start / PAGE_SIZE..range.end / PAGE_SIZE].to_vec(),
        }
    }

    /// The segment's first address and its end.
    pub fn paddr_range(&self) -> Range<Paddr> {
        self.paddr()..self.paddr() + self.size()
    }
}

impl<M: AnyFrameMeta + ?Sized> HasPaddr for Segment<M> {
    fn paddr(&self) -> Paddr {
        self.frames.first().map_or(0, HasPaddr::paddr)
    }
}

impl<M: AnyFrameMeta + ?Sized> HasSize for Segment<M> {
    fn size(&self) -> usize {
        self.frames.len() * PAGE_SIZE
    }
}

impl<M: AnyFrameMeta + ?Sized> Split for Segment<M> {
    fn split(mut self, offset: usize) -> (Self, Self) {
        assert!(offset.is_multiple_of(PAGE_SIZE) && 0 < offset && offset < self.size());
        let rest = self.frames.split_off(offset / PAGE_SIZE);
        (self, Self { frames: rest })
    }
}

impl<M: AnyFrameMeta + ?Sized> From<Frame<M>> for Segment<M> {
    fn from(frame: Frame<M>) -> Self {
        Self {
            frames: alloc::vec![frame],
        }
    }
}

impl<M: AnyFrameMeta> From<Segment<M>> for Segment<dyn AnyFrameMeta> {
    fn from(segment: Segment<M>) -> Self {
        Self {
            frames: segment.frames.into_iter().map(Into::into).collect(),
        }
    }
}

impl<M: AnyUFrameMeta> From<Segment<M>> for USegment {
    fn from(segment: Segment<M>) -> Self {
        Self {
            frames: segment.frames.into_iter().map(Into::into).collect(),
        }
    }
}

impl<M: AnyFrameMeta + ?Sized> IntoIterator for Segment<M> {
    type Item = Frame<M>;
    type IntoIter = alloc::vec::IntoIter<Frame<M>>;

    fn into_iter(self) -> Self::IntoIter {
        self.frames.into_iter()
    }
}

impl<M: AnyUFrameMeta + ?Sized> HasVmReaderWriter for Segment<M> {
    type Types = VmReaderWriterIdentity;

    fn reader(&self) -> VmReader<'_, Infallible> {
        // SAFETY: the frames are contiguous pages of one block, alive while `self` is.
        unsafe { VmReader::from_kernel_space(self.paddr() as *const u8, self.size()) }
    }

    fn writer(&self) -> VmWriter<'_, Infallible> {
        // SAFETY: as for `reader`.
        unsafe { VmWriter::from_kernel_space(self.paddr() as *mut u8, self.size()) }
    }
}

/// Options for allocating frames.
#[derive(Clone, Debug)]
pub struct FrameAllocOptions {
    zeroed: bool,
}

impl Default for FrameAllocOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameAllocOptions {
    /// Options that zero the frames.
    pub fn new() -> Self {
        Self { zeroed: true }
    }

    /// Sets whether the frames are zeroed.
    pub fn zeroed(&mut self, zeroed: bool) -> &mut Self {
        self.zeroed = zeroed;
        self
    }

    /// Allocates a frame.
    pub fn alloc_frame(&self) -> Result<Frame<()>> {
        self.alloc_frame_with(())
    }

    /// Allocates a frame with `metadata`.
    pub fn alloc_frame_with<M: AnyFrameMeta>(&self, metadata: M) -> Result<Frame<M>> {
        let mut meta = Some(metadata);
        let mut segment = self.alloc_segment_with(1, |_| meta.take().unwrap())?;
        Ok(segment.frames.pop().unwrap())
    }

    /// Allocates `nframes` contiguous frames.
    pub fn alloc_segment(&self, nframes: usize) -> Result<Segment<()>> {
        self.alloc_segment_with(nframes, |_| ())
    }

    /// Allocates `nframes` contiguous frames, each with metadata from `metadata_fn`.
    pub fn alloc_segment_with<M: AnyFrameMeta, F>(
        &self,
        nframes: usize,
        mut metadata_fn: F,
    ) -> Result<Segment<M>>
    where
        F: FnMut(Paddr) -> M,
    {
        let mut frames = Vec::new();
        frames
            .try_reserve_exact(nframes)
            .map_err(|_| Error::NoMemory)?;
        let (chunk, first, stale) = chunk::alloc(nframes)?;
        let base = chunk.base() + first * PAGE_SIZE;
        if self.zeroed && stale != 0 {
            // SAFETY: the pages were just taken from the chunk, which is mapped in the
            // personality, and nothing else refers to them.
            unsafe { core::ptr::write_bytes(base as *mut u8, 0, stale * PAGE_SIZE) };
        }
        HELD.fetch_add(nframes * PAGE_SIZE, Ordering::Relaxed);
        for i in 0..nframes {
            let addr = base + i * PAGE_SIZE;
            frames.push(Frame {
                page: Arc::new(Page {
                    chunk: chunk.clone(),
                    addr,
                    meta: metadata_fn(addr),
                }),
            });
        }
        Ok(Segment { frames })
    }
}
