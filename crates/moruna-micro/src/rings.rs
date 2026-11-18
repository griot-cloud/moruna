//! Batches between the engine and a task through pre-shared regions and rings, with no copy
//! and no call per batch (`architecture/data-os.md` §5): stream P's module. Only stream P
//! writes this file and the files under `rings/`.
//!
//! The plane's side is `moruna_plane::RingPlane`. What is here is the engine's: laying a
//! batch out as a record in a task's region, and a payload over a record the task put,
//! decoded in place, whose pages go back to the plane when the last buffer over them is
//! dropped, wherever downstream that is.

use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use moruna_kernel::arrow::buffer::Buffer as ArrowBuffer;
use moruna_kernel::{MorunaError, Payload, Result, Tier};
use moruna_plane::BatchId;
use moruna_plane::ringed::View;
use moruna_sys::Error as KernelError;

/// Batches whose payloads downstream has dropped, for the stage to give back to its plane on
/// its next call.
#[derive(Clone, Debug, Default)]
pub struct Released(Arc<Mutex<Vec<BatchId>>>);

impl Released {
    /// Every batch given back since the last call.
    pub fn take(&self) -> Vec<BatchId> {
        std::mem::take(
            &mut *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    fn give(&self, batch: BatchId) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(batch);
    }
}

/// What keeps a record's pages for as long as a payload over them lives: the engine's
/// mapping of them, and the batch, given back when the last buffer goes.
struct Hold<M> {
    _mapping: M,
    batch: BatchId,
    released: Released,
}

impl<M> Drop for Hold<M> {
    fn drop(&mut self) {
        self.released.give(self.batch);
    }
}

fn kernel_error(msg: String) -> MorunaError {
    MorunaError::Kernel {
        stage: moruna_adapters::UNKNOWN_STAGE,
        seq: moruna_adapters::UNKNOWN_SEQ,
        msg,
        features: None,
    }
}

/// The record of `len` bytes at `ptr`, which `mapping` keeps mapped and no process writes
/// while it lives (DP-I4: the batch is read only everywhere once checked), decoded in place
/// as a payload: no copy. `batch` goes back on `released` when the last buffer is dropped.
///
/// # Safety
///
/// `ptr .. ptr + len` must lie inside the pages `mapping` keeps mapped and readable, and no
/// process may write them until `mapping` is dropped.
#[allow(unsafe_code)]
pub unsafe fn payload_over<M: Send + Sync + std::panic::RefUnwindSafe + 'static>(
    mapping: M,
    ptr: usize,
    len: u64,
    batch: BatchId,
    released: &Released,
    tier: Tier,
) -> Result<Payload> {
    let hold = Arc::new(Hold {
        _mapping: mapping,
        batch,
        released: released.clone(),
    });
    let at = NonNull::new(ptr as *mut u8)
        .ok_or_else(|| kernel_error("a record mapped at address 0".to_string()))?;
    // SAFETY: the caller's contract: the bytes are mapped and unwritten for as long as `hold`
    // lives, which every buffer over them keeps.
    let buffer = unsafe { ArrowBuffer::from_custom_allocation(at, len as usize, hold) };
    let batch = crate::in_place::decode(buffer)?;
    // SAFETY: the record's pages are host memory, mapped into this process.
    Ok(unsafe { Payload::table_in(batch, tier) })
}

/// Lays a record out at `at` of `view`: its framing, then each body buffer at its offset,
/// the gaps zero ([`crate::layout::pieces`]). The bytes of the body it copied.
pub fn write_record<V: View>(
    view: &V,
    at: u64,
    len: u64,
    framing: &[u8],
    placed: &[(usize, ArrowBuffer)],
) -> std::result::Result<u64, KernelError> {
    let body: Vec<(u64, &[u8])> = placed
        .iter()
        .map(|(offset, b)| (*offset as u64, b.as_slice()))
        .collect();
    let pieces = crate::layout::pieces(len, framing, &body).ok_or(KernelError::InvalidArgs)?;
    let zeros = [0u8; 4096];
    for piece in pieces {
        match piece {
            crate::layout::Piece::Bytes(offset, bytes) => view.write(at + offset, bytes)?,
            crate::layout::Piece::Zeros(offset, n) => {
                let mut x = 0;
                while x < n {
                    let k = (n - x).min(zeros.len() as u64);
                    view.write(at + offset + x, &zeros[..k as usize])?;
                    x += k;
                }
            }
        }
    }
    Ok(body.iter().map(|(_, b)| b.len() as u64).sum())
}
