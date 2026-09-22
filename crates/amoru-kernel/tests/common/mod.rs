//! Test-only helpers shared by the CT tests.
//!
//! `FakeAllocator` here is a minimal, private stand-in for the testkit's fake (contracts d.15),
//! which feature F0.3 builds in this same pull request: CT-T2, CT-T4 and CT-T17 name
//! `FakeAllocator` and need it now. It tags real heap allocations with the requested tier, which
//! is exactly what those tests ask of it, and nothing else. F0.3 replaces it with the testkit's
//! fake.

#![allow(dead_code)]

use std::alloc::{Layout, alloc, dealloc};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use amoru_kernel::{
    ALIGNMENT, AllocStats, Allocator, AmoruError, ArenaHandle, Buffer, DType, Result, Tier,
};
use arrow::array::{ArrayRef, Float32Array, Int32Array, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;

/// A region this allocator handed out.
#[derive(Copy, Clone, Debug)]
struct Region {
    len: usize,
    layout: Layout,
    tier: Tier,
}

#[derive(Default)]
struct State {
    regions: BTreeMap<usize, Region>,
    host_in_use: u64,
    pinned_in_use: u64,
    device_in_use: [u64; 8],
}

/// A heap allocator that tags its buffers with the tier they were asked for, so tier inference,
/// `into_arrow_buffer` and `BufferView::of_arrow` work in a test without an arena.
#[derive(Clone)]
pub struct FakeAllocator {
    inner: Arc<Inner>,
}

struct Inner {
    state: Mutex<State>,
    allocations_total: AtomicU64,
    page_bytes: usize,
    pinned: bool,
}

impl FakeAllocator {
    /// A fresh allocator with a 4096-byte page and an unpinned host tier.
    pub fn new() -> FakeAllocator {
        FakeAllocator {
            inner: Arc::new(Inner {
                state: Mutex::new(State::default()),
                allocations_total: AtomicU64::new(0),
                page_bytes: 4096,
                pinned: false,
            }),
        }
    }

    /// The same allocator with a page-locked host tier, so its host tier is `PinnedHost`.
    pub fn pinned() -> FakeAllocator {
        FakeAllocator {
            inner: Arc::new(Inner {
                state: Mutex::new(State::default()),
                allocations_total: AtomicU64::new(0),
                page_bytes: 4096,
                pinned: true,
            }),
        }
    }

    /// Allocations made so far (CT-T4 asserts this does not change across a conversion).
    pub fn allocations_total(&self) -> u64 {
        self.inner.allocations_total.load(Ordering::SeqCst)
    }

    /// Bytes in use in one tier.
    pub fn in_use(&self, tier: Tier) -> u64 {
        let state = self.inner.state.lock().unwrap();
        match tier {
            Tier::Host => state.host_in_use,
            Tier::PinnedHost => state.pinned_in_use,
            Tier::Device(id) => state.device_in_use[id.0 as usize],
            Tier::Disk(_) | Tier::Remote(_, _) => 0,
        }
    }

    /// Allocate and return the buffer, the way the arena will.
    pub fn buffer(&self, bytes: usize, tier: Tier) -> Buffer {
        match self.alloc(bytes, tier) {
            Ok(buffer) => buffer,
            Err(e) => panic!("FakeAllocator could not allocate {bytes} bytes in {tier:?}: {e}"),
        }
    }

    /// An Arrow buffer over an arena region of this allocator, filled from `bytes`.
    pub fn arrow_buffer(&self, bytes: &[u8], tier: Tier) -> arrow::buffer::Buffer {
        let mut buffer = self.buffer(bytes.len().max(1), tier);
        buffer[..bytes.len()].copy_from_slice(bytes);
        match buffer.into_arrow_buffer() {
            Ok(b) => b.slice_with_length(0, bytes.len()),
            Err(e) => panic!("into_arrow_buffer: {e}"),
        }
    }
}

impl ArenaHandle for Inner {
    fn release(&self, ptr: *mut u8, len: usize, tier: Tier) {
        let mut state = self.state.lock().unwrap();
        // A `split_at` half releases part of a region; only the half that starts at the
        // region's own address frees it, and the accounting follows the released length.
        match tier {
            Tier::Host => state.host_in_use = state.host_in_use.saturating_sub(len as u64),
            Tier::PinnedHost => {
                state.pinned_in_use = state.pinned_in_use.saturating_sub(len as u64)
            }
            Tier::Device(id) => {
                let slot = &mut state.device_in_use[id.0 as usize];
                *slot = slot.saturating_sub(len as u64);
            }
            Tier::Disk(_) | Tier::Remote(_, _) => {}
        }
        if let Some(region) = state.regions.remove(&(ptr as usize)) {
            // SAFETY: the region was allocated with this layout by `alloc` below and is
            // released exactly once, by the buffer that owns its first byte.
            unsafe { dealloc(ptr, region.layout) };
            let _ = region.len;
        }
    }
}

impl Allocator for FakeAllocator {
    fn alloc(&self, bytes: usize, tier: Tier) -> Result<Buffer> {
        if !tier.is_resident() {
            return Err(AmoruError::Alloc {
                bytes: bytes as u64,
                tier,
                budget: 0,
                in_use: 0,
            });
        }
        let align = if bytes >= self.inner.page_bytes {
            self.inner.page_bytes
        } else {
            ALIGNMENT
        };
        let len = bytes.max(1);
        let Ok(layout) = Layout::from_size_align(len, align) else {
            return Err(AmoruError::Alloc {
                bytes: bytes as u64,
                tier,
                budget: 0,
                in_use: 0,
            });
        };
        // SAFETY: the layout has a non-zero size and a power-of-two alignment.
        let ptr = unsafe { alloc(layout) };
        if ptr.is_null() {
            return Err(AmoruError::Alloc {
                bytes: bytes as u64,
                tier,
                budget: 0,
                in_use: 0,
            });
        }
        // SAFETY: the region was just allocated; writing zeros initialises every byte.
        unsafe { std::ptr::write_bytes(ptr, 0, len) };
        {
            let mut state = self.inner.state.lock().unwrap();
            state
                .regions
                .insert(ptr as usize, Region { len, layout, tier });
            match tier {
                Tier::Host => state.host_in_use += len as u64,
                Tier::PinnedHost => state.pinned_in_use += len as u64,
                Tier::Device(id) => state.device_in_use[id.0 as usize] += len as u64,
                Tier::Disk(_) | Tier::Remote(_, _) => {}
            }
        }
        self.inner.allocations_total.fetch_add(1, Ordering::SeqCst);
        let handle: Arc<dyn ArenaHandle> = Arc::clone(&self.inner) as Arc<_>;
        // SAFETY: the region is valid for `len` bytes and is released to this allocator
        // exactly once, when the buffer (or its split halves) drop.
        Ok(unsafe { Buffer::from_raw(ptr, len, tier, handle) })
    }

    fn page_bytes(&self) -> usize {
        self.inner.page_bytes
    }

    fn stats(&self) -> AllocStats {
        let state = self.inner.state.lock().unwrap();
        AllocStats {
            host_in_use: state.host_in_use,
            pinned_in_use: state.pinned_in_use,
            device_in_use: state.device_in_use,
            allocations_total: self.inner.allocations_total.load(Ordering::SeqCst),
            payload_copies_total: 0,
            boundary_copies_total: 0,
        }
    }

    fn contains(&self, ptr: *const u8) -> bool {
        self.tier_of(ptr).is_some()
    }

    fn tier_of(&self, ptr: *const u8) -> Option<Tier> {
        let address = ptr as usize;
        let state = self.inner.state.lock().unwrap();
        state
            .regions
            .range(..=address)
            .next_back()
            .filter(|(base, region)| address < *base + region.len)
            .map(|(_, region)| region.tier)
    }

    fn is_pinned(&self) -> bool {
        self.inner.pinned
    }
}

/// A batch of `rows` with an int column, a float column and a string column, over ordinary
/// heap buffers.
pub fn mixed_batch(rows: usize) -> RecordBatch {
    let ints: ArrayRef = Arc::new(Int32Array::from_iter_values(0..rows as i32));
    let floats: ArrayRef = Arc::new(Float32Array::from_iter_values((0..rows).map(|i| i as f32)));
    let strings: ArrayRef = Arc::new(StringArray::from_iter_values(
        (0..rows).map(|i| format!("row-{i}")),
    ));
    let schema = Arc::new(Schema::new(vec![
        Field::new("i", DataType::Int32, false),
        Field::new("f", DataType::Float32, false),
        Field::new("s", DataType::Utf8, false),
    ]));
    match RecordBatch::try_new(schema, vec![ints, floats, strings]) {
        Ok(batch) => batch,
        Err(e) => panic!("mixed_batch: {e}"),
    }
}

/// A one-column batch of `values` in `tier`, its values buffer allocated from `alloc`.
pub fn int_batch_in(alloc: &FakeAllocator, values: &[i32], tier: Tier) -> RecordBatch {
    let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
    let buffer = alloc.arrow_buffer(&bytes, tier);
    let data = arrow::array::ArrayData::builder(DataType::Int32)
        .len(values.len())
        .add_buffer(buffer)
        .build();
    let data = match data {
        Ok(d) => d,
        Err(e) => panic!("int_batch_in: {e}"),
    };
    let column: ArrayRef = arrow::array::make_array(data);
    let schema = Arc::new(Schema::new(vec![Field::new("i", DataType::Int32, false)]));
    match RecordBatch::try_new(schema, vec![column]) {
        Ok(batch) => batch,
        Err(e) => panic!("int_batch_in: {e}"),
    }
}

/// Every dtype that has a zero-copy Arrow form (e.3).
pub fn arrow_dtypes() -> Vec<DType> {
    DType::ALL
        .into_iter()
        .filter(|d| d.arrow_type().is_some())
        .collect()
}

/// An arena token that owns nothing: for a test buffer over leaked memory.
struct NullArena;

impl ArenaHandle for NullArena {
    fn release(&self, _ptr: *mut u8, _len: usize, _tier: Tier) {}
}

/// A buffer of `len` zero bytes tagged with `tier`, over memory this process leaks, so a test
/// can reach a code path that a real allocation of that tier would need hardware for.
pub fn tagged_buffer(len: usize, tier: Tier) -> Buffer {
    let mut bytes = vec![0u8; len.max(1)];
    let ptr = bytes.as_mut_ptr();
    std::mem::forget(bytes);
    // SAFETY: the bytes are leaked, so they outlive every buffer over them, and `NullArena`
    // releases nothing.
    unsafe { Buffer::from_raw(ptr, len, tier, Arc::new(NullArena)) }
}
