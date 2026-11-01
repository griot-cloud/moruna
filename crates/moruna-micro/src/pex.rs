//! Native PEX child's real account sampler and checked record-plane loop.
//! No cgroup/socket/host compatibility path exists here.
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{Allocator, MorunaError, ProcessPeak, Result, Sample, Sampler};
use moruna_svc::{Account, ObjectType, Rights};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
fn fail(message: impl Into<String>) -> MorunaError {
    MorunaError::Plan(format!("native PEX: {}", message.into()))
}
struct OwnAccount {
    account: Account,
    peak: AtomicU64,
}
impl Sampler for OwnAccount {
    fn memory_basis(&self) -> moruna_kernel::MemoryBasis {
        moruna_kernel::MemoryBasis::NativeAccountCharge
    }
    fn sample(&self) -> Result<Sample> {
        let usage = self
            .account
            .usage()
            .map_err(|e| fail(format!("reading own Account: {}", e.name())))?;
        self.peak.fetch_max(usage.peak, Ordering::AcqRel);
        Ok(Sample {
            anon_bytes: usage.used,
            peak_anon_bytes: usage.peak,
            ceiling_bytes: usage.limit,
            at_ns: moruna_svc::now(),
            ..Sample::default()
        })
    }
    fn reset_peak(&self) {}
    fn process_peak(&self) -> ProcessPeak {
        // A lifetime ACCOUNT peak is not one call's/process's exact interval mark.
        ProcessPeak {
            bytes: self.peak.load(Ordering::Acquire),
            exact: false,
            ..ProcessPeak::default()
        }
    }
}
/// Consume the real READ-only own-account observer once, validated before use.
/// The kernel charges this SAME Account to the task; names/figures confer no identity.
pub fn own_sampler() -> Result<(Arc<dyn Sampler>, u64)> {
    let handle = crate::start::take("account")
        .ok_or_else(|| fail("missing actual own Account start slot"))?;
    let (kind, rights) = handle.info().map_err(|e| fail(e.name()))?;
    if kind != ObjectType::Account
        || !rights.contains(Rights::READ)
        || rights.contains(Rights::WRITE)
        || rights.contains(Rights::DUPLICATE)
    {
        return Err(fail("own slot is not a READ Account"));
    }
    let sampler = Arc::new(OwnAccount {
        account: Account(handle),
        peak: AtomicU64::new(0),
    });
    let sample = sampler.sample()?;
    if sample.ceiling_bytes == 0 {
        return Err(fail("own Account has no positive limit"));
    }
    Ok((sampler, sample.ceiling_bytes))
}
/// Execute actual checked records on the task's native data plane. Input is copied into
/// child-owned memory before a user callable may retain it. Output copies are explicit;
/// the original SDK window is put once and only after every byte is written.
pub fn serve_records(
    alloc: &dyn Allocator,
    mut execute: impl FnMut(RecordBatch) -> Result<RecordBatch>,
) -> Result<()> {
    let plane =
        moruna_sys::plane::Plane::take().ok_or_else(|| fail("native plane already taken"))?;
    let mut input = plane.input(0).ok_or_else(|| fail("input already taken"))?;
    let mut output = plane
        .output(1)
        .ok_or_else(|| fail("output already taken"))?;
    while let Some(batch) = input
        .get()
        .map_err(|e| fail(format!("batch_get: {}", e.name())))?
    {
        let copy = moruna_kernel::arrow::buffer::Buffer::from(batch.bytes().to_vec());
        let request = moruna_kernel::ipc::decode(copy, crate::layout::PAGE as usize)?;
        let reply = execute(request)?;
        let (framing, pieces) =
            moruna_kernel::ipc::encode_framing(&reply, crate::layout::PAGE as usize, 0, alloc)?;
        let pieces_geometry = pieces
            .iter()
            .map(|(at, bytes)| (*at as u64, bytes.len() as u64))
            .collect::<Vec<_>>();
        let length = crate::layout::record_len(framing.len() as u64, &pieces_geometry)
            .ok_or_else(|| fail("reply record length overflow"))?;
        let mut window = output
            .window()
            .map_err(|e| fail(format!("output window: {}", e.name())))?
            .ok_or_else(|| fail("engine ended before reply window"))?;
        if length > window.len() {
            return Err(fail(format!(
                "reply needs {length} actual bytes, window has {}",
                window.len()
            )));
        }
        let bytes = &mut window.bytes()[..length as usize];
        bytes.fill(0);
        bytes[..framing.len()].copy_from_slice(&framing);
        for (at, piece) in pieces {
            let end = at
                .checked_add(piece.len())
                .ok_or_else(|| fail("reply piece overflow"))?;
            let destination = bytes
                .get_mut(at..end)
                .ok_or_else(|| fail("reply piece outside record"))?;
            destination.copy_from_slice(&piece);
        }
        window
            .put(0, length)
            .map_err(|(error, _window)| fail(format!("original batch_put: {}", error.name())))?;
    }
    Ok(())
}
