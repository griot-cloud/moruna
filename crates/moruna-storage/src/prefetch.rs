//! Known-future object ranges, bounded by caller-owned arena reservations. This is a
//! source/scheduler integration port, not an inference from placement's decoded-batch IDs.
#![forbid(unsafe_code)]

use futures::{StreamExt, stream};
use moruna_kernel::Buffer;

use crate::{Access, Error, Governed, Object, Operation, Result, Version, cache::Cache};

/// An exact column range from the source plan, already charged to the run's arena.
pub struct PlannedRange {
    /// Object identity.
    pub object: Object,
    /// Pinned immutable object version.
    pub version: Version,
    /// Start of the range.
    pub offset: u64,
    /// The destination allocation; no hidden halo or bounce allocation.
    pub destination: Buffer,
    /// Known-future next-use order; lower is sooner.
    pub next_use: u64,
}
/// A completed prefetched range retains its allocation and complete identity.
pub struct ReadyRange {
    /// Source-plan identity.
    pub object: Object,
    /// Pinned version.
    pub version: Version,
    /// Byte offset.
    pub offset: u64,
    /// Populated original allocation.
    pub buffer: Buffer,
    /// Original next-use position.
    pub next_use: u64,
}
/// Read the nearest future ranges first, at most `parallel` in flight. Every request is
/// separately authorized, including two ranges of the same object. Dropping the future
/// drops in-flight SDK reads with their owned destinations; raw IO ownership is its adapter's.
pub async fn fetch(
    store: &Governed,
    access: &Access,
    ranges: Vec<PlannedRange>,
    parallel: usize,
    byte_budget: u64,
) -> Result<Vec<ReadyRange>> {
    fetch_from(store, access, ranges, parallel, byte_budget).await
}
/// Known-future ranges through the real governed encrypted read-through cache. A hit still
/// verifies current authority; misses fill/persist the same admitted destination.
pub async fn fetch_cached(
    cache: &Cache,
    access: &Access,
    ranges: Vec<PlannedRange>,
    parallel: usize,
    byte_budget: u64,
) -> Result<Vec<ReadyRange>> {
    fetch_from(cache, access, ranges, parallel, byte_budget).await
}
trait Ranges: Sync {
    fn read<'a>(
        &'a self,
        access: &'a Access,
        object: &'a Object,
        version: &'a Version,
        offset: u64,
        destination: Buffer,
    ) -> Operation<'a, Buffer>;
}
impl Ranges for Governed {
    fn read<'a>(
        &'a self,
        access: &'a Access,
        object: &'a Object,
        version: &'a Version,
        offset: u64,
        destination: Buffer,
    ) -> Operation<'a, Buffer> {
        Box::pin(Governed::read(
            self,
            access,
            object,
            version,
            offset,
            destination,
        ))
    }
}
impl Ranges for Cache {
    fn read<'a>(
        &'a self,
        access: &'a Access,
        object: &'a Object,
        version: &'a Version,
        offset: u64,
        destination: Buffer,
    ) -> Operation<'a, Buffer> {
        Box::pin(self.read_through(access, object, version, offset, destination))
    }
}
async fn fetch_from(
    store: &impl Ranges,
    access: &Access,
    mut ranges: Vec<PlannedRange>,
    parallel: usize,
    byte_budget: u64,
) -> Result<Vec<ReadyRange>> {
    if parallel == 0 {
        return Err(Error::Invalid("prefetch concurrency"));
    }
    let total = ranges.iter().try_fold(0u64, |n, r| {
        n.checked_add(r.destination.len() as u64)
            .ok_or(Error::Invalid("prefetch byte overflow"))
    })?;
    if total > byte_budget {
        return Err(Error::Invalid("prefetch reservation budget"));
    }
    ranges.sort_by_key(|r| r.next_use);
    let mut pending = stream::iter(ranges.into_iter().map(|range| async move {
        let buffer = store
            .read(
                access,
                &range.object,
                &range.version,
                range.offset,
                range.destination,
            )
            .await?;
        Ok::<_, Error>(ReadyRange {
            object: range.object,
            version: range.version,
            offset: range.offset,
            buffer,
            next_use: range.next_use,
        })
    }))
    .buffer_unordered(parallel);
    let mut out = Vec::new();
    while let Some(result) = pending.next().await {
        out.push(result?);
    }
    out.sort_by_key(|r| r.next_use);
    Ok(out)
}
