//! Copy-bank durability transaction. Only the enclosing owner may choose liveness;
//! no wire request, client prefix list or caller-chosen vector reaches this module.
use super::async_io::{Transaction, owner};
use super::*;
/// Actual charged/preallocated bank-copy resources, prepared before freezing namespace
/// admission. The private mounted-owner token and generation cannot be caller forged.
pub(crate) struct CompactionResources {
    identity: alloc::rc::Rc<()>,
    expected_generation: u64,
    next: moruna_storage_format::banks::Root,
    journal: Journal,
    entries: Vec<Commit>,
    _index: Memory,
    data: PrivateMemory,
    proof_page: PrivateMemory,
    record_cell: PrivateMemory,
    receipt_cell: PrivateMemory,
    encoded_root: Vec<u8>,
    io: crate::device::ReservedIo,
}
impl Volume {
    /// Reserve actual copy/metadata/IO ownership BEFORE Registry.freeze. A change of
    /// physical-owner generation before consumption refuses and requires re-preparation.
    pub(crate) fn prepare_compaction(
        &self,
        account: &Handle,
    ) -> Result<CompactionResources, Refusal> {
        if !self.admission_open() {
            return Err(Refusal::Unavailable);
        }
        if self
            .device
            .pending_range(0, self.device.capacity())
            .map_err(device_refusal)?
            != 0
        {
            return Err(Refusal::Busy);
        }
        let next = self.root.next().map_err(|_| Refusal::Capacity)?;
        let journal = Journal::new(next.geometry().clone()).map_err(|_| Refusal::Integrity)?;
        let a = next.geometry().alignment as usize;
        let cell = journal.cell_bytes() as usize;
        let index = Memory::create_in(next.geometry().log_bytes.max(1), account)
            .map_err(|_| Refusal::Capacity)?;
        let capacity = usize::try_from(next.geometry().log_bytes / (2 * journal.cell_bytes()))
            .map_err(|_| Refusal::Capacity)?;
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(capacity)
            .map_err(|_| Refusal::Capacity)?;
        let io = crate::device::ReservedIo::new(&self.device, owner(account)?, a.max(cell))
            .map_err(device_refusal)?;
        Ok(CompactionResources {
            identity: self.owner_identity(),
            expected_generation: self.root.generation(),
            encoded_root: next.encode().map_err(|_| Refusal::Capacity)?,
            next,
            journal,
            entries,
            _index: index,
            data: PrivateMemory::create_in(a as u64, account).map_err(|_| Refusal::Capacity)?,
            proof_page: PrivateMemory::create_in(a as u64, account)
                .map_err(|_| Refusal::Capacity)?,
            record_cell: PrivateMemory::create_in(cell as u64, account)
                .map_err(|_| Refusal::Capacity)?,
            receipt_cell: PrivateMemory::create_in(cell as u64, account)
                .map_err(|_| Refusal::Capacity)?,
            io,
        })
    }
    /// Prune only unreachable reserved namespace objects using the sole internally
    /// collected, frozen liveness authority. All other objects are unconditionally kept.
    pub(crate) async fn compact_namespaces_async(
        &mut self,
        live: &crate::namespace::LiveSet,
        resources: CompactionResources,
    ) -> Result<(), Refusal> {
        if !live.matches_volume(self) {
            return Err(Refusal::Denied);
        }
        self.compact_selected_async(resources, Some(live)).await
    }
    async fn compact_selected_async(
        &mut self,
        resources: CompactionResources,
        live: Option<&crate::namespace::LiveSet>,
    ) -> Result<(), Refusal> {
        if self.failed || self.async_failed.get() {
            return Err(Refusal::Unavailable);
        }
        if !alloc::rc::Rc::ptr_eq(&resources.identity, &self.owner_identity)
            || resources.expected_generation != self.root.generation()
        {
            return Err(Refusal::Busy);
        }
        if self
            .device
            .pending_range(0, self.device.capacity())
            .map_err(device_refusal)?
            != 0
        {
            return Err(Refusal::Busy);
        }
        let CompactionResources {
            next,
            mut journal,
            mut entries,
            _index,
            mut data,
            mut proof_page,
            mut record_cell,
            mut receipt_cell,
            encoded_root,
            mut io,
            ..
        } = resources;
        let a = next.geometry().alignment as usize;
        let cell = journal.cell_bytes() as usize;
        let mut planned_data = journal.data_end();
        for old in self.journal.entries() {
            if old
                .metadata
                .object
                .key
                .starts_with(moruna_storage_format::namespace::ROOT_PREFIX)
            {
                if let Some(live) = live {
                    if !live.contains(&old.metadata)? {
                        continue;
                    }
                }
            }
            let new = next
                .geometry()
                .extent(planned_data, old.metadata.bytes)
                .map_err(|_| Refusal::Capacity)?;
            planned_data = journal.extent_end(&new).map_err(|_| Refusal::Integrity)?;
            entries.push(old.clone());
        }
        let pairs = (entries.len() as u64)
            .checked_mul(2 * journal.cell_bytes())
            .ok_or(Refusal::Capacity)?;
        if pairs > next.geometry().log_bytes {
            return Err(Refusal::Capacity);
        }
        let mut transaction = Transaction {
            failed: &self.async_failed,
            complete: false,
        };
        data.with_bytes(|bytes| bytes.fill(0))
            .map_err(|_| Refusal::Unavailable)?;
        // Durably revoke the inactive bank's previous authority before touching it.
        io.write(&self.device, next.slot(), &mut data, a)
            .await
            .map_err(device_refusal)?;
        io.flush(&self.device).await.map_err(device_refusal)?;
        // Clear and sync EVERY inactive receipt cell before copying, so a previous
        // generation's tail cannot become visible under a shorter new journal.
        for at in (next.geometry().log_offset..next.geometry().data_offset).step_by(a) {
            io.write(&self.device, at, &mut data, a)
                .await
                .map_err(device_refusal)?;
        }
        io.flush(&self.device).await.map_err(device_refusal)?;
        for old in entries {
            let mut location = journal
                .plan(&old.metadata.object, &Condition::Absent, old.metadata.bytes)
                .map_err(|_| Refusal::Capacity)?
                .ok_or(Refusal::Integrity)?;
            let (old_offset, old_padded, old_tree, old_root) = match &old.location {
                Location::Extent {
                    offset,
                    padded_bytes,
                    tree,
                    root,
                } => (*offset, *padded_bytes, tree, *root),
                _ => return Err(Refusal::Integrity),
            };
            let (new_offset, new_tree) = match &mut location {
                Location::Extent {
                    offset, tree, root, ..
                } => {
                    *root = old_root;
                    (*offset, tree)
                }
                _ => return Err(Refusal::Integrity),
            };
            // Validate every payload block against the ORIGINAL committed tree before
            // copying bytes. Public Version/content hash remain exactly unchanged.
            for block in 0..old_padded / a as u64 {
                let mut proof = BlockProof::new(&old, self.journal.geometry(), block)
                    .map_err(|_| Refusal::Integrity)?;
                while let Some(step) = proof.next_node() {
                    io.read(&self.device, step.offset, &mut proof_page, a)
                        .await
                        .map_err(device_refusal)?;
                    proof_page
                        .with_bytes(|bytes| {
                            proof
                                .accept_node(&bytes[..a])
                                .map_err(|_| Refusal::Integrity)
                        })
                        .map_err(|_| Refusal::Unavailable)??;
                }
                io.read(&self.device, old_offset + block * a as u64, &mut data, a)
                    .await
                    .map_err(device_refusal)?;
                data.with_bytes(|bytes| {
                    proof
                        .verify_data(&bytes[..a])
                        .map_err(|_| Refusal::Integrity)
                })
                .map_err(|_| Refusal::Unavailable)??;
                io.write(&self.device, new_offset + block * a as u64, &mut data, a)
                    .await
                    .map_err(device_refusal)?;
            }
            if old_tree.len() != new_tree.len() {
                return Err(Refusal::Integrity);
            }
            // Tree bytes themselves are immutable; data verification above has checked
            // all reachable paths. Rebase only their physical offsets in the Commit.
            for (old_level, new_level) in old_tree.iter().zip(new_tree.iter()) {
                if old_level.nodes != new_level.nodes {
                    return Err(Refusal::Integrity);
                }
                for node in 0..old_level.nodes {
                    io.read(
                        &self.device,
                        old_level.offset + node * a as u64,
                        &mut data,
                        a,
                    )
                    .await
                    .map_err(device_refusal)?;
                    io.write(
                        &self.device,
                        new_level.offset + node * a as u64,
                        &mut data,
                        a,
                    )
                    .await
                    .map_err(device_refusal)?;
                }
            }
            let commit = Commit {
                sequence: journal.sequence(),
                metadata: old.metadata,
                content_hash: old.content_hash,
                location,
            };
            journal
                .validate_commit(&commit)
                .map_err(|_| Refusal::Integrity)?;
            let record = Frame::Commit(commit)
                .encode()
                .map_err(|_| Refusal::Capacity)?;
            let receipt = Frame::Receipt {
                sequence: journal.sequence(),
                commit_hash: *blake3::hash(&record).as_bytes(),
            }
            .encode()
            .map_err(|_| Refusal::Capacity)?;
            if record.len() > cell || receipt.len() > cell {
                return Err(Refusal::Capacity);
            }
            record_cell
                .with_bytes(|bytes| {
                    bytes.fill(0);
                    bytes[..record.len()].copy_from_slice(&record);
                })
                .map_err(|_| Refusal::Unavailable)?;
            receipt_cell
                .with_bytes(|bytes| {
                    bytes.fill(0);
                    bytes[..receipt.len()].copy_from_slice(&receipt);
                })
                .map_err(|_| Refusal::Unavailable)?;
            io.flush(&self.device).await.map_err(device_refusal)?;
            io.write(&self.device, journal.sequence(), &mut record_cell, cell)
                .await
                .map_err(device_refusal)?;
            io.flush(&self.device).await.map_err(device_refusal)?;
            io.write(
                &self.device,
                journal.sequence() + cell as u64,
                &mut receipt_cell,
                cell,
            )
            .await
            .map_err(device_refusal)?;
            io.flush(&self.device).await.map_err(device_refusal)?;
            let applied = record_cell
                .with_bytes(|record| {
                    receipt_cell
                        .with_bytes(|receipt| {
                            journal
                                .replay(&record[..cell], &receipt[..cell])
                                .map_err(|_| Refusal::Integrity)
                        })
                        .map_err(|_| Refusal::Unavailable)?
                })
                .map_err(|_| Refusal::Unavailable)??;
            if !applied {
                return Err(Refusal::Integrity);
            }
        }
        // Every copied object and receipt is stable BEFORE the only authority switch.
        data.with_bytes(|bytes| {
            bytes.fill(0);
            bytes[..encoded_root.len()].copy_from_slice(&encoded_root);
        })
        .map_err(|_| Refusal::Unavailable)?;
        io.write(&self.device, next.slot(), &mut data, a)
            .await
            .map_err(device_refusal)?;
        io.flush(&self.device).await.map_err(device_refusal)?;
        // Retire the previous authority before returning or admitting later writes.
        data.with_bytes(|bytes| bytes.fill(0))
            .map_err(|_| Refusal::Unavailable)?;
        io.write(&self.device, self.root.slot(), &mut data, a)
            .await
            .map_err(device_refusal)?;
        io.flush(&self.device).await.map_err(device_refusal)?;
        self.journal = journal;
        self.root = next;
        transaction.complete = true;
        Ok(())
    }
}
