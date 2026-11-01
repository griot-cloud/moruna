//! Original-completion asynchronous volume IO. No blocking driver call is made here.
//! The actor watches Device::async_wait_handle and repolls on actual channel signals.
use super::*;
use crate::device::{Completion, Poll, Ticket};
use alloc::rc::Rc;
use core::{cell::Cell, future::poll_fn, task::Poll as TaskPoll};
use moruna_svc::Rights;

/// Dropping a future before its complete publication/read leaves the volume closed.
/// The shared Controller independently retains every submitted DMA original.
pub(super) struct Transaction<'a> {
    pub(super) failed: &'a Cell<bool>,
    pub(super) complete: bool,
}
impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.failed.set(true);
        }
    }
}
impl Volume {
    /// A WAIT-only driver handle for this exact physical owner's executor watch.
    pub(crate) fn async_wait_handle(&self) -> Result<Handle, Refusal> {
        self.device.async_wait_handle().map_err(device_refusal)
    }
    /// Explicit bounded admission; configuration never creates a second connection.
    pub fn configure_async(&self, limit: usize) -> Result<(), Refusal> {
        self.device.configure_async(limit).map_err(device_refusal)
    }
    pub(crate) async fn sync_async(&mut self) -> Result<(), Refusal> {
        if self.failed || self.async_failed.get() {
            return Err(Refusal::Unavailable);
        }
        let mut transaction = Transaction {
            failed: &self.async_failed,
            complete: false,
        };
        flush(&self.device).await?;
        transaction.complete = true;
        Ok(())
    }
    pub(crate) async fn put_async(
        &mut self,
        object: &Object,
        condition: &Condition,
        source: Option<Loan>,
        bytes: u64,
        account: &Handle,
    ) -> Result<Metadata, Refusal> {
        if self.failed || self.async_failed.get() {
            return Err(Refusal::Unavailable);
        }
        self.object(object)?;
        if source.as_ref().map(Loan::len).unwrap_or(0) != bytes {
            return Err(Refusal::Invalid);
        }
        let a = self.journal.geometry().alignment as usize;
        let mut location = self
            .journal
            .plan(object, condition, bytes)
            .map_err(|_| Refusal::Capacity)?
            .ok_or(Refusal::Conflict)?;
        let leaves = bytes.div_ceil(a as u64);
        let mut unique = [0; 16];
        moruna_svc::random(&mut unique).map_err(|_| Refusal::Unavailable)?;
        let cell = self.journal.cell_bytes() as usize;
        let preview = Frame::Commit(Commit {
            sequence: self.journal.sequence(),
            metadata: Metadata {
                object: object.clone(),
                bytes,
                version: Version {
                    etag: Some(hex(&[0; 32])),
                    generation: Some(hex(&unique)),
                },
            },
            content_hash: [0; 32],
            location: location.clone(),
        })
        .encode()
        .map_err(|_| Refusal::Capacity)?;
        if preview.len() > cell {
            return Err(Refusal::Capacity);
        }
        // All large reservations precede original IO. Heap containers and actual backing
        // are independently charged; this reservation bounds digest metadata explicitly.
        let _budget = Memory::create_in(
            leaves.checked_mul(64).ok_or(Refusal::Capacity)?.max(1),
            account,
        )
        .map_err(|_| Refusal::Capacity)?;
        let mut scratch =
            PrivateMemory::create_in(a as u64, account).map_err(|_| Refusal::Capacity)?;
        let mut record_cell =
            PrivateMemory::create_in(cell as u64, account).map_err(|_| Refusal::Capacity)?;
        let mut receipt_cell =
            PrivateMemory::create_in(cell as u64, account).map_err(|_| Refusal::Capacity)?;
        let owner = owner(account)?;
        let mut digests = Vec::new();
        digests
            .try_reserve_exact(usize::try_from(leaves).map_err(|_| Refusal::Capacity)?)
            .map_err(|_| Refusal::Capacity)?;
        let mut parents = Vec::new();
        parents
            .try_reserve_exact(
                usize::try_from(leaves.div_ceil((a / 32) as u64)).map_err(|_| Refusal::Capacity)?,
            )
            .map_err(|_| Refusal::Capacity)?;
        let mut transaction = Transaction {
            failed: &self.async_failed,
            complete: false,
        };
        let mut hash = blake3::Hasher::new();
        if let Location::Extent {
            offset, tree, root, ..
        } = &mut location
        {
            for block in 0..leaves {
                let count = (bytes - block * a as u64).min(a as u64) as usize;
                let digest = scratch
                    .with_bytes(|data| {
                        data[..a].fill(0);
                        source
                            .as_ref()
                            .ok_or(Refusal::Invalid)?
                            .read(block * a as u64, &mut data[..count])
                            .map_err(|_| Refusal::Unavailable)?;
                        hash.update(&data[..count]);
                        leaf_digest(&data[..count], a).map_err(|_| Refusal::Integrity)
                    })
                    .map_err(|_| Refusal::Unavailable)??;
                write_private(
                    &self.device,
                    &owner,
                    *offset + block * a as u64,
                    &mut scratch,
                    a,
                )
                .await?;
                self.copied = self
                    .copied
                    .checked_add(count as u64)
                    .ok_or(Refusal::Capacity)?;
                digests.push(digest);
            }
            for level in tree {
                parents.clear();
                for (index, children) in digests.chunks(a / 32).enumerate() {
                    let digest = scratch
                        .with_bytes(|data| {
                            tree_node(&mut data[..a], children).map_err(|_| Refusal::Integrity)
                        })
                        .map_err(|_| Refusal::Unavailable)??;
                    write_private(
                        &self.device,
                        &owner,
                        level.offset + index as u64 * a as u64,
                        &mut scratch,
                        a,
                    )
                    .await?;
                    parents.push(digest);
                }
                core::mem::swap(&mut parents, &mut digests);
            }
            if let Some(digest) = digests.first() {
                *root = *digest;
            }
        } else {
            return Err(Refusal::Integrity);
        }
        let digest = *hash.finalize().as_bytes();
        let metadata = Metadata {
            object: object.clone(),
            bytes,
            version: Version {
                etag: Some(hex(&digest)),
                generation: Some(hex(&unique)),
            },
        };
        let commit = Commit {
            sequence: self.journal.sequence(),
            metadata: metadata.clone(),
            content_hash: digest,
            location,
        };
        self.journal
            .validate_commit(&commit)
            .map_err(|_| Refusal::Integrity)?;
        let record = Frame::Commit(commit)
            .encode()
            .map_err(|_| Refusal::Capacity)?;
        let receipt = Frame::Receipt {
            sequence: self.journal.sequence(),
            commit_hash: *blake3::hash(&record).as_bytes(),
        }
        .encode()
        .map_err(|_| Refusal::Capacity)?;
        if record.len() > cell || receipt.len() > cell {
            return Err(Refusal::Capacity);
        }
        record_cell
            .with_bytes(|data| {
                data.fill(0);
                data[..record.len()].copy_from_slice(&record);
            })
            .map_err(|_| Refusal::Unavailable)?;
        receipt_cell
            .with_bytes(|data| {
                data.fill(0);
                data[..receipt.len()].copy_from_slice(&receipt);
            })
            .map_err(|_| Refusal::Unavailable)?;
        flush(&self.device).await?;
        write_private(
            &self.device,
            &owner,
            self.journal.sequence(),
            &mut record_cell,
            cell,
        )
        .await?;
        flush(&self.device).await?;
        write_private(
            &self.device,
            &owner,
            self.journal.sequence() + cell as u64,
            &mut receipt_cell,
            cell,
        )
        .await?;
        flush(&self.device).await?;
        let applied = record_cell
            .with_bytes(|record| {
                receipt_cell
                    .with_bytes(|receipt| {
                        self.journal
                            .replay(&record[..cell], &receipt[..cell])
                            .map_err(|_| Refusal::Integrity)
                    })
                    .map_err(|_| Refusal::Unavailable)?
            })
            .map_err(|_| Refusal::Unavailable)??;
        if !applied {
            return Err(Refusal::Integrity);
        }
        transaction.complete = true;
        Ok(metadata)
    }
    pub(crate) async fn read_async(
        &mut self,
        object: &Object,
        version: &Version,
        offset: u64,
        length: u64,
        destination: Option<&Loan>,
        account: &Handle,
    ) -> Result<Outcome, Refusal> {
        if self.failed || self.async_failed.get() {
            return Err(Refusal::Unavailable);
        }
        self.object(object)?;
        let commit = self
            .journal
            .current(&object.key)
            .ok_or(Refusal::Missing)?
            .clone();
        if &commit.metadata.version != version {
            return Err(Refusal::Conflict);
        }
        if offset
            .checked_add(length)
            .is_none_or(|end| end > commit.metadata.bytes)
            || destination.map(Loan::len).unwrap_or(0) != length
        {
            return Err(Refusal::Invalid);
        }
        let metadata = commit.metadata.clone();
        if length == 0 {
            return Ok(Outcome::Read {
                metadata,
                offset,
                length,
            });
        }
        let a = self.journal.geometry().alignment as u64;
        let start = match &commit.location {
            Location::Extent { offset, .. } => *offset,
            _ => return Err(Refusal::Integrity),
        };
        let mut data = PrivateMemory::create_in(a, account).map_err(|_| Refusal::Capacity)?;
        let mut node = PrivateMemory::create_in(a, account).map_err(|_| Refusal::Capacity)?;
        let owner = owner(account)?;
        let mut transaction = Transaction {
            failed: &self.async_failed,
            complete: false,
        };
        for block in offset / a..(offset + length).div_ceil(a) {
            let mut proof = BlockProof::new(&commit, self.journal.geometry(), block)
                .map_err(|_| Refusal::Integrity)?;
            while let Some(step) = proof.next_node() {
                read_private(&self.device, &owner, step.offset, &mut node, a as usize).await?;
                node.with_bytes(|bytes| {
                    proof
                        .accept_node(&bytes[..a as usize])
                        .map_err(|_| Refusal::Integrity)
                })
                .map_err(|_| Refusal::Unavailable)??;
            }
            read_private(
                &self.device,
                &owner,
                start + block * a,
                &mut data,
                a as usize,
            )
            .await?;
            let copied = data
                .with_bytes(|bytes| {
                    proof
                        .verify_data(&bytes[..a as usize])
                        .map_err(|_| Refusal::Integrity)?;
                    let lo = offset.max(block * a);
                    let hi = (offset + length).min((block + 1) * a);
                    destination
                        .ok_or(Refusal::Invalid)?
                        .write(
                            lo - offset,
                            &bytes[(lo - block * a) as usize..(hi - block * a) as usize],
                        )
                        .map_err(|_| Refusal::Unavailable)?;
                    Ok::<_, Refusal>(hi - lo)
                })
                .map_err(|_| Refusal::Unavailable)??;
            self.copied = self.copied.checked_add(copied).ok_or(Refusal::Capacity)?;
        }
        transaction.complete = true;
        Ok(Outcome::Read {
            metadata,
            offset,
            length,
        })
    }
}
pub(super) fn owner(account: &Handle) -> Result<Rc<moruna_sys::Account>, Refusal> {
    Ok(Rc::new(moruna_sys::Account(
        account
            .duplicate(Rights::READ | Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER)
            .map_err(|_| Refusal::Denied)?,
    )))
}
async fn original(device: &Device, ticket: Ticket) -> Result<Completion, Refusal> {
    poll_fn(|_cx| match device.poll(&ticket) {
        Ok(Poll::Pending { .. }) => TaskPoll::Pending,
        Ok(Poll::Complete(completion)) => TaskPoll::Ready(Ok(completion)),
        Ok(Poll::Quarantined(cause)) | Err(cause) => TaskPoll::Ready(Err(device_refusal(cause))),
    })
    .await
}
pub(super) async fn flush(device: &Device) -> Result<(), Refusal> {
    let completion = original(device, device.submit_flush().map_err(device_refusal)?).await?;
    completion.result.map_err(device_refusal)?;
    if !completion.stable {
        return Err(Refusal::Integrity);
    }
    Ok(())
}
pub(super) async fn write_private(
    device: &Device,
    account: &Rc<moruna_sys::Account>,
    offset: u64,
    source: &mut PrivateMemory,
    length: usize,
) -> Result<(), Refusal> {
    let sector = device.info().sector_size as usize;
    let max = device.info().max_sectors as usize * sector;
    if !offset.is_multiple_of(sector as u64) || !length.is_multiple_of(sector) {
        return Err(Refusal::Invalid);
    }
    for at in (0..length).step_by(max) {
        let len = (length - at).min(max);
        let memory = dma(len as u64, &account.0).map_err(|_| Refusal::Capacity)?;
        let loan = Loan::new(memory, 0, len as u64, true).map_err(|_| Refusal::Unavailable)?;
        source
            .with_bytes(|bytes| loan.write(0, &bytes[at..at + len]))
            .map_err(|_| Refusal::Unavailable)?
            .map_err(|_| Refusal::Unavailable)?;
        let memory = loan
            .duplicate(
                Rights::READ | Rights::WRITE | Rights::MAP | Rights::DUPLICATE | Rights::TRANSFER,
            )
            .map_err(|_| Refusal::Unavailable)?;
        let ticket = device
            .submit_write(
                (offset + at as u64) / sector as u64,
                (len / sector) as u32,
                memory,
                account.clone(),
            )
            .map_err(device_refusal)?;
        let completion = original(device, ticket).await?;
        if completion.result.map_err(device_refusal)? != len as u64 {
            return Err(Refusal::Integrity);
        }
    }
    Ok(())
}
pub(super) async fn read_private(
    device: &Device,
    account: &Rc<moruna_sys::Account>,
    offset: u64,
    destination: &mut PrivateMemory,
    length: usize,
) -> Result<(), Refusal> {
    let sector = device.info().sector_size as usize;
    let max = device.info().max_sectors as usize * sector;
    if !offset.is_multiple_of(sector as u64) || !length.is_multiple_of(sector) {
        return Err(Refusal::Invalid);
    }
    for at in (0..length).step_by(max) {
        let len = (length - at).min(max);
        let memory = dma(len as u64, &account.0).map_err(|_| Refusal::Capacity)?;
        let ticket = device
            .submit_read(
                (offset + at as u64) / sector as u64,
                (len / sector) as u32,
                memory,
                account.clone(),
            )
            .map_err(device_refusal)?;
        let completion = original(device, ticket).await?;
        if completion.result.map_err(device_refusal)? != len as u64 {
            return Err(Refusal::Integrity);
        }
        let loan = Loan::new(
            completion.memory.ok_or(Refusal::Integrity)?,
            0,
            len as u64,
            false,
        )
        .map_err(|_| Refusal::Unavailable)?;
        destination
            .with_bytes(|bytes| loan.read(0, &mut bytes[at..at + len]))
            .map_err(|_| Refusal::Unavailable)?
            .map_err(|_| Refusal::Unavailable)?;
    }
    Ok(())
}
