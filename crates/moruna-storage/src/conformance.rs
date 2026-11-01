//! The same executable storage contract for bucket, durable-file and eventual raw adapters.
//! Running against memory is conformance of the adapter, not a cloud/disk performance proof.
#![forbid(unsafe_code)]

use crate::{Access, Condition, Error, Governed, Object, Result};
use moruna_kernel::{Allocator, Buffer};
use std::sync::Arc;

/// Run isolated contract cases under a caller-supplied unique prefix and real access context.
/// Tests may supply a controlled verifier; no permissive authorizer ships in the library.
pub async fn check(
    store: &Governed,
    access: &Access,
    alloc: &dyn Allocator,
    prefix: &Object,
) -> Result<Vec<&'static str>> {
    let object = |suffix: &str| Object {
        key: format!("{}/{suffix}", prefix.key),
        ..prefix.clone()
    };
    let mut cases = Vec::new();
    let first = object("range");
    let initial = store
        .write(access, &first, view(alloc, b"abcdefgh")?)
        .await?;
    let destination = buffer(alloc, 4)?;
    let pointer = destination.host_ptr();
    let got = store
        .read(access, &first, &initial.version, 2, destination)
        .await?;
    if got.host_ptr() != pointer || got.as_ref() != b"cdef" {
        return Err(Error::Integrity);
    }
    cases.push("ST-T1 exact range and original arena allocation");
    for offset in [1, initial.bytes] {
        let destination = buffer(alloc, 0)?;
        let pointer = destination.host_ptr();
        let empty = store
            .read(access, &first, &initial.version, offset, destination)
            .await?;
        if !empty.is_empty() || empty.host_ptr() != pointer {
            return Err(Error::Integrity);
        }
    }
    cases.push("ST-T1 empty range at unaligned offset and EOF");

    if !matches!(
        store.write(access, &first, view(alloc, b"other")?).await,
        Err(Error::Conflict)
    ) {
        return Err(Error::Integrity);
    }
    cases.push("ST-T1 immutable create conflict");
    let second = store
        .replace(
            access,
            &first,
            Condition::Match(initial.version.clone()),
            view(alloc, b"ABCDEFGH")?,
        )
        .await?;
    if !matches!(
        store
            .replace(
                access,
                &first,
                Condition::Match(initial.version),
                view(alloc, b"stale")?
            )
            .await,
        Err(Error::Conflict)
    ) {
        return Err(Error::Integrity);
    }
    cases.push("ST-T1 stale conditional replacement refused");
    let left = store.replace(
        access,
        &first,
        Condition::Match(second.version.clone()),
        view(alloc, b"winner-left")?,
    );
    let right = store.replace(
        access,
        &first,
        Condition::Match(second.version),
        view(alloc, b"winner-right")?,
    );
    let (left, right) = futures::join!(left, right);
    match (left, right) {
        (Ok(_), Err(Error::Conflict)) | (Err(Error::Conflict), Ok(_)) => {}
        _ => return Err(Error::Integrity),
    }
    cases.push("ST-T1 concurrent CAS has one winner");
    let empty = object("empty");
    let metadata = store.write(access, &empty, view(alloc, b"")?).await?;
    let got = store
        .read(access, &empty, &metadata.version, 0, buffer(alloc, 0)?)
        .await?;
    if !got.is_empty() || metadata.bytes != 0 {
        return Err(Error::Integrity);
    }
    cases.push("ST-T1 empty object");
    let nested = object("nested/leaf");
    store
        .write(access, &nested, view(alloc, b"nested")?)
        .await?;
    let found = store.list(access, prefix).await?;
    if ![&first, &empty, &nested]
        .iter()
        .all(|o| found.iter().any(|m| m.object == **o))
    {
        return Err(Error::Integrity);
    }
    cases.push("ST-T1 recursive prefix list");
    Ok(cases)
}
fn buffer(alloc: &dyn Allocator, bytes: usize) -> Result<Buffer> {
    crate::allocate_exact(alloc, bytes)
}
fn view(alloc: &dyn Allocator, bytes: &[u8]) -> Result<moruna_kernel::BufferView> {
    let mut buffer = buffer(alloc, bytes.len())?;
    buffer.as_mut().copy_from_slice(bytes);
    Ok(Arc::new(buffer).view())
}
